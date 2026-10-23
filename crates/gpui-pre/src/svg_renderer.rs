use crate::{
    AssetSource, DevicePixels, IsZero, RenderImage, Result, SharedString, Size,
    swap_rgba_pa_to_bgra,
};
use image::Frame;
use resvg::tiny_skia::Pixmap;
use smallvec::SmallVec;
use std::{
    hash::Hash,
    sync::{Arc, LazyLock, OnceLock},
};

#[cfg(target_os = "macos")]
const EMOJI_FONT_FAMILIES: &[&str] = &["Apple Color Emoji", ".AppleColorEmojiUI"];

#[cfg(target_os = "windows")]
const EMOJI_FONT_FAMILIES: &[&str] = &["Segoe UI Emoji", "Segoe UI Symbol"];

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
const EMOJI_FONT_FAMILIES: &[&str] = &[
    "Noto Color Emoji",
    "Emoji One",
    "Twitter Color Emoji",
    "JoyPixels",
];

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
)))]
const EMOJI_FONT_FAMILIES: &[&str] = &[];

fn is_emoji_presentation(c: char) -> bool {
    static EMOJI_PRESENTATION_REGEX: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new("\\p{Emoji_Presentation}").unwrap());
    let mut buf = [0u8; 4];
    EMOJI_PRESENTATION_REGEX.is_match(c.encode_utf8(&mut buf))
}

fn font_has_char(db: &usvg::fontdb::Database, id: usvg::fontdb::ID, ch: char) -> bool {
    db.with_face_data(id, |font_data, face_index| {
        ttf_parser::Face::parse(font_data, face_index)
            .ok()
            .and_then(|face| face.glyph_index(ch))
            .is_some()
    })
    .unwrap_or(false)
}

fn select_emoji_font(
    ch: char,
    fonts: &[usvg::fontdb::ID],
    db: &usvg::fontdb::Database,
    families: &[&str],
) -> Option<usvg::fontdb::ID> {
    for family_name in families {
        let query = usvg::fontdb::Query {
            families: &[usvg::fontdb::Family::Name(family_name)],
            weight: usvg::fontdb::Weight(400),
            stretch: usvg::fontdb::Stretch::Normal,
            style: usvg::fontdb::Style::Normal,
        };

        let Some(id) = db.query(&query) else {
            continue;
        };

        if fonts.contains(&id) || !font_has_char(db, id, ch) {
            continue;
        }

        return Some(id);
    }

    None
}

/// When rendering SVGs, we render them at twice the size to get a higher-quality result.
pub const SMOOTH_SVG_SCALE_FACTOR: f32 = 2.;

#[derive(Clone, PartialEq, Hash, Eq)]
#[expect(missing_docs)]
pub struct RenderSvgParams {
    pub path: SharedString,
    pub size: Size<DevicePixels>,
}

#[derive(Clone)]
/// A struct holding everything necessary to render SVGs.
pub struct SvgRenderer {
    asset_source: Arc<dyn AssetSource>,
    usvg_options: Arc<usvg::Options<'static>>,
    untrusted_options: Arc<usvg::Options<'static>>,
}

/// A parsed SVG document that can be rasterized at any scale.
///
/// Produced by [`SvgRenderer::parse_svg`] and rasterized by
/// [`SvgRenderer::render_parsed`]. Parsing resolves fonts and converts text
/// to paths, so callers that need to rasterize the same SVG at multiple
/// scales should retain this value to avoid re-paying the parse cost.
pub struct ParsedSvg(usvg::Tree);

/// The size in which to rasterize the SVG.
#[derive(Clone, Copy)]
pub enum SvgSize {
    /// A width in device pixels. The SVG retains its aspect ratio.
    Size(Size<DevicePixels>),
    /// An exact width and height in device pixels.
    ExactSize(Size<DevicePixels>),
    /// A logical scaling factor to apply to the size provided by the SVG.
    ScaleFactor(f32),
}

impl From<f32> for SvgSize {
    fn from(scale_factor: f32) -> Self {
        Self::ScaleFactor(scale_factor)
    }
}

/// Biggest encoded image an untrusted SVG may pull in; the header bound below is what bites.
pub const UNTRUSTED_IMAGE_MAX_BYTES: u64 = 16_777_216 * 4;

/// Longest edge, in pixels, an untrusted data: image may name: the preview raster's own edge bound.
pub const UNTRUSTED_IMAGE_MAX_EDGE: usize = 8192;

