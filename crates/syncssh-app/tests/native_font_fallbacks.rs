//! Native coverage regression; runs on the main thread without opening a window.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[allow(dead_code, unused_imports)]
#[path = "../src/fonts.rs"]
mod fonts;

#[cfg(target_os = "macos")]
fn main() {
    use gpui::{font, px, Application, FontFallbacks};
    use gpui_terminal::{ColorPalette, TerminalRenderer};

    let app = Application::headless();
    let text_system = app.text_system();
    fonts::register(&text_system).unwrap();
    let mut renderer =
        TerminalRenderer::new("Menlo".into(), px(14.), 1.25, ColorPalette::default());
    renderer.font_fallbacks = Some(FontFallbacks::from_fonts(fonts::fallback_families(
        "Menlo",
        "Monaco",
        &text_system,
    )));
    let primary = text_system.resolve_font(&font("Menlo"));
    assert!(text_system.advance(primary, px(14.), '中').is_err());
    assert!(text_system.advance(primary, px(14.), '\u{f115}').is_err());

    for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
        let ascii = renderer.font_for_text(&"ASCII".into(), bold, italic, &text_system);
        assert_eq!(ascii.family.as_ref(), "Menlo");
        for (text, family) in [
            ("中文", "PingFang SC"),
            ("拼音 abc", "PingFang SC"), // Pending IME text may mix CJK and Latin.
            ("\u{f115}", fonts::BUNDLED_FONT_FAMILY),
            ("\u{f0c82}", fonts::BUNDLED_FONT_FAMILY),
        ] {
            let selected = renderer.font_for_text(&text.into(), bold, italic, &text_system);
            assert_eq!(selected.family.as_ref(), family);
            assert_eq!(selected.weight, ascii.weight);
            assert_eq!(selected.style, ascii.style);
            let id = text_system.resolve_font(&selected);
            for ch in text.chars() {
                assert!(text_system.advance(id, px(14.), ch).unwrap().width > px(0.));
            }
        }
        for text in ["日本語あいう", "한국어"] {
            let selected = renderer.font_for_text(&text.into(), bold, italic, &text_system);
            assert_ne!(selected.family.as_ref(), "Menlo");
            let id = text_system.resolve_font(&selected);
            for ch in text.chars() {
                assert!(text_system.advance(id, px(14.), ch).is_ok());
            }
        }
    }

    // A live font edit must invalidate previously cached font selections, even
    // though the renderer shares its cache with clones used during painting.
    let mut updated = renderer.clone();
    updated.font_family = "Monaco".into();
    updated.font_fallbacks = Some(FontFallbacks::from_fonts(vec!["PingFang TC".into()]));
    assert_eq!(
        updated
            .font_for_text(&"ASCII".into(), false, false, &text_system)
            .family
            .as_ref(),
        "Monaco",
    );
    assert_eq!(
        updated
            .font_for_text(&"中文".into(), false, false, &text_system)
            .family
            .as_ref(),
        "PingFang TC",
    );
    println!("Native font fallback: ASCII, CJK, IME text, Nerd icons, four styles and live edits passed.");
}

#[cfg(target_os = "windows")]
fn main() {
    use gpui::{px, Application, FontFallbacks};
    use gpui_terminal::{ColorPalette, TerminalRenderer};

    let app = Application::headless();
    let text_system = app.text_system();
    fonts::register(&text_system).unwrap();
    let mut renderer =
        TerminalRenderer::new("Consolas".into(), px(14.), 1.25, ColorPalette::default());
    renderer.font_fallbacks = Some(FontFallbacks::from_fonts(fonts::fallback_families(
        "Consolas",
        "Lucida Console",
        &text_system,
    )));
    for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
        assert_eq!(
            renderer
                .font_for_text(&"ASCII".into(), bold, italic, &text_system)
                .family
                .as_ref(),
            "Consolas",
        );
        let cjk = renderer.font_for_text(&"中文".into(), bold, italic, &text_system);
        assert_ne!(cjk.family.as_ref(), "Consolas");
        assert_ne!(cjk.family.as_ref(), "Lucida Console");
        assert_ne!(cjk.family.as_ref(), fonts::BUNDLED_FONT_FAMILY);
        for text in ["\u{f115}", "\u{f0c82}"] {
            assert_eq!(
                renderer
                    .font_for_text(&text.into(), bold, italic, &text_system)
                    .family
                    .as_ref(),
                fonts::BUNDLED_FONT_FAMILY,
            );
        }
    }
    println!("Windows native font fallback: ASCII, CJK, Nerd icons and four styles passed.");
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn main() {
    println!(
        "Linux uses the user's monospace font configuration; platform defaults are tested separately."
    );
}
