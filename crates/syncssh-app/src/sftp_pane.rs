//! File management and bounded transfer queues on the authenticated SSH connection.
use gpui::{prelude::*, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    Disableable, Sizable,
};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use syncssh_session::{
    BatchProgress, BatchReport, RemoteDirectory, RemoteEntry, RemoteEntryKind, ResumeStore,
    SftpHandle, SftpOperation, TransferJob,
};

const PANEL: u32 = 0x101927;
const LINE: u32 = 0x223047;
const MUTED: u32 = 0x8291aa;
const MINT: u32 = 0x6ce5c3;
#[derive(Clone)]
enum Form {
    Directory,
    Rename(RemoteEntry),
    Permissions(RemoteEntry),
    Delete(Vec<RemoteEntry>),
}
enum WorkerEvent {
    Progress(BatchProgress),
    Finished(BatchReport),
}

pub struct SftpPane {
    handle: SftpHandle,
    focus: FocusHandle,
    directory_input: Entity<InputState>,
    local_input: Entity<InputState>,
    remote_name: Entity<InputState>,
    form_input: Entity<InputState>,
    _directory_events: Subscription,
    directory: Option<String>,
    entries: Vec<RemoteEntry>,
    selected: BTreeSet<usize>,
    upload_paths: Vec<PathBuf>,
    form: Option<Form>,
    listing: bool,
    transferring: bool,
    managing: bool,
    choosing_path: bool,
    generation: u64,
    cancellation: Arc<AtomicBool>,
    status: String,
    error: Option<String>,
    progress: String,
    results: Vec<(String, bool)>,
    failed: Vec<TransferJob>,
    checkpoints: ResumeStore,
    retain_partial: bool,
    refresh_after_batch: bool,
}
impl SftpPane {
    pub fn new(handle: SftpHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let directory_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("远端目录");
            s.set_value(".", window, cx);
            s
        });
        let local_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("本地文件或目录绝对路径"));
        let remote_name =
            cx.new(|cx| InputState::new(window, cx).placeholder("单项上传可改名；留空保留名称"));
        let form_input = cx.new(|cx| InputState::new(window, cx));
        let events = cx.subscribe_in(
            &directory_input,
            window,
            |this, _, event: &InputEvent, w, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.go_to_input(w, cx);
                }
            },
        );
        Self {
            handle,
            focus: cx.focus_handle(),
            directory_input,
            local_input,
            remote_name,
            form_input,
            _directory_events: events,
            directory: None,
            entries: vec![],
            selected: BTreeSet::new(),
            upload_paths: vec![],
            form: None,
            listing: false,
            transferring: false,
            managing: false,
            choosing_path: false,
            generation: 0,
            cancellation: Arc::new(AtomicBool::new(false)),
            status: "正在打开 SFTP…".into(),
            error: None,
            progress: String::new(),
            results: vec![],
            failed: vec![],
            checkpoints: ResumeStore::default(),
            retain_partial: false,
            refresh_after_batch: false,
        }
    }
    /// Reconnecting preserves pending/failed jobs and user paths in this pane.
    pub fn rebind_handle(
        &mut self,
        handle: SftpHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancellation.store(true, Ordering::Release);
        self.handle = handle;
        self.listing = false;
        self.choosing_path = false;
        if self.transferring {
            // Let the old worker settle and deliver its final failed-job list.
            // Its checkpoint remains shared; no new transfer starts automatically.
            self.refresh_after_batch = true;
            self.status = "连接已恢复，正在收尾旧队列；可随后手动重试".into();
            cx.notify();
        } else {
            self.generation = self.generation.wrapping_add(1);
            self.refresh(window, cx);
        }
    }
    pub fn refresh(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.load_directory(self.directory.clone().unwrap_or_else(|| ".".into()), w, cx);
    }
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.cancellation.store(true, Ordering::Release);
        self.status = if self.managing {
            "正在停止后续操作；已提交的请求等待服务器确认"
        } else if self.retain_partial {
            "正在暂停；保留本面板断点，重试前将核对已传内容"
        } else {
            "正在取消；未完成文件将清理，已完成项目保留"
        }
        .into();
        cx.notify();
    }
    fn busy(&self) -> bool {
        self.listing || self.transferring || self.choosing_path
    }
    fn ready(&mut self, cx: &mut Context<Self>) -> bool {
        if self.busy() {
            return false;
        }
        if self.handle.is_closed() {
            self.error = Some("SSH 连接已关闭，请重新连接。".into());
            cx.notify();
            return false;
        }
        true
    }
    fn begin(&mut self) -> (u64, Arc<AtomicBool>) {
        self.generation = self.generation.wrapping_add(1);
        self.cancellation = Arc::new(AtomicBool::new(false));
        self.error = None;
        (self.generation, self.cancellation.clone())
    }
    fn go_to_input(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let path = self.directory_input.read(cx).value().to_string();
        self.load_directory(path, w, cx);
    }
    fn load_directory(&mut self, path: String, w: &mut Window, cx: &mut Context<Self>) {
        if !self.ready(cx) {
            return;
        }
        let (generation, cancel) = self.begin();
        self.listing = true;
        self.form = None;
        self.status = "正在读取目录…".into();
        let handle = self.handle.clone();
        let (tx, rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(handle.list_with_cancel(path, cancel));
        });
        cx.spawn_in(w, async move |this, cx| {
            if let Ok(result) = rx.recv_async().await {
                let _ = this.update_in(cx, |this, w, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.listing = false;
                    match result {
                        Ok(RemoteDirectory { path, entries }) => {
                            this.directory_input
                                .update(cx, |s, cx| s.set_value(path.clone(), w, cx));
                            this.directory = Some(path);
                            this.entries = entries;
                            this.selected.clear();
                            this.status =
                                format!("{} 项 · 点击选择，目录右侧进入", this.entries.len());
                        }
                        Err(e) => {
                            this.error = Some(e.to_string());
                            this.status = "目录读取失败".into();
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }
    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.ready(cx) {
            return;
        }
        if !self.selected.remove(&index) {
            self.selected.insert(index);
        }
        self.form = None;
        self.status = format!("已选择 {} 项", self.selected.len());
        cx.notify();
    }
    fn chosen(&self) -> Vec<RemoteEntry> {
        self.selected
            .iter()
            .filter_map(|i| self.entries.get(*i).cloned())
            .collect()
    }
    fn choose_upload(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.ready(cx) {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.choosing_path = true;
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("选择文件或目录，可多选".into()),
        });
        cx.spawn_in(w, async move |this, cx| {
            let result = prompt.await;
            let _ = this.update_in(cx, |this, w, cx| {
                if this.generation != generation || !this.choosing_path {
                    return;
                }
                this.choosing_path = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        this.upload_paths = paths;
                        let text = if this.upload_paths.len() == 1 {
                            this.upload_paths[0].display().to_string()
                        } else {
                            format!("已选择 {} 项", this.upload_paths.len())
                        };
                        this.local_input
                            .update(cx, |s, cx| s.set_value(text, w, cx));
                    }
                    Ok(Err(e)) => this.error = Some(e.to_string()),
                    Err(e) => this.error = Some(e.to_string()),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn upload(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.ready(cx) {
            return;
        }
        let Some(dir) = self.directory.clone() else {
            return;
        };
        let input = self.local_input.read(cx).value().to_string();
        let paths = if self.upload_paths.len() > 1
            && input == format!("已选择 {} 项", self.upload_paths.len())
        {
            self.upload_paths.clone()
        } else {
            vec![PathBuf::from(input)]
        };
        let name = self.remote_name.read(cx).value().to_string();
        let mut jobs = Vec::new();
        for path in &paths {
            let source_name = path.file_name().and_then(|s| s.to_str());
            let target = if paths.len() == 1 && !name.is_empty() {
                Some(name.as_str())
            } else {
                source_name
            };
            let Some(remote) = target.and_then(|n| remote_child(&dir, n)) else {
                self.error = Some("请输入安全文件名。".into());
                cx.notify();
                return;
            };
            if !path.is_absolute() {
                self.error = Some("本地路径必须是绝对路径。".into());
                cx.notify();
                return;
            }
            jobs.push(TransferJob::Upload {
                local: path.clone(),
                remote,
            });
        }
        self.start_batch(jobs, w, cx);
    }
    fn choose_download(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.ready(cx) {
            return;
        }
        let entries = self.chosen();
        if entries.is_empty() {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.choosing_path = true;
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择下载目标目录".into()),
        });
        cx.spawn_in(w, async move |this, cx| {
            let result = prompt.await;
            let _ = this.update_in(cx, |this, w, cx| {
                if this.generation != generation || !this.choosing_path {
                    return;
                }
                this.choosing_path = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(base) = paths.into_iter().next() {
                            this.start_batch(
                                entries
                                    .into_iter()
                                    .map(|e| TransferJob::Download {
                                        remote: e.path,
                                        local: base.join(e.name),
                                    })
                                    .collect(),
                                w,
                                cx,
                            );
                        }
                    }
                    Ok(Err(e)) => this.error = Some(e.to_string()),
                    Err(e) => this.error = Some(e.to_string()),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn start_batch(&mut self, jobs: Vec<TransferJob>, w: &mut Window, cx: &mut Context<Self>) {
        if jobs.is_empty() || !self.ready(cx) {
            return;
        }
        let (generation, cancel) = self.begin();
        self.transferring = true;
        self.managing = false;
        self.results.clear();
        self.failed = jobs.clone();
        self.status = "正在处理传输队列…".into();
        self.form = None;
        let handle = self.handle.clone();
        let (tx, rx) = flume::bounded(16);
        let checkpoints = self.checkpoints.clone();
        let retain_partial = self.retain_partial;
        std::thread::spawn(move || {
            let progress = tx.clone();
            let update = move |p| {
                let _ = progress.try_send(WorkerEvent::Progress(p));
            };
            let report = if retain_partial {
                handle.transfer_batch_resumable(jobs, cancel, checkpoints, update)
            } else {
                handle.transfer_batch(jobs, cancel, update)
            };
            let _ = tx.send(WorkerEvent::Finished(report));
        });
        cx.spawn_in(w, async move |this, cx| {
            while let Ok(event) = rx.recv_async().await {
                let done = matches!(event, WorkerEvent::Finished(_));
                if this
                    .update_in(cx, |this, w, cx| {
                        if this.generation != generation {
                            return;
                        }
                        match event {
                            WorkerEvent::Progress(BatchProgress::Started { index, job }) => {
                                this.status = format!("第 {} 项：{}", index + 1, job.description());
                                this.progress.clear();
                            }
                            WorkerEvent::Progress(BatchProgress::Progress { progress, .. }) => {
                                this.progress = match progress.total {
                                    Some(n) => format!(
                                        "{} / {}",
                                        format_bytes(progress.transferred),
                                        format_bytes(n)
                                    ),
                                    None => format_bytes(progress.transferred),
                                };
                            }
                            WorkerEvent::Progress(BatchProgress::Finished(_)) => {}
                            WorkerEvent::Finished(report) => {
                                this.transferring = false;
                                let refresh = this.refresh_after_batch
                                    || report.results.iter().any(|item| {
                                        matches!(item.job, TransferJob::Upload { .. })
                                            && item.result.is_ok()
                                    });
                                this.failed = report.failed_jobs();
                                let failures = this.failed.len();
                                this.status = format!(
                                    "队列结束：{} 项成功，{} 项未完成",
                                    report.results.len() - failures,
                                    failures
                                );
                                this.results = report
                                    .results
                                    .into_iter()
                                    .map(|r| {
                                        let ok = r.result.is_ok();
                                        (
                                            format!(
                                                "{} · {}",
                                                r.job.description(),
                                                r.result
                                                    .err()
                                                    .map(|e| e.to_string())
                                                    .unwrap_or_else(|| "完成".into())
                                            ),
                                            ok,
                                        )
                                    })
                                    .collect();
                                this.progress.clear();
                                this.refresh_after_batch = false;
                                if refresh {
                                    this.refresh(w, cx);
                                }
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                    || done
                {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }
    fn open_form(&mut self, kind: u8, w: &mut Window, cx: &mut Context<Self>) {
        if !self.ready(cx) {
            return;
        }
        let entries = self.chosen();
        let form = match kind {
            0 => Some(Form::Directory),
            1 if entries.len() == 1 => Some(Form::Rename(entries[0].clone())),
            2 if entries.len() == 1 => Some(Form::Permissions(entries[0].clone())),
            3 if !entries.is_empty() => Some(Form::Delete(entries)),
            _ => None,
        };
        let value = match &form {
            Some(Form::Rename(e)) => e.name.clone(),
            Some(Form::Permissions(e)) => {
                format!("{:04o}", e.permissions.unwrap_or(0o644) & 0o7777)
            }
            _ => String::new(),
        };
        self.form_input
            .update(cx, |s, cx| s.set_value(value, w, cx));
        self.form = form;
        self.error = None;
        cx.notify();
    }
    fn submit_form(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.ready(cx) {
            return;
        }
        let Some(form) = self.form.clone() else {
            return;
        };
        let Some(dir) = self.directory.clone() else {
            return;
        };
        let value = self.form_input.read(cx).value().to_string();
        let mut operations = Vec::new();
        match form {
            Form::Directory => {
                let Some(path) = remote_child(&dir, &value) else {
                    self.error = Some("请输入不含分隔符的目录名。".into());
                    cx.notify();
                    return;
                };
                operations.push(SftpOperation::CreateDirectory { path });
            }
            Form::Rename(entry) => {
                let Some(destination) = remote_child(&dir, &value) else {
                    self.error = Some("请输入安全的新名称。".into());
                    cx.notify();
                    return;
                };
                operations.push(SftpOperation::Rename {
                    source: entry.path,
                    destination,
                });
            }
            Form::Permissions(entry) => {
                let Ok(permissions) = u32::from_str_radix(&value, 8) else {
                    self.error = Some("请输入八进制权限，例如 0644。".into());
                    cx.notify();
                    return;
                };
                if value.len() > 4 || permissions > 0o7777 {
                    self.error = Some("权限范围为 0000–7777。".into());
                    cx.notify();
                    return;
                }
                operations.push(SftpOperation::Chmod {
                    path: entry.path,
                    permissions,
                });
            }
            // Only the explicit confirmation button invokes this branch. Directories must be empty.
            Form::Delete(entries) => {
                for entry in entries {
                    operations.push(if entry.kind == RemoteEntryKind::Directory {
                        SftpOperation::RemoveEmptyDirectory { path: entry.path }
                    } else {
                        SftpOperation::RemoveFile { path: entry.path }
                    });
                }
            }
        }
        let (generation, cancel) = self.begin();
        self.transferring = true;
        self.managing = true;
        self.form = None;
        self.status = "正在执行文件操作…".into();
        let handle = self.handle.clone();
        let (tx, rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let results: Vec<_> = operations
                .into_iter()
                .map(|op| {
                    let name = operation_label(&op);
                    (name, handle.manage(op, cancel.clone()))
                })
                .collect();
            let _ = tx.send(results);
        });
        cx.spawn_in(w, async move |this, cx| {
            if let Ok(results) = rx.recv_async().await {
                let _ = this.update_in(cx, |this, w, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.transferring = false;
                    this.managing = false;
                    this.refresh_after_batch = false;
                    let failures = results.iter().filter(|(_, r)| r.is_err()).count();
                    this.results = results
                        .into_iter()
                        .map(|(n, r)| {
                            let ok = r.is_ok();
                            (
                                format!(
                                    "{n} · {}",
                                    r.err()
                                        .map(|e| e.to_string())
                                        .unwrap_or_else(|| "完成".into())
                                ),
                                ok,
                            )
                        })
                        .collect();
                    this.status = format!("文件操作完成，{failures} 项失败");
                    this.refresh(w, cx);
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }
}
impl Drop for SftpPane {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
    }
}
impl Focusable for SftpPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for SftpPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::appearance::palette(cx);
        let disabled = self.busy() || self.handle.is_closed();
        let count = self.selected.len();
        let mut list = div().flex().flex_col().gap_1();
        if self.entries.is_empty() {
            list = list.child(
                palette
                    .text(
                        if self.listing {
                            "正在读取…"
                        } else {
                            "此目录为空"
                        },
                        MUTED,
                        12.,
                    )
                    .p_3(),
            );
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let selected = self.selected.contains(&index);
            let is_dir = entry.kind == RemoteEntryKind::Directory;
            let path = entry.path.clone();
            list = list.child(
                div()
                    .id(ElementId::Name(format!("sftp-entry-{index}").into()))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(palette.color(if selected { 0x21364a } else { PANEL }))
                    .child(
                        Button::new(ElementId::Name(format!("sftp-select-{index}").into()))
                            .small()
                            .ghost()
                            .label(if selected { "☑" } else { "☐" })
                            .disabled(disabled)
                            .on_click(cx.listener(move |this, _, _, cx| this.select(index, cx))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                palette
                                    .text(
                                        entry.name.clone(),
                                        if is_dir { MINT } else { 0xd5deee },
                                        12.,
                                    )
                                    .text_ellipsis(),
                            )
                            .child(palette.text(
                                match entry.kind {
                                    RemoteEntryKind::File => {
                                        entry.size.map(format_bytes).unwrap_or_default()
                                    }
                                    RemoteEntryKind::Directory => "目录".into(),
                                    RemoteEntryKind::Symlink => "符号链接（不跟随）".into(),
                                    RemoteEntryKind::Other => "特殊文件".into(),
                                },
                                MUTED,
                                10.,
                            )),
                    )
                    .when(is_dir, |d| {
                        d.child(
                            Button::new(ElementId::Name(format!("sftp-enter-{index}").into()))
                                .small()
                                .ghost()
                                .label("进入")
                                .disabled(disabled)
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.load_directory(path.clone(), w, cx)
                                })),
                        )
                    }),
            );
        }
        let mut form = div().flex().flex_col().gap_2();
        if let Some(action) = &self.form {
            let (title, delete) = match action {
                Form::Directory => ("新建目录".into(), false),
                Form::Rename(e) => (format!("重命名 {}", e.name), false),
                Form::Permissions(e) => (format!("权限 {}（八进制）", e.name), false),
                Form::Delete(entries) => (
                    format!(
                        "确认永久删除 {} 项？仅删除文件和空目录。\n{}",
                        entries.len(),
                        entries
                            .iter()
                            .take(5)
                            .map(|e| e.name.as_str())
                            .collect::<Vec<_>>()
                            .join("、")
                    ),
                    true,
                ),
            };
            form = form
                .p_2()
                .bg(palette.color(0x1b2b3f))
                .child(palette.text(title, if delete { 0xffb2bd } else { MINT }, 12.))
                .when(!delete, |d| {
                    d.child(Input::new(&self.form_input).small().disabled(disabled))
                })
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("sftp-confirm-form")
                                .small()
                                .label(if delete { "确认删除" } else { "应用" })
                                .disabled(disabled)
                                .on_click(cx.listener(|this, _, w, cx| this.submit_form(w, cx))),
                        )
                        .child(
                            Button::new("sftp-dismiss-form")
                                .small()
                                .ghost()
                                .label("取消")
                                .disabled(disabled)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.form = None;
                                    cx.notify();
                                })),
                        ),
                );
        }
        let mut results = div().flex().flex_col().gap_1();
        for (line, ok) in self.results.iter().rev().take(40).rev() {
            results =
                results.child(palette.text(line.clone(), if *ok { MUTED } else { 0xffb2bd }, 10.));
        }
        div()
            .track_focus(&self.focus)
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(palette.color(PANEL))
            .border_l_1()
            .border_color(palette.color(LINE))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(palette.color(LINE))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(palette.text("SFTP 文件管理", MINT, 14.))
                            .child(palette.text(
                                if self.handle.is_closed() {
                                    "连接已关闭"
                                } else {
                                    "复用当前 SSH"
                                },
                                MUTED,
                                10.,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(div().flex_1().min_w_0().child(
                                Input::new(&self.directory_input).small().disabled(disabled),
                            ))
                            .child(
                                Button::new("sftp-go")
                                    .small()
                                    .label("前往")
                                    .disabled(disabled)
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.go_to_input(w, cx)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Button::new("sftp-parent")
                                    .small()
                                    .ghost()
                                    .label("↑ 上级")
                                    .disabled(disabled || self.directory.is_none())
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        if let Some(p) = &this.directory {
                                            this.load_directory(parent_directory(p), w, cx);
                                        }
                                    })),
                            )
                            .child(
                                Button::new("sftp-refresh")
                                    .small()
                                    .ghost()
                                    .label("刷新")
                                    .disabled(disabled)
                                    .on_click(cx.listener(|this, _, w, cx| this.refresh(w, cx))),
                            )
                            .child(
                                Button::new("sftp-all")
                                    .small()
                                    .ghost()
                                    .label("全选")
                                    .disabled(disabled)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.selected.len() == this.entries.len() {
                                            this.selected.clear();
                                        } else {
                                            this.selected = (0..this.entries.len()).collect();
                                        }
                                        this.form = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("sftp-download")
                                    .small()
                                    .label(format!("下载 {count} 项"))
                                    .disabled(disabled || count == 0)
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.choose_download(w, cx)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Button::new("sftp-mkdir")
                                    .small()
                                    .ghost()
                                    .label("新建目录")
                                    .disabled(disabled || self.directory.is_none())
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.open_form(0, w, cx)),
                                    ),
                            )
                            .child(
                                Button::new("sftp-rename")
                                    .small()
                                    .ghost()
                                    .label("重命名")
                                    .disabled(disabled || count != 1)
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.open_form(1, w, cx)),
                                    ),
                            )
                            .child(
                                Button::new("sftp-chmod")
                                    .small()
                                    .ghost()
                                    .label("权限")
                                    .disabled(disabled || count != 1)
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.open_form(2, w, cx)),
                                    ),
                            )
                            .child(
                                Button::new("sftp-delete")
                                    .small()
                                    .ghost()
                                    .label("删除…")
                                    .disabled(disabled || count == 0)
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.open_form(3, w, cx)),
                                    ),
                            ),
                    )
                    .child(form),
            )
            .child(
                div()
                    .id("sftp-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_2()
                    .child(list),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(palette.color(LINE))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(palette.text("上传文件或目录", 0xd5deee, 12.))
                            .child(
                                Button::new("sftp-select-upload")
                                    .small()
                                    .ghost()
                                    .label("多选…")
                                    .disabled(disabled)
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.choose_upload(w, cx)),
                                    ),
                            ),
                    )
                    .child(
                        Button::new("sftp-retain-partial")
                            .small()
                            .ghost()
                            .label(if self.retain_partial {
                                "☑ 保留断点，重试时校验后续传"
                            } else {
                                "☐ 保留断点，重试时校验后续传"
                            })
                            .disabled(disabled)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.retain_partial = !this.retain_partial;
                                cx.notify();
                            })),
                    )
                    .when(self.retain_partial, |d| {
                        d.child(palette.text(
                            "断点仅保留在当前标签；远端 .part 需续传或手动删除",
                            MUTED,
                            10.,
                        ))
                    })
                    .child(Input::new(&self.local_input).small().disabled(disabled))
                    .child(Input::new(&self.remote_name).small().disabled(disabled))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("sftp-upload")
                                    .small()
                                    .primary()
                                    .label("加入并开始")
                                    .disabled(disabled || self.directory.is_none())
                                    .on_click(cx.listener(|this, _, w, cx| this.upload(w, cx))),
                            )
                            .child(palette.text("文件不覆盖 · 链接不跟随", MUTED, 10.)),
                    ),
            )
            .child(
                div()
                    .id("sftp-status")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .border_t_1()
                    .border_color(palette.color(LINE))
                    .child(palette.text(
                        self.error.clone().unwrap_or_else(|| self.status.clone()),
                        if self.error.is_some() {
                            0xffb2bd
                        } else {
                            MUTED
                        },
                        11.,
                    ))
                    .when(self.listing || self.transferring, |d| {
                        d.child(
                            div()
                                .flex()
                                .justify_between()
                                .child(palette.text(self.progress.clone(), MINT, 11.))
                                .child(
                                    Button::new("sftp-cancel")
                                        .small()
                                        .ghost()
                                        .label(
                                            if self.retain_partial
                                                && !self.managing
                                                && self.transferring
                                            {
                                                "暂停"
                                            } else {
                                                "取消"
                                            },
                                        )
                                        .disabled(self.cancellation.load(Ordering::Acquire))
                                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                                ),
                        )
                    })
                    .when(!self.failed.is_empty(), |d| {
                        d.child(
                            Button::new("sftp-retry")
                                .small()
                                .label(format!("重试 {} 项", self.failed.len()))
                                .disabled(disabled)
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.start_batch(this.failed.clone(), w, cx)
                                })),
                        )
                    })
                    .child(results),
            )
    }
}
fn operation_label(operation: &SftpOperation) -> String {
    match operation {
        SftpOperation::CreateDirectory { path } => format!("新建目录 {path}"),
        SftpOperation::Rename {
            source,
            destination,
        } => format!("重命名 {source} → {destination}"),
        SftpOperation::RemoveFile { path } => format!("删除文件 {path}"),
        SftpOperation::RemoveEmptyDirectory { path } => format!("删除空目录 {path}"),
        SftpOperation::Chmod { path, permissions } => format!("权限 {permissions:04o} · {path}"),
    }
}
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
}
fn remote_child(dir: &str, name: &str) -> Option<String> {
    safe_name(name).then(|| format!("{}/{}", dir.trim_end_matches('/'), name))
}
fn parent_directory(dir: &str) -> String {
    dir.trim_end_matches('/')
        .rsplit_once('/')
        .map(|(p, _)| if p.is_empty() { "/" } else { p })
        .unwrap_or("/")
        .into()
}
fn format_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.)
    } else {
        format!("{:.1} MiB", n as f64 / (1024. * 1024.))
    }
}
#[cfg(test)]
mod tests {
    use super::{parent_directory, remote_child};
    #[test]
    fn names_stay_in_directory() {
        for n in ["", ".", "..", "../x", "a/b", "a\\b", "a\nx"] {
            assert!(remote_child("/home", n).is_none());
        }
        assert_eq!(remote_child("/", "中文.txt").as_deref(), Some("/中文.txt"));
    }
    #[test]
    fn parent_preserves_root() {
        assert_eq!(parent_directory("/"), "/");
        assert_eq!(parent_directory("/home/user/"), "/home");
    }
}
