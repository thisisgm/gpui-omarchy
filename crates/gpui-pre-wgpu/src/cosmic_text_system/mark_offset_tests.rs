use super::*;
use gpui::px;

const NOTO_SANS_SINHALA: &[u8] = include_bytes!("../../tests/fonts/NotoSansSinhala-Regular.ttf");

/// Each glyph's id and origin for `text` shaped at 1000 px, which is one pixel per font unit.
fn origins(text: &str) -> Vec<(u32, f32, f32)> {
    let text_system = CosmicTextSystem::new_without_system_fonts("Noto Sans Sinhala");
    text_system
        .add_fonts(vec![Cow::Borrowed(NOTO_SANS_SINHALA)])
        .unwrap();
    let font_id = text_system
        .font_id(&gpui::font("Noto Sans Sinhala"))
        .unwrap();
    let runs = [FontRun {
        len: text.len(),
        font_id,
    }];
    let layout = text_system.layout_line(text, px(1000.), &runs);
    layout
        .runs
        .iter()
        .flat_map(|run| &run.glyphs)
        .map(|glyph| {
            let position = glyph.position.map(f32::from);
            (glyph.id.0, position.x, position.y)
        })
        .collect()
}

#[test]
fn sinhala_vowel_signs_and_al_lakuna_sit_on_their_consonant() {
    let cases: [(&str, &[(u32, f32, f32)]); 4] = [
        (
            "පළමු",
            &[
                (48, 0., 0.),
                (62, 830., 0.),
                (52, 1614., 0.),
                (106, 1686., 0.),
            ],
        ),
        (
            "අවසන්",
            &[
                (5, 0., 0.),
                (57, 720., 0.),
                (60, 1548., 0.),
                (46, 2446., 0.),
                (79, 3088., 0.),
            ],
        ),
        (
            "පරිශීලක",
            &[
                (48, 0., 0.),
                (176, 830., 0.),
                (58, 1532., 0.),
                (96, 1699., 0.),
                (56, 2447., 0.),
                (23, 3299., 0.),
            ],
        ),
        // GPOS lowers this al-lakuna by 2 units, and y grows downward.
        ("ජ්", &[(31, 0., 0.), (81, 372., 2.)]),
    ];
    for (text, expected) in cases {
        let actual = origins(text);
        let matches = actual.len() == expected.len()
            && actual
                .iter()
                .zip(expected)
                .all(|(a, e)| a.0 == e.0 && (a.1 - e.1).abs() < 0.01 && (a.2 - e.2).abs() < 0.01);
        assert!(
            matches,
            "{text}: got {actual:?}, HarfBuzz places {expected:?}"
        );
    }
}
