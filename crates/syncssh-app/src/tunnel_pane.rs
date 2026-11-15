use gpui::{prelude::*, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    Disableable, Sizable,
};
use std::collections::HashSet;
use syncssh_core::{TunnelMode, TunnelRule};
use syncssh_session::{TunnelHandle, TunnelKind, TunnelSpec, TunnelState, TunnelStatus};

pub struct SaveTunnelRules(pub Vec<TunnelRule>);

pub fn specification(rule: &TunnelRule) -> TunnelSpec {
    TunnelSpec {
        kind: match rule.mode {
            TunnelMode::Local => TunnelKind::Local,
            TunnelMode::Remote => TunnelKind::Remote,
            TunnelMode::Dynamic => TunnelKind::Dynamic,
        },
        listen_host: rule.listen_host.clone(),
        listen_port: rule.listen_port,
        target_host: rule.target_host.clone(),
        target_port: rule.target_port,
    }
}

pub struct TunnelPane {
    handle: TunnelHandle,
    rules: Vec<TunnelRule>,
    statuses: Vec<TunnelStatus>,
    pending: HashSet<String>,
    fields: Vec<(&'static str, &'static str, Entity<InputState>)>,
    mode: TunnelMode,
    auto_start: bool,
    editing: Option<String>,
    error: Option<String>,
    message: String,
    focus: FocusHandle,
    _watch: Task<()>,
}

