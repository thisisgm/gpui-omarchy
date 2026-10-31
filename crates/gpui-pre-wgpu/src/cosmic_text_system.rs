use anyhow::{Context as _, Ok, Result};
use collections::HashMap;
use cosmic_text::{
    Attrs, AttrsList, Ellipsize, Family, Font as CosmicTextFont,
    FontFeatures as CosmicFontFeatures, FontSystem, ShapeBuffer, ShapeLine, Stretch, Style, Weight,
};
use gpui::{
    Bounds, DevicePixels, FallbackFontClass, Font, FontFallbacks, FontFeatures, FontId,
    FontMetrics, FontRun, GlyphId, IsZero as _, LineLayout, MissingGlyph, MissingGlyphSink, Pixels,
    PlatformTextSystem, RenderGlyphParams, SUBPIXEL_VARIANTS_X, SUBPIXEL_VARIANTS_Y, ShapedGlyph,
    ShapedRun, SharedString, Size, TextEpochConfig, TextEpochToken, TextRenderingMode, point, size,
};

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use crate::fontconfig_on_demand::OnDemandFonts;
use itertools::Itertools;
use parking_lot::RwLock;
use smallvec::SmallVec;
use std::{borrow::Cow, ops::Range, sync::Arc};
use swash::{
    scale::{Render, ScaleContext, Source, StrikeWith},
    zeno::{Format, Vector},
};
use unicode_segmentation::UnicodeSegmentation;