/// Most pixels an untrusted data: image may name: the preview raster's own pixel bound.
pub const UNTRUSTED_IMAGE_MAX_PIXELS: usize = 16_777_216;

static SYSTEM_FONT_DB: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

/// The font resolver both option sets share: bundled fonts over the system database.
fn preview_font_resolver(
    asset_source: Arc<dyn AssetSource>,
    enriched_fontdb: Arc<OnceLock<Arc<usvg::fontdb::Database>>>,
) -> usvg::FontResolver<'static> {
    let default_font_resolver = usvg::FontResolver::default_font_selector();
    let font_resolver = Box::new({
        let asset_source = asset_source.clone();
        move |font: &usvg::Font, db: &mut Arc<usvg::fontdb::Database>| {
            if db.is_empty() {
                let fontdb = enriched_fontdb.get_or_init(|| {
                    let mut db = (**SYSTEM_FONT_DB).clone();
                    load_bundled_fonts(&*asset_source, &mut db);
                    fix_generic_font_families(&mut db);
                    Arc::new(db)
                });
                *db = fontdb.clone();
            }
            if let Some(id) = default_font_resolver(font, db) {
                return Some(id);
            }
            // fontdb doesn't recognize CSS system font keywords like "system-ui"
            // or "ui-sans-serif", so fall back to sans-serif before any face.
            let sans_query = usvg::fontdb::Query {
                families: &[usvg::fontdb::Family::SansSerif],
                ..Default::default()
            };
            db.query(&sans_query)
                .or_else(|| db.faces().next().map(|f| f.id))
        }
    });
    let default_fallback_selection = usvg::FontResolver::default_fallback_selector();
    let fallback_selection = Box::new(
        move |ch: char, fonts: &[usvg::fontdb::ID], db: &mut Arc<usvg::fontdb::Database>| {
            if is_emoji_presentation(ch) {
                if let Some(id) = select_emoji_font(ch, fonts, db.as_ref(), EMOJI_FONT_FAMILIES) {
                    return Some(id);
                }
            }

            default_fallback_selection(ch, fonts, db)
        },
    );
    usvg::FontResolver {
        select_font: font_resolver,
        select_fallback: fallback_selection,
    }
}

/// An image resolver for SVG from untrusted files: no file path loads, and an oversize data: image is refused.
fn untrusted_image_href_resolver() -> usvg::ImageHrefResolver<'static> {
    usvg::ImageHrefResolver {
        resolve_data: Box::new(|mime, data, _| {
            if data.len() as u64 > UNTRUSTED_IMAGE_MAX_BYTES {
                return None;
            }
            match mime {
                "image/jpg" | "image/jpeg" => Some(usvg::ImageKind::JPEG(data)),
                "image/png" => Some(usvg::ImageKind::PNG(data)),
                "image/gif" => Some(usvg::ImageKind::GIF(data)),
                "image/webp" => Some(usvg::ImageKind::WEBP(data)),
                _ => None,
            }
            .filter(|kind| {
                let (w, h) = match kind {
                    usvg::ImageKind::JPEG(data)
                    | usvg::ImageKind::PNG(data)
                    | usvg::ImageKind::GIF(data)
                    | usvg::ImageKind::WEBP(data) => match imagesize::blob_size(data) {
                        Ok(size) => (size.width, size.height),
                        Err(_) => return false,
                    },
                    usvg::ImageKind::SVG(_) => return false,
                };
                if let usvg::ImageKind::GIF(data) = kind
                    && !gif_frames_within_bounds(data)
                {
                    return false;
                }
                w <= UNTRUSTED_IMAGE_MAX_EDGE
                    && h <= UNTRUSTED_IMAGE_MAX_EDGE
                    && w.saturating_mul(h) <= UNTRUSTED_IMAGE_MAX_PIXELS
            })
        }),
        resolve_string: Box::new(|_, _| None),
    }
}

