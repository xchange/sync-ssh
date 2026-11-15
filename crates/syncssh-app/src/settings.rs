//! Local appearance preferences; no credentials or session contents are stored here.
use crate::backup_settings::BackupPane;
use anyhow::{ensure, Context as _, Result};
use gpui::{prelude::*, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputState},
    select::{SearchableVec, Select, SelectState},
    Disableable, IconName, IndexPath, Selectable, Sizable,
};
use gpui_terminal::ColorPalette;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

const MAX_SETTINGS_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    #[default]
    #[serde(alias = "aurora")]
    Dark,
    Light,
    Midnight,
}

#[derive(Clone, Copy, Debug)]
pub struct UiPalette {
    pub bg: u32,
    pub panel: u32,
    pub line: u32,
    pub muted: u32,
    pub accent: u32,
    pub text: u32,
    pub input: u32,
    pub selection: u32,
    pub error: u32,
    pub warning: u32,
}

impl UiPalette {
    /// Map the existing SyncSSH semantic colors for panels shared by all themes.
    pub fn map_color(self, color: u32) -> u32 {
        if self.bg == 0x0a101c {
            return color;
        }
        match color {
            0x030811 | 0x0a101c => self.bg,
            0x0e1725 | 0x101927 | 0x111b2a | 0x111c2c | 0x111d2d => self.panel,
            0x142033 | 0x152334 | 0x152236 | 0x1b2b3f => self.input,
            0x1b2d44 | 0x1b2e3e | 0x21364a | 0x22324a | 0x24344e => self.selection,
            0x223047 | 0x30445f => self.line,
            0x8291aa | 0x9cacc4 | 0xb6c5db => self.muted,
            0x6ce5c3 => self.accent,
            0xc1d1e9 | 0xd5dede | 0xd5deee | 0xd6e3f6 | 0xdbe8fb | 0xe1eaf8 | 0xe1edfe
            | 0xe6effe | 0xecf4ff | 0xf0f6ff | 0xf1f7ff => self.text,
            0xffb2bd | 0xf38c92 => self.error,
            0xf1bd72 | 0xffd59c => self.warning,
            0x3b202b => {
                if self.bg == 0xf5f7fb {
                    0xffe5eb
                } else {
                    0x301b23
                }
            }
            0x442f20 => {
                if self.bg == 0xf5f7fb {
                    0xffedcf
                } else {
                    0x33281c
                }
            }
            _ => color,
        }
    }
}

