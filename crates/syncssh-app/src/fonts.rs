//! Platform terminal defaults and an embedded fallback for prompt/directory icons.
use gpui::TextSystem;
use std::borrow::Cow;

pub const BUNDLED_FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";

/// Query the OS font database rather than GPUI's list, which also contains
/// synthetic fallback names for fonts that may not actually be installed.
pub fn installed_families(text_system: &TextSystem) -> Vec<String> {
    let mut names = font_kit::source::SystemSource::new()
        .all_families()
        .unwrap_or_else(|_| text_system.all_font_names());
    names.retain(|name| !name.starts_with('.'));
    names.push(BUNDLED_FONT_FAMILY.into());
    if cfg!(target_os = "linux") {
        names.push("monospace".into());
    }
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    names
}

/// Both defaults are system-provided monospaced fonts. CJK coverage is supplied
/// separately, without changing the primary font's terminal cell metrics.
pub fn default_families() -> (&'static str, &'static str) {
    default_families_for(std::env::consts::OS)
}

fn default_families_for(platform: &str) -> (&'static str, &'static str) {
    match platform {
        "macos" => ("Menlo", "Monaco"),
        "windows" => ("Consolas", "Lucida Console"),
        _ => ("monospace", "monospace"),
    }
}

pub fn fallback_families(primary: &str, backup: &str, text_system: &TextSystem) -> Vec<String> {
    fallback_families_for(
        std::env::consts::OS,
        primary,
        backup,
        &text_system.all_font_names(),
    )
}

fn fallback_families_for(
    platform: &str,
    primary: &str,
    backup: &str,
    installed: &[String],
) -> Vec<String> {
    let system_fonts: &[&str] = match platform {
        "macos" => &[
            "PingFang SC",
            "PingFang TC",
            "Hiragino Sans",
            "Apple SD Gothic Neo",
            "Apple Color Emoji",
        ],
        "windows" => &[
            "NSimSun",
            "MingLiU",
            "MS Gothic",
            "GulimChe",
            "Microsoft YaHei UI",
            "Microsoft JhengHei UI",
            "Yu Gothic UI",
            "Malgun Gothic",
            "Segoe UI Emoji",
        ],
        _ => &[], // Linux font selection and CJK coverage are managed by the user.
    };
    let mut families = Vec::new();
    let mut add = |family: &str| {
        if !family.eq_ignore_ascii_case(primary)
            && !families
                .iter()
                .any(|name: &String| name.eq_ignore_ascii_case(family))
        {
            families.push(family.to_owned());
        }
    };
    add(backup);
    for family in system_fonts {
        if let Some(installed) = installed
            .iter()
            .find(|name| name.eq_ignore_ascii_case(family))
        {
            add(installed);
        }
    }
    if matches!(platform, "macos" | "windows") {
        add(BUNDLED_FONT_FAMILY);
    }
    families
}

/// Register all styles before GPUI resolves or caches any terminal fonts.
/// These fonts are private to this process; nothing is installed into the OS.
pub fn register(text_system: &TextSystem) -> anyhow::Result<()> {
    text_system.add_fonts(vec![
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/JetBrainsMonoNerdFontMono-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/JetBrainsMonoNerdFontMono-Bold.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/JetBrainsMonoNerdFontMono-Italic.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/JetBrainsMonoNerdFontMono-BoldItalic.ttf"
        )),
    ])
}

#[cfg(test)]
mod tests {
    use super::{default_families_for, fallback_families_for, BUNDLED_FONT_FAMILY};

    #[test]
    fn platform_defaults_use_system_monospace_fonts() {
        assert_eq!(default_families_for("macos"), ("Menlo", "Monaco"));
        assert_eq!(
            default_families_for("windows"),
            ("Consolas", "Lucida Console")
        );
        assert_eq!(default_families_for("linux"), ("monospace", "monospace"));
    }

    #[test]
    fn backup_precedes_installed_cjk_fonts_and_bundled_icons() {
        let installed =
            ["PingFang SC", "Apple SD Gothic Neo", "NSimSun", "MS Gothic"].map(str::to_owned);
        assert_eq!(
            fallback_families_for("macos", "Menlo", "Custom Mono", &installed),
            [
                "Custom Mono",
                "PingFang SC",
                "Apple SD Gothic Neo",
                BUNDLED_FONT_FAMILY
            ],
        );
        assert_eq!(
            fallback_families_for("windows", "Consolas", "Lucida Console", &installed),
            [
                "Lucida Console",
                "NSimSun",
                "MS Gothic",
                BUNDLED_FONT_FAMILY
            ],
        );
        assert_eq!(
            fallback_families_for("linux", "monospace", "monospace", &installed),
            Vec::<String>::new(),
        );
        assert_eq!(
            fallback_families_for("linux", "monospace", "User CJK Mono", &installed),
            ["User CJK Mono"],
        );
    }

    #[test]
    fn fallback_families_are_unique_and_skip_primary() {
        let installed = ["PingFang SC".to_owned()];
        assert_eq!(
            fallback_families_for("macos", "Menlo", "pingfang sc", &installed),
            ["pingfang sc", BUNDLED_FONT_FAMILY],
        );
        assert_eq!(
            fallback_families_for("macos", "PingFang SC", "pingfang sc", &installed),
            [BUNDLED_FONT_FAMILY],
        );
    }
}