pub struct CosmicTextSystem(RwLock<CosmicTextSystemState>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FontKey {
    family: SharedString,
    features: FontFeatures,
    fallbacks: Option<FontFallbacks>,
}

impl FontKey {
    fn new(family: SharedString, features: FontFeatures, fallbacks: Option<FontFallbacks>) -> Self {
        Self {
            family,
            features,
            fallbacks,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LoadedFontKey {
    database_id: cosmic_text::fontdb::ID,
    font: FontKey,
}

struct CosmicTextSystemState {
    generation: usize,
    custom_fonts: Vec<Arc<Cow<'static, [u8]>>>,
    preferred_fallbacks: HashMap<(FontId, char), SharedString>,
    font_system: FontSystem,
    scratch: ShapeBuffer,
    swash_scale_context: ScaleContext,
    pending_glyph_images: HashMap<RenderGlyphParams, swash::scale::image::Image>,
    /// Contains all already loaded fonts, including all faces. Indexed by `FontId`.
    loaded_fonts: Vec<LoadedFont>,
    loaded_font_ids_by_key: HashMap<LoadedFontKey, FontId>,
    /// Caches the `FontId`s associated with a specific family to avoid iterating the font database
    /// for every font face in a family.
    font_ids_by_family_cache: HashMap<FontKey, SmallVec<[FontId; 4]>>,
    system_font_fallback: String,
    missing_glyph_sink: Option<Arc<dyn MissingGlyphSink>>,
    /// Set when installed fonts are read on first use instead of all at startup.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    on_demand: Option<OnDemandFonts>,
}

struct LoadedFont {
    font: Arc<CosmicTextFont>,
    features: CosmicFontFeatures,
    is_known_emoji_font: bool,
    /// resolved at load time so `layout_line` shares one chain across faces.
    /// `Arc` keeps clone cheap on the per-run hot path.
    user_fallback_chain: Arc<[(FontId, SharedString)]>,
}

struct FontMatchProperties {
    primary_family_name: SharedString,
    stretch: Stretch,
    style: Style,
    weight: Weight,
    features: CosmicFontFeatures,
    fallback_chain: Arc<[(FontId, SharedString)]>,
}

impl FontMatchProperties {
    fn attributes<'a>(&'a self, font_id: FontId, family_name: &'a str) -> Attrs<'a> {
        Attrs::new()
            .metadata(font_id.0)
            .family(Family::Name(family_name))
            .stretch(self.stretch)
            .style(self.style)
            .weight(self.weight)
            .font_features(self.features.clone())
    }
}

impl CosmicTextSystem {
    /// Returns the selected face's weight and style, which may differ from the request.
    pub fn font_weight_and_style(
        &self,
        font_id: FontId,
    ) -> Result<(gpui::FontWeight, gpui::FontStyle)> {
        let state = self.0.read();
        let font = state
            .loaded_fonts
            .get(font_id.local_index())
            .filter(|_| font_id.text_epoch() == state.generation)
            .context("invalid font ID")?;
        let face = state
            .font_system
            .db()
            .face(font.font.id())
            .context("font face not found")?;
        let style = match face.style {
            cosmic_text::Style::Normal => gpui::FontStyle::Normal,
            cosmic_text::Style::Italic => gpui::FontStyle::Italic,
            cosmic_text::Style::Oblique => gpui::FontStyle::Oblique,
        };
        Ok((gpui::FontWeight(face.weight.0 as f32), style))
    }

    /// Builds reports for unresolved source indices after an outer text system
    /// has applied its own fallback.
    pub fn missing_glyphs(
        &self,
        text: &str,
        font_runs: &[FontRun],
        missing_text_indices: impl IntoIterator<Item = usize>,
    ) -> Vec<MissingGlyph> {
        self.0
            .read()
            .missing_glyphs(text, font_runs, missing_text_indices)
    }

    pub fn new(system_font_fallback: &str) -> Self {
        Self::from_font_system(FontSystem::new(), system_font_fallback)
    }

    pub fn new_without_system_fonts(system_font_fallback: &str) -> Self {
        let font_system = FontSystem::new_with_locale_and_db(
            "en-US".to_string(),
            cosmic_text::fontdb::Database::new(),
        );
        Self::from_font_system(font_system, system_font_fallback)
    }

    /// Starts with no fonts parsed; families and fallback faces are read through fontconfig when first used.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn new_on_demand(system_font_fallback: &str) -> Self {
        let font_system = FontSystem::new_with_locale_and_db(
            locale_from_env(),
            cosmic_text::fontdb::Database::new(),
        );
        let system = Self::from_font_system(font_system, system_font_fallback);
        system.0.write().on_demand = Some(OnDemandFonts::default());
        system
    }

    fn from_font_system(font_system: FontSystem, system_font_fallback: &str) -> Self {
        Self(RwLock::new(CosmicTextSystemState {
            generation: 0,
            custom_fonts: Vec::new(),
            preferred_fallbacks: HashMap::default(),
            font_system,
            scratch: ShapeBuffer::default(),
            swash_scale_context: ScaleContext::new(),
            pending_glyph_images: HashMap::default(),
            loaded_fonts: Vec::new(),
            loaded_font_ids_by_key: HashMap::default(),
            font_ids_by_family_cache: HashMap::default(),
            system_font_fallback: system_font_fallback.to_string(),
            missing_glyph_sink: None,
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            on_demand: None,
        }))
    }
}

/// Bounds raster images kept between a bounds query and its rasterization.
const MAX_PENDING_GLYPH_IMAGES: usize = 512;
/// Rejects a locale string too long to be a real BCP 47 tag.
const MAX_LOCALE_TAG_LEN: usize = 128;
/// Divides stale-font metrics safely; every other metric of a stale font is zero.
const STALE_FONT_UNITS_PER_EM: u32 = 1;

/// The BCP 47 tag cosmic-text's script fallback ranks by, from the POSIX locale.
/// Sample input: LANG="ja_JP.UTF-8" gives "ja-JP"; unset or "C" gives "en-US".
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn locale_from_env() -> String {
    let posix = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    normalize_locale(&posix).unwrap_or_else(|_| "en-US".into())
}

/// Sample input: "ja_JP.UTF-8@euro" gives "ja-JP"; "C" or "POSIX" gives "en-US".
fn normalize_locale(posix: &str) -> Result<String> {
    let tag = posix
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-");
    if tag == "C" || tag == "POSIX" {
        return Ok("en-US".into());
    }
    anyhow::ensure!(
        !tag.is_empty()
            && tag.len() <= MAX_LOCALE_TAG_LEN
            && tag
                .split('-')
                .all(|part| !part.is_empty()
                    && part.bytes().all(|byte| byte.is_ascii_alphanumeric())),
        "invalid text locale"
    );
    Ok(tag)
}

impl PlatformTextSystem for CosmicTextSystem {
    fn text_epoch(&self) -> usize {
        self.0.read().generation
    }
    fn prepare_text_epoch(
        &self,
        config: &TextEpochConfig,
        token: &TextEpochToken,
    ) -> Result<Arc<dyn PlatformTextSystem>> {
        let locale = normalize_locale(&config.locale)?;
        anyhow::ensure!(!config.fonts.is_empty(), "text epoch requires held fonts");
        let (custom_fonts, system_fallback, missing_glyph_sink) = {
            let state = self.0.read();
            (
                state.custom_fonts.clone(),
                state.system_font_fallback.clone(),
                state.missing_glyph_sink.clone(),
            )
        };
        anyhow::ensure!(token.is_current(), "text epoch superseded");
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let on_demand = OnDemandFonts::reload(&locale, config.fontconfig_file.as_deref())?;
        let mut db = cosmic_text::fontdb::Database::new();
        for source in &custom_fonts {
            db.load_font_source(cosmic_text::fontdb::Source::Binary(source.clone()));
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        db.load_system_fonts();
        let system = Self::from_font_system(
            FontSystem::new_with_locale_and_db(locale, db),
            &system_fallback,
        );
        {
            let mut state = system.0.write();
            state.generation = token.generation();
            state.custom_fonts = custom_fonts;
            state.missing_glyph_sink = missing_glyph_sink;
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            {
                state.on_demand = Some(on_demand);
            }
        }
        let mut font_ids = Vec::new();
        for font in &config.fonts {
            anyhow::ensure!(token.is_current(), "text epoch superseded");
            font_ids.push(system.font_id(font)?);
        }
        {
            let mut state = system.0.write();
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            state.prepare_fallbacks(&font_ids, &config.text, token)?;
            state.prewarm_fonts(&font_ids);
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            state.on_demand.as_mut().unwrap().freeze();
        }
        anyhow::ensure!(token.is_current(), "text epoch superseded");
        Ok(Arc::new(system))
    }

    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn set_missing_glyph_sink(&self, sink: Option<Arc<dyn MissingGlyphSink>>) {
        self.0.write().missing_glyph_sink = sink;
    }

    fn all_font_names(&self) -> Vec<String> {
        let state = self.0.read();
        #[allow(unused_mut)]
        let mut result = state
            .font_system
            .db()
            .faces()
            .filter_map(|face| face.families.first().map(|family| family.0.clone()))
            .collect_vec();
        // On demand the database holds only what was used so far; fontconfig's cache lists the rest.
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(on_demand) = &state.on_demand {
            result.extend(on_demand.family_names());
        }
        result.sort_unstable();
        result.dedup();
        result
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let mut state = self.0.write();
        let key = FontKey::new(
            font.family.clone(),
            font.features.clone(),
            font.fallbacks.clone(),
        );
        let candidates = if let Some(font_ids) = state.font_ids_by_family_cache.get(&key) {
            font_ids.as_slice()
        } else {
            let font_ids =
                state.load_family(&font.family, &font.features, font.fallbacks.as_ref())?;
            state.font_ids_by_family_cache.insert(key.clone(), font_ids);
            state.font_ids_by_family_cache[&key].as_ref()
        };

        let ix = find_best_match(font, candidates, &state)?;

        Ok(candidates[ix])
    }

    fn font_family_name(&self, id: FontId) -> Option<SharedString> {
        let state = self.0.read();
        if id.text_epoch() != state.generation {
            return None;
        }
        let loaded = state.loaded_fonts.get(id.local_index())?;
        state
            .font_system
            .db()
            .face(loaded.font.id())?
            .families
            .first()
            .map(|family| family.0.clone().into())
    }

    fn prewarm_fonts(&self, font_ids: &[FontId]) {
        self.0.write().prewarm_fonts(font_ids);
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let state = self.0.read();
        let Some(loaded_font) = state.current_font(font_id) else {
            log::error!("font metrics requested for {font_id:?} from another text epoch");
            return FontMetrics {
                units_per_em: STALE_FONT_UNITS_PER_EM,
                ascent: 0.,
                descent: 0.,
                line_gap: 0.,
                underline_position: 0.,
                underline_thickness: 0.,
                cap_height: 0.,
                x_height: 0.,
                bounding_box: Bounds::default(),
            };
        };
        let metrics = loaded_font.font.as_swash().metrics(&[]);

        FontMetrics {
            units_per_em: metrics.units_per_em as u32,
            ascent: metrics.ascent,
            descent: -metrics.descent,
            line_gap: metrics.leading,
            underline_position: metrics.underline_offset,
            underline_thickness: metrics.stroke_size,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: Bounds {
                origin: point(0.0, 0.0),
                size: size(metrics.max_width, metrics.ascent + metrics.descent),
            },
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let lock = self.0.read();
        let glyph_metrics = lock
            .current_font(font_id)
            .context("font belongs to another text epoch")?
            .font
            .as_swash()
            .glyph_metrics(&[]);
        let glyph_id = glyph_id.0 as u16;
        Ok(Bounds {
            origin: point(0.0, 0.0),
            size: size(
                glyph_metrics.advance_width(glyph_id),
                glyph_metrics.advance_height(glyph_id),
            ),
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.write().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.write().rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, runs)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Subpixel
    }
}

impl CosmicTextSystemState {
    fn current_font(&self, font_id: FontId) -> Option<&LoadedFont> {
        (font_id.text_epoch() == self.generation)
            .then(|| self.loaded_fonts.get(font_id.local_index()))
            .flatten()
    }

    fn loaded_font(&self, font_id: FontId) -> &LoadedFont {
        assert_eq!(
            font_id.text_epoch(),
            self.generation,
            "font belongs to another text epoch"
        );
        &self.loaded_fonts[font_id.local_index()]
    }

    fn font_match_properties(&self, font_id: FontId) -> Option<FontMatchProperties> {
        if font_id.text_epoch() != self.generation
            || font_id.local_index() >= self.loaded_fonts.len()
        {
            return None;
        }
        let loaded_font = self.loaded_font(font_id);
        let Some(face) = self.font_system.db().face(loaded_font.font.id()) else {
            log::warn!("font face not found in database for font_id {:?}", font_id);
            return None;
        };
        let Some(first_family) = face.families.first() else {
            log::warn!("font face has no family names for font_id {:?}", font_id);
            return None;
        };

        Some(FontMatchProperties {
            primary_family_name: first_family.0.clone().into(),
            stretch: face.stretch,
            style: face.style,
            weight: face.weight,
            features: loaded_font.features.clone(),
            fallback_chain: Arc::clone(&loaded_font.user_fallback_chain),
        })
    }

    fn prewarm_fonts(&mut self, font_ids: &[FontId]) {
        for &font_id in font_ids {
            let Some(properties) = self.font_match_properties(font_id) else {
                continue;
            };
            let primary_attributes =
                properties.attributes(font_id, &properties.primary_family_name);
            self.font_system.get_font_matches(&primary_attributes);

            for (fallback_id, fallback_name) in &*properties.fallback_chain {
                let fallback_attributes = properties.attributes(*fallback_id, fallback_name);
                self.font_system.get_font_matches(&fallback_attributes);
            }
        }
    }

    #[profiling::function]
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.font_ids_by_family_cache.clear();
        let db = self.font_system.db_mut();
        for bytes in fonts {
            let source = Arc::new(bytes);
            db.load_font_source(cosmic_text::fontdb::Source::Binary(source.clone()));
            self.custom_fonts.push(source);
        }
        Ok(())
    }

    fn faces_in_family(&self, name: &str) -> SmallVec<[(cosmic_text::fontdb::ID, String); 4]> {
        self.font_system
            .db()
            .faces()
            .filter(|face| face.families.iter().any(|family| name == family.0))
            .map(|face| (face.id, face.post_script_name.clone()))
            .collect()
    }

    /// Reads the family's files through fontconfig; true when a new face entered the database.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn load_on_demand_family(&mut self, family: &str) -> bool {
        let Some(on_demand) = self.on_demand.as_mut() else {
            return false;
        };
        let files = on_demand.new_family_files(family);
        self.load_font_files(files)
    }

    /// Reads fontconfig's covering face for each character; true when a new face entered the database.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn load_on_demand_fallbacks(&mut self, chars: impl IntoIterator<Item = char>) -> bool {
        let Some(on_demand) = self.on_demand.as_mut() else {
            return false;
        };
        let files = on_demand.new_covering_files(chars);
        self.load_font_files(files)
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn load_font_files(&mut self, files: Vec<std::path::PathBuf>) -> bool {
        if files.is_empty() {
            return false;
        }
        // db_mut also drops cosmic-text's match cache, so its fallback sees the new faces.
        let db = self.font_system.db_mut();
        let mut loaded = false;
        for file in files {
            match db.load_font_file(&file) {
                std::result::Result::Ok(()) => loaded = true,
                Err(error) => log::warn!("could not load font {}: {error}", file.display()),
            }
        }
        loaded
    }

    /// Resolves exact fontconfig families before freezing the snapshot, including collection face identity.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn prepare_fallbacks(
        &mut self,
        font_ids: &[FontId],
        text: &[SharedString],
        token: &TextEpochToken,
    ) -> Result<()> {
        let mut chars = std::collections::BTreeSet::new();
        for line in text {
            chars.extend(line.chars());
        }
        let mut loaded_paths = std::collections::HashSet::new();
        for &font_id in font_ids {
            let properties = self
                .font_match_properties(font_id)
                .context("invalid held font")?;
            for &ch in &chars {
                anyhow::ensure!(token.is_current(), "text epoch superseded");
                if ch.is_control()
                    || matches!(ch, '\u{200c}' | '\u{200d}' | '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
                    || self.glyph_for_char(font_id, ch).is_some()
                    || properties
                        .fallback_chain
                        .iter()
                        .any(|(id, _)| self.glyph_for_char(*id, ch).is_some())
                {
                    continue;
                }
                let matched = self
                    .on_demand
                    .as_mut()
                    .unwrap()
                    .covering(&properties.primary_family_name, ch)?;
                let Some((path, family)) = matched else {
                    log::warn!("no fontconfig face covers U+{:04X}", ch as u32);
                    continue;
                };
                if loaded_paths.insert(path.clone()) {
                    self.load_font_files(vec![path]);
                }
                let family: SharedString = family.into();
                self.load_on_demand_family(&family);
                let ids = self.load_family(&family, &FontFeatures::default(), None)?;
                if !ids.iter().any(|id| self.glyph_for_char(*id, ch).is_some()) {
                    log::warn!(
                        "fontconfig family {family} does not cover U+{:04X}",
                        ch as u32
                    );
                    continue;
                }
                self.preferred_fallbacks.insert((font_id, ch), family);
            }
        }
        Ok(())
    }

    #[profiling::function]
    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
    ) -> Result<SmallVec<[FontId; 4]>> {
        let loaded_font_key = FontKey::new(
            SharedString::from(name.to_owned()),
            features.clone(),
            fallbacks.cloned(),
        );

        // recurse with `fallbacks = None` so a fallback family cannot pull in
        // another chain. missing fallback families are dropped so a typo in
        // settings still lets the primary family load.
        let user_fallback_chain: Arc<[(FontId, SharedString)]> = match fallbacks {
            Some(fallbacks) if !fallbacks.fallback_list().is_empty() => {
                let mut chain: Vec<(FontId, SharedString)> = Vec::new();
                for fallback_name in fallbacks.fallback_list() {
                    let fb_key = FontKey::new(
                        SharedString::from(fallback_name.clone()),
                        features.clone(),
                        None,
                    );
                    let fb_ids = if let Some(cached) = self.font_ids_by_family_cache.get(&fb_key) {
                        cached.clone()
                    } else {
                        let loaded = self.load_family(fallback_name, features, None)?;
                        self.font_ids_by_family_cache
                            .insert(fb_key.clone(), loaded.clone());
                        loaded
                    };
                    let Some(&fb_id) = fb_ids.first() else {
                        continue;
                    };
                    let db_id = self.loaded_fonts[fb_id.local_index()].font.id();
                    if let Some(face) = self.font_system.db().face(db_id)
                        && let Some(family) = face.families.first()
                    {
                        chain.push((fb_id, SharedString::from(family.0.clone())));
                    }
                }
                Arc::from(chain)
            }
            _ => Arc::from(Vec::new()),
        };

        let name = gpui::font_name_with_fallbacks(name, &self.system_font_fallback).to_string();

        #[allow(unused_mut)]
        let mut families = self.faces_in_family(&name);
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if families.is_empty() && self.load_on_demand_family(&name) {
            families = self.faces_in_family(&name);
        }

        let cosmic_features = cosmic_font_features(features)?;

        let mut loaded_font_ids = SmallVec::new();
        for (database_id, postscript_name) in families {
            let key = LoadedFontKey {
                database_id,
                font: loaded_font_key.clone(),
            };
            if let Some(&font_id) = self.loaded_font_ids_by_key.get(&key) {
                self.loaded_fonts[font_id.local_index()].user_fallback_chain =
                    Arc::clone(&user_fallback_chain);
                loaded_font_ids.push(font_id);
                continue;
            }

            let font = self
                .font_system
                .get_font(database_id, cosmic_text::Weight::NORMAL)
                .context("Could not load font")?;

            // HACK: To let the storybook run and render Windows caption icons. We should actually do better font fallback.
            let allowed_bad_font_names = [
                "SegoeFluentIcons", // NOTE: Segoe fluent icons postscript name is inconsistent
                "Segoe Fluent Icons",
            ];

            // This only decides whether a face can be a primary font. The
            // editor measures layout with the width of 'm' and would panic
            // without it.
            //
            // Don't remove the face from the font database. Arabic, Devanagari
            // and emoji fonts often have no Latin glyphs, and they are exactly
            // what cosmic-text falls back to when the primary font lacks a
            // glyph.
            if font.as_swash().charmap().map('m') == 0
                && !allowed_bad_font_names.contains(&postscript_name.as_str())
            {
                continue;
            };

            let font_id = FontId::for_text_epoch(self.loaded_fonts.len(), self.generation)?;
            loaded_font_ids.push(font_id);
            self.loaded_fonts.push(LoadedFont {
                font,
                features: cosmic_features.clone(),
                is_known_emoji_font: check_is_known_emoji_font(&postscript_name),
                user_fallback_chain: Arc::clone(&user_fallback_chain),
            });
            self.loaded_font_ids_by_key.insert(key, font_id);
        }

        Ok(loaded_font_ids)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let glyph_metrics = self
            .current_font(font_id)
            .context("font belongs to another text epoch")?
            .font
            .as_swash()
            .glyph_metrics(&[]);
        Ok(Size {
            width: glyph_metrics.advance_width(glyph_id.0 as u16),
            height: glyph_metrics.advance_height(glyph_id.0 as u16),
        })
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        if font_id.text_epoch() != self.generation
            || font_id.local_index() >= self.loaded_fonts.len()
        {
            return None;
        }
        let glyph_id = self.loaded_font(font_id).font.as_swash().charmap().map(ch);
        if glyph_id == 0 {
            None
        } else {
            Some(GlyphId(glyph_id.into()))
        }
    }

    fn raster_bounds(&mut self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let image = self.render_glyph_image(params)?;
        let bounds = Bounds {
            origin: point(image.placement.left.into(), (-image.placement.top).into()),
            size: size(image.placement.width.into(), image.placement.height.into()),
        };
        if !bounds.is_zero() {
            if self.pending_glyph_images.len() >= MAX_PENDING_GLYPH_IMAGES {
                self.pending_glyph_images.clear();
            }
            self.pending_glyph_images.insert(params.clone(), image);
        }
        Ok(bounds)
    }

    #[profiling::function]
    fn rasterize_glyph(
        &mut self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        }

        let mut image = match self.pending_glyph_images.remove(params) {
            Some(image) => image,
            None => self.render_glyph_image(params)?,
        };
        let bitmap_size = glyph_bounds.size;
        match image.content {
            swash::scale::image::Content::Color | swash::scale::image::Content::SubpixelMask => {
                // Convert from RGBA to BGRA.
                for pixel in image.data.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                Ok((bitmap_size, image.data))
            }
            swash::scale::image::Content::Mask => {
                if params.subpixel_rendering {
                    // We must always return RGBA data when subpixel rendering is requested.
                    let expanded = image.data.iter().flat_map(|&a| [a, a, a, a]).collect();
                    Ok((bitmap_size, expanded))
                } else {
                    Ok((bitmap_size, image.data))
                }
            }
        }
    }

    fn render_glyph_image(
        &mut self,
        params: &RenderGlyphParams,
    ) -> Result<swash::scale::image::Image> {
        anyhow::ensure!(
            params.font_id.text_epoch() == self.generation,
            "stale text epoch glyph"
        );
        let loaded_font = self
            .loaded_fonts
            .get(params.font_id.local_index())
            .context("invalid font ID")?;
        let font_ref = loaded_font.font.as_swash();
        let pixel_size = f32::from(params.font_size);

        let subpixel_offset = Vector::new(
            params.subpixel_variant.x as f32 / SUBPIXEL_VARIANTS_X as f32 / params.scale_factor,
            params.subpixel_variant.y as f32 / SUBPIXEL_VARIANTS_Y as f32 / params.scale_factor,
        );

        let mut scaler = self
            .swash_scale_context
            .builder(font_ref)
            .size(pixel_size * params.scale_factor)
            .hint(true)
            .build();

        let sources: &[Source] = if params.is_emoji {
            &[
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ]
        } else {
            &[Source::Bitmap(StrikeWith::ExactSize), Source::Outline]
        };

        let mut renderer = Render::new(sources);
        if params.subpixel_rendering {
            // There seems to be a bug in Swash where the B and R values are swapped.
            renderer
                .format(Format::subpixel_bgra())
                .offset(subpixel_offset);
        } else {
            renderer.format(Format::Alpha).offset(subpixel_offset);
        }

        let glyph_id: u16 = params.glyph_id.0.try_into()?;
        renderer
            .render(&mut scaler, glyph_id)
            .with_context(|| format!("unable to render glyph via swash for {params:?}"))
    }

    /// This is used when cosmic_text has chosen a fallback font instead of using the requested
    /// font, typically to handle some unicode characters. When this happens, `loaded_fonts` may not
    /// yet have an entry for this fallback font, and so one is added.
    ///
    /// Note that callers shouldn't use this `FontId` somewhere that will retrieve the corresponding
    /// `LoadedFont.features`, as it will have an arbitrarily chosen or empty value. The only
    /// current use of this field is for the *input* of `layout_line`, and so it's fine to use
    /// `font_id_for_cosmic_id` when computing the *output* of `layout_line`.
    fn font_id_for_cosmic_id(&mut self, id: cosmic_text::fontdb::ID) -> Result<FontId> {
        if let Some(ix) = self
            .loaded_fonts
            .iter()
            .position(|loaded_font| loaded_font.font.id() == id)
        {
            FontId::for_text_epoch(ix, self.generation)
        } else {
            let font = self
                .font_system
                .get_font(id, cosmic_text::Weight::NORMAL)
                .context("failed to get fallback font from cosmic-text font system")?;
            let face = self
                .font_system
                .db()
                .face(id)
                .context("fallback font face not found in cosmic-text database")?;

            let font_id = FontId::for_text_epoch(self.loaded_fonts.len(), self.generation)?;
            self.loaded_fonts.push(LoadedFont {
                font,
                features: CosmicFontFeatures::new(),
                is_known_emoji_font: check_is_known_emoji_font(&face.post_script_name),
                user_fallback_chain: Arc::from(Vec::new()),
            });

            Ok(font_id)
        }
    }

    #[profiling::function]
    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        if font_runs
            .iter()
            .any(|run| run.font_id.text_epoch() != self.generation)
        {
            log::error!("layout_line got a font run from another text epoch");
            return LineLayout {
                font_size,
                len: text.len(),
                ..LineLayout::default()
            };
        }
        if contains_paragraph_separator(text) {
            self.layout_line_with_separators(text, font_size, font_runs)
        } else {
            self.layout_line_no_separators(text, font_size, font_runs)
        }
    }

    fn layout_line_with_separators(
        &mut self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
    ) -> LineLayout {
        let mut layout = LineLayout {
            font_size,
            len: text.len(),
            ..Default::default()
        };
        let mut paragraph_start = 0;

        for (separator_start, separator) in text
            .char_indices()
            .filter(|(_, character)| is_paragraph_separator(*character))
        {
            let separator_end = separator_start + separator.len_utf8();
            self.shape_segment(
                text,
                paragraph_start..separator_start,
                font_size,
                font_runs,
                &mut layout,
            );
            self.shape_segment(
                text,
                separator_start..separator_end,
                font_size,
                font_runs,
                &mut layout,
            );
            paragraph_start = separator_end;
        }

        self.shape_segment(
            text,
            paragraph_start..text.len(),
            font_size,
            font_runs,
            &mut layout,
        );

        layout
    }

    fn shape_segment(
        &mut self,
        text: &str,
        range: Range<usize>,
        font_size: Pixels,
        font_runs: &[FontRun],
        layout: &mut LineLayout,
    ) {
        if range.is_empty() {
            return;
        }

        let segment_font_runs = clip_font_runs(font_runs, range.clone());
        let segment =
            self.layout_line_no_separators(&text[range.clone()], font_size, &segment_font_runs);

        let mut segment_runs = segment.runs;
        for run in &mut segment_runs {
            for glyph in &mut run.glyphs {
                glyph.index += range.start;
                glyph.position.x += layout.width;
            }
        }

        for mut run in segment_runs {
            if let Some(same_run) = layout
                .runs
                .last_mut()
                .filter(|last| last.font_id == run.font_id)
            {
                same_run.glyphs.append(&mut run.glyphs);
            } else {
                layout.runs.push(run);
            }
        }

        layout.width += segment.width;
        layout.ascent = layout.ascent.max(segment.ascent);
        layout.descent = layout.descent.max(segment.descent);
    }

    fn layout_line_no_separators(
        &mut self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
    ) -> LineLayout {
        let mut attrs_list = AttrsList::new(&Attrs::new());
        let mut offs = 0;
        for run in font_runs {
            let run_end = offs + run.len;

            let Some(properties) = self.font_match_properties(run.font_id) else {
                offs = run_end;
                continue;
            };

            let primary_attrs = properties.attributes(run.font_id, &properties.primary_family_name);
            let fallback_attrs: SmallVec<[Attrs<'_>; 4]> = properties
                .fallback_chain
                .iter()
                .map(|(font_id, family_name)| properties.attributes(*font_id, family_name))
                .collect();

            let spans = if properties.fallback_chain.is_empty() {
                let mut spans = SmallVec::<[RunSpan; 4]>::new();
                spans.push(RunSpan {
                    start: offs,
                    end: run_end,
                    slot: None,
                    font_id: run.font_id,
                });
                spans
            } else {
                let loaded_fonts = &self.loaded_fonts;
                let covers = |id: FontId, ch: char| charmap_covers(loaded_fonts, id, ch);
                compute_run_spans(
                    text,
                    offs,
                    run.len,
                    run.font_id,
                    &properties.fallback_chain,
                    &covers,
                )
            };

            for span in spans {
                let attrs = match span.slot {
                    None => &primary_attrs,
                    Some(ix) => &fallback_attrs[ix],
                };
                attrs_list.add_span(span.start..span.end, attrs);
            }
            for (offset, grapheme) in text[offs..run_end].grapheme_indices(true) {
                if let Some(family) = grapheme
                    .chars()
                    .find_map(|ch| self.preferred_fallbacks.get(&(run.font_id, ch)))
                {
                    attrs_list.add_span(
                        offs + offset..offs + offset + grapheme.len(),
                        &primary_attrs.clone().family(Family::Name(family)),
                    );
                }
            }
            offs = run_end;
        }

        let line = ShapeLine::new(
            &mut self.font_system,
            text,
            &attrs_list,
            cosmic_text::Shaping::Advanced,
            4,
        );
        let mut layout_lines = Vec::with_capacity(1);
        line.layout_to_buffer(
            &mut self.scratch,
            f32::from(font_size),
            None, // We do our own wrapping
            cosmic_text::Wrap::None,
            Ellipsize::None,
            None,
            &mut layout_lines,
            None,
            cosmic_text::Hinting::Disabled,
        );

        let Some(layout) = layout_lines.first() else {
            return LineLayout {
                font_size,
                width: Pixels::ZERO,
                ascent: Pixels::ZERO,
                descent: Pixels::ZERO,
                runs: Vec::new(),
                len: text.len(),
            };
        };

        // Each character is looked up once, so this re-layout happens at most once per new script.
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if self.on_demand.is_some() {
            let uncovered: SmallVec<[char; 8]> = layout
                .glyphs
                .iter()
                .filter(|glyph| glyph.glyph_id == 0)
                .filter_map(|glyph| text.get(glyph.start..).and_then(|rest| rest.chars().next()))
                .collect();
            if !uncovered.is_empty() && self.load_on_demand_fallbacks(uncovered) {
                return self.layout_line_no_separators(text, font_size, font_runs);
            }
        }

        let missing_glyphs = self.missing_glyph_sink.as_ref().map(|_| {
            self.missing_glyphs(
                text,
                font_runs,
                layout
                    .glyphs
                    .iter()
                    .filter(|glyph| glyph.glyph_id == 0)
                    .map(|glyph| glyph.start),
            )
        });

        let mut runs: Vec<ShapedRun> = Vec::new();
        for glyph in &layout.glyphs {
            let mut font_id = FontId(glyph.metadata);
            let mut loaded_font = self.loaded_font(font_id);
            if loaded_font.font.id() != glyph.font_id {
                match self.font_id_for_cosmic_id(glyph.font_id) {
                    std::result::Result::Ok(resolved_id) => {
                        font_id = resolved_id;
                        loaded_font = self.loaded_font(font_id);
                    }
                    Err(error) => {
                        log::warn!(
                            "failed to resolve cosmic font id {:?}: {error:#}",
                            glyph.font_id
                        );
                        continue;
                    }
                }
            }
            let is_emoji = loaded_font.is_known_emoji_font;

            // HACK: Prevent crash caused by variation selectors.
            if glyph.glyph_id == 3 && is_emoji {
                continue;
            }

            // cosmic-text keeps shaper offsets apart from the pen, in ems; ShapedGlyph adds them.
            let shaped_glyph = ShapedGlyph {
                id: GlyphId(glyph.glyph_id as u32),
                position: point(
                    (glyph.x + glyph.x_offset * glyph.font_size).into(),
                    (glyph.y - glyph.y_offset * glyph.font_size).into(),
                ),
                index: glyph.start,
                is_emoji,
            };

            if let Some(last_run) = runs
                .last_mut()
                .filter(|last_run| last_run.font_id == font_id)
            {
                last_run.glyphs.push(shaped_glyph);
            } else {
                runs.push(ShapedRun {
                    font_id,
                    glyphs: vec![shaped_glyph],
                });
            }
        }

        if let Some((sink, missing_glyphs)) = self.missing_glyph_sink.as_ref().zip(missing_glyphs) {
            sink.report(missing_glyphs);
        }

        LineLayout {
            font_size,
            width: layout.w.into(),
            ascent: layout.max_ascent.into(),
            descent: layout.max_descent.into(),
            runs,
            len: text.len(),
        }
    }

    fn missing_glyphs(
        &self,
        text: &str,
        font_runs: &[FontRun],
        missing_text_indices: impl IntoIterator<Item = usize>,
    ) -> Vec<MissingGlyph> {
        let mut missing_text_indices = missing_text_indices.into_iter().peekable();
        if missing_text_indices.peek().is_none() {
            return Vec::new();
        }
        let mut missing_text_indices = missing_text_indices.collect::<Vec<_>>();
        missing_text_indices.sort_unstable();
        missing_text_indices.dedup();

        let mut font_run_index = 0;
        let mut font_run_end = font_runs.first().map_or(0, |font_run| font_run.len);
        let mut missing_glyphs = Vec::new();
        let mut missing_index = 0;
        for (grapheme_start, grapheme) in text.grapheme_indices(true) {
            let grapheme_end = grapheme_start + grapheme.len();
            while missing_text_indices
                .get(missing_index)
                .is_some_and(|text_index| *text_index < grapheme_start)
            {
                missing_index += 1;
            }
            let Some(&text_index) = missing_text_indices.get(missing_index) else {
                break;
            };
            if text_index >= grapheme_end {
                continue;
            }

            while font_run_end <= text_index && font_run_index + 1 < font_runs.len() {
                font_run_index += 1;
                let Some(font_run) = font_runs.get(font_run_index) else {
                    break;
                };
                font_run_end += font_run.len;
            }
            let font_class = self.fallback_font_class(
                font_runs
                    .get(font_run_index)
                    .or_else(|| font_runs.last())
                    .map(|font_run| font_run.font_id),
            );
            missing_glyphs.push(MissingGlyph::new(grapheme.into(), font_class));
            while missing_text_indices
                .get(missing_index)
                .is_some_and(|text_index| *text_index < grapheme_end)
            {
                missing_index += 1;
            }
        }
        missing_glyphs
    }

    fn fallback_font_class(&self, font_id: Option<FontId>) -> FallbackFontClass {
        let Some(loaded_font) = font_id.and_then(|font_id| self.current_font(font_id)) else {
            return FallbackFontClass::Proportional;
        };
        let is_monospace = self
            .font_system
            .db()
            .face(loaded_font.font.id())
            .is_some_and(|face| face.monospaced);
        if is_monospace {
            FallbackFontClass::Monospace
        } else {
            FallbackFontClass::Proportional
        }
    }
}

#[inline(always)]
fn is_paragraph_separator(character: char) -> bool {
    unicode_bidi::bidi_class(character) == unicode_bidi::BidiClass::B
}

fn contains_paragraph_separator(text: &str) -> bool {
    if text
        .bytes()
        .any(|byte| matches!(byte, b'\n' | b'\r' | 0x1c | 0x1d | 0x1e))
    {
        return true;
    }

    !text.is_ascii() && text.chars().any(is_paragraph_separator)
}

fn clip_font_runs(font_runs: &[FontRun], range: Range<usize>) -> SmallVec<[FontRun; 4]> {
    let mut clipped = SmallVec::new();
    let mut offs = 0;
    for run in font_runs {
        let run_start = offs;
        offs += run.len;
        if offs <= range.start {
            continue;
        }
        if run_start >= range.end {
            break;
        }
        let start = run_start.max(range.start);
        let end = offs.min(range.end);
        if start < end {
            clipped.push(FontRun {
                len: end - start,
                font_id: run.font_id,
            });
        }
    }
    clipped
}

#[cfg(feature = "font-kit")]
fn find_best_match(
    font: &Font,
    candidates: &[FontId],
    state: &CosmicTextSystemState,
) -> Result<usize> {
    let candidate_properties = candidates
        .iter()
        .map(|font_id| {
            let database_id = state.loaded_font(*font_id).font.id();
            let face_info = state
                .font_system
                .db()
                .face(database_id)
                .context("font face not found in database")?;
            Ok(face_info_into_properties(face_info))
        })
        .collect::<Result<SmallVec<[_; 4]>>>()?;

    let ix =
        font_kit::matching::find_best_match(&candidate_properties, &font_into_properties(font))
            .context("requested font family contains no font matching the other parameters")?;

    Ok(ix)
}

#[cfg(not(feature = "font-kit"))]
fn find_best_match(
    font: &Font,
    candidates: &[FontId],
    state: &CosmicTextSystemState,
) -> Result<usize> {
    if candidates.is_empty() {
        anyhow::bail!("requested font family contains no font matching the other parameters");
    }
    if candidates.len() == 1 {
        return Ok(0);
    }

    let target_weight = font.weight.0;
    let target_italic = matches!(
        font.style,
        gpui::FontStyle::Italic | gpui::FontStyle::Oblique
    );

    let mut best_index = 0;
    let mut best_score = u32::MAX;

    for (index, font_id) in candidates.iter().enumerate() {
        let database_id = state.loaded_font(*font_id).font.id();
        let face_info = state
            .font_system
            .db()
            .face(database_id)
            .context("font face not found in database")?;

        let is_italic = matches!(
            face_info.style,
            cosmic_text::Style::Italic | cosmic_text::Style::Oblique
        );
        let style_penalty: u32 = if is_italic == target_italic { 0 } else { 1000 };
        let weight_diff = (face_info.weight.0 as i32 - target_weight as i32).unsigned_abs();
        let score = style_penalty + weight_diff;

        if score < best_score {
            best_score = score;
            best_index = index;
        }
    }

    Ok(best_index)
}

/// one contiguous slice of a `FontRun` that maps to a single slot. `slot` is
/// `None` for the primary font and `Some(ix)` for `fallback_chain[ix]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunSpan {
    start: usize,
    end: usize,
    slot: Option<usize>,
    font_id: FontId,
}

/// walks `text[run_offset..run_offset + run_len]` and groups codepoints into
/// spans. inheriting codepoints stay in the current span so shaping clusters
/// like emoji zwj sequences and combining marks are not torn apart.
fn compute_run_spans(
    text: &str,
    run_offset: usize,
    run_len: usize,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
    covers: &impl Fn(FontId, char) -> bool,
) -> SmallVec<[RunSpan; 4]> {
    let mut spans = SmallVec::new();
    let run_end = run_offset + run_len;
    if run_end <= run_offset {
        return spans;
    }
    if fallback_chain.is_empty() {
        spans.push(RunSpan {
            start: run_offset,
            end: run_end,
            slot: None,
            font_id: primary,
        });
        return spans;
    }
    let run_text = &text[run_offset..run_end];
    let mut span_start = run_offset;
    let mut span_slot: Option<usize> = None;
    let mut span_font_id = primary;
    for (grapheme_idx, grapheme) in run_text.grapheme_indices(true) {
        let abs = run_offset + grapheme_idx;
        let ch = grapheme.chars().next().unwrap_or('\0');
        let next_slot = pick_covering_slot(ch, span_slot, primary, fallback_chain, covers);
        if next_slot == span_slot {
            continue;
        }
        if abs > span_start {
            spans.push(RunSpan {
                start: span_start,
                end: abs,
                slot: span_slot,
                font_id: span_font_id,
            });
        }
        span_start = abs;
        span_slot = next_slot;
        span_font_id = slot_font_id(next_slot, primary, fallback_chain);
    }
    if span_start < run_end {
        spans.push(RunSpan {
            start: span_start,
            end: run_end,
            slot: span_slot,
            font_id: span_font_id,
        });
    }
    spans
}

fn slot_font_id(
    slot: Option<usize>,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
) -> FontId {
    match slot {
        None => primary,
        Some(ix) => fallback_chain[ix].0,
    }
}

fn pick_covering_slot(
    ch: char,
    current: Option<usize>,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
    covers: &impl Fn(FontId, char) -> bool,
) -> Option<usize> {
    if (ch as u32) <= 0x7F {
        return None;
    }
    if covers(primary, ch) {
        return None;
    }
    let current_id = slot_font_id(current, primary, fallback_chain);
    if covers(current_id, ch) {
        return current;
    }

    fallback_chain
        .iter()
        .position(|(fb_id, _)| covers(*fb_id, ch))
}

fn charmap_covers(loaded_fonts: &[LoadedFont], id: FontId, ch: char) -> bool {
    loaded_fonts
        .get(id.local_index())
        .is_some_and(|loaded| loaded.font.as_swash().charmap().map(ch) != 0)
}

fn cosmic_font_features(features: &FontFeatures) -> Result<CosmicFontFeatures> {
    let mut result = CosmicFontFeatures::new();
    for feature in features.0.iter() {
        let name_bytes: [u8; 4] = feature
            .0
            .as_bytes()
            .try_into()
            .context("Incorrect feature flag format")?;

        let tag = cosmic_text::FeatureTag::new(&name_bytes);

        result.set(tag, feature.1);
    }
    Ok(result)
}

#[cfg(feature = "font-kit")]
fn font_into_properties(font: &gpui::Font) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match font.style {
            gpui::FontStyle::Normal => font_kit::properties::Style::Normal,
            gpui::FontStyle::Italic => font_kit::properties::Style::Italic,
            gpui::FontStyle::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(font.weight.0),
        stretch: Default::default(),
    }
}