impl ThemeChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "SyncSSH 深色",
            Self::Light => "明亮",
            Self::Midnight => "午夜黑",
        }
    }

    pub fn palette(self) -> UiPalette {
        match self {
            Self::Dark => UiPalette {
                bg: 0x0a101c,
                panel: 0x101927,
                line: 0x223047,
                muted: 0x8291aa,
                accent: 0x6ce5c3,
                text: 0xd5deee,
                input: 0x152334,
                selection: 0x24364e,
                error: 0xffb2bd,
                warning: 0xffd59c,
            },
            Self::Light => UiPalette {
                bg: 0xf5f7fb,
                panel: 0xeaf0f7,
                line: 0xc7d2e2,
                muted: 0x53657d,
                accent: 0x08775f,
                text: 0x233047,
                input: 0xffffff,
                selection: 0xd2e5f5,
                error: 0xb52442,
                warning: 0x8a4a0b,
            },
            Self::Midnight => UiPalette {
                bg: 0x000000,
                panel: 0x101012,
                line: 0x303037,
                muted: 0x9999a6,
                accent: 0xb7a3ff,
                text: 0xe8e8ed,
                input: 0x19191e,
                selection: 0x302a45,
                error: 0xffa5b5,
                warning: 0xffd39b,
            },
        }
    }

    pub fn terminal_colors(self) -> ColorPalette {
        let p = self.palette();
        let rgb = |n: u32| ((n >> 16) as u8, (n >> 8) as u8, n as u8);
        let (r, g, b) = rgb(p.bg);
        let builder = ColorPalette::builder().background(r, g, b);
        let (r, g, b) = rgb(p.text);
        let builder = builder.foreground(r, g, b);
        let (r, g, b) = rgb(p.accent);
        let builder = builder.cursor(r, g, b);
        if self == Self::Light {
            builder
                .black(0x23, 0x30, 0x47)
                .red(0xb4, 0x29, 0x43)
                .green(0x19, 0x70, 0x43)
                .yellow(0x87, 0x5b, 0x05)
                .blue(0x24, 0x57, 0xb4)
                .magenta(0x7d, 0x3b, 0xae)
                .cyan(0x07, 0x71, 0x85)
                .white(0x65, 0x70, 0x83)
                .bright_black(0x53, 0x65, 0x7d)
                .bright_red(0xc2, 0x27, 0x3e)
                .bright_green(0x0d, 0x7c, 0x49)
                .bright_yellow(0x8b, 0x5a, 0x00)
                .bright_blue(0x1a, 0x58, 0xc7)
                .bright_magenta(0x87, 0x3b, 0xb9)
                .bright_cyan(0x00, 0x72, 0x87)
                .bright_white(0x23, 0x30, 0x47)
                .build()
        } else {
            builder
                .black(0x12, 0x1a, 0x29)
                .red(0xf3, 0x7c, 0x91)
                .green(0x72, 0xe4, 0xb4)
                .yellow(0xf1, 0xd3, 0x8e)
                .blue(0x7d, 0xb5, 0xfb)
                .magenta(0xbd, 0x9b, 0xfa)
                .cyan(0x6b, 0xd8, 0xe7)
                .white(0xd5, 0xde, 0xee)
                .build()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorChoice {
    Block,
    #[default]
    Underline,
    Beam,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredEffects")]
pub struct Effects {
    #[serde(rename = "input_effects")]
    pub input: bool,
    #[serde(rename = "mouse_effects")]
    pub mouse: bool,
}

impl Default for Effects {
    fn default() -> Self {
        Self {
            input: true,
            mouse: true,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredEffects {
    effects: Option<bool>,
    input_effects: Option<bool>,
    mouse_effects: Option<bool>,
}

impl From<StoredEffects> for Effects {
    fn from(stored: StoredEffects) -> Self {
        let legacy = stored.effects.unwrap_or(true);
        Self {
            input: stored.input_effects.unwrap_or(legacy),
            mouse: stored.mouse_effects.unwrap_or(legacy),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub theme: ThemeChoice,
    pub font_family: String,
    pub fallback_font_family: String,
    pub font_size: f32,
    pub line_height: f32,
    pub cursor: CursorChoice,
    pub cursor_blink: bool,
    pub scrollback: usize,
    #[serde(flatten)]
    pub effects: Effects,
    pub sidebar_collapsed: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            font_family: crate::fonts::default_families().0.into(),
            fallback_font_family: crate::fonts::default_families().1.into(),
            font_size: 14.,
            line_height: 1.25,
            cursor: CursorChoice::Underline,
            cursor_blink: true,
            scrollback: 10_000,
            effects: Effects::default(),
            sidebar_collapsed: false,
        }
    }
}

impl Preferences {
    pub fn validate(&self) -> Result<()> {
        for (label, family) in [
            ("终端字体", &self.font_family),
            ("备份字体", &self.fallback_font_family),
        ] {
            ensure!(
                !family.trim().is_empty()
                    && family.len() <= 128
                    && !family.chars().any(char::is_control),
                "{label}名称不能为空，且不能超过 128 字节或包含控制字符"
            );
        }
        ensure!(
            self.font_size.is_finite() && (8.0..=40.0).contains(&self.font_size),
            "字号范围为 8–40"
        );
        ensure!(
            self.line_height.is_finite() && (1.0..=2.0).contains(&self.line_height),
            "行高范围为 1.0–2.0"
        );
        ensure!(
            (1_000..=100_000).contains(&self.scrollback),
            "滚动历史范围为 1,000–100,000 行"
        );
        Ok(())
    }

    /// A missing file means first launch; malformed preferences are reported to the caller.
    pub fn load(directory: &Path) -> Result<Self> {
        let mut file = match fs::File::open(directory.join("preferences.json")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(error).context("无法读取个性化设置"),
        };
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_SETTINGS_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_SETTINGS_BYTES,
            "个性化设置文件过大"
        );
        let preferences: Self = serde_json::from_slice(&bytes).context("个性化设置文件格式无效")?;
        preferences.validate()?;
        Ok(preferences)
    }

    pub fn save(&self, directory: &Path) -> Result<()> {
        self.validate()?;
        fs::create_dir_all(directory).context("无法创建设置目录")?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(directory).context("无法创建设置临时文件")?;
        serde_json::to_writer_pretty(&mut temporary, self)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(directory.join("preferences.json"))
            .context("无法保存个性化设置")?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub enum SettingsAction {
    ChangePassword,
    ToggleVault,
    Zmodem,
    Sftp,
    Tunnels,
    AutoReconnect,
}

pub struct SettingsContext {
    pub unlocked: bool,
    pub has_terminal: bool,
    pub has_sftp: bool,
    pub has_tunnels: bool,
    pub auto_reconnect: Option<bool>,
    pub status: String,
}

pub enum SettingsEvent {
    Apply(Preferences),
    Action(SettingsAction),
    Close,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SettingsSection {
    #[default]
    Appearance,
    Terminal,
    CursorEffects,
    Security,
    CloudBackup,
    Tools,
    About,
}

impl SettingsSection {
    const ALL: [Self; 7] = [
        Self::Appearance,
        Self::Terminal,
        Self::CursorEffects,
        Self::Security,
        Self::CloudBackup,
        Self::Tools,
        Self::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Appearance => "外观",
            Self::Terminal => "终端",
            Self::CursorEffects => "光标与特效",
            Self::Security => "凭据",
            Self::CloudBackup => "备份",
            Self::Tools => "终端工具",
            Self::About => "关于",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Appearance => "选择工作区和终端的配色。",
            Self::Terminal => "调整字体、显示密度和滚动历史。",
            Self::CursorEffects => "设置光标样式，分别控制输入与鼠标的视觉效果。",
            Self::Security => "管理本机加密凭据库。",
            Self::CloudBackup => "选择本地凭据库文件，或配置云服务进行加密备份与恢复。",
            Self::Tools => "对当前终端或 SSH 会话执行操作。",
            Self::About => "版本信息与当前工作区状态。",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Appearance => IconName::Palette,
            Self::Terminal => IconName::SquareTerminal,
            Self::CursorEffects => IconName::Asterisk,
            Self::Security => IconName::CircleUser,
            Self::CloudBackup => IconName::Globe,
            Self::Tools => IconName::Settings2,
            Self::About => IconName::Info,
        }
    }
}

pub struct SettingsPane {
    draft: Preferences,
    font: Entity<SelectState<SearchableVec<String>>>,
    fallback_font: Entity<SelectState<SearchableVec<String>>>,
    font_choices: Vec<String>,
    fields: Vec<(&'static str, &'static str, Entity<InputState>)>,
    error: Option<String>,
    context: SettingsContext,
    section: SettingsSection,
    focus: FocusHandle,
    focus_end: FocusHandle,
    backup: Entity<BackupPane>,
    busy: bool,
}

impl EventEmitter<SettingsEvent> for SettingsPane {}
impl SettingsPane {
    pub fn new(
        preferences: Preferences,
        context: SettingsContext,
        backup: Entity<BackupPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let font_choices = crate::fonts::installed_families(cx.text_system());
        let mut make_select = |current: &str, cx: &mut Context<Self>| {
            let selected = font_choices
                .iter()
                .position(|family| family == current)
                .map(|row| IndexPath::default().row(row));
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(font_choices.clone()),
                    selected,
                    window,
                    cx,
                )
                .searchable(true)
            })
        };
        let font = make_select(&preferences.font_family, cx);
        let fallback_font = make_select(&preferences.fallback_font_family, cx);
        let fields = [
            ("size", "字号 · 8–40", preferences.font_size.to_string()),
            (
                "line",
                "行高倍数 · 1.0–2.0",
                preferences.line_height.to_string(),
            ),
            (
                "history",
                "滚动历史行数 · 1,000–100,000",
                preferences.scrollback.to_string(),
            ),
        ]
        .into_iter()
        .map(|(key, label, value)| {
            let input = cx.new(|cx| {
                let mut input = InputState::new(window, cx);
                input.set_value(value, window, cx);
                input
            });
            (key, label, input)
        })
        .collect();
        Self {
            draft: preferences,
            font,
            fallback_font,
            font_choices,
            fields,
            error: None,
            context,
            section: SettingsSection::default(),
            focus: cx.focus_handle(),
            focus_end: cx.focus_handle(),
            backup,
            busy: false,
        }
    }

    pub fn show_backup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.section = SettingsSection::CloudBackup;
        self.focus.focus(window);
        cx.notify();
    }

    pub fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        cx.notify();
    }

    pub fn set_unlocked(&mut self, cx: &mut Context<Self>) {
        self.context.unlocked = true;
        cx.notify();
    }

    pub fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.error = Some(message);
        cx.notify();
    }

    pub fn focus_first(&self, window: &mut Window, _: &mut Context<Self>) {
        self.focus.focus(window);
    }

    pub fn cycle_focus(&self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if backwards {
            window.focus_prev();
        } else {
            window.focus_next();
        }
        if !self.focus.contains_focused(window, cx) {
            if backwards {
                self.focus_end.focus(window);
                window.focus_prev();
            } else {
                self.focus.focus(window);
                window.focus_next();
            }
        }
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        let result = (|| -> Result<Preferences> {
            let value = |key| {
                self.fields
                    .iter()
                    .find(|f| f.0 == key)
                    .unwrap()
                    .2
                    .read(cx)
                    .value()
                    .trim()
                    .to_string()
            };
            let mut preferences = self.draft.clone();
            preferences.font_family = self
                .font
                .read(cx)
                .selected_value()
                .cloned()
                .unwrap_or_else(|| self.draft.font_family.clone());
            preferences.fallback_font_family = self
                .fallback_font
                .read(cx)
                .selected_value()
                .cloned()
                .unwrap_or_else(|| self.draft.fallback_font_family.clone());
            preferences.font_size = value("size").parse().context("请输入有效字号")?;
            preferences.line_height = value("line").parse().context("请输入有效行高倍数")?;
            preferences.scrollback = value("history").parse().context("请输入整数历史行数")?;
            preferences.validate()?;
            Ok(preferences)
        })();
        match result {
            Ok(preferences) => cx.emit(SettingsEvent::Apply(preferences)),
            Err(error) => {
                self.error = Some(error.to_string());
                self.section = SettingsSection::Terminal;
            }
        }
        cx.notify();
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let defaults = Preferences::default();
        for (select, family) in [
            (&self.font, &defaults.font_family),
            (&self.fallback_font, &defaults.fallback_font_family),
        ] {
            select.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(self.font_choices.clone()), window, cx);
                select.set_selected_value(family, window, cx);
            });
        }
        for (key, _, field) in &self.fields {
            let value = match *key {
                "size" => defaults.font_size.to_string(),
                "line" => defaults.line_height.to_string(),
                _ => defaults.scrollback.to_string(),
            };
            field.update(cx, |field, cx| field.set_value(value, window, cx));
        }
        self.draft = defaults;
        self.error = None;
        cx.notify();
    }
}