impl EventEmitter<SaveTunnelRules> for TunnelPane {}
impl TunnelPane {
    pub fn saved(&mut self, rules: &[TunnelRule], cx: &mut Context<Self>) {
        if self.rules == rules {
            self.message = "隧道规则已随连接配置加密保存".into();
            cx.notify();
        }
    }
    pub fn start_saved_rules(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self
            .rules
            .iter()
            .filter(|r| r.auto_start && self.running(r).is_none())
            .map(|r| r.id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.toggle(id, window, cx);
        }
    }
    pub fn new(
        handle: TunnelHandle,
        rules: Vec<TunnelRule>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fields = [
            ("name", "规则名称", ""),
            ("listen_host", "监听地址", "127.0.0.1"),
            ("listen_port", "监听端口（0 自动分配）", "0"),
            ("target_host", "目标地址", "127.0.0.1"),
            ("target_port", "目标端口", ""),
        ]
        .into_iter()
        .map(|(key, label, value)| {
            let state = cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(label);
                s.set_value(value, window, cx);
                s
            });
            (key, label, state)
        })
        .collect();
        let mut receiver = handle.subscribe();
        let watch = cx.spawn_in(window, async move |this, cx| {
            while receiver.changed().await.is_ok() {
                let statuses = receiver.borrow_and_update().clone();
                if this
                    .update_in(cx, |this, _, cx| {
                        this.statuses = statuses;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            statuses: handle.snapshot(),
            handle,
            rules,
            pending: HashSet::new(),
            fields,
            mode: TunnelMode::Local,
            auto_start: false,
            editing: None,
            error: None,
            message: "隧道与当前终端共用 SSH 连接".into(),
            focus: cx.focus_handle(),
            _watch: watch,
        }
    }
    fn value(&self, key: &str, cx: &App) -> String {
        self.fields
            .iter()
            .find(|f| f.0 == key)
            .map(|f| f.2.read(cx).value().trim().to_string())
            .unwrap_or_default()
    }
    fn running(&self, rule: &TunnelRule) -> Option<&TunnelStatus> {
        let spec = specification(rule);
        self.statuses
            .iter()
            .rev()
            .find(|s| s.spec == spec && s.state == TunnelState::Running)
    }
    fn edit(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rule) = self.rules.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        if self.running(&rule).is_some() || self.pending.contains(&id) {
            return;
        }
        self.mode = rule.mode;
        self.auto_start = rule.auto_start;
        self.editing = Some(id);
        self.error = None;
        for (key, _, state) in &self.fields {
            let value = match *key {
                "name" => rule.name.clone(),
                "listen_host" => rule.listen_host.clone(),
                "listen_port" => rule.listen_port.to_string(),
                "target_host" => rule.target_host.clone(),
                _ => rule.target_port.to_string(),
            };
            state.update(cx, |s, cx| s.set_value(value, window, cx));
        }
        cx.notify();
    }
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<TunnelRule> {
            if let Some(id) = &self.editing {
                anyhow::ensure!(
                    !self.pending.contains(id)
                        && !self
                            .rules
                            .iter()
                            .find(|rule| &rule.id == id)
                            .is_some_and(|rule| self.running(rule).is_some()),
                    "请先停止此隧道，再更新规则"
                );
            }
            let rule = TunnelRule {
                id: self
                    .editing
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: self.value("name", cx),
                mode: self.mode,
                listen_host: self.value("listen_host", cx),
                listen_port: self
                    .value("listen_port", cx)
                    .parse()
                    .map_err(|_| anyhow::anyhow!("监听端口须为 0–65535"))?,
                target_host: if self.mode == TunnelMode::Dynamic {
                    String::new()
                } else {
                    self.value("target_host", cx)
                },
                target_port: if self.mode == TunnelMode::Dynamic {
                    0
                } else {
                    self.value("target_port", cx)
                        .parse()
                        .map_err(|_| anyhow::anyhow!("目标端口须为 1–65535"))?
                },
                auto_start: self.auto_start,
            };
            rule.validate()?;
            anyhow::ensure!(
                self.editing.is_some() || self.rules.len() < 16,
                "每台服务器最多保存 16 条规则"
            );
            anyhow::ensure!(
                !self
                    .rules
                    .iter()
                    .any(|r| r.id != rule.id && specification(r) == specification(&rule)),
                "已有相同隧道规则"
            );
            Ok(rule)
        })();
        match result {
            Ok(rule) => {
                if let Some(existing) = self.rules.iter_mut().find(|r| r.id == rule.id) {
                    *existing = rule;
                } else {
                    self.rules.push(rule);
                }
                self.editing = None;
                self.error = None;
                self.message = "规则已修改，点击“保存规则”可随连接配置加密保存".into();
                self.fields[0]
                    .2
                    .update(cx, |s, cx| s.set_value("", window, cx));
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        cx.notify();
    }
    fn toggle(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.contains(&id) {
            return;
        }
        let Some(rule) = self.rules.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        let running = self.running(&rule).map(|s| s.id);
        let rule_name = rule.name.clone();
        let handle = self.handle.clone();
        self.pending.insert(id.clone());
        self.error = None;
        let (tx, rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let result = if let Some(id) = running {
                handle.stop(id)
            } else {
                handle.start(specification(&rule)).map(|_| ())
            };
            let _ = tx.send(result.map_err(|e| e.to_string()));
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(result) = rx.recv_async().await {
                let _ = this.update_in(cx, |this, _, cx| {
                    this.pending.remove(&id);
                    this.statuses = this.handle.snapshot();
                    match result {
                        Ok(()) => {
                            this.message = if running.is_some() {
                                "隧道已停止"
                            } else {
                                "隧道已启动"
                            }
                            .into()
                        }
                        Err(e) => this.error = Some(format!("{rule_name}：{e}")),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }
}
impl Focusable for TunnelPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for TunnelPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::appearance::palette(cx);
        let mut content = div().flex().flex_col().gap_3().p_4();
        content = content
            .child(
                div()
                    .text_lg()
                    .text_color(palette.color(0x6ce5c3))
                    .child("SSH 隧道"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(palette.color(0x8291aa))
                    .child(self.message.clone()),
            )
            .child(
                Button::new("save-tunnel-rules")
                    .small()
                    .label("保存到连接配置")
                    .disabled(!self.pending.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.emit(SaveTunnelRules(this.rules.clone()));
                    })),
            );
        for rule in &self.rules {
            let spec = specification(rule);
            let status = self
                .running(rule)
                .or_else(|| self.statuses.iter().rev().find(|s| s.spec == spec));
            let running = status.is_some_and(|s| s.state == TunnelState::Running);
            let pending = self.pending.contains(&rule.id);
            let port = status
                .filter(|_| running)
                .map(|s| s.bound_port)
                .unwrap_or(rule.listen_port);
            let detail = if rule.mode == TunnelMode::Dynamic {
                format!("SOCKS5 · {}:{}", rule.listen_host, port)
            } else {
                format!(
                    "{} · {}:{} → {}:{}",
                    if rule.mode == TunnelMode::Local {
                        "本地"
                    } else {
                        "远程"
                    },
                    rule.listen_host,
                    port,
                    rule.target_host,
                    rule.target_port
                )
            };
            let id = rule.id.clone();
            let edit_id = id.clone();
            let remove_id = id.clone();
            let state = if pending {
                "处理中…".into()
            } else if running {
                format!(
                    "运行中 · {} 个连接",
                    status.map(|s| s.active_connections).unwrap_or(0)
                )
            } else if self.handle.is_closed() {
                "SSH 已断开".into()
            } else {
                "已停止".into()
            };
            let mut card = div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .bg(palette.color(0x152236))
                .rounded_md()
                .child(div().text_sm().child(rule.name.clone()))
                .child(div().text_xs().child(detail))
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.color(0x8291aa))
                        .child(state),
                );
            if let Some(error) = status.and_then(|s| s.last_error.as_ref()) {
                card = card.child(
                    div()
                        .text_xs()
                        .text_color(palette.color(0xf38c92))
                        .child(error.clone()),
                );
            }
            card =
                card.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new(SharedString::from(format!("run-{id}")))
                                .small()
                                .label(if running { "停止" } else { "启动" })
                                .disabled(pending || self.handle.is_closed())
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.toggle(id.clone(), w, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("edit-{edit_id}")))
                                .small()
                                .ghost()
                                .label("编辑")
                                .disabled(running || pending)
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.edit(edit_id.clone(), w, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("remove-{remove_id}")))
                                .small()
                                .ghost()
                                .label("移除规则")
                                .disabled(running || pending)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.rules.retain(|r| r.id != remove_id);
                                    if this.editing.as_ref() == Some(&remove_id) {
                                        this.editing = None;
                                    }
                                    this.message = "规则已修改，请保存".into();
                                    cx.notify();
                                })),
                        ),
                );
            content = content.child(card);
        }
        let mut types = div().flex().gap_1();
        for (index, mode, label) in [
            (0usize, TunnelMode::Local, "本地 -L"),
            (1usize, TunnelMode::Remote, "远程 -R"),
            (2usize, TunnelMode::Dynamic, "动态 -D"),
        ] {
            types = types.child(
                Button::new(("tunnel-kind", index))
                    .small()
                    .label(format!(
                        "{}{}",
                        if self.mode == mode { "● " } else { "" },
                        label
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mode = mode;
                        cx.notify();
                    })),
            );
        }
        content = content
            .child(div().mt_2().text_sm().child(if self.editing.is_some() {
                "编辑规则"
            } else {
                "添加规则"
            }))
            .child(types);
        let explanation = match self.mode {
            TunnelMode::Local => "本机监听 → 经 SSH 访问远端网络中的目标",
            TunnelMode::Remote => "SSH 服务器监听 → 经隧道访问本机网络中的目标",
            TunnelMode::Dynamic => "本机 SOCKS5 代理，目标名称由 SSH 服务器解析（仅 TCP）",
        };
        content = content.child(
            div()
                .text_xs()
                .text_color(palette.color(0x8291aa))
                .child(explanation),
        );
        for (key, label, input) in &self.fields {
            if self.mode == TunnelMode::Dynamic && key.starts_with("target_") {
                continue;
            }
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().child(*label))
                    .child(Input::new(input).small()),
            );
        }
        content=content.child(Button::new("tunnel-auto").small().ghost().label(format!("{} SSH 连接后自动启动",if self.auto_start{"☑"}else{"☐"})).on_click(cx.listener(|this,_,_,cx|{this.auto_start=!this.auto_start;cx.notify();})))
            .child(Button::new("add-tunnel").small().label(if self.editing.is_some(){"更新规则"}else{"添加规则"}).on_click(cx.listener(|this,_,w,cx|this.add(w,cx))))
            .child(div().text_xs().text_color(palette.color(0x8291aa)).child("默认仅监听回环地址；改为其他地址会允许对应网络访问。远程监听范围由服务器策略决定。"));
        if let Some(error) = &self.error {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(palette.color(0xf38c92))
                    .child(error.clone()),
            );
        }
        div()
            .id("tunnel-scroll")
            .track_focus(&self.focus)
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .bg(palette.color(0x101927))
            .text_color(palette.color(0xd5dede))
            .child(content)
    }
}