#[cfg(feature = "font-kit")]
fn face_info_into_properties(
    face_info: &cosmic_text::fontdb::FaceInfo,
) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match face_info.style {
            cosmic_text::Style::Normal => font_kit::properties::Style::Normal,
            cosmic_text::Style::Italic => font_kit::properties::Style::Italic,
            cosmic_text::Style::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(face_info.weight.0.into()),
        stretch: match face_info.stretch {
            cosmic_text::Stretch::Condensed => font_kit::properties::Stretch::CONDENSED,
            cosmic_text::Stretch::Expanded => font_kit::properties::Stretch::EXPANDED,
            cosmic_text::Stretch::ExtraCondensed => font_kit::properties::Stretch::EXTRA_CONDENSED,
            cosmic_text::Stretch::ExtraExpanded => font_kit::properties::Stretch::EXTRA_EXPANDED,
            cosmic_text::Stretch::Normal => font_kit::properties::Stretch::NORMAL,
            cosmic_text::Stretch::SemiCondensed => font_kit::properties::Stretch::SEMI_CONDENSED,
            cosmic_text::Stretch::SemiExpanded => font_kit::properties::Stretch::SEMI_EXPANDED,
            cosmic_text::Stretch::UltraCondensed => font_kit::properties::Stretch::ULTRA_CONDENSED,
            cosmic_text::Stretch::UltraExpanded => font_kit::properties::Stretch::ULTRA_EXPANDED,
        },
    }
}

