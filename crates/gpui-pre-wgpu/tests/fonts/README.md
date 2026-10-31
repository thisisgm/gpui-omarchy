# Mark placement fixture

Noto Sans Sinhala Regular 3.000, copyright 2022 The Noto Project Authors, SIL Open Font
License 1.1. The file comes unchanged from Arch Linux noto-fonts 1:2026.10.01-1, installed
under /usr/share/fonts/noto. LICENSE-NotoSansSinhala.txt contains the license.
Upstream: https://github.com/notofonts/sinhala.

Its vowel signs and al-lakuna are zero-advance glyphs that GPOS moves onto their consonant,
so `cosmic_text_system/mark_offset_tests.rs` can check that shaped glyph positions keep
those offsets. The expected origins are HarfBuzz 14.6.0's for this file.

SHA256: 9e32612d47004552f3125e78648a9e2e7899a216ccd3cefbb93a9b5f4c809feb
