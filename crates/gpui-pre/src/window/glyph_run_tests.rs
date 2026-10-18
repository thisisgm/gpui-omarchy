// ! A glyph run pushes the sprites painting its glyphs one by one pushes, and looks tiles up only
// when its key moves.
use crate::{
    AtlasState, Bounds, ContentMask, DevicePixels, Font, FontId, FontMetrics, FontRun, GlyphId,
    GlyphRun, HeadlessAtlasBackend, Hsla, IntoElement, LineLayout, NoopTextSystem, Pixels,
    PlatformTextSystem, Point, RenderGlyphParams, ShapedGlyph, ShapedRun, Size, Styled as _,
    TestAppContext, TestDispatcher, TextRenderingMode, VisualTestContext, Window, canvas, point,
    px, size,
};
use anyhow::Result;
use std::{borrow::Cow, cell::RefCell, rc::Rc, sync::Arc};

// Raster bounds that differ by glyph, subpixel variant, scale, dilation and emoji, so a stale tile
// or offset shows.
struct Glyphs;

impl PlatformTextSystem for Glyphs {
    fn add_fonts(&self, _: Vec<Cow<'static, [u8]>>) -> Result<()> {
        Ok(())
    }
    fn all_font_names(&self) -> Vec<String> {
        Vec::new()
    }
    fn font_id(&self, _: &Font) -> Result<FontId> {
        Ok(FontId(1))
    }
    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        NoopTextSystem.font_metrics(font_id)
    }
    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        NoopTextSystem.typographic_bounds(font_id, glyph_id)
    }
    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        NoopTextSystem.advance(font_id, glyph_id)
    }
    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        NoopTextSystem.glyph_for_char(font_id, ch)
    }
    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let id = params.glyph_id.0 as i32;
        let scale = (params.scale_factor * 2.) as i32;
        let (variant, dilation) = (params.subpixel_variant.x as i32, params.dilation as i32);
        Ok(match (id, params.is_emoji) {
            (0, _) => Bounds::default(),
            (_, true) => Bounds {
                origin: point(DevicePixels(-1), DevicePixels(-6 * scale)),
                size: size(DevicePixels(8 * scale), DevicePixels(8 * scale)),
            },
            _ => Bounds {
                origin: point(
                    DevicePixels(variant - id % 3),
                    DevicePixels(-8 - id % 5 - dilation),
                ),
                size: size(
                    DevicePixels(3 + id % 7 + variant + dilation),
                    DevicePixels(9 + id % 4 + scale),
                ),
            },
        })
    }
    fn rasterize_glyph(
        &self,
        _: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        let area = raster_bounds.size.width.0 * raster_bounds.size.height.0;
        Ok((raster_bounds.size, vec![0; area as usize]))
    }
    fn layout_line(&self, _: &str, font_size: Pixels, _: &[FontRun]) -> LineLayout {
        LineLayout {
            font_size,
            ..LineLayout::default()
        }
    }
    fn recommended_rendering_mode(&self, font_id: FontId, _: Pixels) -> TextRenderingMode {
        if font_id.0 == 1 {
            TextRenderingMode::Subpixel
        } else {
            TextRenderingMode::Grayscale
        }
    }
    fn glyph_dilation_for_color(&self, color: Hsla) -> u8 {
        u8::from(color.l > 0.5)
    }
}

// Two fonts, fractional advances, a zero-sized glyph, vertical offsets either side of the baseline,
// and an emoji.
fn line() -> Arc<LineLayout> {
    let glyph = |id: u32, x: f32, y: f32, is_emoji: bool| ShapedGlyph {
        id: GlyphId(id),
        position: point(px(x), px(y)),
        index: 0,
        is_emoji,
    };
    let first = (1..12)
        .map(|id| glyph(id, id as f32 * 8.37 + 0.13, 0., false))
        .chain([glyph(0, 101., 0., false), glyph(13, 104.2, -1.6, false)]);
    Arc::new(LineLayout {
        font_size: px(14.),
        width: px(140.),
        ascent: px(11.),
        descent: px(3.),
        runs: vec![
            ShapedRun {
                font_id: FontId(1),
                glyphs: first.collect(),
            },
            ShapedRun {
                font_id: FontId(2),
                glyphs: vec![
                    glyph(21, 110.55, 0., false),
                    glyph(22, 118.9, 0.35, false),
                    glyph(5, 126.31, 0., true),
                ],
            },
        ],
        len: 16,
    })
}

