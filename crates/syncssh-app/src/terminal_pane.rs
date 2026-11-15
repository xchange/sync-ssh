//! Terminal presentation. Decorative particles never contain keyboard text or hitboxes.
use crate::settings::{CursorChoice, Effects, Preferences, UiPalette};
use gpui::{prelude::*, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    Disableable, Selectable, Sizable,
};
use gpui_terminal::{
    search::{SearchOptions, SearchSummary},
    view::{InputPulse, InputWriteError, SearchChanged, TerminalExited},
    TerminalConfig, TerminalView,
};
use std::time::{Duration, Instant};
use syncssh_session::{SessionGuard, TerminalIo, ZmodemDirection, ZmodemEvent, ZmodemHandle};

const MAX_PARTICLES: usize = 128;
const PARTICLE_LIFETIME: Duration = Duration::from_millis(650);

actions!(
    syncssh_search,
    [
        ToggleTerminalSearch,
        SearchNext,
        SearchPrevious,
        SearchClose
    ]
);

/// The terminal reader has ended; emitted even while this tab is hidden.
pub struct PaneExited;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransferPhase {
    Requested,
    Choosing,
    Transferring,
    Cancelling,
    Finished,
}

struct ZmodemTransfer {
    id: u64,
    uploading: bool,
    phase: TransferPhase,
    name: String,
    transferred: u64,
    total: Option<u64>,
    status: String,
    error: Option<String>,
}

impl ZmodemTransfer {
    fn active(&self) -> bool {
        self.phase != TransferPhase::Finished
    }

    fn accepts_picker(&self, id: u64) -> bool {
        self.id == id && self.phase == TransferPhase::Choosing
    }
}

#[derive(Clone, Copy)]
enum EffectSource {
    Input,
    Mouse,
}

impl EffectSource {
    fn enabled(self, effects: Effects) -> bool {
        match self {
            Self::Input => effects.input,
            Self::Mouse => effects.mouse,
        }
    }
}

#[derive(Clone)]
struct Particle {
    source: EffectSource,
    origin: Point<Pixels>,
    velocity: Point<f32>,
    born: Instant,
    size: f32,
    hue: f32,
}

pub struct TerminalPane {
    terminal: Entity<TerminalView>,
    _guard: SessionGuard,
    _input_subscription: Subscription,
    _error_subscription: Subscription,
    _exit_subscription: Subscription,
    _search_input_subscription: Subscription,
    _search_results_subscription: Subscription,
    search_input: Entity<InputState>,
    search_open: bool,
    search_focused: bool,
    search_case_sensitive: bool,
    search_regex: bool,
    search_summary: SearchSummary,
    exited: bool,
    _zmodem_events: Task<()>,
    zmodem: ZmodemHandle,
    transfer: Option<ZmodemTransfer>,
    show_zmodem_help: bool,
    parent_input_enabled: bool,
    input_error: bool,
    particles: Vec<Particle>,
    effects: Effects,
    palette: UiPalette,
    last_pointer: Option<(Point<Pixels>, Instant)>,
    pulse: Option<Instant>,
    seed: u32,
}