impl Focusable for SettingsPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SettingsPane {
    fn field(&self, key: &str) -> AnyElement {
        let (_, label, state) = self.fields.iter().find(|field| field.0 == key).unwrap();
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .gap_2()
            .child(div().text_sm().child(*label))
            .child(Input::new(state).small())
            .into_any_element()
    }

    fn appearance_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::appearance::palette(cx);
        let mut themes = div().grid().grid_cols(3).gap_3();
        for (index, theme) in [ThemeChoice::Dark, ThemeChoice::Light, ThemeChoice::Midnight]
            .into_iter()
            .enumerate()
        {
            let colors = theme.palette();
            themes = themes.child(
                Button::new(("pref-theme", index))
                    .ghost()
                    .w_full()
                    .h_auto()
                    .p_3()
                    .border_1()
                    .border_color(rgb(if self.draft.theme == theme {
                        palette.accent
                    } else {
                        palette.line
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .w_full()
                            .min_w_0()
                            .child(
                                div()
                                    .h(px(76.))
                                    .w_full()
                                    .rounded_md()
                                    .bg(rgb(colors.bg))
                                    .p_3()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(colors.accent))
                                            .child("$ ssh server"),
                                    )
                                    .child(
                                        div()
                                            .mt_2()
                                            .h(px(3.))
                                            .w_3_4()
                                            .rounded_full()
                                            .bg(rgb(colors.muted)),
                                    )
                                    .child(
                                        div()
                                            .mt_2()
                                            .h(px(3.))
                                            .w_1_2()
                                            .rounded_full()
                                            .bg(rgb(colors.accent)),
                                    ),
                            )
                            .child(div().text_sm().child(format!(
                                "{}{}",
                                if self.draft.theme == theme {
                                    "● "
                                } else {
                                    ""
                                },
                                theme.label()
                            ))),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.draft.theme = theme;
                        cx.notify();
                    })),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(div().text_sm().child("界面与终端主题"))
            .child(themes)
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(palette.muted))
                    .child("保存后应用于整个工作区，包括已打开的终端与分屏。"),
            )
            .into_any_element()
    }

    fn terminal_view(&self, cx: &App) -> AnyElement {
        let palette = crate::appearance::palette(cx);
        div().flex().flex_col().gap_5()
            .child(div().flex().flex_col().gap_2()
                .child("终端字体（推荐等宽字体）")
                .child(Select::new(&self.font).w_full().search_placeholder("搜索系统字体…")
                    .placeholder(self.draft.font_family.clone()).disabled(self.busy)))
            .child(div().flex().flex_col().gap_2()
                .child("备份字体（终端字体缺少字符时使用）")
                .child(Select::new(&self.fallback_font).w_full().search_placeholder("搜索系统字体…")
                    .placeholder(self.draft.fallback_font_family.clone()).disabled(self.busy)))
            .child(div().grid().grid_cols(2).gap_4().child(self.field("size")).child(self.field("line")))
            .child(self.field("history"))
            .child(div().text_xs().text_color(rgb(palette.muted)).child("下拉列表支持搜索系统已安装字体及应用内置字体。缺少字符时依次使用备份字体及系统字体；Windows / macOS 自动补充中日韩字符与图标，Linux 由系统字体配置决定。"))
            .child(div().text_xs().text_color(rgb(palette.muted)).child("减少滚动历史会丢弃超出部分；字号和行高变化会同步调整终端尺寸。"))
            .into_any_element()
    }

    fn cursor_effects_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::appearance::palette(cx);
        let mut cursor = div().flex().flex_wrap().gap_2();
        for (index, (choice, label)) in [
            (CursorChoice::Block, "方块"),
            (CursorChoice::Underline, "下划线"),
            (CursorChoice::Beam, "竖线"),
        ]
        .into_iter()
        .enumerate()
        {
            cursor = cursor.child(
                Button::new(("pref-cursor", index))
                    .small()
                    .label(label)
                    .selected(self.draft.cursor == choice)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.draft.cursor = choice;
                        cx.notify();
                    })),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().text_sm().child("光标形状"))
                    .child(cursor)
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(palette.muted))
                            .child("终端程序可以临时覆盖光标样式。"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .py_3()
                    .border_t_1()
                    .border_color(rgb(palette.line))
                    .child(
                        Checkbox::new("pref-blink")
                            .label("光标闪烁")
                            .checked(self.draft.cursor_blink)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.draft.cursor_blink = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .pl_6()
                            .text_xs()
                            .text_color(rgb(palette.muted))
                            .child("默认开启，在终端获得焦点时闪烁。"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .py_3()
                    .border_t_1()
                    .border_color(rgb(palette.line))
                    .child(
                        Checkbox::new("pref-input-effects")
                            .label("输入特效")
                            .checked(self.draft.effects.input)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.draft.effects.input = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .pl_6()
                            .text_xs()
                            .text_color(rgb(palette.muted))
                            .child("键入时显示光标光晕和粒子效果。"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .py_3()
                    .border_t_1()
                    .border_color(rgb(palette.line))
                    .child(
                        Checkbox::new("pref-mouse-effects")
                            .label("鼠标特效")
                            .checked(self.draft.effects.mouse)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.draft.effects.mouse = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .pl_6()
                            .text_xs()
                            .text_color(rgb(palette.muted))
                            .child("移动鼠标时显示流光轨迹，可独立于输入特效开启。"),
                    ),
            )
            .into_any_element()
    }

    fn security_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::appearance::palette(cx);
        let mut actions = div().flex().flex_wrap().gap_3();
        for (index, (action, label, enabled)) in [
            (
                SettingsAction::ChangePassword,
                "更换主密码",
                self.context.unlocked,
            ),
            (
                SettingsAction::ToggleVault,
                if self.context.unlocked {
                    "锁定凭据库"
                } else {
                    "解锁凭据库"
                },
                true,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            actions = actions.child(
                Button::new(("settings-account", index))
                    .small()
                    .label(label)
                    .disabled(!enabled)
                    .on_click(
                        cx.listener(move |_, _, _, cx| cx.emit(SettingsEvent::Action(action))),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(div().text_sm().child(if self.context.unlocked {
                "凭据库已解锁"
            } else {
                "凭据库已锁定"
            }))
            .child(actions)
            .child(
                div().text_xs().text_color(rgb(palette.muted)).child(
                    "连接凭据在本机加密后保存与备份。锁定凭据库会关闭 SSH 会话，保留本地终端。",
                ),
            )
            .into_any_element()
    }

    fn tools_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut tools = div().grid().grid_cols(2).gap_3();
        for (index, (action, label, enabled)) in [
            (
                SettingsAction::Zmodem,
                "ZMODEM 传输",
                self.context.has_terminal,
            ),
            (SettingsAction::Sftp, "SFTP 文件", self.context.has_sftp),
            (
                SettingsAction::Tunnels,
                "SSH 隧道",
                self.context.has_tunnels,
            ),
            (
                SettingsAction::AutoReconnect,
                if self.context.auto_reconnect == Some(true) {
                    "关闭自动重连"
                } else {
                    "开启自动重连"
                },
                self.context.auto_reconnect.is_some(),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            tools = tools.child(
                Button::new(("settings-terminal", index))
                    .w_full()
                    .justify_start()
                    .label(label)
                    .disabled(!enabled)
                    .on_click(
                        cx.listener(move |_, _, _, cx| cx.emit(SettingsEvent::Action(action))),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(tools)
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(crate::appearance::palette(cx).muted))
                    .child("SFTP、隧道与自动重连需要 SSH 会话。每个工作区最多支持 4 个窗格。"),
            )
            .into_any_element()
    }

    fn about_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::appearance::palette(cx);
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(div().text_2xl().child("SyncSSH"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.muted))
                    .child(concat!("版本 ", env!("CARGO_PKG_VERSION"))),
            )
            .child(
                div()
                    .text_sm()
                    .child("为终端保留更多空间的原生 SSH 客户端。"),
            )
            .child(
                div()
                    .mt_3()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(palette.line))
                    .child(div().text_sm().child("工作区状态"))
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(palette.muted))
                            .child(self.context.status.clone()),
                    ),
            )
            .child(
                Button::new("settings-copy-status")
                    .ghost()
                    .small()
                    .label("复制状态")
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            this.context.status.clone(),
                        ))
                    })),
            )
            .into_any_element()
    }
}