fn check_is_known_emoji_font(postscript_name: &str) -> bool {
    // TODO: Include other common emoji fonts
    postscript_name == "NotoColorEmoji"
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    mod locale_epochs {
        use super::*;
        use std::{
            future::Future,
            path::PathBuf,
            sync::atomic::{AtomicUsize, Ordering},
            task::{Context, Poll, Wake, Waker},
        };

        fn wait<T>(future: impl Future<Output = T>) -> T {
            struct ThreadWake(std::thread::Thread);
            impl Wake for ThreadWake {
                fn wake(self: Arc<Self>) {
                    self.0.unpark();
                }
            }
            let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
            let mut context = Context::from_waker(&waker);
            let mut future = std::pin::pin!(future);
            loop {
                match future.as_mut().poll(&mut context) {
                    Poll::Ready(value) => return value,
                    Poll::Pending => std::thread::park(),
                }
            }
        }

        struct Fixture {
            root: PathBuf,
        }
        impl Fixture {
            fn new() -> Result<Self> {
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                let parent =
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".superpowers/i50-locale");
                std::fs::create_dir_all(&parent)?;
                let root = parent.join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir(&root)?;
                std::fs::write(root.join(".flea-test-sandbox"), b"i50 locale fixture")?;
                std::fs::create_dir(root.join("fonts"))?;
                std::fs::create_dir(root.join("cache"))?;
                std::fs::write(
                    root.join("fonts/primary.ttf"),
                    font("Epoch Latin", false, 0, 0),
                )?;
                let families = ["JP", "KR", "SC", "TC", "HK"];
                let header_len = 12 + families.len() * 4;
                let mut collection = b"ttcf\0\x01\0\0".to_vec();
                collection.extend((families.len() as u32).to_be_bytes());
                let mut faces = Vec::new();
                let mut offset = header_len;
                for (index, family) in families.iter().enumerate() {
                    collection.extend((offset as u32).to_be_bytes());
                    let face = font(
                        &format!("Noto Sans CJK {family}"),
                        true,
                        index as u16,
                        offset,
                    );
                    offset += face.len();
                    faces.push(face);
                }
                for face in faces {
                    collection.extend(face);
                }
                std::fs::write(root.join("fonts/cjk.ttc"), collection)?;
                let fixture = Self { root };
                fixture.preference("default")?;
                Ok(fixture)
            }

            fn preference(&self, preference: &str) -> Result<()> {
                let mut rules = String::new();
                for (family, locale) in [
                    ("JP", "ja"),
                    ("KR", "ko"),
                    ("SC", "zh-cn"),
                    ("TC", "zh-tw"),
                    ("HK", "zh-hk"),
                ] {
                    rules.push_str(&format!("<match target=\"scan\"><test name=\"family\"><string>Noto Sans CJK {family}</string></test><edit name=\"lang\" mode=\"assign\"><string>{locale}</string></edit></match>"));
                }
                if preference != "default" {
                    rules.push_str(&format!("<alias><family>Epoch Latin</family><accept><family>Noto Sans CJK {}</family></accept></alias>", preference.to_uppercase()));
                }
                rules.push_str("<alias><family>Epoch Latin</family><default><family>Noto Sans CJK SC</family></default></alias>");
                std::fs::write(
                    self.root.join("fonts.conf"),
                    format!(
                        "<?xml version=\"1.0\"?><!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\"><fontconfig><dir>{}/fonts</dir><cachedir>{}/cache</cachedir>{rules}</fontconfig>",
                        self.root.display(),
                        self.root.display()
                    ),
                )?;
                Ok(())
            }

            fn config(&self, locale: &str) -> TextEpochConfig {
                TextEpochConfig {
                    locale: locale.into(),
                    fonts: vec![gpui::font("Epoch Latin")],
                    text: vec!["m骨".into()],
                    fontconfig_file: Some(self.root.join("fonts.conf")),
                }
            }

            fn prepare(
                &self,
                source: &gpui::TextSystem,
                locale: &str,
            ) -> Result<gpui::PreparedTextEpoch> {
                wait(source.prepare_text_epoch(self.config(locale))?.prepared())
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let parent =
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".superpowers/i50-locale");
                if self.root.is_absolute()
                    && self.root.parent() == Some(parent.as_path())
                    && self.root.join(".flea-test-sandbox").is_file()
                {
                    let _ = std::fs::remove_dir_all(&self.root);
                }
            }
        }

        fn source() -> gpui::TextSystem {
            gpui::TextSystem::new(Arc::new(CosmicTextSystem::new_without_system_fonts(
                "Epoch Latin",
            )))
        }

        fn shape(system: Arc<gpui::TextSystem>) -> gpui::ShapedLine {
            let text: SharedString = "m骨".into();
            let run = gpui::TextRun {
                len: text.len(),
                font: gpui::font("Epoch Latin"),
                color: gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            gpui::WindowTextSystem::new(system).shape_line(text, gpui::px(14.), &[run], None)
        }

        fn assert_face(
            prepared: &gpui::PreparedTextEpoch,
            expected: &str,
            width: f32,
        ) -> Result<()> {
            let line = shape(prepared.text_system().clone());
            assert!(prepared.text_system().accepts_layout(&line));
            assert_eq!(line.runs.len(), 2);
            let han = &line.runs[1];
            assert_eq!(han.glyphs.len(), 1);
            assert_ne!(han.glyphs[0].id.0, 0);
            let id = han.font_id;
            assert_eq!(id.text_epoch(), prepared.token().generation());
            assert_eq!(
                prepared.text_system().font_family_name(id).as_deref(),
                Some(format!("Noto Sans CJK {expected}").as_str())
            );
            let font = prepared.text_system().get_font_for_id(id);
            assert!(
                font.is_none(),
                "fallback identities must not alias the Latin primary"
            );
            let advance = prepared.text_system().advance(id, gpui::px(1000.), '骨')?;
            assert_eq!(
                advance.width,
                gpui::px(width),
                "wrong collection face: {expected}"
            );
            Ok(())
        }

        #[test]
        fn i50_locale_han_face_selection() -> Result<()> {
            let fixture = Fixture::new()?;
            let source = source();
            let lang_before = std::env::var_os("LANG");
            for preference in ["jp", "kr", "sc", "tc", "hk", "default"] {
                fixture.preference(preference)?;
                for (locale, expected, width) in [
                    ("ja_JP.UTF-8", "JP", 1000.),
                    ("ko_KR.UTF-8", "KR", 1100.),
                    ("zh_CN.UTF-8", "SC", 1200.),
                    ("zh_TW.UTF-8", "TC", 1300.),
                    ("zh_HK.UTF-8", "HK", 1400.),
                ] {
                    let prepared = fixture.prepare(&source, locale)?;
                    assert_face(&prepared, expected, width)?;
                }
            }
            assert_eq!(std::env::var_os("LANG"), lang_before);
            Ok(())
        }

        #[test]
        fn i50_locale_preference_reload() -> Result<()> {
            let fixture = Fixture::new()?;
            let source = source();
            for (preference, expected, width) in [
                ("jp", "JP", 1000.),
                ("kr", "KR", 1100.),
                ("sc", "SC", 1200.),
                ("tc", "TC", 1300.),
                ("hk", "HK", 1400.),
                ("default", "SC", 1200.),
            ] {
                fixture.preference(preference)?;
                assert_face(&fixture.prepare(&source, "en_US.UTF-8")?, expected, width)?;
            }
            Ok(())
        }

        #[test]
        fn i50_locale_same_language_font_change() -> Result<()> {
            let fixture = Fixture::new()?;
            fixture.preference("jp")?;
            let first = fixture.prepare(&source(), "en_US.UTF-8")?.commit()?;
            let old = shape(first.clone());
            fixture.preference("tc")?;
            let prepared = fixture.prepare(&first, "en_US.UTF-8")?;
            assert_face(&prepared, "TC", 1300.)?;
            assert!(!prepared.text_system().accepts_layout(&old));
            let window = gpui::WindowTextSystem::new(first.clone());
            let run = gpui::TextRun {
                len: "m骨".len(),
                font: gpui::font("Epoch Latin"),
                color: gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let cached_old = window.shape_line(
                "m骨".into(),
                gpui::px(14.),
                std::slice::from_ref(&run),
                None,
            );
            let wrapper = first.line_wrapper(gpui::font("Epoch Latin"), gpui::px(14.));
            let generation = prepared.token().generation();
            first.install_text_epoch(&prepared)?;
            drop(wrapper);
            std::fs::write(fixture.root.join("fonts.conf"), "invalid after preparation")?;
            let installed = window.shape_line("m骨".into(), gpui::px(14.), &[run], None);
            assert_eq!(first.text_epoch(), generation);
            assert_eq!(
                first.font_family_name(installed.runs[1].font_id).as_deref(),
                Some("Noto Sans CJK TC")
            );
            assert!(!first.accepts_layout(&cached_old));
            assert_ne!(installed.width, cached_old.width);
            assert_eq!(cached_old.width, old.width);
            Ok(())
        }

        #[test]
        fn i50_locale_failure_keeps_complete_epoch() -> Result<()> {
            let fixture = Fixture::new()?;
            assert_busy_install_keeps_old_epoch(&fixture)?;
            let staged = fixture.prepare(&source(), "ja_JP.UTF-8")?;
            let weak = Arc::downgrade(staged.text_system());
            drop(
                staged
                    .text_system()
                    .line_wrapper(gpui::font("Epoch Latin"), gpui::px(14.)),
            );
            drop(staged);
            assert!(
                weak.upgrade().is_none(),
                "staged wrapper pool retained a retired epoch"
            );
            let current = fixture.prepare(&source(), "ja_JP.UTF-8")?.commit()?;
            let before = shape(current.clone());
            for failure in 0..3 {
                let mut config = fixture.config("ko_KR.UTF-8");
                match failure {
                    0 => config.locale = "".into(),
                    1 => config.fontconfig_file = Some(fixture.root.join("absent.conf")),
                    _ => config.fonts = vec![gpui::font("Absent primary")],
                }
                let error = match wait(current.prepare_text_epoch(config)?.prepared()) {
                    std::result::Result::Ok(_) => anyhow::bail!("failure case {failure} prepared"),
                    Err(error) => format!("{error:#}"),
                };
                let expected = [
                    "invalid text locale",
                    "fontconfig configuration could not be loaded",
                    "requested font family contains no font matching the other parameters",
                ][failure];
                assert!(error.contains(expected), "case {failure}: {error}");
                assert_eq!(shape(current.clone()).width, before.width);
                assert!(current.accepts_layout(&before));
            }
            Ok(())
        }

        #[test]
        fn i50_locale_uncovered_held_text_prepares() -> Result<()> {
            let fixture = Fixture::new()?;
            let mut config = fixture.config("ja_JP.UTF-8");
            config.text.push("\u{2068}m\u{2069}".into());
            config.text.push("\u{10ffff}".into());
            let prepared = wait(source().prepare_text_epoch(config)?.prepared())?;
            assert_face(&prepared, "JP", 1000.)?;
            Ok(())
        }

        #[test]
        fn i50_locale_stale_font_id_fails_softly() -> Result<()> {
            let fixture = Fixture::new()?;
            let current = fixture.prepare(&source(), "ja_JP.UTF-8")?.commit()?;
            let stale = current.resolve_font(&gpui::font("Epoch Latin"));
            let next = fixture.prepare(&current, "ko_KR.UTF-8")?;
            current.install_text_epoch(&next)?;
            assert_ne!(stale.text_epoch(), current.text_epoch());
            assert_eq!(current.ascent(stale, gpui::px(14.)), gpui::px(0.));
            assert_eq!(current.descent(stale, gpui::px(14.)), gpui::px(0.));
            assert!(current.advance(stale, gpui::px(14.), 'm').is_err());
            assert!(
                next.text_system()
                    .advance(
                        current.resolve_font(&gpui::font("Epoch Latin")),
                        gpui::px(14.),
                        'm'
                    )
                    .is_ok()
            );
            Ok(())
        }

        #[test]
        fn i50_locale_fonts_added_during_preparation_rejected() -> Result<()> {
            let fixture = Fixture::new()?;
            let current = fixture.prepare(&source(), "ja_JP.UTF-8")?.commit()?;
            let generation = current.text_epoch();
            let raced = fixture.prepare(&current, "ko_KR.UTF-8")?;
            current.add_fonts(vec![font("Epoch Added", false, 0, 0).into()])?;
            let error = match current.install_text_epoch(&raced) {
                std::result::Result::Ok(()) => {
                    anyhow::bail!("installed an epoch missing added fonts")
                }
                Err(error) => format!("{error:#}"),
            };
            assert!(
                error.contains("text fonts changed during preparation"),
                "{error}"
            );
            assert_eq!(current.text_epoch(), generation);
            let error = match raced.commit() {
                std::result::Result::Ok(_) => {
                    anyhow::bail!("committed an epoch missing added fonts")
                }
                Err(error) => format!("{error:#}"),
            };
            assert!(
                error.contains("text fonts changed during preparation"),
                "{error}"
            );
            let next = fixture.prepare(&current, "ko_KR.UTF-8")?;
            current.install_text_epoch(&next)?;
            let added = current.resolve_font(&gpui::font("Epoch Added"));
            assert_eq!(
                current.font_family_name(added).as_deref(),
                Some("Epoch Added")
            );
            Ok(())
        }

        #[test]
        fn i50_locale_stale_missing_glyph_report() -> Result<()> {
            let system = CosmicTextSystem::new_without_system_fonts("Epoch Latin");
            system.add_fonts(vec![font("Epoch Latin", false, 0, 0).into()])?;
            let current = system.font_id(&gpui::font("Epoch Latin"))?;
            let stale = FontId::for_text_epoch(current.local_index(), current.text_epoch() + 7)?;
            assert!(system.advance(current, GlyphId(1)).is_ok());
            assert!(system.advance(stale, GlyphId(1)).is_err());
            let runs = [FontRun {
                len: "x".len(),
                font_id: stale,
            }];
            let missing = system.missing_glyphs("x", &runs, [0]);
            assert_eq!(missing.len(), 1);
            assert_eq!(missing[0].grapheme(), "x");
            Ok(())
        }

        #[test]
        fn i50_locale_fonts_added_while_preparing_rejected() -> Result<()> {
            let fixture = Fixture::new()?;
            let entered = Arc::new(std::sync::Barrier::new(2));
            let release = Arc::new(std::sync::Barrier::new(2));
            let source = Arc::new(gpui::TextSystem::new(Arc::new(GatedTextSystem {
                entered: entered.clone(),
                release: release.clone(),
                first: std::sync::atomic::AtomicBool::new(false),
                block_names: std::sync::atomic::AtomicBool::new(false),
                block_add: std::sync::atomic::AtomicBool::new(true),
                inner: CosmicTextSystem::new_without_system_fonts("Epoch Latin"),
            })));
            let adder = source.clone();
            let worker = std::thread::spawn(move || {
                adder.add_fonts(vec![font("Epoch Added", false, 0, 0).into()])
            });
            entered.wait();
            let raced = fixture.prepare(&source, "ja_JP.UTF-8");
            release.wait();
            worker.join().unwrap()?;
            let error = match source.install_text_epoch(&raced?) {
                std::result::Result::Ok(()) => {
                    anyhow::bail!("installed an epoch missing fonts added while preparing")
                }
                Err(error) => format!("{error:#}"),
            };
            assert!(
                error.contains("text fonts changed during preparation"),
                "{error}"
            );
            Ok(())
        }

        #[test]
        fn i50_locale_commit_during_font_add_rejected() -> Result<()> {
            let fixture = Fixture::new()?;
            let entered = Arc::new(std::sync::Barrier::new(2));
            let release = Arc::new(std::sync::Barrier::new(2));
            let source = Arc::new(gpui::TextSystem::new(Arc::new(GatedTextSystem {
                entered: entered.clone(),
                release: release.clone(),
                first: std::sync::atomic::AtomicBool::new(false),
                block_names: std::sync::atomic::AtomicBool::new(false),
                block_add: std::sync::atomic::AtomicBool::new(true),
                inner: CosmicTextSystem::new_without_system_fonts("Epoch Latin"),
            })));
            let prepared = fixture.prepare(&source, "ja_JP.UTF-8")?;
            let adder = source;
            let worker = std::thread::spawn(move || {
                adder.add_fonts(vec![font("Epoch Added", false, 0, 0).into()])
            });
            entered.wait();
            let committed = prepared.commit();
            release.wait();
            worker.join().unwrap()?;
            let error = match committed {
                std::result::Result::Ok(_) => {
                    anyhow::bail!("committed an epoch while fonts were being added")
                }
                Err(error) => format!("{error:#}"),
            };
            assert!(
                error.contains("text fonts changed during preparation"),
                "{error}"
            );
            Ok(())
        }

        struct GatedTextSystem {
            entered: Arc<std::sync::Barrier>,
            release: Arc<std::sync::Barrier>,
            first: std::sync::atomic::AtomicBool,
            block_names: std::sync::atomic::AtomicBool,
            block_add: std::sync::atomic::AtomicBool,
            inner: CosmicTextSystem,
        }

        impl PlatformTextSystem for GatedTextSystem {
            fn prepare_text_epoch(
                &self,
                config: &TextEpochConfig,
                token: &TextEpochToken,
            ) -> Result<Arc<dyn PlatformTextSystem>> {
                if self.first.swap(false, Ordering::AcqRel) {
                    self.entered.wait();
                    self.release.wait();
                }
                self.inner.prepare_text_epoch(config, token)
            }
            fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
                if self.block_add.swap(false, Ordering::AcqRel) {
                    self.entered.wait();
                    self.release.wait();
                }
                self.inner.add_fonts(fonts)
            }
            fn all_font_names(&self) -> Vec<String> {
                if self.block_names.swap(false, Ordering::AcqRel) {
                    self.entered.wait();
                    self.release.wait();
                }
                self.inner.all_font_names()
            }
            fn font_id(&self, font: &Font) -> Result<FontId> {
                self.inner.font_id(font)
            }
            fn font_metrics(&self, id: FontId) -> FontMetrics {
                self.inner.font_metrics(id)
            }
            fn typographic_bounds(&self, id: FontId, glyph: GlyphId) -> Result<Bounds<f32>> {
                self.inner.typographic_bounds(id, glyph)
            }
            fn advance(&self, id: FontId, glyph: GlyphId) -> Result<Size<f32>> {
                self.inner.advance(id, glyph)
            }
            fn glyph_for_char(&self, id: FontId, ch: char) -> Option<GlyphId> {
                self.inner.glyph_for_char(id, ch)
            }
            fn glyph_raster_bounds(
                &self,
                params: &RenderGlyphParams,
            ) -> Result<Bounds<DevicePixels>> {
                self.inner.glyph_raster_bounds(params)
            }
            fn rasterize_glyph(
                &self,
                params: &RenderGlyphParams,
                bounds: Bounds<DevicePixels>,
            ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
                self.inner.rasterize_glyph(params, bounds)
            }
            fn layout_line(&self, text: &str, size: Pixels, runs: &[FontRun]) -> LineLayout {
                self.inner.layout_line(text, size, runs)
            }
            fn recommended_rendering_mode(&self, id: FontId, size: Pixels) -> TextRenderingMode {
                self.inner.recommended_rendering_mode(id, size)
            }
        }

        fn assert_busy_install_keeps_old_epoch(fixture: &Fixture) -> Result<()> {
            let entered = Arc::new(std::sync::Barrier::new(2));
            let release = Arc::new(std::sync::Barrier::new(2));
            let platform = Arc::new(GatedTextSystem {
                entered: entered.clone(),
                release: release.clone(),
                first: std::sync::atomic::AtomicBool::new(false),
                block_names: std::sync::atomic::AtomicBool::new(false),
                block_add: std::sync::atomic::AtomicBool::new(false),
                inner: CosmicTextSystem::new_without_system_fonts("Epoch Latin"),
            });
            let source = Arc::new(gpui::TextSystem::new(platform.clone()));
            let prepared = fixture.prepare(&source, "ja_JP.UTF-8")?;
            platform.block_names.store(true, Ordering::Release);
            let reader = source.clone();
            let worker = std::thread::spawn(move || reader.all_font_names());
            entered.wait();
            let result = source.install_text_epoch(&prepared);
            let unchanged = source.text_epoch() == 0;
            release.wait();
            worker.join().unwrap();
            assert!(result.is_err() && unchanged);
            source.install_text_epoch(&prepared)?;
            assert_eq!(source.text_epoch(), prepared.token().generation());
            assert_face(&prepared, "JP", 1000.)?;
            Ok(())
        }

        fn assert_pending_work_cancelled(fixture: &Fixture) -> Result<()> {
            let entered = Arc::new(std::sync::Barrier::new(2));
            let release = Arc::new(std::sync::Barrier::new(2));
            let source = gpui::TextSystem::new(Arc::new(GatedTextSystem {
                entered: entered.clone(),
                release: release.clone(),
                first: std::sync::atomic::AtomicBool::new(true),
                block_names: std::sync::atomic::AtomicBool::new(false),
                block_add: std::sync::atomic::AtomicBool::new(false),
                inner: CosmicTextSystem::new_without_system_fonts("Epoch Latin"),
            }));
            let running = source.prepare_text_epoch(fixture.config("ja_JP.UTF-8"))?;
            entered.wait();
            let pending = source.prepare_text_epoch(fixture.config("zh_CN.UTF-8"))?;
            let newest = source.prepare_text_epoch(fixture.config("ko_KR.UTF-8"))?;
            let running_stale = !running.token().is_current();
            let pending_stale = !pending.token().is_current();
            release.wait();
            assert!(running_stale && pending_stale);
            assert!(wait(running.prepared()).is_err());
            assert!(wait(pending.prepared()).is_err());
            assert_face(&wait(newest.prepared())?, "KR", 1100.)?;
            Ok(())
        }

        #[test]
        fn i50_locale_epoch_stale_shape_rejected() -> Result<()> {
            use gpui::{AtlasKey, HeadlessAtlas, PlatformAtlas};
            let fixture = Fixture::new()?;
            assert_pending_work_cancelled(&fixture)?;
            let source = source();
            let old = fixture.prepare(&source, "ja_JP.UTF-8")?;
            let old_line = shape(old.text_system().clone());
            let next = fixture.prepare(&source, "ko_KR.UTF-8")?;
            assert!(!old.token().is_current());
            assert!(old.commit().is_err());
            assert!(!next.text_system().accepts_layout(&old_line));
            let new_line = shape(next.text_system().clone());
            let params = |id| RenderGlyphParams {
                font_id: id,
                glyph_id: GlyphId(2),
                font_size: gpui::px(14.),
                subpixel_variant: gpui::point(0, 0),
                scale_factor: 1.,
                is_emoji: false,
                subpixel_rendering: false,
                dilation: 0,
            };
            let atlas = HeadlessAtlas::default();
            let old_key = AtlasKey::Glyph(params(old_line.runs[1].font_id));
            let new_key = AtlasKey::Glyph(params(new_line.runs[1].font_id));
            let mut build = || {
                Ok(Some((
                    gpui::size(gpui::DevicePixels(1), gpui::DevicePixels(1)),
                    Cow::Borrowed(&[255u8][..]),
                )))
            };
            assert!(
                atlas
                    .get_or_insert_with(old_key.clone(), &mut build)?
                    .is_some()
            );
            let before = atlas.generation();
            assert!(atlas.get_or_insert_with(new_key, &mut build)?.is_some());
            assert_ne!(atlas.generation(), before);
            assert!(!atlas.contains(&old_key));
            assert!(atlas.get_or_insert_with(old_key, &mut build).is_err());
            let task = source.prepare_text_epoch(fixture.config("en_US.UTF-8"))?;
            let cancelled = task.token();
            drop(task);
            assert!(!cancelled.is_current());
            Ok(())
        }

        fn font(family: &str, han: bool, variant: u16, base: usize) -> Vec<u8> {
            fn u16_at(bytes: &mut [u8], offset: usize, value: u16) {
                bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
            }
            fn u32_at(bytes: &mut [u8], offset: usize, value: u32) {
                bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
            }
            let mut tables = std::collections::BTreeMap::<[u8; 4], Vec<u8>>::new();
            let mut head = vec![0; 54];
            u32_at(&mut head, 0, 0x10000);
            u32_at(&mut head, 12, 0x5f0f3cf5);
            u16_at(&mut head, 18, 1000);
            u16_at(&mut head, 40, 600);
            u16_at(&mut head, 42, 800);
            tables.insert(*b"head", head);
            let mut hhea = vec![0; 36];
            u32_at(&mut hhea, 0, 0x10000);
            u16_at(&mut hhea, 4, 800);
            u16_at(&mut hhea, 6, (-200i16) as u16);
            u16_at(&mut hhea, 10, 1400);
            u16_at(&mut hhea, 34, 3);
            tables.insert(*b"hhea", hhea);
            let mut maxp = vec![0; 6];
            u32_at(&mut maxp, 0, 0x5000);
            u16_at(&mut maxp, 4, 3);
            tables.insert(*b"maxp", maxp);
            let mut metrics = Vec::new();
            for advance in [500, 600, 1000 + variant * 100] {
                metrics.extend(advance.to_be_bytes());
                metrics.extend(0u16.to_be_bytes());
            }
            tables.insert(*b"hmtx", metrics);
            let groups = if han { 2 } else { 1 };
            let mut cmap = vec![0; 12 + 16 + groups * 12];
            u16_at(&mut cmap, 2, 1);
            u16_at(&mut cmap, 4, 3);
            u16_at(&mut cmap, 6, 10);
            u32_at(&mut cmap, 8, 12);
            u16_at(&mut cmap, 12, 12);
            u32_at(&mut cmap, 16, (16 + groups * 12) as u32);
            u32_at(&mut cmap, 24, groups as u32);
            for (index, cp) in [109, 0x9aa8].iter().take(groups).enumerate() {
                let start = 28 + index * 12;
                u32_at(&mut cmap, start, *cp);
                u32_at(&mut cmap, start + 4, *cp);
                u32_at(&mut cmap, start + 8, index as u32 + 1);
            }
            tables.insert(*b"cmap", cmap);
            let mut names = Vec::new();
            let mut records = Vec::new();
            for (id, value) in [
                (1, family.to_owned()),
                (2, "Regular".into()),
                (4, family.to_owned()),
                (6, family.replace(' ', "")),
                (16, family.to_owned()),
            ] {
                let start = names.len();
                for unit in value.encode_utf16() {
                    names.extend(unit.to_be_bytes());
                }
                for value in [
                    3u16,
                    1,
                    0x409,
                    id,
                    (names.len() - start) as u16,
                    start as u16,
                ] {
                    records.extend(value.to_be_bytes());
                }
            }
            let mut name = vec![0; 6];
            u16_at(&mut name, 2, 5);
            u16_at(&mut name, 4, 66);
            name.extend(records);
            name.extend(names);
            tables.insert(*b"name", name);
            let mut post = vec![0; 32];
            u32_at(&mut post, 0, 0x30000);
            tables.insert(*b"post", post);
            let mut os2 = vec![0; 96];
            u16_at(&mut os2, 0, 4);
            u16_at(&mut os2, 2, 600);
            u16_at(&mut os2, 4, 400);
            u16_at(&mut os2, 6, 5);
            u16_at(&mut os2, 62, 0x40);
            u16_at(&mut os2, 64, 109);
            u16_at(&mut os2, 66, if han { 0x9aa8 } else { 109 });
            u16_at(&mut os2, 68, 800);
            u16_at(&mut os2, 70, (-200i16) as u16);
            u16_at(&mut os2, 74, 800);
            u16_at(&mut os2, 76, 200);
            tables.insert(*b"OS/2", os2);
            let mut glyf = Vec::new();
            for _ in 0..3 {
                let mut glyph = vec![0; 36];
                u16_at(&mut glyph, 0, 1);
                u16_at(&mut glyph, 6, 500);
                u16_at(&mut glyph, 8, 700);
                u16_at(&mut glyph, 10, 3);
                glyph[14..18].fill(1);
                for (index, delta) in [0i16, 500, 0, -500, 0, 0, 700, 0].iter().enumerate() {
                    u16_at(&mut glyph, 18 + index * 2, *delta as u16);
                }
                glyf.extend(glyph);
            }
            tables.insert(*b"glyf", glyf);
            let mut loca = Vec::new();
            for offset in [0u16, 18, 36, 54] {
                loca.extend(offset.to_be_bytes());
            }
            tables.insert(*b"loca", loca);
            let count = tables.len();
            let mut output = vec![0; 12 + count * 16];
            u32_at(&mut output, 0, 0x10000);
            u16_at(&mut output, 4, count as u16);
            u16_at(&mut output, 6, 128);
            u16_at(&mut output, 8, 3);
            u16_at(&mut output, 10, count as u16 * 16 - 128);
            for (index, (tag, mut data)) in tables.into_iter().enumerate() {
                let length = data.len();
                while data.len() % 4 != 0 {
                    data.push(0);
                }
                let checksum = data
                    .chunks_exact(4)
                    .map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()))
                    .fold(0u32, u32::wrapping_add);
                let record = 12 + index * 16;
                output[record..record + 4].copy_from_slice(&tag);
                u32_at(&mut output, record + 4, checksum);
                let offset = base + output.len();
                u32_at(&mut output, record + 8, offset as u32);
                u32_at(&mut output, record + 12, length as u32);
                output.extend(data);
            }
            output
        }
    }
    use super::*;
    #[cfg(feature = "test-support")]
    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    use std::{cell::RefCell, rc::Rc};

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn all_font_names_tracks_available_families() -> Result<()> {
        let text_system = gpui::TextSystem::new(Arc::new(
            CosmicTextSystem::new_without_system_fonts("IBM Plex Sans"),
        ));
        assert!(text_system.all_font_names().is_empty());

        text_system.add_fonts(vec![Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/lilex/Lilex-Regular.ttf"
        ))])?;
        assert_eq!(text_system.all_font_names(), ["Lilex"]);

        text_system.add_fonts(vec![
            Cow::Borrowed(IBM_PLEX),
            Cow::Borrowed(include_bytes!("../../../assets/fonts/lilex/Lilex-Bold.ttf")),
        ])?;
        assert_eq!(text_system.all_font_names(), ["IBM Plex Sans", "Lilex"]);
        Ok(())
    }

    fn fid(i: usize) -> FontId {
        FontId(i)
    }

    fn chain(ids: &[usize]) -> SmallVec<[(FontId, SharedString); 4]> {
        ids.iter()
            .map(|&i| (fid(i), SharedString::from(format!("fb{i}"))))
            .collect()
    }

    fn span(start: usize, end: usize, slot: Option<usize>, font_id: FontId) -> RunSpan {
        RunSpan {
            start,
            end,
            slot,
            font_id,
        }
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    const IBM_PLEX: &[u8] =
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");
    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    const LILEX: &[u8] = include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf");

    /// Every code point of `Bidi_Class=B`, each of which starts a new bidi
    /// paragraph and so can split one line into mixed-direction paragraphs.
    const SEPARATORS: &[char] = &[
        '\u{000a}', '\u{000d}', '\u{001c}', '\u{001d}', '\u{001e}', '\u{0085}', '\u{2029}',
    ];

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    fn text_system() -> Result<CosmicTextSystem> {
        let text_system = CosmicTextSystem::new_without_system_fonts("IBM Plex Sans");
        text_system.add_fonts(vec![Cow::Borrowed(IBM_PLEX)])?;
        Ok(text_system)
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn font_properties_describe_the_selected_face() -> Result<()> {
        let text_system = text_system()?;
        let regular = gpui::font("IBM Plex Sans");
        let regular_id = text_system.font_id(&regular)?;
        for (weight, style) in [
            (gpui::FontWeight::MEDIUM, gpui::FontStyle::Normal),
            (gpui::FontWeight::BOLD, gpui::FontStyle::Italic),
            (gpui::FontWeight::NORMAL, gpui::FontStyle::Oblique),
        ] {
            let requested = Font {
                weight,
                style,
                ..regular.clone()
            };
            let font_id = text_system.font_id(&requested)?;
            assert_eq!(font_id, regular_id);
            assert_eq!(
                text_system.font_weight_and_style(font_id)?,
                (gpui::FontWeight::NORMAL, gpui::FontStyle::Normal)
            );
        }
        Ok(())
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    fn layout_text(text_system: &CosmicTextSystem, text: &str) -> Result<LineLayout> {
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let runs = [FontRun {
            len: text.len(),
            font_id,
        }];
        Ok(text_system.layout_line(text, gpui::px(14.0), &runs))
    }

    /// Mirrors the original crash: mixed-direction text reaching the shaper
    /// through `shape_text`, which only splits lines on `\n`.
    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn shape_text_with_mixed_direction_paragraphs() -> Result<()> {
        let platform_text_system = Arc::new(text_system()?);
        let text_system = Arc::new(gpui::TextSystem::new(platform_text_system));
        let window_text_system = gpui::WindowTextSystem::new(text_system);

        let text: SharedString = "first line\n\u{05d0}\u{001c}A".into();
        let runs = [gpui::TextRun {
            len: text.len(),
            font: gpui::font("IBM Plex Sans"),
            ..Default::default()
        }];

        let lines = window_text_system.shape_text(text, gpui::px(14.0), &runs, None, None)?;

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].len(), "\u{05d0}\u{001c}A".len());
        assert!(lines[1].width() > Pixels::ZERO);
        Ok(())
    }

    #[cfg(feature = "test-support")]
    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn reports_graphemes_that_exhaust_font_fallback() -> Result<()> {
        let platform_text_system = Arc::new(text_system()?);
        let dispatcher = gpui::TestDispatcher::new(0);
        let cx =
            gpui::TestAppContext::build_with_text_system(dispatcher, None, platform_text_system);
        let observed = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let observed = observed.clone();
            cx.on_missing_glyphs(move |missing_glyphs, _| {
                observed.borrow_mut().extend_from_slice(missing_glyphs);
            })
        });
        let text: SharedString = "界".into();

        cx.update(|cx| {
            let text_system = gpui::WindowTextSystem::new(cx.text_system().clone());
            let runs = [gpui::TextRun {
                len: text.len(),
                font: gpui::font("IBM Plex Sans"),
                ..Default::default()
            }];
            text_system.shape_line(text, gpui::px(14.0), &runs, None);
        });
        cx.run_until_parked();

        let observed = observed.borrow();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].grapheme(), "界");
        assert_eq!(
            observed[0].font_class(),
            gpui::FallbackFontClass::Proportional
        );
        Ok(())
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn combines_missing_glyphs_from_one_grapheme() -> Result<()> {
        let text_system = text_system()?;
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let text = "x\u{0301}";
        let runs = [FontRun {
            len: text.len(),
            font_id,
        }];

        let missing_glyphs = text_system
            .0
            .read()
            .missing_glyphs(text, &runs, [0, "x".len()]);

        assert_eq!(missing_glyphs.len(), 1);
        assert_eq!(missing_glyphs[0].grapheme(), text);
        Ok(())
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn adding_fonts_invalidates_cached_line_layouts() -> Result<()> {
        let platform_text_system = Arc::new(text_system()?);
        let text_system = Arc::new(gpui::TextSystem::new(platform_text_system.clone()));
        let window_text_system = gpui::WindowTextSystem::new(text_system.clone());
        let text: SharedString = "cached text".into();
        let runs = [gpui::TextRun {
            len: text.len(),
            font: gpui::font("IBM Plex Sans"),
            ..Default::default()
        }];

        let first_layout = window_text_system.shape_line(text.clone(), gpui::px(14.0), &runs, None);
        let cached_layout =
            window_text_system.shape_line(text.clone(), gpui::px(14.0), &runs, None);
        assert!(std::ptr::eq::<LineLayout>(
            &**first_layout,
            &**cached_layout
        ));
        let loaded_font_count = platform_text_system.0.read().loaded_fonts.len();

        text_system.add_fonts(vec![Cow::Borrowed(LILEX)])?;

        let refreshed_layout = window_text_system.shape_line(text, gpui::px(14.0), &runs, None);
        assert!(!std::ptr::eq::<LineLayout>(
            &**first_layout,
            &**refreshed_layout
        ));
        assert_eq!(
            platform_text_system.0.read().loaded_fonts.len(),
            loaded_font_count
        );
        Ok(())
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn layout_line_with_mixed_direction_paragraphs() -> Result<()> {
        let text_system = text_system()?;

        for separator in SEPARATORS {
            for text in [
                format!("\u{05d0}{separator}A"),
                format!("A{separator}\u{05d0}"),
            ] {
                let layout = layout_text(&text_system, &text)?;

                assert_eq!(layout.len, text.len(), "{text:?}");
                assert!(layout.width > Pixels::ZERO, "{text:?}");
                assert!(
                    layout.runs.iter().any(|run| !run.glyphs.is_empty()),
                    "{text:?}"
                );
            }
        }

        Ok(())
    }

    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn layout_line_with_separators_at_line_edges() -> Result<()> {
        let text_system = text_system()?;

        for text in [
            "\u{001c}",
            "\u{001c}\u{001c}",
            "\u{001c}\u{05d0}",
            "\u{05d0}\u{001c}",
            "\u{05d0}\u{001c}\u{001c}A",
            "\u{001c}\u{05d0}\u{001c}A\u{001c}",
        ] {
            let layout = layout_text(&text_system, text)?;
            assert_eq!(layout.len, text.len(), "{text:?}");
        }

        Ok(())
    }

    /// Glyph indices must stay absolute and positions ordered across segment
    /// boundaries, otherwise cursor placement and hit testing desync. Uses
    /// single-direction text so visual order matches logical order.
    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn layout_line_keeps_indices_and_positions_ordered_across_paragraphs() -> Result<()> {
        let text_system = text_system()?;
        let text = "ab\u{001c}cd\u{2029}ef";
        let layout = layout_text(&text_system, text)?;

        let glyphs: Vec<_> = layout.runs.iter().flat_map(|run| &run.glyphs).collect();
        assert!(!glyphs.is_empty());

        for glyph in &glyphs {
            assert!(glyph.index < text.len(), "{:?}", glyph.index);
            assert!(text.is_char_boundary(glyph.index), "{:?}", glyph.index);
        }
        for pair in glyphs.windows(2) {
            assert!(pair[0].index < pair[1].index);
            assert!(pair[0].position.x <= pair[1].position.x);
        }

        // Every segment contributes width, so the whole line is wider than its
        // leading paragraph alone.
        assert!(layout.width > layout_text(&text_system, "ab")?.width);
        Ok(())
    }

    /// A font run boundary that does not line up with a paragraph boundary must
    /// still be clipped to the right segments.
    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn layout_line_with_font_run_straddling_a_separator() -> Result<()> {
        let text_system = text_system()?;
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let text = "ab\u{001c}\u{05d0}\u{05d1}";

        // The run boundary falls inside the trailing RTL paragraph.
        let runs = [
            FontRun {
                len: "ab\u{001c}\u{05d0}".len(),
                font_id,
            },
            FontRun {
                len: "\u{05d1}".len(),
                font_id,
            },
        ];
        let layout = text_system.layout_line(text, gpui::px(14.0), &runs);

        assert_eq!(layout.len, text.len());
        assert!(layout.width > Pixels::ZERO);
        Ok(())
    }

    /// Lines with no separator take the fast path and must be shaped exactly as
    /// they were before paragraph splitting existed.
    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn layout_line_without_separators_takes_fast_path() -> Result<()> {
        let text_system = text_system()?;

        for text in [
            "hello world",
            "\u{05d0}\u{05d1}\u{05d2}",
            "mixed \u{05d0}\u{05d1}",
        ] {
            assert!(!contains_paragraph_separator(text), "{text:?}");
            let layout = layout_text(&text_system, text)?;
            assert_eq!(layout.len, text.len(), "{text:?}");
            assert!(layout.width > Pixels::ZERO, "{text:?}");
        }

        Ok(())
    }

    /// cosmic-text sums word widths to get a line's width but accumulates glyph
    /// advances to position glyphs, so a trailing zero-advance glyph (here a
    /// zero-width space) can land a few ulps past the width. When that glyph is a
    /// wrap boundary, the row before it extends past the line's width, and hit
    /// testing in that sliver used to panic (ZED-BW8, ZED-75K, ZED-81Z).
    #[cfg(not(feature = "ci-exclude-bundled-font-tests"))]
    #[test]
    fn index_for_position_past_line_width() -> Result<()> {
        let text_system = Arc::new(gpui::TextSystem::new(Arc::new(text_system()?)));
        let window_text_system = gpui::WindowTextSystem::new(text_system);
        let text: SharedString = "Warning: this will delete files\u{200b}".into();
        let runs = [gpui::TextRun {
            len: text.len(),
            font: gpui::font("IBM Plex Sans"),
            ..Default::default()
        }];
        let lines =
            window_text_system.shape_text(text, gpui::px(14.), &runs, Some(gpui::px(4.)), None)?;
        let line = &lines[0];
        let width = line.unwrapped_layout.width;
        let boundary_glyph = |row: usize| {
            let boundary = line.wrap_boundaries()[row];
            &line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix]
        };
        let row = (1..line.wrap_boundaries().len())
            .find(|row| boundary_glyph(*row).position.x > width)
            .expect("trailing zero-width space should be a wrap boundary past the line width");
        let row_start_x = f32::from(boundary_glyph(row - 1).position.x);
        let row_end = boundary_glyph(row);

        let mut x = f32::from(width) - row_start_x;
        while x + row_start_x < f32::from(width) {
            x = x.next_up();
        }
        assert!(gpui::px(x + row_start_x) < row_end.position.x);

        let line_height = gpui::px(20.);
        let position = gpui::point(gpui::px(x), line_height * row as f32 + gpui::px(1.));
        assert_eq!(
            line.index_for_position(position, line_height),
            Err(row_end.index)
        );
        Ok(())
    }

    #[test]
    fn paragraph_separator_detection() {
        for separator in SEPARATORS {
            assert!(is_paragraph_separator(*separator), "{separator:?}");
            assert!(contains_paragraph_separator(&format!("a{separator}b")));
        }

        for text in [
            "",
            "plain ascii",
            "\u{05d0}",
            "tab\there",
            "emoji \u{1f600}",
        ] {
            assert!(!contains_paragraph_separator(text), "{text:?}");
        }
    }

    #[test]
    fn font_runs_are_clipped_to_segment() {
        let runs = [
            FontRun {
                len: 3,
                font_id: fid(1),
            },
            FontRun {
                len: 4,
                font_id: fid(2),
            },
        ];

        assert_eq!(clip_font_runs(&runs, 0..7).as_slice(), &runs);
        assert_eq!(
            clip_font_runs(&runs, 2..5).as_slice(),
            &[
                FontRun {
                    len: 1,
                    font_id: fid(1)
                },
                FontRun {
                    len: 2,
                    font_id: fid(2)
                },
            ]
        );
        assert_eq!(
            clip_font_runs(&runs, 3..7).as_slice(),
            &[FontRun {
                len: 4,
                font_id: fid(2)
            }]
        );
        assert!(clip_font_runs(&runs, 5..5).is_empty());
    }

    #[test]
    fn primary_wins_over_current_fallback_when_primary_covers() {
        let primary = fid(0);
        let fb = chain(&[1, 2]);
        let covers = |id: FontId, _: char| id == fid(0) || id == fid(1);
        assert_eq!(
            pick_covering_slot('a', Some(0), primary, &fb, &covers),
            None
        );
    }

    #[test]
    fn primary_preferred_over_fallback_when_both_cover() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |_: FontId, _: char| true;
        assert_eq!(pick_covering_slot('a', None, primary, &fb, &covers), None);
    }

    #[test]
    fn falls_through_chain_in_order() {
        let primary = fid(0);
        let fb = chain(&[1, 2, 3]);
        // only fallback 2 at index 1 covers.
        let covers = |id: FontId, _: char| id == fid(2);
        assert_eq!(
            pick_covering_slot('字', None, primary, &fb, &covers),
            Some(1)
        );
    }

    #[test]
    fn no_coverage_returns_primary() {
        let primary = fid(0);
        let fb = chain(&[1, 2]);
        let covers = |_: FontId, _: char| false;
        // nothing covers. return `None` so the `cosmic-text` built in script
        // fallback can take over during shaping.
        assert_eq!(
            pick_covering_slot('\u{1F600}', Some(1), primary, &fb, &covers),
            None
        );
    }

    #[test]
    fn empty_chain_always_returns_primary() {
        let primary = fid(0);
        let fb: SmallVec<[(FontId, SharedString); 4]> = SmallVec::new();
        let covers = |_: FontId, _: char| false;
        assert_eq!(pick_covering_slot('a', None, primary, &fb, &covers), None);
    }

    #[test]
    fn slot_font_id_resolution() {
        let primary = fid(7);
        let fb = chain(&[10, 20]);
        assert_eq!(slot_font_id(None, primary, &fb), fid(7));
        assert_eq!(slot_font_id(Some(0), primary, &fb), fid(10));
        assert_eq!(slot_font_id(Some(1), primary, &fb), fid(20));
    }

    #[test]
    fn run_spans_with_no_chain_emit_one_primary_span() {
        let primary = fid(0);
        let fb: SmallVec<[(FontId, SharedString); 4]> = SmallVec::new();
        let covers = |_: FontId, _: char| false;
        let text = "hello";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), None, primary)]);
    }

    #[test]
    fn run_spans_use_byte_offsets_for_multibyte_chars() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // primary covers ascii. fallback covers cjk.
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        let text = "a字b";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        // '字' is 3 bytes so split is at 1 then 4.
        assert_eq!(
            spans.as_slice(),
            &[
                span(0, 1, None, primary),
                span(1, 4, Some(0), fid(1)),
                span(4, 5, None, primary),
            ]
        );
    }

    #[test]
    fn run_spans_respect_run_offset() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        // outer text has a prefix that is not part of this run.
        let text = "xx字y";
        let run_offset = 2;
        let run_len = text.len() - run_offset;
        let spans = compute_run_spans(text, run_offset, run_len, primary, &fb, &covers);
        assert_eq!(
            spans.as_slice(),
            &[span(2, 5, Some(0), fid(1)), span(5, 6, None, primary)]
        );
    }

    #[test]
    fn run_spans_keep_combining_marks_with_base_in_fallback() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // primary covers ascii only. fallback covers the base char.
        // combining mark must stay in the fallback span even when fallback
        // does not advertise coverage of it.
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                ch == '\u{0905}'
            }
        };
        // \u{0905} devanagari short a + \u{0902} candrabindu mark.
        let text = "\u{0905}\u{0902}";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_keep_zwj_inside_emoji_cluster() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // only fallback covers the emoji codepoints. zwj must not split.
        let covers = |id: FontId, ch: char| id == fid(1) && ch != '\u{200D}';
        // family zwj sequence woman zwj girl.
        let text = "\u{1F469}\u{200D}\u{1F467}";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_collapse_adjacent_same_slot() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        let text = "字字字";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_empty_run_returns_no_spans() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |_: FontId, _: char| true;
        let spans = compute_run_spans("anything", 3, 0, primary, &fb, &covers);
        assert!(spans.is_empty());
    }
}

#[cfg(test)]
mod mark_offset_tests;