impl TerminalPane {
    pub fn new(
        io: TerminalIo,
        zmodem: ZmodemHandle,
        events: flume::Receiver<ZmodemEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let TerminalIo {
            reader,
            writer,
            resize,
            guard,
            ..
        } = io;
        let defaults = Preferences::default();
        let config = TerminalConfig {
            cols: 100,
            rows: 30,
            font_fallbacks: crate::fonts::fallback_families(
                &defaults.font_family,
                &defaults.fallback_font_family,
                cx.text_system(),
            ),
            font_family: defaults.font_family,
            font_size: px(defaults.font_size),
            line_height_multiplier: defaults.line_height,
            scrollback: defaults.scrollback,
            padding: Edges::all(px(18.)),
            colors: defaults.theme.terminal_colors(),
        };
        let terminal = cx.new(|cx| {
            TerminalView::new(writer, reader, config, cx).with_resize_callback(move |cols, rows| {
                resize(
                    cols.min(u16::MAX as usize) as u16,
                    rows.min(u16::MAX as usize) as u16,
                )
            })
        });
        terminal.read(cx).focus_handle().focus(window);
        let subscription = cx.subscribe(&terminal, |this, terminal, _: &InputPulse, cx| {
            this.input_error = false;
            if !this.effects.input {
                cx.notify();
                return;
            }
            let cursor = terminal.read(cx).cursor_bounds();
            let origin = point(
                cursor.origin.x + cursor.size.width * 0.5,
                cursor.origin.y + cursor.size.height * 0.5,
            );
            this.pulse = Some(Instant::now());
            this.burst(origin, 9, EffectSource::Input);
            cx.notify();
        });
        let error_subscription = cx.subscribe(&terminal, |this, _, _: &InputWriteError, cx| {
            this.input_error = true;
            cx.notify();
        });
        let exit_subscription = cx.subscribe(&terminal, |this, _, _: &TerminalExited, cx| {
            this.exited = true;
            this.sync_input_enabled(cx);
            cx.emit(PaneExited);
            cx.notify();
        });
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("搜索终端输出"));
        let search_input_subscription =
            cx.subscribe_in(&search_input, window, |this, _, event, _, cx| match event {
                InputEvent::Change if this.search_open => this.update_search(cx),
                InputEvent::Focus => {
                    this.search_focused = true;
                    this.sync_input_enabled(cx);
                }
                InputEvent::Blur => {
                    this.search_focused = false;
                    this.sync_input_enabled(cx);
                }
                _ => {}
            });
        let search_results_subscription =
            cx.subscribe(&terminal, |this, terminal, _: &SearchChanged, cx| {
                this.search_summary = terminal.read(cx).search_summary().clone();
                cx.notify();
            });
        let zmodem_events = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv_async().await {
                if this
                    .update(cx, |this, cx| this.on_zmodem_event(event, cx))
                    .is_err()
                {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(transfer) = &mut this.transfer {
                    if transfer.active() {
                        transfer.phase = TransferPhase::Finished;
                        transfer.status = "ZMODEM 传输已结束".into();
                        transfer.error = Some(
                            if transfer.uploading {
                                "连接已关闭。请检查对端是否留下未完成的上传文件。"
                            } else {
                                "连接已关闭。未完成的本地下载不会保存为目标文件。"
                            }
                            .into(),
                        );
                    }
                }
                this.sync_input_enabled(cx);
                cx.notify();
            });
        });
        Self {
            terminal,
            _guard: guard,
            _input_subscription: subscription,
            _error_subscription: error_subscription,
            _exit_subscription: exit_subscription,
            _search_input_subscription: search_input_subscription,
            _search_results_subscription: search_results_subscription,
            search_input,
            search_open: false,
            search_focused: false,
            search_case_sensitive: false,
            search_regex: false,
            search_summary: SearchSummary::default(),
            exited: false,
            _zmodem_events: zmodem_events,
            zmodem,
            transfer: None,
            show_zmodem_help: false,
            parent_input_enabled: true,
            input_error: false,
            particles: Vec::new(),
            effects: Effects::default(),
            palette: Preferences::default().theme.palette(),
            last_pointer: None,
            pulse: None,
            seed: 0x6d2b79f5,
        }
    }

    pub fn apply_preferences(&mut self, preferences: &Preferences, cx: &mut Context<Self>) {
        self.palette = preferences.theme.palette();
        self.set_effects(preferences.effects, cx);
        let font_fallbacks = crate::fonts::fallback_families(
            &preferences.font_family,
            &preferences.fallback_font_family,
            cx.text_system(),
        );
        self.terminal.update(cx, |terminal, cx| {
            let mut config = terminal.config().clone();
            config.font_family = preferences.font_family.clone();
            config.font_fallbacks = font_fallbacks;
            config.font_size = px(preferences.font_size);
            config.line_height_multiplier = preferences.line_height;
            config.scrollback = preferences.scrollback;
            config.colors = preferences.theme.terminal_colors();
            terminal.update_config(config, cx);
            use gpui_terminal::view::CursorShape;
            let shape = match preferences.cursor {
                CursorChoice::Block => CursorShape::Block,
                CursorChoice::Underline => CursorShape::Underline,
                CursorChoice::Beam => CursorShape::Beam,
            };
            terminal.set_cursor_style(shape, preferences.cursor_blink, cx);
        });
        cx.notify();
    }

    /// Modal dialogs disable terminal input even if native focus is stale.
    pub fn set_input_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.parent_input_enabled = enabled;
        self.sync_input_enabled(cx);
    }

    fn sync_input_enabled(&mut self, cx: &mut Context<Self>) {
        let enabled = terminal_input_allowed(
            self.parent_input_enabled && !self.exited && !self.search_focused,
            self.transfer.as_ref().is_some_and(ZmodemTransfer::active),
        );
        self.terminal
            .update(cx, |terminal, cx| terminal.set_input_enabled(enabled, cx));
    }

    pub fn is_exited(&self) -> bool {
        self.exited
    }

    /// The preferred focus target may be the search field while the user has
    /// clicked back into the terminal body. Check both live handles before the
    /// pane is replaced so reconnect cannot restore a handle from the old pane.
    pub fn contains_focused(&self, window: &Window, cx: &App) -> bool {
        self.terminal
            .read(cx)
            .focus_handle()
            .contains_focused(window, cx)
            || self
                .search_input
                .focus_handle(cx)
                .contains_focused(window, cx)
    }

    pub fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.parent_input_enabled {
            return;
        }
        if self.search_open {
            self.close_search(window, cx);
        } else {
            self.search_open = true;
            self.search_focused = true;
            self.sync_input_enabled(cx);
            self.search_input
                .update(cx, |input, cx| input.focus(window, cx));
            self.update_search(cx);
            cx.notify();
        }
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.parent_input_enabled {
            return;
        }
        self.search_open = false;
        self.search_focused = false;
        self.terminal
            .update(cx, |terminal, cx| terminal.clear_search(cx));
        self.sync_input_enabled(cx);
        self.terminal.read(cx).focus_handle().focus(window);
        cx.notify();
    }

    fn update_search(&mut self, cx: &mut Context<Self>) {
        let options = SearchOptions {
            query: self.search_input.read(cx).value().to_string(),
            regex: self.search_regex,
            case_sensitive: self.search_case_sensitive,
        };
        self.terminal
            .update(cx, |terminal, cx| terminal.set_search(options, cx));
    }

    fn navigate_search(&mut self, backwards: bool, cx: &mut Context<Self>) {
        if self.parent_input_enabled && self.search_open {
            self.terminal
                .update(cx, |terminal, cx| terminal.navigate_search(backwards, cx));
        }
    }

    fn search_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = if let Some(error) = &self.search_summary.error {
            error.clone()
        } else if self.search_summary.searching {
            "搜索中…".into()
        } else if self.search_input.read(cx).value().is_empty() {
            "全部输出与历史".into()
        } else {
            format!(
                "{} / {}{}",
                self.search_summary.current,
                self.search_summary.matches,
                if self.search_summary.limited { "+" } else { "" }
            )
        };
        div()
            .key_context("SyncSSHSearch")
            .flex()
            .flex_col()
            .flex_shrink_0()
            .px_2()
            .py_2()
            .gap_1()
            .bg(rgb(self.palette.map_color(0x152334)))
            .border_b_1()
            .border_color(rgb(self.palette.map_color(0x30445f)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.search_input).small()),
                    )
                    .child(
                        Button::new("terminal-search-case")
                            .small()
                            .ghost()
                            .label("Aa")
                            .selected(self.search_case_sensitive)
                            .tooltip("区分大小写")
                            .disabled(!self.parent_input_enabled)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.search_case_sensitive = !this.search_case_sensitive;
                                this.update_search(cx);
                            })),
                    )
                    .child(
                        Button::new("terminal-search-regex")
                            .small()
                            .ghost()
                            .label(".*")
                            .selected(self.search_regex)
                            .tooltip("正则表达式")
                            .disabled(!self.parent_input_enabled)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.search_regex = !this.search_regex;
                                this.update_search(cx);
                            })),
                    )
                    .child(
                        Button::new("terminal-search-previous")
                            .small()
                            .ghost()
                            .label("↑")
                            .tooltip("上一个 · Shift+Enter")
                            .disabled(
                                !self.parent_input_enabled
                                    || self.search_summary.matches == 0
                                    || self.search_summary.searching,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.navigate_search(true, cx))),
                    )
                    .child(
                        Button::new("terminal-search-next")
                            .small()
                            .ghost()
                            .label("↓")
                            .tooltip("下一个 · Enter")
                            .disabled(
                                !self.parent_input_enabled
                                    || self.search_summary.matches == 0
                                    || self.search_summary.searching,
                            )
                            .on_click(
                                cx.listener(|this, _, _, cx| this.navigate_search(false, cx)),
                            ),
                    )
                    .child(
                        Button::new("terminal-search-close")
                            .small()
                            .ghost()
                            .label("×")
                            .tooltip("关闭搜索 · Escape")
                            .disabled(!self.parent_input_enabled)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_search(window, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(if self.search_summary.error.is_some() {
                        self.palette.error
                    } else {
                        self.palette.muted
                    }))
                    .child(count),
            )
            .into_any_element()
    }

    pub fn toggle_zmodem_help(&mut self, cx: &mut Context<Self>) {
        if self.parent_input_enabled {
            self.show_zmodem_help = !self.show_zmodem_help;
            cx.notify();
        }
    }

    fn on_zmodem_event(&mut self, event: ZmodemEvent, cx: &mut Context<Self>) {
        match event {
            ZmodemEvent::Requested { id, direction } => {
                let uploading = matches!(direction, ZmodemDirection::Upload);
                self.transfer = Some(ZmodemTransfer {
                    id,
                    uploading,
                    phase: TransferPhase::Requested,
                    name: String::new(),
                    transferred: 0,
                    total: None,
                    status: if uploading {
                        "检测到 rz · 请选择要上传的本地文件"
                    } else {
                        "检测到 sz · 请选择下载文件的保存目录"
                    }
                    .into(),
                    error: None,
                });
                self.show_zmodem_help = false;
                self.input_error = false;
            }
            ZmodemEvent::Progress {
                id,
                name,
                transferred,
                total,
            } => {
                if let Some(transfer) = self.transfer.as_mut().filter(|t| t.id == id && t.active())
                {
                    transfer.name = name.chars().filter(|c| !c.is_control()).take(180).collect();
                    transfer.transferred = transferred;
                    transfer.total = total;
                    if transfer.phase != TransferPhase::Cancelling {
                        transfer.phase = TransferPhase::Transferring;
                        transfer.status = if transfer.uploading {
                            "正在上传"
                        } else {
                            "正在下载"
                        }
                        .into();
                    }
                }
            }
            ZmodemEvent::Finished { id, result } => {
                if let Some(transfer) = self.transfer.as_mut().filter(|t| t.id == id) {
                    transfer.phase = TransferPhase::Finished;
                    transfer.name.clear();
                    transfer.status = match result {
                        Ok(summary) => format!(
                            "{} · {} 个文件 · {}{}",
                            if transfer.uploading {
                                "上传完成"
                            } else {
                                "下载完成"
                            },
                            summary.files,
                            format_bytes(summary.bytes),
                            if summary.skipped > 0 {
                                format!(" · 跳过 {} 个文件", summary.skipped)
                            } else {
                                String::new()
                            },
                        ),
                        Err(error) => {
                            if transfer.error.is_none() {
                                transfer.error = Some(error);
                            }
                            "ZMODEM 传输已结束".into()
                        }
                    };
                }
            }
        }
        self.sync_input_enabled(cx);
        cx.notify();
    }

    fn choose_zmodem_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.parent_input_enabled {
            return;
        }
        let Some(transfer) = self
            .transfer
            .as_mut()
            .filter(|t| t.phase == TransferPhase::Requested)
        else {
            return;
        };
        let id = transfer.id;
        let uploading = transfer.uploading;
        transfer.phase = TransferPhase::Choosing;
        transfer.status = if uploading {
            "正在选择上传文件…"
        } else {
            "正在选择保存目录…"
        }
        .into();
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: uploading,
            directories: !uploading,
            multiple: uploading,
            prompt: Some(
                if uploading {
                    "选择要通过 ZMODEM 上传的文件"
                } else {
                    "选择 ZMODEM 下载保存目录"
                }
                .into(),
            ),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = prompt.await;
            let _ = this.update_in(cx, |this, _, cx| {
                // A previous picker can finish after cancellation, disconnection, or a new request.
                let Some(transfer) = this.transfer.as_mut().filter(|t| t.accepts_picker(id)) else {
                    return;
                };
                let command = match result {
                    Ok(Ok(Some(paths))) if !paths.is_empty() => {
                        if uploading {
                            this.zmodem.send_files(id, paths)
                        } else if paths.len() == 1 {
                            this.zmodem
                                .receive_to(id, paths.into_iter().next().unwrap())
                        } else {
                            Err("请选择一个保存目录。".into())
                        }
                    }
                    Ok(Ok(_)) => {
                        this.zmodem.cancel(id);
                        transfer.phase = TransferPhase::Cancelling;
                        transfer.status = "已取消选择，正在结束 ZMODEM…".into();
                        cx.notify();
                        return;
                    }
                    _ => Err("无法打开文件选择器，请取消传输后重试。".into()),
                };
                match command {
                    Ok(()) => {
                        transfer.phase = TransferPhase::Transferring;
                        transfer.status = if uploading {
                            "正在开始上传…"
                        } else {
                            "正在开始下载…"
                        }
                        .into();
                    }
                    Err(error) => {
                        transfer.error = Some(error);
                        transfer.phase = TransferPhase::Cancelling;
                        transfer.status = "正在结束 ZMODEM…".into();
                        this.zmodem.cancel(id);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn cancel_zmodem(&mut self, cx: &mut Context<Self>) {
        if !self.parent_input_enabled {
            return;
        }
        if let Some(transfer) = self.transfer.as_mut().filter(|t| t.active()) {
            self.zmodem.cancel(transfer.id);
            transfer.phase = TransferPhase::Cancelling;
            transfer.status = "正在取消 ZMODEM 传输…".into();
            cx.notify();
        }
    }

    fn zmodem_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut panel = div()
            .occlude()
            .absolute()
            .bottom_2()
            .left_2()
            .right_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(self.palette.map_color(0x30445f)))
            .bg(rgb(self.palette.map_color(0x152334)))
            .text_color(rgb(self.palette.map_color(0xd5deee)))
            .text_sm()
            .flex()
            .flex_col()
            .gap_2();
        let mut actions = div().flex().items_center().gap_2().child(
            div()
                .flex_1()
                .text_color(rgb(self.palette.map_color(0x6ce5c3)))
                .child("ZMODEM"),
        );
        if let Some(transfer) = &self.transfer {
            if transfer.phase == TransferPhase::Requested {
                actions = actions.child(
                    Button::new("zmodem-choose")
                        .small()
                        .primary()
                        .label(if transfer.uploading {
                            "选择文件并上传"
                        } else {
                            "选择保存目录"
                        })
                        .disabled(!self.parent_input_enabled)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.choose_zmodem_path(window, cx)),
                        ),
                );
            }
            if transfer.active() {
                actions = actions.child(
                    Button::new("zmodem-cancel")
                        .small()
                        .label(if transfer.phase == TransferPhase::Cancelling {
                            "正在取消…"
                        } else {
                            "取消传输"
                        })
                        .disabled(
                            !self.parent_input_enabled
                                || transfer.phase == TransferPhase::Cancelling,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_zmodem(cx))),
                );
            } else {
                actions = actions.child(
                    Button::new("zmodem-dismiss")
                        .small()
                        .ghost()
                        .label("关闭")
                        .disabled(!self.parent_input_enabled)
                        .on_click(cx.listener(|this, _, window, cx| {
                            if !this.parent_input_enabled {
                                return;
                            }
                            this.transfer = None;
                            this.terminal.read(cx).focus_handle().focus(window);
                            cx.notify();
                        })),
                );
            }
            panel = panel.child(actions).child(transfer.status.clone());
            if !transfer.name.is_empty() {
                panel = panel
                    .child(div().overflow_hidden().child(transfer.name.clone()))
                    .child(progress_label(transfer.transferred, transfer.total));
            }
            if let Some(error) = &transfer.error {
                panel = panel.child(
                    div()
                        .text_color(rgb(self.palette.map_color(0xffb2bd)))
                        .child(error.clone()),
                );
            }
            if transfer.active() {
                panel = panel.child(
                    div()
                        .text_xs()
                        .text_color(rgb(self.palette.map_color(0x9cacc4)))
                        .child(if transfer.uploading {
                            "传输期间暂停当前终端输入。请选择普通文件；对端决定文件的保存位置。"
                        } else {
                            "传输期间暂停当前终端输入。文件保存在所选目录，同名文件不会覆盖。"
                        }),
                );
            }
        } else {
            actions = actions.child(
                Button::new("zmodem-help-close")
                    .small()
                    .ghost()
                    .label("关闭")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_zmodem_help = false;
                        cx.notify();
                    })),
            );
            panel = panel
                .child(actions)
                .child("上传：在终端执行 rz，再选择本地文件。")
                .child("下载：在终端执行 sz 文件名，再选择本地保存目录。")
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(self.palette.map_color(0x9cacc4)))
                        .child(
                            "对端需提供 rz / sz。支持多个普通文件；传输独占当前终端，可随时取消。",
                        ),
                );
        }
        panel.into_any_element()
    }

    pub fn set_effects(&mut self, effects: Effects, cx: &mut Context<Self>) {
        self.effects = effects;
        self.particles
            .retain(|particle| particle.source.enabled(effects));
        if !effects.input {
            self.pulse = None;
        }
        if !effects.mouse {
            self.last_pointer = None;
        }
        cx.notify();
    }

    fn random(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        self.seed as f32 / u32::MAX as f32
    }

    fn burst(&mut self, origin: Point<Pixels>, count: usize, source: EffectSource) {
        if !source.enabled(self.effects) {
            return;
        }
        let now = Instant::now();
        self.particles
            .retain(|particle| now.duration_since(particle.born) < PARTICLE_LIFETIME);
        for _ in 0..count {
            let angle = self.random() * std::f32::consts::TAU;
            let speed = if matches!(source, EffectSource::Input) {
                28. + self.random() * 95.
            } else {
                8. + self.random() * 20.
            };
            let size = if matches!(source, EffectSource::Input) {
                1.2 + self.random() * 2.
            } else {
                2. + self.random() * 2.5
            };
            let hue = 0.43 + self.random() * 0.16;
            self.particles.push(Particle {
                source,
                origin,
                velocity: point(angle.cos() * speed, angle.sin() * speed - 15.),
                born: now,
                size,
                hue,
            });
        }
        if self.particles.len() > MAX_PARTICLES {
            self.particles.drain(..self.particles.len() - MAX_PARTICLES);
        }
    }

    fn on_pointer(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.effects.mouse {
            return;
        }
        let now = Instant::now();
        if let Some((previous, time)) = self.last_pointer {
            let dx = f32::from(event.position.x - previous.x);
            let dy = f32::from(event.position.y - previous.y);
            if now.duration_since(time) < Duration::from_millis(14) || dx * dx + dy * dy < 5. {
                return;
            }
            for step in 1..=3 {
                let t = step as f32 / 3.;
                self.burst(
                    point(previous.x + px(dx * t), previous.y + px(dy * t)),
                    1,
                    EffectSource::Mouse,
                );
            }
        }
        self.last_pointer = Some((event.position, now));
        cx.notify();
    }
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.search_open {
            self.search_input.focus_handle(cx)
        } else {
            self.terminal.read(cx).focus_handle().clone()
        }
    }
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        self.particles
            .retain(|particle| now.duration_since(particle.born) < PARTICLE_LIFETIME);
        let pulse = self
            .pulse
            .filter(|instant| now.duration_since(*instant) < Duration::from_millis(400));
        let particles = self.particles.clone();
        let cursor = self.terminal.read(cx).cursor_bounds();
        if !particles.is_empty() || pulse.is_some() {
            window.request_animation_frame();
        }
        let terminal_body = div()
            .relative()
            .w_full()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .on_mouse_move(cx.listener(Self::on_pointer))
            .child(self.terminal.clone())
            .when(
                self.input_error && self.transfer.is_none() && !self.show_zmodem_help,
                |element| {
                    element.child(
                        div()
                            .absolute()
                            .bottom_2()
                            .left_2()
                            .right_2()
                            .p_2()
                            .rounded_md()
                            .bg(rgb(self.palette.map_color(0x442f20)))
                            .text_color(rgb(self.palette.map_color(0xffd59c)))
                            .text_sm()
                            .child("输入未完全发送：连接已关闭或发送队列繁忙，请检查终端后重试。"),
                    )
                },
            )
            .child(
                canvas(
                    |bounds, _, _| bounds,
                    move |bounds, _, window, _| {
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            if let Some(instant) = pulse {
                                let fade = 1. - now.duration_since(instant).as_secs_f32() / 0.4;
                                for spread in [14., 9., 5.] {
                                    let padding = px(spread * (1.1 - fade * 0.3));
                                    window.paint_quad(quad(
                                        Bounds::new(
                                            point(
                                                cursor.origin.x - padding,
                                                cursor.origin.y - padding,
                                            ),
                                            size(
                                                cursor.size.width + padding * 2.,
                                                cursor.size.height + padding * 2.,
                                            ),
                                        ),
                                        px(10.),
                                        hsla(0.45, 0.85, 0.65, fade * 0.028),
                                        px(0.),
                                        transparent_black(),
                                        Default::default(),
                                    ));
                                }
                            }
                            for particle in particles {
                                let age = now.duration_since(particle.born).as_secs_f32();
                                let remaining = 1. - age / PARTICLE_LIFETIME.as_secs_f32();
                                let center = point(
                                    particle.origin.x + px(particle.velocity.x * age),
                                    particle.origin.y
                                        + px(particle.velocity.y * age + 16. * age * age),
                                );
                                for (scale, opacity) in [(3.8, 0.065), (1.8, 0.18), (1., 0.85)] {
                                    let radius = px(particle.size * remaining * scale);
                                    window.paint_quad(quad(
                                        Bounds::new(
                                            point(center.x - radius, center.y - radius),
                                            size(radius * 2., radius * 2.),
                                        ),
                                        radius,
                                        hsla(particle.hue, 0.85, 0.7, remaining * opacity),
                                        px(0.),
                                        transparent_black(),
                                        Default::default(),
                                    ));
                                }
                            }
                        });
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .when(
                self.transfer.is_some() || self.show_zmodem_help,
                |element| element.child(self.zmodem_view(cx)),
            );
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .key_context("SyncSSHPane")
            .on_action(cx.listener(|this, _: &ToggleTerminalSearch, window, cx| {
                this.toggle_search(window, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &SearchNext, _, cx| {
                this.navigate_search(false, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &SearchPrevious, _, cx| {
                this.navigate_search(true, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &SearchClose, window, cx| {
                this.close_search(window, cx);
                cx.stop_propagation();
            }))
            .when(self.search_open, |element| {
                element.child(self.search_view(cx))
            })
            .child(terminal_body)
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        if let Some(transfer) = self.transfer.as_ref().filter(|t| t.active()) {
            self.zmodem.cancel(transfer.id);
        }
    }
}

impl EventEmitter<PaneExited> for TerminalPane {}

fn terminal_input_allowed(parent_enabled: bool, transfer_active: bool) -> bool {
    parent_enabled && !transfer_active
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / (1024. * 1024. * 1024.))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024. * 1024.))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.)
    } else {
        format!("{bytes} B")
    }
}