// Walks, never decodes: `GIF89a 0A00 0A00 00 00 00 | 21 F9 04 00 00 00 00 00 | 2C 0000 0000 0A00 0A00 00 02 01 00 00 | 3B`, a header with screen descriptor, an extension, an image and the trailer.
fn gif_frames_within_bounds(mut data: &[u8]) -> bool {
    fn take<'a>(data: &mut &'a [u8], count: usize) -> Option<&'a [u8]> {
        let (head, tail) = data.split_at_checked(count)?;
        *data = tail;
        Some(head)
    }
    fn table(data: &mut &[u8], flags: u8) -> Option<()> {
        if flags & 0x80 != 0 {
            take(data, 3 * (2usize << (flags & 7)))?;
        }
        Some(())
    }
    fn blocks(data: &mut &[u8]) -> Option<()> {
        loop {
            let count = take(data, 1)?[0] as usize;
            if count == 0 {
                return Some(());
            }
            take(data, count)?;
        }
    }
    let valid = |data: &mut &[u8]| -> Option<()> {
        let header = take(data, 13)?;
        if !matches!(&header[..6], b"GIF87a" | b"GIF89a") {
            return None;
        }
        table(data, header[10])?;
        let mut frames = 0;
        loop {
            match take(data, 1)?[0] {
                0x3b => return (frames > 0).then_some(()),
                0x21 => {
                    take(data, 1)?;
                    blocks(data)?;
                }
                0x2c => {
                    let descriptor = take(data, 9)?;
                    let word =
                        |at| u16::from_le_bytes([descriptor[at], descriptor[at + 1]]) as usize;
                    let (x, y, w, h) = (word(0), word(2), word(4), word(6));
                    if w == 0
                        || h == 0
                        || x + w > UNTRUSTED_IMAGE_MAX_EDGE
                        || y + h > UNTRUSTED_IMAGE_MAX_EDGE
                        || w * h > UNTRUSTED_IMAGE_MAX_PIXELS
                    {
                        return None;
                    }
                    table(data, descriptor[8])?;
                    take(data, 1)?;
                    blocks(data)?;
                    frames += 1;
                }
                _ => return None,
            }
        }
    };
    valid(&mut data).is_some()
}

impl SvgRenderer {
    /// Creates a new SVG renderer with the provided asset source.
    pub fn new(asset_source: Arc<dyn AssetSource>) -> Self {
        // Build the enriched font DB lazily on first SVG render rather than
        // eagerly at construction time. This avoids the expensive deep-clone
        // of the system font database for code paths that never render SVGs
        // (e.g. tests).
        let enriched_fontdb: Arc<OnceLock<Arc<usvg::fontdb::Database>>> = Arc::new(OnceLock::new());

        let options = usvg::Options {
            font_resolver: preview_font_resolver(asset_source.clone(), enriched_fontdb.clone()),
            ..Default::default()
        };
        let untrusted = usvg::Options {
            font_resolver: preview_font_resolver(asset_source.clone(), enriched_fontdb),
            image_href_resolver: untrusted_image_href_resolver(),
            ..Default::default()
        };
        Self {
            asset_source,
            usvg_options: Arc::new(options),
            untrusted_options: Arc::new(untrusted),
        }
    }

    /// Parses SVG data into a [`ParsedSvg`] that can be rasterized at any scale.
    pub fn parse_svg(&self, bytes: &[u8]) -> Result<ParsedSvg, usvg::Error> {
        usvg::Tree::from_data(bytes, &self.usvg_options).map(ParsedSvg)
    }

    /// Parses SVG from an untrusted file: an `<image href>` to a file loads nothing.
    pub fn parse_svg_untrusted(&self, bytes: &[u8]) -> Result<ParsedSvg, usvg::Error> {
        usvg::Tree::from_data(bytes, &self.untrusted_options).map(ParsedSvg)
    }

    /// Rasterizes a previously parsed SVG into an image buffer.
    pub fn render_parsed(
        &self,
        svg: &ParsedSvg,
        size: impl Into<SvgSize>,
    ) -> Result<Arc<RenderImage>, usvg::Error> {
        let (size, image_scale_factor) = match size.into() {
            SvgSize::Size(size) => (SvgSize::Size(size), 1.0),
            SvgSize::ExactSize(size) => (SvgSize::ExactSize(size), 1.0),
            SvgSize::ScaleFactor(scale_factor) => (
                SvgSize::ScaleFactor(scale_factor * SMOOTH_SVG_SCALE_FACTOR),
                SMOOTH_SVG_SCALE_FACTOR,
            ),
        };
        let pixmap = rasterize_tree(&svg.0, size)?;
        let mut buffer =
            image::ImageBuffer::from_raw(pixmap.width(), pixmap.height(), pixmap.take()).unwrap();

        for pixel in buffer.chunks_exact_mut(4) {
            swap_rgba_pa_to_bgra(pixel);
        }

        let mut image = RenderImage::new(SmallVec::from_const([Frame::new(buffer)]));
        image.scale_factor = image_scale_factor;
        Ok(Arc::new(image))
    }

