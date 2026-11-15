//! Regressions for missing directory icons in the bundled fallback font.
//! These tests inspect the shipped font bytes, not fonts installed on the test machine.
#[allow(dead_code)]
#[path = "../src/fonts.rs"]
mod fonts;

use ttf_parser::{name_id, Face};

const FACES: [(&str, &[u8], bool, bool); 4] = [
    (
        "Regular",
        include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-Regular.ttf"),
        false,
        false,
    ),
    (
        "Bold",
        include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-Bold.ttf"),
        true,
        false,
    ),
    (
        "Italic",
        include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-Italic.ttf"),
        false,
        true,
    ),
    (
        "BoldItalic",
        include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-BoldItalic.ttf"),
        true,
        true,
    ),
];

// Source for directory icon code points:
// https://github.com/eza-community/eza/blob/main/src/output/icons.rs
// The F115/F108 glyphs reproduce the generic-folder/Desktop boxes in the report;
// F0C82 etc also exercise supplementary-plane mappings, not only a BMP cmap.
const DIRECTORY_AND_PROMPT_SYMBOLS: [char; 12] = [
    '\u{f115}',  // eza: generic/open directory
    '\u{f108}',  // Desktop
    '\u{f0c82}', // Documents
    '\u{f024d}', // Downloads
    '\u{f0fce}', // Movies
    '\u{f1359}', // Music
    '\u{f024f}', // Pictures
    '\u{f00c6}', // Material Design supplementary-plane symbol
    '\u{f024b}', // Material Design supplementary-plane symbol
    '\u{e5ff}',  // Nerd Fonts folder
    '\u{e0b0}',  // Powerline separator
    '\u{f02a2}', // Git
];

#[test]
fn every_shipped_style_matches_the_bundled_fallback_family() {
    for (style, bytes, bold, italic) in FACES {
        let face = Face::parse(bytes, 0).unwrap_or_else(|error| panic!("{style}: {error:?}"));
        let families: Vec<_> = face
            .names()
            .into_iter()
            .filter(|name| matches!(name.name_id, name_id::FAMILY | name_id::TYPOGRAPHIC_FAMILY))
            .filter_map(|name| name.to_string())
            .collect();
        assert!(
            families
                .iter()
                .any(|name| name == fonts::BUNDLED_FONT_FAMILY),
            "{style}: bundled family {:?} is absent from {families:?}",
            fonts::BUNDLED_FONT_FAMILY,
        );
        assert_eq!(face.is_bold(), bold, "{style}: wrong weight");
        assert_eq!(face.is_italic(), italic, "{style}: wrong slant");
        assert!(face.is_monospaced(), "{style}: must use the Mono variant");
    }
}

#[test]
fn directory_and_prompt_icons_have_real_outlines_in_all_styles() {
    for (style, bytes, _, _) in FACES {
        let face = Face::parse(bytes, 0).unwrap();
        for character in DIRECTORY_AND_PROMPT_SYMBOLS {
            let glyph = face
                .glyph_index(character)
                .unwrap_or_else(|| panic!("{style}: missing icon U+{:04X}", character as u32));
            assert_ne!(glyph.0, 0, "{style}: icon maps to .notdef");
            let bounds = face
                .glyph_bounding_box(glyph)
                .unwrap_or_else(|| panic!("{style}: empty outline for U+{:04X}", character as u32));
            assert!(bounds.x_min < bounds.x_max && bounds.y_min < bounds.y_max);
        }
    }
}

#[test]
fn icons_and_ascii_keep_the_same_terminal_cell_advance() {
    for (style, bytes, _, _) in FACES {
        let face = Face::parse(bytes, 0).unwrap();
        let cell = face
            .glyph_hor_advance(face.glyph_index('M').unwrap())
            .unwrap();
        assert!(cell > 0);
        for character in ['i', 'm', '0', '@', ' ']
            .into_iter()
            .chain(DIRECTORY_AND_PROMPT_SYMBOLS)
        {
            let glyph = face.glyph_index(character).unwrap();
            assert_eq!(
                face.glyph_hor_advance(glyph),
                Some(cell),
                "{style}: U+{:04X} would disturb fixed-cell alignment",
                character as u32,
            );
        }
    }
}