fn progress_label(transferred: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => format!(
            "{} / {} · {:.0}%",
            format_bytes(transferred),
            format_bytes(total),
            (transferred as f64 / total as f64 * 100.).min(100.),
        ),
        _ => format_bytes(transferred),
    }
}

#[cfg(test)]
mod zmodem_tests {
    use super::{terminal_input_allowed, TransferPhase, ZmodemTransfer};

    #[test]
    fn completing_transfer_does_not_unlock_parent_modal() {
        assert!(!terminal_input_allowed(false, false));
        assert!(!terminal_input_allowed(false, true));
        assert!(!terminal_input_allowed(true, true));
        assert!(terminal_input_allowed(true, false));
    }

    #[test]
    fn stale_file_picker_cannot_start_new_or_cancelled_transfer() {
        let mut transfer = ZmodemTransfer {
            id: 2,
            uploading: true,
            phase: TransferPhase::Choosing,
            name: String::new(),
            transferred: 0,
            total: None,
            status: String::new(),
            error: None,
        };
        assert!(transfer.accepts_picker(2));
        assert!(!transfer.accepts_picker(1));
        transfer.phase = TransferPhase::Cancelling;
        assert!(!transfer.accepts_picker(2));
        transfer.phase = TransferPhase::Finished;
        assert!(!transfer.accepts_picker(2));
    }
}