fn by_glyph(window: &mut Window, line: &LineLayout, origin: Point<Pixels>, color: Hsla) {
    for run in &line.runs {
        for glyph in &run.glyphs {
            let at = point(origin.x + glyph.position.x, origin.y + glyph.position.y);
            if glyph.is_emoji {
                window
                    .paint_emoji(at, run.font_id, glyph.id, line.font_size)
                    .unwrap();
            } else {
                window
                    .paint_glyph(at, run.font_id, glyph.id, line.font_size, color)
                    .unwrap();
            }
        }
    }
}

// Paints into a cleared scene and returns every sprite the paint pushed, read before anything else
// draws.
fn sprites(cx: &mut VisualTestContext, paint: impl FnOnce(&mut Window) + 'static) -> Vec<String> {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let out = Rc::clone(&seen);
    cx.update(|window, _| window.next_frame.scene.clear());
    cx.draw(
        point(px(0.), px(0.)),
        size(px(400.), px(300.)),
        move |_, _| {
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let scene = &window.next_frame.scene;
                    let from = (
                        scene.monochrome_sprites.len(),
                        scene.subpixel_sprites.len(),
                        scene.polychrome_sprites.len(),
                    );
                    paint(window);
                    let scene = &window.next_frame.scene;
                    let mono = scene.monochrome_sprites[from.0..]
                        .iter()
                        .map(|s| format!("{s:?}"));
                    let subpixel = scene.subpixel_sprites[from.1..]
                        .iter()
                        .map(|s| format!("{s:?}"));
                    let poly = scene.polychrome_sprites[from.2..]
                        .iter()
                        .map(|s| format!("{s:?}"));
                    *out.borrow_mut() = mono.chain(subpixel).chain(poly).collect();
                },
            )
            .size_full()
            .into_any_element()
        },
    );
    seen.take()
}

#[derive(Clone, Copy)]
struct Step {
    origin: Point<Pixels>,
    color: Hsla,
    scale: f32,
    opacity: Option<f32>,
    mask: Option<Bounds<Pixels>>,
}

fn paint(window: &mut Window, step: Step, f: impl FnOnce(&mut Window)) {
    window.with_element_opacity(step.opacity, |window| {
        window.with_content_mask(step.mask.map(|bounds| ContentMask { bounds }), f)
    });
}

fn gray(l: f32) -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l,
        a: 1.,
    }
}

#[test]
fn a_glyph_run_pushes_the_sprites_its_glyphs_push_one_by_one_and_resolves_only_when_its_key_moves()
{
    let mut app =
        TestAppContext::build_with_text_system(TestDispatcher::new(0), None, Arc::new(Glyphs));
    let cx = app.add_empty_window();
    let run = Rc::new(GlyphRun::new(line()));
    let at = |x: f32, y: f32| point(px(x), px(y));
    let base = Step {
        origin: at(10., 20.),
        color: gray(0.2),
        scale: 2.,
        opacity: None,
        mask: None,
    };
    let mask = Some(Bounds::new(at(40., 30.), size(px(60.), px(24.))));
    // Each step paints the run under one new condition, with whether that condition must look tiles
    // up again.
    let steps = [
        (base, true),
        (
            Step {
                origin: at(10., 57.4),
                ..base
            },
            false,
        ),
        (
            Step {
                origin: at(10., 3.5),
                ..base
            },
            false,
        ),
        (
            Step {
                origin: at(10.13, 3.5),
                ..base
            },
            true,
        ),
        (
            Step {
                origin: at(10.13, 3.5),
                color: gray(0.9),
                ..base
            },
            true,
        ),
        (
            Step {
                origin: at(10.13, 99.75),
                color: gray(0.95),
                ..base
            },
            false,
        ),
        (
            Step {
                origin: at(10.13, 99.75),
                color: gray(0.9),
                scale: 1.5,
                ..base
            },
            true,
        ),
        (
            Step {
                origin: at(37.62, 41.3),
                opacity: Some(0.5),
                mask,
                scale: 1.5,
                ..base
            },
            true,
        ),
        (
            Step {
                origin: at(37.62, 28.8),
                opacity: Some(0.5),
                mask,
                scale: 1.5,
                ..base
            },
            false,
        ),
    ];
    let mut resolves = 0;
    for (number, (step, resolves_again)) in steps.into_iter().enumerate() {
        cx.simulate_scale_factor_change(step.scale);
        let line = Arc::clone(run.line());
        let expected = sprites(cx, move |window| {
            paint(window, step, |window| {
                by_glyph(window, &line, step.origin, step.color)
            })
        });
        let painted_run = Rc::clone(&run);
        let painted = sprites(cx, move |window| {
            paint(window, step, |window| {
                window
                    .paint_glyph_run(step.origin, &painted_run, step.color)
                    .unwrap()
            })
        });
        assert!(
            !expected.is_empty(),
            "step {number} painted nothing to compare"
        );
        assert_eq!(
            painted, expected,
            "step {number} differs from painting glyph by glyph"
        );
        resolves += usize::from(resolves_again);
        assert_eq!(
            run.resolves(),
            resolves,
            "step {number} looked tiles up when it should not, or missed one"
        );
    }
}