impl Render for SettingsPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::appearance::palette(cx);
        let mut menu = div().flex().flex_col().gap_1();
        for (index, section) in SettingsSection::ALL.into_iter().enumerate() {
            menu = menu.child(
                Button::new(("settings-section", index))
                    .ghost()
                    .w_full()
                    .h(px(40.))
                    .justify_start()
                    .icon(section.icon())
                    .label(section.label())
                    .selected(self.section == section)
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.section = section;
                        this.focus.focus(window);
                        cx.notify();
                    })),
            );
        }
        let content = match self.section {
            SettingsSection::Appearance => self.appearance_view(cx),
            SettingsSection::Terminal => self.terminal_view(cx),
            SettingsSection::CursorEffects => self.cursor_effects_view(cx),
            SettingsSection::Security => self.security_view(cx),
            SettingsSection::CloudBackup => self.backup.clone().into_any_element(),
            SettingsSection::Tools => self.tools_view(cx),
            SettingsSection::About => self.about_view(cx),
        };
        div()
            .id("settings-pane")
            .tab_group()
            .track_focus(&self.focus)
            .tab_index(0)
            .tab_stop(false)
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(palette.panel))
            .text_color(rgb(palette.text))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .h(px(62.))
                    .flex_shrink_0()
                    .px_6()
                    .border_b_1()
                    .border_color(rgb(palette.line))
                    .child(div().text_lg().child("设置"))
                    .child(
                        Button::new("settings-close")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip("关闭设置 · Escape")
                            .disabled(self.busy)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Close))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("settings-menu")
                            .w(px(184.))
                            .flex_shrink_0()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_3()
                            .py_4()
                            .bg(rgb(palette.bg))
                            .border_r_1()
                            .border_color(rgb(palette.line))
                            .child(menu),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(
                                div()
                                    .px_6()
                                    .pt_6()
                                    .pb_4()
                                    .flex_shrink_0()
                                    .child(div().text_lg().child(self.section.label()))
                                    .child(
                                        div()
                                            .mt_2()
                                            .text_sm()
                                            .text_color(rgb(palette.muted))
                                            .child(self.section.description()),
                                    ),
                            )
                            .child(if self.section == SettingsSection::CloudBackup {
                                div().flex_1().min_h_0().child(content).into_any_element()
                            } else {
                                div()
                                    .id(("settings-content", self.section as usize))
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .px_6()
                                    .pb_6()
                                    .child(content)
                                    .into_any_element()
                            }),
                    ),
            )
            .when(self.section != SettingsSection::CloudBackup, |root| {
                root.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_shrink_0()
                        .px_6()
                        .py_3()
                        .gap_2()
                        .border_t_1()
                        .border_color(rgb(palette.line))
                        .when_some(self.error.clone(), |footer, error| {
                            footer
                                .child(div().text_sm().text_color(rgb(palette.error)).child(error))
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_xs()
                                        .text_color(rgb(palette.muted))
                                        .child("设置保存在本机，保存后应用于全部终端。"),
                                )
                                .child(
                                    Button::new("settings-reset")
                                        .ghost()
                                        .small()
                                        .label("恢复默认")
                                        .disabled(self.busy)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reset(window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("settings-save")
                                        .primary()
                                        .small()
                                        .label("保存并应用")
                                        .disabled(self.busy)
                                        .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                                ),
                        ),
                )
            })
            .child(
                div()
                    .track_focus(&self.focus_end)
                    .tab_index(isize::MAX)
                    .tab_stop(false)
                    .h(px(0.))
                    .flex_shrink_0(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{CursorChoice, Effects, Preferences, ThemeChoice};

    #[test]
    fn legacy_effects_migrate_and_explicit_choices_are_preserved() {
        for enabled in [false, true] {
            let old =
                serde_json::json!({ "effects": enabled, "cursor_blink": false, "font_size": 18.0 });
            let preferences: Preferences = serde_json::from_value(old).unwrap();
            assert_eq!(
                preferences.effects,
                Effects {
                    input: enabled,
                    mouse: enabled
                }
            );
            assert!(!preferences.cursor_blink);
            assert_eq!(preferences.font_size, 18.);
        }
        let mixed: Preferences = serde_json::from_value(serde_json::json!({
            "effects": false, "input_effects": true
        }))
        .unwrap();
        assert_eq!(
            mixed.effects,
            Effects {
                input: true,
                mouse: false
            }
        );
        let defaults: Preferences = serde_json::from_str("{}").unwrap();
        assert!(defaults.cursor_blink);
        assert_eq!(defaults.effects, Effects::default());
    }

    #[test]
    fn input_and_mouse_effects_persist_independently() {
        let directory = tempfile::tempdir().unwrap();
        for input in [false, true] {
            for mouse in [false, true] {
                let preferences = Preferences {
                    effects: Effects { input, mouse },
                    ..Preferences::default()
                };
                preferences.save(directory.path()).unwrap();
                assert_eq!(Preferences::load(directory.path()).unwrap(), preferences);
                let saved = serde_json::to_value(&preferences).unwrap();
                assert_eq!(saved["input_effects"], input);
                assert_eq!(saved["mouse_effects"], mouse);
                assert!(saved.get("effects").is_none());
            }
        }
    }

    #[test]
    fn preferences_roundtrip_defaults_and_validation() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            Preferences::load(directory.path()).unwrap(),
            Preferences::default()
        );
        let preferences = Preferences {
            theme: ThemeChoice::Light,
            font_family: "Custom Terminal Mono".into(),
            fallback_font_family: "Custom CJK Mono".into(),
            font_size: 18.,
            line_height: 1.5,
            cursor: CursorChoice::Beam,
            cursor_blink: true,
            scrollback: 20_000,
            effects: Effects {
                input: false,
                mouse: true,
            },
            sidebar_collapsed: true,
            ..Preferences::default()
        };
        preferences.save(directory.path()).unwrap();
        assert_eq!(Preferences::load(directory.path()).unwrap(), preferences);
        let invalid = Preferences {
            font_size: f32::NAN,
            ..preferences.clone()
        };
        assert!(invalid.save(directory.path()).is_err());
        assert_eq!(Preferences::load(directory.path()).unwrap(), preferences);
        std::fs::write(
            directory.path().join("preferences.json"),
            r#"{"theme":"midnight"}"#,
        )
        .unwrap();
        assert_eq!(Preferences::load(directory.path()).unwrap().font_size, 14.);
        std::fs::write(
            directory.path().join("preferences.json"),
            r#"{"scrollback":999999999}"#,
        )
        .unwrap();
        assert!(Preferences::load(directory.path()).is_err());
    }

    #[test]
    fn old_font_preferences_keep_the_primary_and_get_a_default_backup() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("preferences.json"),
            r#"{"font_family":"My Existing Mono","font_size":17,"cursor_blink":false}"#,
        )
        .unwrap();
        let preferences = Preferences::load(directory.path()).unwrap();
        assert_eq!(preferences.font_family, "My Existing Mono");
        assert_eq!(
            preferences.fallback_font_family,
            crate::fonts::default_families().1
        );
        assert_eq!(preferences.font_size, 17.);
        assert!(!preferences.cursor_blink);
        preferences.save(directory.path()).unwrap();
        assert_eq!(Preferences::load(directory.path()).unwrap(), preferences);
    }

    #[test]
    fn invalid_backup_font_names_do_not_overwrite_saved_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let original = Preferences::default();
        original.save(directory.path()).unwrap();
        for name in ["".into(), "   ".into(), "Bad\nFont".into(), "x".repeat(129)] {
            let invalid = Preferences {
                fallback_font_family: name,
                ..original.clone()
            };
            assert!(invalid.save(directory.path()).is_err());
            assert_eq!(Preferences::load(directory.path()).unwrap(), original);
        }
    }
}