    /// Renders the given bytes into an image buffer.
    pub fn render_single_frame(
        &self,
        bytes: &[u8],
        scale_factor: f32,
    ) -> Result<Arc<RenderImage>, usvg::Error> {
        let svg = self.parse_svg(bytes)?;
        self.render_parsed(&svg, scale_factor)
    }

    pub(crate) fn render_alpha_mask(
        &self,
        params: &RenderSvgParams,
        bytes: Option<&[u8]>,
    ) -> Result<Option<(Size<DevicePixels>, Vec<u8>)>> {
        anyhow::ensure!(!params.size.is_zero(), "can't render at a zero size");

        let render_pixmap = |bytes| {
            let pixmap = self.render_pixmap(bytes, SvgSize::Size(params.size))?;

            // Convert the pixmap's pixels into an alpha mask.
            let size = Size::new(
                DevicePixels(pixmap.width() as i32),
                DevicePixels(pixmap.height() as i32),
            );
            let alpha_mask = pixmap
                .pixels()
                .iter()
                .map(|p| p.alpha())
                .collect::<Vec<_>>();

            Ok(Some((size, alpha_mask)))
        };

        if let Some(bytes) = bytes {
            render_pixmap(bytes)
        } else if let Some(bytes) = self.asset_source.load(&params.path)? {
            render_pixmap(&bytes)
        } else {
            Ok(None)
        }
    }

    fn render_pixmap(&self, bytes: &[u8], size: SvgSize) -> Result<Pixmap, usvg::Error> {
        let tree = usvg::Tree::from_data(bytes, &self.usvg_options)?;
        rasterize_tree(&tree, size)
    }
}

fn rasterize_tree(tree: &usvg::Tree, size: SvgSize) -> Result<Pixmap, usvg::Error> {
    // Cap the size of the rendered pixmap to avoid texture allocation panics
    // Related issue: #56466
    const MAX_SIZE: f32 = 8192.0;

    let svg_size = tree.size();
    let (mut width, mut height) = match size {
        SvgSize::Size(size) => {
            let scale = i32::from(size.width) as f32 / svg_size.width();
            (svg_size.width() * scale, svg_size.height() * scale)
        }
        SvgSize::ExactSize(size) => (i32::from(size.width) as f32, i32::from(size.height) as f32),
        SvgSize::ScaleFactor(scale) => (svg_size.width() * scale, svg_size.height() * scale),
    };

    if width > MAX_SIZE {
        log::warn!("Attempted to render pixmap where width ({width}) > MAX_SIZE ({MAX_SIZE})");
    }
    if height > MAX_SIZE {
        log::warn!("Attempted to render pixmap where height ({height}) > MAX_SIZE ({MAX_SIZE})");
    }
    let scale = (MAX_SIZE / width).min(MAX_SIZE / height).min(1.0);
    width *= scale;
    height *= scale;

    // Render the SVG to a pixmap with the specified width and height.
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width as u32, height as u32)
        .ok_or(usvg::Error::InvalidSize)?;

    let transform = resvg::tiny_skia::Transform::from_scale(
        width / svg_size.width(),
        height / svg_size.height(),
    );

    resvg::render(tree, transform, &mut pixmap.as_mut());

    Ok(pixmap)
}

fn load_bundled_fonts(asset_source: &dyn AssetSource, db: &mut usvg::fontdb::Database) {
    let font_paths = [
        "fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf",
        "fonts/lilex/Lilex-Regular.ttf",
    ];
    for path in font_paths {
        match asset_source.load(path) {
            Ok(Some(data)) => db.load_font_data(data.into_owned()),
            Ok(None) => log::warn!("Bundled font not found: {path}"),
            Err(error) => log::warn!("Failed to load bundled font {path}: {error}"),
        }
    }
}