#[test]
fn a_glyph_run_moved_to_another_atlas_looks_its_tiles_up_there() {
    let mut app =
        TestAppContext::build_with_text_system(TestDispatcher::new(0), None, Arc::new(Glyphs));
    let run = Rc::new(GlyphRun::new(line()));
    let origin = point(px(10.), px(20.));
    let color = gray(0.2);
    let first = app.add_empty_window();
    let painted_run = Rc::clone(&run);
    sprites(first, move |window| {
        window.paint_glyph_run(origin, &painted_run, color).unwrap()
    });
    // The second atlas hands out its ids in another order, so a tile carried over from the first
    // would show.
    let second = app.add_empty_window();
    let line = Arc::clone(run.line());
    sprites(second, move |window| {
        by_glyph(window, &line, point(px(10.13), px(20.)), color)
    });
    let line = Arc::clone(run.line());
    let expected = sprites(second, move |window| by_glyph(window, &line, origin, color));
    let painted_run = Rc::clone(&run);
    let painted = sprites(second, move |window| {
        window.paint_glyph_run(origin, &painted_run, color).unwrap()
    });
    assert!(!expected.is_empty());
    assert_eq!(painted, expected);
    assert_eq!(run.resolves(), 2);
}

#[test]
fn every_atlas_and_every_clear_gets_its_own_generation() {
    let mut first = AtlasState::new(HeadlessAtlasBackend::default());
    let second = AtlasState::new(HeadlessAtlasBackend::default());
    let before = first.generation();
    assert_ne!(before, second.generation());
    first.clear(|_| {});
    assert_ne!(first.generation(), before);
    assert_ne!(first.generation(), second.generation());
}

#[test]
fn a_glyph_run_looks_a_removed_glyph_tile_up_again() {
    let mut app =
        TestAppContext::build_with_text_system(TestDispatcher::new(0), None, Arc::new(Glyphs));
    let cx = app.add_empty_window();
    let run = Rc::new(GlyphRun::new(line()));
    let origin = point(px(10.), px(20.));
    let color = gray(0.2);
    let painted_run = Rc::clone(&run);
    sprites(cx, move |window| {
        window.paint_glyph_run(origin, &painted_run, color).unwrap();
    });
    let removed_run = Rc::clone(&run);
    cx.update(move |window, _| {
        let line = removed_run.line();
        let shaped = &line.runs[0];
        let glyph = &shaped.glyphs[0];
        let scale_factor = window.scale_factor();
        let x = (origin.x + glyph.position.x).0 * scale_factor;
        let variants = crate::SUBPIXEL_VARIANTS_X as f32;
        let quantized_x = super::round_half_toward_zero(x * variants) / variants;
        let key = RenderGlyphParams {
            font_id: shaped.font_id,
            glyph_id: glyph.id,
            font_size: line.font_size,
            subpixel_variant: point((quantized_x.fract() * variants) as u8, 0),
            scale_factor,
            is_emoji: false,
            subpixel_rendering: window
                .should_use_subpixel_rendering(shaped.font_id, line.font_size),
            dilation: window.text_system().glyph_dilation_for_color(color),
        }
        .into();
        assert!(
            window.sprite_atlas.contains(&key),
            "the run's first glyph has no tile"
        );
        let before = window.sprite_atlas.generation();
        window.sprite_atlas.remove(&key);
        assert!(!window.sprite_atlas.contains(&key));
        assert_ne!(
            window.sprite_atlas.generation(),
            before,
            "removing a glyph's tile kept the generation"
        );
    });
    let painted_run = Rc::clone(&run);
    let painted = sprites(cx, move |window| {
        window.paint_glyph_run(origin, &painted_run, color).unwrap();
    });
    let line = Arc::clone(run.line());
    let expected = sprites(cx, move |window| by_glyph(window, &line, origin, color));
    assert!(!expected.is_empty());
    assert_eq!(
        painted, expected,
        "the glyph run reused a removed atlas tile"
    );
    assert_eq!(
        run.resolves(),
        2,
        "the removal did not make the run look its tiles up again"
    );
}