// fontdb defaults generic families to Microsoft fonts ("Arial", "Times New Roman")
// which aren't installed on most Linux systems. fontconfig normally overrides these,
// but when it fails the defaults remain and all generic family queries return None.
fn fix_generic_font_families(db: &mut usvg::fontdb::Database) {
    use usvg::fontdb::{Family, Query};

    let families_and_fallbacks: &[(Family<'_>, &str)] = &[
        (Family::SansSerif, "IBM Plex Sans"),
        // No serif font bundled; use sans-serif as best available fallback.
        (Family::Serif, "IBM Plex Sans"),
        (Family::Monospace, "Lilex"),
        (Family::Cursive, "IBM Plex Sans"),
        (Family::Fantasy, "IBM Plex Sans"),
    ];

    for (family, fallback_name) in families_and_fallbacks {
        let query = Query {
            families: &[*family],
            ..Default::default()
        };
        if db.query(&query).is_none() {
            match family {
                Family::SansSerif => db.set_sans_serif_family(*fallback_name),
                Family::Serif => db.set_serif_family(*fallback_name),
                Family::Monospace => db.set_monospace_family(*fallback_name),
                Family::Cursive => db.set_cursive_family(*fallback_name),
                Family::Fantasy => db.set_fantasy_family(*fallback_name),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    use usvg::fontdb::{Database, Family, Query};

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    const IBM_PLEX_REGULAR: &[u8] =
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");
    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    const LILEX_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf");

    #[test]
    fn renders_parsed_svg_at_requested_size() -> Result<()> {
        let renderer = SvgRenderer::new(Arc::new(()));
        let svg = renderer.parse_svg(
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="24pt" height="12pt"></svg>"#,
        )?;
        let requested_size = Size::new(DevicePixels(24), DevicePixels(12));
        let image = renderer.render_parsed(&svg, SvgSize::ExactSize(requested_size))?;

        assert_eq!(image.size(0), requested_size);
        Ok(())
    }

    #[test]
    fn preserves_aspect_ratio_for_width_constrained_size() -> Result<()> {
        let renderer = SvgRenderer::new(Arc::new(()));
        let svg = renderer.parse_svg(
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="24pt" height="12pt"></svg>"#,
        )?;
        let image = renderer.render_parsed(
            &svg,
            SvgSize::Size(Size::new(DevicePixels(24), DevicePixels(24))),
        )?;

        assert_eq!(image.size(0), Size::new(DevicePixels(24), DevicePixels(12)));
        Ok(())
    }

    #[test]
    fn untrusted_string_hrefs_load_nothing() {
        let resolver = untrusted_image_href_resolver();
        let options = usvg::Options::default();
        assert!((resolver.resolve_string)("/dev/zero", &options).is_none());
        assert!((resolver.resolve_string)("/etc/hostname", &options).is_none());
        assert!((resolver.resolve_string)("https://example.com/a.png", &options).is_none());
    }

    #[test]
    fn untrusted_data_images_stay_within_the_byte_bound() {
        let resolver = untrusted_image_href_resolver();
        let options = usvg::Options::default();
        let small = Arc::new(png_with_size(1, 1));
        let kept = (resolver.resolve_data)("image/png", small.clone(), &options);
        assert!(matches!(kept, Some(usvg::ImageKind::PNG(data)) if Arc::ptr_eq(&data, &small)));
        let mut oversized = decodable_png();
        oversized.resize((UNTRUSTED_IMAGE_MAX_BYTES + 1) as usize, 0);
        assert!(image::load_from_memory_with_format(&oversized, image::ImageFormat::Png).is_ok());
        let big = Arc::new(oversized);
        assert!((resolver.resolve_data)("image/png", big, &options).is_none());
        assert!((resolver.resolve_data)("image/svg+xml", small, &options).is_none());
    }

    #[test]
    fn untrusted_parse_ignores_a_device_href() {
        let renderer = SvgRenderer::new(Arc::new(()));
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\" fill=\"#3366cc\"/><image x=\"0\" y=\"0\" width=\"10\" height=\"10\" xlink:href=\"/dev/zero\"/></svg>".to_vec();
        let (parsed, waiting) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = parsed.send(renderer.parse_svg_untrusted(&svg).is_ok());
        });
        // Opening /dev/zero for reading never ends; returning at all proves nothing opened it.
        assert_eq!(
            waiting.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(true)
        );
    }

    fn decodable_png() -> Vec<u8> {
        use image::ImageEncoder;
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(&[255, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        encoded
    }

    #[test]
    fn untrusted_parse_keeps_a_small_data_image() {
        let renderer = SvgRenderer::new(Arc::new(()));
        let encoded = decodable_png();
        let data = Arc::new(encoded.clone());
        let resolver = untrusted_image_href_resolver();
        assert!(matches!(
            (resolver.resolve_data)("image/png", data, &usvg::Options::default()),
            Some(usvg::ImageKind::PNG(_))
        ));
        let url: String = encoded.iter().map(|b| format!("%{b:02X}")).collect();
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"><image width=\"10\" height=\"10\" href=\"data:image/png,{url}\"/></svg>"
        );
        let parsed = renderer.parse_svg_untrusted(svg.as_bytes()).unwrap();
        let rendered = renderer
            .render_parsed(
                &parsed,
                SvgSize::ExactSize(Size::new(DevicePixels(10), DevicePixels(10))),
            )
            .unwrap();
        let bytes = rendered.as_bytes(0).unwrap();
        assert_eq!(&bytes[(5 * 10 + 5) * 4..][..4], &[0, 0, 255, 255]);
    }

    fn png_with_size(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82];
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0]);
        bytes
    }

    fn gif_with_size(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0x70, 0, 0]);
        bytes.push(0x2c);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0, 2, 0, 0x3b]);
        bytes
    }

    fn jpeg_with_size(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08];
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        bytes
    }

    fn webp_with_size(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(b"VP8 ");
        bytes.extend_from_slice(&20u32.to_le_bytes());
        bytes.extend_from_slice(&[0x10, 0x6D, 0xD2, 0x9D, 0x01, 0x2A]);
        bytes.extend_from_slice(&(width & 0x3FFF).to_le_bytes());
        bytes.extend_from_slice(&(height & 0x3FFF).to_le_bytes());
        bytes
    }

    #[test]
    fn untrusted_liar_dimensions_are_refused() {
        let resolver = untrusted_image_href_resolver();
        let options = usvg::Options::default();
        let resolve = |mime: &str, data: Vec<u8>| {
            (resolver.resolve_data)(mime, Arc::new(data), &options).is_some()
        };
        assert!(!resolve("image/png", png_with_size(8193, 1)));
        assert!(!resolve("image/png", png_with_size(4097, 4097)));
        assert!(resolve("image/png", png_with_size(4096, 4096)));
        assert!(resolve("image/png", png_with_size(256, 256)));
        assert!(!resolve("image/gif", gif_with_size(8193, 1)));
        assert!(resolve("image/gif", gif_with_size(10, 10)));
        assert!(!resolve("image/jpeg", jpeg_with_size(8193, 1)));
        assert!(resolve("image/jpeg", jpeg_with_size(10, 10)));
        assert!(!resolve("image/webp", webp_with_size(9000, 9000)));
        assert!(!resolve("image/png", vec![0, 1, 2, 3]));
    }

    fn images(group: &usvg::Group) -> usize {
        group
            .children()
            .iter()
            .map(|node| match node {
                usvg::Node::Image(_) => 1,
                usvg::Node::Group(group) => images(group),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn untrusted_liar_header_never_reaches_the_decoder() {
        // A hand-built 30000 by 30000 header with an empty zlib stream and PNG's CRC-32 (reflected IEEE, from all ones): the gate must refuse it before any decode.
        const CRC32_REFLECTED_POLYNOMIAL: u32 = 0xEDB8_8320;
        const CRC32_INITIAL: u32 = u32::MAX;
        let chunk = |kind: &[u8; 4], body: &[u8]| {
            let mut crc = CRC32_INITIAL;
            for byte in kind.iter().chain(body) {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (CRC32_REFLECTED_POLYNOMIAL & 0u32.wrapping_sub(crc & 1));
                }
            }
            let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
            bytes.extend_from_slice(kind);
            bytes.extend_from_slice(body);
            bytes.extend_from_slice(&(!crc).to_be_bytes());
            bytes
        };
        let mut header = 30000u32.to_be_bytes().to_vec();
        header.extend_from_slice(&30000u32.to_be_bytes());
        header.extend_from_slice(&[8, 0, 0, 0, 0]);
        let mut encoded = b"\x89PNG\r\n\x1a\n".to_vec();
        encoded.extend(chunk(b"IHDR", &header));
        encoded.extend(chunk(
            b"IDAT",
            &[0x78, 0x01, 0x01, 0, 0, 0xFF, 0xFF, 0, 0, 0, 1],
        ));
        encoded.extend(chunk(b"IEND", &[]));
        let url: String = encoded.iter().map(|b| format!("%{b:02X}")).collect();
        let svg = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\" fill=\"#3366cc\"/><image x=\"0\" y=\"0\" width=\"10\" height=\"10\" href=\"data:image/png,{url}\"/></svg>").into_bytes();
        let renderer = SvgRenderer::new(Arc::new(()));
        let (done, waiting) = std::sync::mpsc::channel();
        let start = std::time::Instant::now();
        std::thread::spawn(move || {
            let outcome = (|| {
                let parsed = renderer.parse_svg_untrusted(&svg).ok()?;
                let kept = images(parsed.0.root());
                let image = renderer
                    .render_parsed(
                        &parsed,
                        SvgSize::ExactSize(Size::new(DevicePixels(10), DevicePixels(10))),
                    )
                    .ok()?;
                let bytes = image.as_bytes(0)?;
                let center = (5 * 10 + 5) * 4;
                Some((kept, bytes[center..center + 4].to_vec()))
            })();
            let _ = done.send((start.elapsed(), outcome));
        });
        match waiting.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok((elapsed, Some((kept, pixel)))) => {
                eprintln!("liar header decoded nothing in {elapsed:?}; center pixel {pixel:?}");
                assert_eq!(kept, 0, "the refused image never became a node");
                assert!(elapsed < std::time::Duration::from_secs(10));
                assert_eq!(pixel, vec![0xCC, 0x66, 0x33, 0xFF]);
            }
            Ok((elapsed, None)) => panic!("liar header rendered nothing after {elapsed:?}"),
            Err(_) => panic!("liar header render never returned"),
        }
    }

    #[test]
    fn untrusted_small_data_image_still_draws() {
        use image::ImageEncoder;
        let mut pixels = Vec::with_capacity(256 * 256 * 3);
        for _ in 0..256 * 256 {
            pixels.extend_from_slice(&[255, 0, 0]);
        }
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(&pixels, 256, 256, image::ExtendedColorType::Rgb8)
            .expect("a solid test image encodes");
        let url: String = encoded.iter().map(|b| format!("%{b:02X}")).collect();
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"256\" height=\"256\"><image x=\"0\" y=\"0\" width=\"256\" height=\"256\" href=\"data:image/png,{url}\"/></svg>"
        );
        let renderer = SvgRenderer::new(Arc::new(()));
        let parsed = renderer
            .parse_svg_untrusted(svg.as_bytes())
            .expect("a kept image parses");
        let image = renderer
            .render_parsed(
                &parsed,
                SvgSize::ExactSize(Size::new(DevicePixels(256), DevicePixels(256))),
            )
            .expect("a kept image renders");
        let bytes = image.as_bytes(0).expect("a rendered frame");
        let center = (128 * 256 + 128) * 4;
        assert_eq!(&bytes[center..center + 4], &[0, 0, 255, 255]);
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    fn db_with_bundled_fonts() -> Database {
        let mut db = Database::new();
        db.load_font_data(IBM_PLEX_REGULAR.to_vec());
        db.load_font_data(LILEX_REGULAR.to_vec());
        db
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn text_with_split_glyph_clusters_in_mixed_fonts_does_not_panic() {
        let mut db = Database::new();
        db.load_font_data(IBM_PLEX_REGULAR.to_vec());
        db.load_font_data(LILEX_REGULAR.to_vec());
        let options = usvg::Options {
            fontdb: std::sync::Arc::new(db),
            ..Default::default()
        };

        // A base letter followed by a stack of combining marks. Under HarfBuzz's
        // default cluster merging every mark glyph shares the base's byte index,
        // which is the "glyph splitting" condition that triggered the panic. The
        // chunk must use two different fonts so the buggy merge path runs.
        let zalgo = "e\u{0301}\u{0302}\u{0303}\u{0304}\u{0306}\u{0307}\u{0308}\u{030a}";
        let svg = format!(
            r#"<svg viewBox="0 0 200 200" xmlns="http://www.w3.org/2000/svg"><text font-family="Lilex" font-size="32">{zalgo}<tspan font-family="IBM Plex Sans">{zalgo}</tspan></text></svg>"#
        );

        // Before the fix this aborts via panic with a message like
        // "removal index (is 5) should be < len (is 5)".
        usvg::Tree::from_data(svg.as_bytes(), &options)
            .expect("SVG with mixed-font text should parse");
    }

    #[test]
    fn test_is_emoji_presentation() {
        let cases = [
            ("a", false),
            ("Z", false),
            ("1", false),
            ("#", false),
            ("*", false),
            ("漢", false),
            ("中", false),
            ("カ", false),
            ("©", false),
            ("♥", false),
            ("😀", true),
            ("✅", true),
            ("🇺🇸", true),
            // SVG fallback is not cluster-aware yet
            ("©️", false),
            ("♥️", false),
            ("1️⃣", false),
        ];
        for (s, expected) in cases {
            assert_eq!(
                is_emoji_presentation(s.chars().next().unwrap()),
                expected,
                "for char {:?}",
                s
            );
        }
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn fix_generic_font_families_sets_all_families() {
        let mut db = db_with_bundled_fonts();
        fix_generic_font_families(&mut db);

        let families = [
            Family::SansSerif,
            Family::Serif,
            Family::Monospace,
            Family::Cursive,
            Family::Fantasy,
        ];

        for family in families {
            let query = Query {
                families: &[family],
                ..Default::default()
            };
            assert!(
                db.query(&query).is_some(),
                "Expected generic family {family:?} to resolve after fix_generic_font_families"
            );
        }
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn test_select_emoji_font_skips_family_without_glyph() {
        let mut db = db_with_bundled_fonts();

        let ibm_plex_sans = db
            .query(&usvg::fontdb::Query {
                families: &[usvg::fontdb::Family::Name("IBM Plex Sans")],
                weight: usvg::fontdb::Weight(400),
                stretch: usvg::fontdb::Stretch::Normal,
                style: usvg::fontdb::Style::Normal,
            })
            .unwrap();
        let lilex = db
            .query(&usvg::fontdb::Query {
                families: &[usvg::fontdb::Family::Name("Lilex")],
                weight: usvg::fontdb::Weight(400),
                stretch: usvg::fontdb::Stretch::Normal,
                style: usvg::fontdb::Style::Normal,
            })
            .unwrap();
        let selected = select_emoji_font('│', &[], &db, &["IBM Plex Sans", "Lilex"]).unwrap();

        assert_eq!(selected, lilex);
        assert!(!font_has_char(&db, ibm_plex_sans, '│'));
        assert!(font_has_char(&db, selected, '│'));
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn fix_generic_font_families_monospace_resolves_to_lilex() {
        let mut db = db_with_bundled_fonts();
        fix_generic_font_families(&mut db);

        let query = Query {
            families: &[Family::Monospace],
            ..Default::default()
        };
        let id = db.query(&query).expect("Monospace should resolve");
        let face = db.face(id).expect("Face should exist");
        assert!(
            face.families.iter().any(|(name, _)| name.contains("Lilex")),
            "Monospace should map to Lilex, got {:?}",
            face.families
        );
    }

    #[test]
    fn fix1_d5_gif_frame_bounds_override_a_small_logical_screen() {
        let mut encoded = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut encoded);
            encoder
                .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                    8193,
                    1,
                    image::Rgba([255, 0, 0, 255]),
                )))
                .unwrap();
        }
        encoded[6..10].copy_from_slice(&[1, 0, 1, 0]);
        let dimensions = imagesize::blob_size(&encoded).unwrap();
        assert_eq!((dimensions.width, dimensions.height), (1, 1));
        let resolver = untrusted_image_href_resolver();
        assert!(
            (resolver.resolve_data)("image/gif", Arc::new(encoded), &usvg::Options::default())
                .is_none()
        );
        let mut valid = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut valid);
            encoder
                .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                    2,
                    1,
                    image::Rgba([255, 0, 0, 255]),
                )))
                .unwrap();
        }
        assert!(
            (resolver.resolve_data)("image/gif", Arc::new(valid), &usvg::Options::default())
                .is_some()
        );
    }

    #[test]
    fn fix1_d5_gif_checks_later_frame_offsets_area_and_truncation() {
        let first = gif_with_size(1, 1);
        let mut later = first[..first.len() - 1].to_vec();
        let frame = gif_with_size(8193, 1);
        later.extend_from_slice(&frame[13..]);
        assert!(!gif_frames_within_bounds(&later));
        for (at, value) in [(18, 4097u16), (16, 8192u16)] {
            let mut bad = gif_with_size(4097, 1);
            if at == 18 {
                bad[20..22].copy_from_slice(&4097u16.to_le_bytes());
            }
            bad[at..at + 2].copy_from_slice(&value.to_le_bytes());
            assert!(!gif_frames_within_bounds(&bad));
        }
        assert!(gif_frames_within_bounds(&first));
        for end in 0..first.len() {
            assert!(!gif_frames_within_bounds(&first[..end]));
        }
    }
}
