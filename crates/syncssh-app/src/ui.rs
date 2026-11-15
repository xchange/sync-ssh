use crate::{
    backup_settings::{BackupEvent, BackupPane, BackupProvider, CloudConfig},
    settings::{
        Effects, Preferences, SettingsAction, SettingsContext, SettingsEvent, SettingsPane,
    },
    sftp_pane::SftpPane,
    terminal_pane::{PaneExited, TerminalPane},
    tunnel_pane::{SaveTunnelRules, TunnelPane},
};
use anyhow::{bail, ensure, Context as _, Result};
use gpui::{prelude::*, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    radio::Radio,
    ActiveTheme, Disableable, IconName, Sizable,
};
use std::{
    collections::BTreeMap,
    io::Read,
    path::PathBuf,
    time::{Duration, Instant},
};
use syncssh_core::{Credential, S3Provider, ServerProfile, VaultData, VaultStore};
use syncssh_session::{
    connect_ssh, local_shell, with_zmodem, HostKey, KnownHosts, SessionError, SessionMonitor,
    SessionStatus, SftpHandle, SshAuth, SshConfig, TerminalIo, TunnelHandle,
};
use zeroize::Zeroizing;

mod workspace_layout;
use gpui_component::resizable::{resizable_panel, ResizablePanelGroup, ResizableState};
use workspace_layout::{LayoutNode, SplitDirection, Workspace, MAX_PANES};

actions!(
    syncssh,
    [
        Quit,
        NewTab,
        SplitHorizontal,
        SplitVertical,
        ClosePane,
        OpenSettings,
        ToggleSidebar,
        LockVault,
        ModalNext,
        ModalPrevious,
        ModalClose
    ]
);
const BG: u32 = 0x0a101c;
const PANEL: u32 = 0x101927;
const LINE: u32 = 0x223047;
const MUTED: u32 = 0x8291aa;
const MINT: u32 = 0x6ce5c3;

struct Tab {
    id: u64,
    title: String,
    remote: bool,
    profile_id: Option<String>,
    connection_identity: Option<Vec<(String, u16, String)>>,
    pane: Entity<TerminalPane>,
    _pane_subscription: Subscription,
    sftp: Option<SftpHandle>,
    files: Option<Entity<SftpPane>>,
    files_visible: bool,
    tunnel_handle: Option<TunnelHandle>,
    tunnel_pane: Option<Entity<TunnelPane>>,
    _tunnel_subscription: Option<Subscription>,
    tunnels_visible: bool,
    monitor: Option<SessionMonitor>,
    session_state: Option<SessionStatus>,
    reconnecting: bool,
    generation: u64,
    retry_count: u32,
    retry_at: Option<Instant>,
    reconnect_notice: Option<String>,
    auto_reconnect: bool,
}
#[derive(Clone, PartialEq)]
enum Panel {
    Settings,
    Unlock,
    Server,
    ChangePassword,
    Trust,
    Delete,
}
struct Field {
    key: &'static str,
    label: &'static str,
    state: Entity<InputState>,
}
fn automatic_backup_targets(data: &VaultData) -> Vec<CloudConfig> {
    let mut targets = Vec::new();
    for provider in S3Provider::ALL {
        if data.object_storage_automatic(provider) {
            if let Some(config) = data.object_storage(provider) {
                targets.push(CloudConfig::S3(config.clone()));
            }
        }
    }
    if data.webdav_backup_on_save {
        if let Some(config) = &data.webdav {
            targets.push(CloudConfig::WebDav(config.clone()));
        }
    }
    targets
}

// Report each destination independently; a partial failure must never skip another backup.
fn upload_results(
    targets: &[CloudConfig],
    mut upload: impl FnMut(&CloudConfig) -> Result<String>,
) -> Vec<String> {
    targets
        .iter()
        .map(|target| {
            let provider = target.provider().label();
            match upload(target) {
                Ok(key) => format!("{provider} 备份成功 · {key}"),
                Err(_) => format!("{provider} 备份失败，请手动重试"),
            }
        })
        .collect()
}

struct PendingRestore {
    bytes: Vec<u8>,
    password: Zeroizing<String>,
    count: usize,
    local_target: Option<PathBuf>,
}
enum Outcome {
    Unlocked(VaultData, Zeroizing<String>),
    Saved(VaultData, String),
    BackupSaved(VaultData, BackupProvider, String, Option<String>),
    PasswordChanged(VaultData, Zeroizing<String>),
    Connected(TerminalIo, String, Option<String>),
    Challenge(HostKey, SshConfig, String, String),
    RestoreReady(PendingRestore),
    Restored(VaultData, Zeroizing<String>, Option<VaultStore>),
    LocalMoved(VaultStore),
}

pub struct SyncSSH {
    directory: PathBuf,
    vault: VaultStore,
    master: Option<Zeroizing<String>>,
    data: VaultData,
    tabs: Vec<Tab>,
    workspaces: Vec<Workspace>,
    split_states: BTreeMap<u64, Entity<ResizableState>>,
    pending_split: Option<(u64, SplitDirection)>,
    next_tab_id: u64,
    _connection_tick: Task<()>,
    active: usize,
    panel: Option<Panel>,
    fields: Vec<Field>,
    search: Entity<InputState>,
    modal_focus: FocusHandle,
    edit_id: Option<String>,
    auth: usize,
    clear_passphrase: bool,
    clear_certificate: bool,
    busy: bool,
    status: String,
    status_warning: bool,
    error: Option<String>,
    preferences: Preferences,
    preferences_from_disk: bool,
    settings_pane: Option<Entity<SettingsPane>>,
    _settings_subscription: Option<Subscription>,
    backup_pane: Option<Entity<BackupPane>>,
    _backup_subscription: Option<Subscription>,
    trust: Option<(HostKey, SshConfig, String, String)>,
    restore: Option<PendingRestore>,
}

impl SyncSSH {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let directory = std::env::args()
            .skip(1)
            .collect::<Vec<_>>()
            .windows(2)
            .find(|pair| pair[0] == "--data-dir")
            .map(|pair| PathBuf::from(&pair[1]))
            .unwrap_or_else(|| {
                directories::ProjectDirs::from("dev", "SyncSSH", "SyncSSH")
                    .map(|p| p.data_local_dir().to_owned())
                    .unwrap_or_else(|| PathBuf::from(".sync-ssh"))
            });
        let preferences_from_disk = directory.join("preferences.json").exists();
        let (preferences, settings_error) = match Preferences::load(&directory) {
            Ok(value) => (value, None),
            Err(error) => (
                Preferences::default(),
                Some(format!("个性化设置未能载入，已使用默认值：{error}")),
            ),
        };
        let (vault, location_error) = match crate::local_vault::load(&directory) {
            Ok(vault) => (vault, None),
            Err(error) => (
                VaultStore::open_existing(directory.join("credentials.vault")),
                Some(format!(
                    "凭据库位置未能载入：{error}。请在备份设置中重新选择文件。"
                )),
            ),
        };
        let settings_error = settings_error.or(location_error);
        crate::appearance::apply(preferences.theme, window, cx);
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索名称、主机或分组"));
        cx.observe(&search, |_, _, cx| cx.notify()).detach();
        let connection_tick = cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(500)).await;
            if this
                .update_in(cx, |this, window, cx| this.poll_connections(window, cx))
                .is_err()
            {
                break;
            }
        });
        let mut app = Self {
            vault,
            directory,
            master: None,
            data: VaultData::default(),
            tabs: Vec::new(),
            workspaces: Vec::new(),
            split_states: BTreeMap::new(),
            pending_split: None,
            next_tab_id: 1,
            _connection_tick: connection_tick,
            active: 0,
            panel: None,
            fields: Vec::new(),
            search,
            edit_id: None,
            modal_focus: cx.focus_handle(),
            auth: 0,
            clear_passphrase: false,
            clear_certificate: false,
            busy: false,
            status: "本地终端 · 凭据库已锁定".into(),
            status_warning: false,
            error: None,
            preferences,
            preferences_from_disk,
            settings_pane: None,
            _settings_subscription: None,
            backup_pane: None,
            _backup_subscription: None,
            trust: None,
            restore: None,
        };
        app.open_panel(Panel::Unlock, window, cx);
        app.new_local(window, cx);
        app.error = settings_error;
        app
    }

    fn add_field(
        &mut self,
        key: &'static str,
        label: &'static str,
        value: &str,
        masked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = value.to_owned();
        let state = cx.new(|cx| {
            let mut state = if matches!(key, "master" | "confirm") {
                crate::ascii_password::state(window, cx)
                    .placeholder("仅限英文、数字、半角符号和空格")
            } else {
                InputState::new(window, cx).masked(masked)
            };
            state.set_value(value, window, cx);
            state
        });
        self.fields.push(Field { key, label, state });
    }
    fn value(&self, key: &str, cx: &App) -> String {
        self.fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.state.read(cx).value().to_string())
            .unwrap_or_default()
    }
    fn optional(&self, key: &str, cx: &App) -> Option<String> {
        let value = self.value(key, cx);
        (!value.is_empty()).then_some(value)
    }
    fn close_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.panel == Some(Panel::Trust) && self.trust.is_some() {
            self.pending_split = None;
        }
        self.panel = None;
        self.settings_pane = None;
        self._settings_subscription = None;
        for tab in &self.tabs {
            tab.pane
                .update(cx, |pane, cx| pane.set_input_enabled(true, cx));
        }
        self.fields.clear();
        self.backup_pane = None;
        self._backup_subscription = None;
        self.error = None;
        self.trust = None;
        self.restore = None;
        self.focus_terminal(window, cx);
        cx.notify();
    }
    fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get(self.active) {
            window.focus(&tab.pane.focus_handle(cx));
        }
    }
    fn select_pane(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.panel.is_some() {
            return;
        }
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.active = index;
            if let Some(workspace) = self.workspaces.iter_mut().find(|w| w.root.contains(id)) {
                workspace.focused = id;
            }
            cx.notify();
        }
    }
    fn active_workspace(&self) -> Option<&Workspace> {
        let id = self.tabs.get(self.active)?.id;
        self.workspaces
            .iter()
            .find(|workspace| workspace.root.contains(id))
    }
    fn split_active(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.panel.is_some() {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if self
            .active_workspace()
            .is_some_and(|w| w.root.panes().len() >= MAX_PANES)
        {
            self.error = Some(format!("每个工作区最多 {MAX_PANES} 个窗格，请新建标签。"));
            cx.notify();
            return;
        }
        let id = tab.id;
        let profile_id = tab.profile_id.clone();
        if tab.remote
            && (self.master.is_none()
                || !self
                    .data
                    .servers
                    .iter()
                    .any(|s| Some(&s.id) == profile_id.as_ref()))
        {
            self.error = Some("服务器配置不可用，无法创建新的分屏连接。".into());
            cx.notify();
            return;
        }
        self.pending_split = Some((id, direction));
        if let Some(profile_id) = profile_id {
            self.connect(&profile_id, window, cx);
        } else {
            self.spawn_local(window, cx);
        }
        if !self.busy {
            self.pending_split = None;
        }
    }
    fn split_profile(
        &mut self,
        profile_id: &str,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.panel.is_some() || self.master.is_none() {
            return;
        }
        let Some(target) = self.tabs.get(self.active).map(|tab| tab.id) else {
            self.connect(profile_id, window, cx);
            return;
        };
        if self
            .active_workspace()
            .is_some_and(|w| w.root.panes().len() >= MAX_PANES)
        {
            self.error = Some(format!("每个工作区最多 {MAX_PANES} 个窗格，请新建标签。"));
            cx.notify();
            return;
        }
        self.pending_split = Some((target, direction));
        self.connect(profile_id, window, cx);
        if !self.busy {
            self.pending_split = None;
        }
    }
    fn split_local(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.panel.is_some() {
            return;
        }
        let Some(target) = self.tabs.get(self.active).map(|tab| tab.id) else {
            self.new_local(window, cx);
            return;
        };
        if self
            .active_workspace()
            .is_some_and(|w| w.root.panes().len() >= MAX_PANES)
        {
            self.error = Some(format!("每个工作区最多 {MAX_PANES} 个窗格，请新建标签。"));
            cx.notify();
            return;
        }
        self.pending_split = Some((target, direction));
        self.spawn_local(window, cx);
    }
    fn prune_workspaces(&mut self) {
        let active_id = self.tabs.get(self.active).map(|tab| tab.id);
        let ids = self.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>();
        self.workspaces = std::mem::take(&mut self.workspaces)
            .into_iter()
            .filter_map(|mut workspace| {
                workspace.root = workspace.root.retain(&|id| ids.contains(&id))?;
                if !workspace.root.contains(workspace.focused) {
                    workspace.focused = workspace.root.panes()[0];
                }
                Some(workspace)
            })
            .collect();
        let splits = self
            .workspaces
            .iter()
            .flat_map(|w| w.root.split_ids())
            .collect::<Vec<_>>();
        self.split_states.retain(|id, _| splits.contains(id));
        self.active = active_id
            .and_then(|id| self.tabs.iter().position(|t| t.id == id))
            .unwrap_or_else(|| self.active.min(self.tabs.len().saturating_sub(1)));
    }
    fn close_panes(&mut self, ids: &[u64], window: &mut Window, cx: &mut Context<Self>) {
        if self.panel.is_some() {
            return;
        }
        let active_id = self.tabs.get(self.active).map(|tab| tab.id);
        let workspace_id = self.active_workspace().map(|workspace| workspace.id);
        for tab in self.tabs.iter().filter(|tab| ids.contains(&tab.id)) {
            if let Some(files) = &tab.files {
                files.update(cx, |pane, cx| pane.cancel(cx));
            }
        }
        self.tabs.retain(|tab| !ids.contains(&tab.id));
        self.prune_workspaces();
        let focus = active_id.filter(|id| !ids.contains(id)).or_else(|| {
            self.workspaces
                .iter()
                .find(|w| Some(w.id) == workspace_id)
                .map(|w| w.focused)
        });
        if let Some(id) = focus {
            self.select_pane(id, cx);
        }
        self.focus_terminal(window, cx);
        cx.notify();
    }
    fn toggle_sftp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel.is_some() {
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Some(handle) = tab.sftp.clone() else {
            return;
        };
        if tab.files.is_none() {
            let pane = cx.new(|cx| SftpPane::new(handle, window, cx));
            pane.update(cx, |pane, cx| pane.refresh(window, cx));
            tab.files = Some(pane);
        }
        tab.files_visible = !tab.files_visible;
        if tab.files_visible {
            tab.tunnels_visible = false;
            if let Some(pane) = &tab.files {
                window.focus(&pane.focus_handle(cx));
            }
        } else {
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }
    fn toggle_tunnels(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel.is_some() {
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
            if tab.tunnel_pane.is_none() {
                return;
            }
            tab.tunnels_visible = !tab.tunnels_visible;
            if tab.tunnels_visible {
                tab.files_visible = false;
            }
        }
        self.focus_terminal(window, cx);
        cx.notify();
    }
    fn install_connection(
        &mut self,
        replace: Option<u64>,
        io: TerminalIo,
        title: String,
        profile_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let split = if replace.is_none() {
            self.pending_split.take()
        } else {
            None
        };
        if let Some((target, _)) = split {
            ensure!(
                self.workspaces
                    .iter()
                    .any(|w| w.root.contains(target) && w.root.panes().len() < MAX_PANES),
                "分屏目标已关闭，新的连接已取消。"
            );
        }
        let previous_focus = window.focused(cx);
        let preserve_focus = replace
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .is_some_and(|tab| {
                !tab.pane.read(cx).contains_focused(window, cx)
                    && !tab
                        .tunnel_pane
                        .as_ref()
                        .is_some_and(|pane| pane.focus_handle(cx).contains_focused(window, cx))
            });
        let remote = profile_id.is_some();
        let connection_identity = profile_id
            .as_ref()
            .and_then(|id| self.data.servers.iter().find(|s| &s.id == id))
            .map(|s| vec![(s.host.clone(), s.port, s.username.clone())]);
        let options = profile_id
            .as_ref()
            .and_then(|id| self.data.servers.iter().find(|s| &s.id == id))
            .map(|s| s.connection.clone())
            .unwrap_or_default();
        let sftp = io.sftp.clone();
        let tunnel_handle = io.tunnels.clone();
        let monitor = io.status.clone();
        let (io, zmodem, events) = with_zmodem(io)?;
        let pane = cx.new(|cx| TerminalPane::new(io, zmodem, events, window, cx));
        let pane_subscription = cx.subscribe_in(&pane, window, |this, _, _: &PaneExited, w, cx| {
            this.poll_connections(w, cx);
            cx.notify();
        });
        pane.update(cx, |pane, cx| {
            pane.apply_preferences(&self.preferences, cx);
            pane.set_input_enabled(self.panel.is_none(), cx);
        });
        let mut tunnel_subscription = None;
        let tunnel_pane = tunnel_handle.as_ref().map(|handle| {
            let pane =
                cx.new(|cx| TunnelPane::new(handle.clone(), options.tunnels.clone(), window, cx));
            if let Some(profile_id) = profile_id.clone() {
                tunnel_subscription = Some(cx.subscribe_in(
                    &pane,
                    window,
                    move |this, _, event: &SaveTunnelRules, w, cx| {
                        if this.busy || this.master.is_none() {
                            this.error = Some("请等待当前保存完成后再保存隧道规则。".into());
                            cx.notify();
                            return;
                        }
                        let mut data = this.data.clone();
                        if let Some(profile) = data.servers.iter_mut().find(|s| s.id == profile_id)
                        {
                            profile.connection.tunnels = event.0.clone();
                            if let Err(error) = profile.validate() {
                                this.error = Some(error.to_string());
                                cx.notify();
                                return;
                            }
                            this.save_data(data, w, cx);
                        }
                    },
                ));
            }
            pane
        });
        if let Some(tunnels) = &tunnel_pane {
            tunnels.update(cx, |pane, cx| pane.start_saved_rules(window, cx));
        }
        let mut tab = Tab {
            id: replace.unwrap_or(self.next_tab_id),
            title,
            remote,
            profile_id,
            connection_identity,
            pane,
            _pane_subscription: pane_subscription,
            sftp,
            files: None,
            files_visible: false,
            tunnel_handle,
            tunnel_pane,
            _tunnel_subscription: tunnel_subscription,
            tunnels_visible: false,
            monitor,
            session_state: remote.then_some(SessionStatus::Connected),
            reconnecting: false,
            generation: 0,
            retry_count: 0,
            retry_at: None,
            reconnect_notice: None,
            auto_reconnect: options.auto_reconnect,
        };
        if let Some(id) = replace {
            let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
                return Ok(());
            };
            tab.generation = self.tabs[index].generation.wrapping_add(1);
            tab.tunnels_visible = self.tabs[index].tunnels_visible;
            tab.files_visible = self.tabs[index].files_visible;
            if let (Some(files), Some(handle)) = (
                self.tabs[index]
                    .files
                    .clone()
                    .filter(|_| self.tabs[index].connection_identity == tab.connection_identity),
                tab.sftp.clone(),
            ) {
                files.update(cx, |files, cx| files.rebind_handle(handle, window, cx));
                tab.files = Some(files);
            } else if tab.files_visible {
                if let Some(handle) = tab.sftp.clone() {
                    let files = cx.new(|cx| SftpPane::new(handle, window, cx));
                    files.update(cx, |p, cx| p.refresh(window, cx));
                    tab.files = Some(files);
                }
            }
            self.tabs[index] = tab;
        } else {
            let id = tab.id;
            if let Some((target, direction)) = split {
                let workspace = self
                    .workspaces
                    .iter_mut()
                    .find(|w| w.root.contains(target))
                    .expect("validated split target");
                ensure!(
                    workspace.root.split(target, id, direction),
                    "分屏布局已变化，请重试。"
                );
                workspace.focused = id;
                self.split_states
                    .insert(id, cx.new(|_| ResizableState::default()));
            } else {
                self.workspaces.push(Workspace::new(id));
            }
            self.next_tab_id = self.next_tab_id.wrapping_add(1);
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
        }
        self.status = if remote {
            "SSH 会话已连接 · 主机身份已验证"
        } else {
            "本地会话已就绪"
        }
        .into();
        if preserve_focus {
            if let Some(focus) = previous_focus {
                window.focus(&focus);
            }
        } else if self.panel.is_none() {
            self.focus_terminal(window, cx);
        } else if let Some(field) = self.fields.first() {
            field.state.update(cx, |state, cx| state.focus(window, cx));
        } else {
            window.focus(&self.modal_focus);
        }
        Ok(())
    }
    fn poll_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        let mut reconnect = Vec::new();
        for tab in &mut self.tabs {
            if let Some(monitor) = &tab.monitor {
                let state = monitor.state();
                if state != SessionStatus::Connected {
                    tab.monitor = None;
                    tab.session_state = Some(state.clone());
                    if let Some(files) = &tab.files {
                        files.update(cx, |p, cx| p.cancel(cx));
                    }
                    if matches!(state, SessionStatus::Disconnected { .. }) && tab.auto_reconnect {
                        tab.retry_at = Some(Instant::now() + reconnect_delay(0));
                    }
                    changed = true;
                }
            }
            if tab
                .retry_at
                .is_some_and(|deadline| deadline <= Instant::now())
                && !tab.reconnecting
                && tab.auto_reconnect
                && !self.busy
                && self.panel.is_none()
            {
                reconnect.push(tab.id);
            }
        }
        for id in reconnect {
            self.reconnect_tab(id, true, window, cx);
        }
        if changed {
            cx.notify();
        }
    }
    fn reconnect_tab(
        &mut self,
        id: u64,
        automatic: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.master.is_none() || self.panel.is_some() {
            return;
        }
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        if self.tabs[index].reconnecting {
            return;
        }
        if automatic && (!self.tabs[index].auto_reconnect || self.tabs[index].retry_count >= 5) {
            self.tabs[index].retry_at = None;
            return;
        }
        let profile_id = self.tabs[index].profile_id.clone();
        let Some(profile) = profile_id
            .as_ref()
            .and_then(|id| self.data.servers.iter().find(|s| &s.id == id))
            .cloned()
        else {
            self.tabs[index].retry_at = None;
            self.tabs[index].reconnect_notice = Some("服务器配置已移除，无法重新连接。".into());
            cx.notify();
            return;
        };
        let config = match ssh_config(&profile) {
            Ok(config) => config,
            Err(error) => {
                self.tabs[index].retry_at = None;
                self.tabs[index].reconnect_notice = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let original_config = config.clone();
        let tab = &mut self.tabs[index];
        tab.generation = tab.generation.wrapping_add(1);
        let generation = tab.generation;
        if !automatic {
            tab.retry_count = 0;
        }
        tab.retry_count += 1;
        tab.retry_at = None;
        tab.reconnecting = true;
        tab.reconnect_notice = Some(format!("正在重新连接（第 {} 次）…", tab.retry_count));
        let path = self.directory.join("known_hosts.json");
        let (tx, rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let result = KnownHosts::load(path)
                .and_then(|known| connect_ssh(config, known, 100, 30))
                .map_err(|e| {
                    let retryable = retryable_ssh_error(&e);
                    (e.to_string(), retryable)
                });
            let _ = tx.send(result);
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(result) = rx.recv_async().await {
                let _ = this.update_in(cx, |this, window, cx| {
                    let Some(index) = this
                        .tabs
                        .iter()
                        .position(|t| t.id == id && t.generation == generation)
                    else {
                        return;
                    };
                    this.tabs[index].reconnecting = false;
                    match result {
                        Ok(io) => {
                            if this
                                .data
                                .servers
                                .iter()
                                .find(|p| p.id == profile.id)
                                .and_then(|p| ssh_config(p).ok())
                                .as_ref()
                                != Some(&original_config)
                            {
                                this.tabs[index].reconnect_notice =
                                    Some("连接配置在重连期间已变化，请重新连接。".into());
                                cx.notify();
                                return;
                            }
                            let title = profile.name.clone();
                            if let Err(error) =
                                this.install_connection(Some(id), io, title, profile_id, window, cx)
                            {
                                this.tabs[index].reconnect_notice = Some(error.to_string());
                            }
                        }
                        Err((error, retryable)) => {
                            let tab = &mut this.tabs[index];
                            tab.reconnect_notice = Some(format!("重连失败：{error}"));
                            if retryable && tab.auto_reconnect && tab.retry_count < 5 {
                                tab.retry_at =
                                    Some(Instant::now() + reconnect_delay(tab.retry_count));
                            }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }
    fn cancel_reconnect(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tab.generation = tab.generation.wrapping_add(1);
            tab.reconnecting = false;
            tab.retry_at = None;
            tab.reconnect_notice = Some("已停止重连，可手动再试。".into());
        }
        cx.notify();
    }
    fn toggle_auto_reconnect(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.master.is_none() {
            return;
        }
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        tab.auto_reconnect = !tab.auto_reconnect;
        if !tab.auto_reconnect {
            tab.retry_at = None;
        } else if matches!(tab.session_state, Some(SessionStatus::Disconnected { .. }))
            && !tab.reconnecting
        {
            tab.retry_count = 0;
            tab.retry_at = Some(Instant::now() + reconnect_delay(0));
        }
        let mut data = self.data.clone();
        if let Some(profile) = data
            .servers
            .iter_mut()
            .find(|s| Some(&s.id) == tab.profile_id.as_ref())
        {
            profile.connection.auto_reconnect = tab.auto_reconnect;
            self.save_data(data, window, cx);
        }
        cx.notify();
    }
    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut preferences = self.preferences.clone();
        preferences.sidebar_collapsed = !preferences.sidebar_collapsed;
        if let Err(error) = preferences.save(&self.directory) {
            self.error = Some(error.to_string());
        } else {
            self.preferences = preferences;
            self.preferences_from_disk = true;
        }
        self.focus_terminal(window, cx);
        cx.notify();
    }

    fn open_backup_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_panel(Panel::Settings, window, cx);
        if let Some(settings) = &self.settings_pane {
            settings.update(cx, |pane, cx| pane.show_backup(window, cx));
        }
    }

    fn backup_event(&mut self, event: &BackupEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        match event {
            BackupEvent::ChooseLocalSave => self.choose_local_save(window, cx),
            BackupEvent::ReloadLocal => self.lock(window, cx),
            BackupEvent::OpenLocal { path, password } => {
                self.preview_local_open(path.clone(), password.clone(), window, cx)
            }
            BackupEvent::Save {
                config,
                automatic,
                upload,
            } => self.backup(config.clone(), *automatic, *upload, window, cx),
            BackupEvent::Restore {
                config,
                key,
                password,
            } => self.preview_restore(config.clone(), key.clone(), password.clone(), window, cx),
            BackupEvent::ConfirmRestore => {
                let Some(pending) = self.restore.take() else {
                    if let Some(backup) = &self.backup_pane {
                        backup.update(cx, |pane, cx| {
                            pane.fail("恢复预览已失效，请重新下载验证。".into(), cx)
                        });
                    }
                    return;
                };
                let store = self.vault.clone();
                let directory = self.directory.clone();
                self.job(
                    "正在恢复加密凭据库…",
                    move || {
                        if let Some(target) = pending.local_target {
                            let (store, data) = crate::local_vault::open_verified(
                                &target,
                                &pending.bytes,
                                &pending.password,
                                &directory,
                            )?;
                            Ok(Outcome::Restored(data, pending.password, Some(store)))
                        } else {
                            let data = store.import_encrypted(&pending.password, &pending.bytes)?;
                            Ok(Outcome::Restored(data, pending.password, None))
                        }
                    },
                    window,
                    cx,
                );
            }
            BackupEvent::CancelRestore => {
                self.restore = None;
            }
        }
    }

    fn settings_action(
        &mut self,
        action: SettingsAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_panel(window, cx);
        match action {
            SettingsAction::ChangePassword => self.open_panel(Panel::ChangePassword, window, cx),
            SettingsAction::ToggleVault => {
                if self.master.is_some() {
                    self.lock(window, cx)
                } else {
                    self.open_panel(Panel::Unlock, window, cx)
                }
            }
            SettingsAction::Zmodem => {
                if let Some(tab) = self.tabs.get(self.active) {
                    tab.pane.update(cx, |pane, cx| pane.toggle_zmodem_help(cx));
                }
            }
            SettingsAction::Sftp => self.toggle_sftp(window, cx),
            SettingsAction::Tunnels => self.toggle_tunnels(window, cx),
            SettingsAction::AutoReconnect => {
                if let Some(id) = self.tabs.get(self.active).map(|tab| tab.id) {
                    self.toggle_auto_reconnect(id, window, cx);
                }
            }
        }
    }

    fn choose_local_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(password) = self.master.clone() else {
            return;
        };
        self.busy = true;
        if let Some(settings) = &self.settings_pane {
            settings.update(cx, |pane, cx| pane.set_busy(true, cx));
        }
        if let Some(backup) = &self.backup_pane {
            backup.update(cx, |pane, cx| pane.set_busy(true, cx));
        }
        let parent = self.vault.path().parent().unwrap_or(&self.directory);
        let prompt = cx.prompt_for_new_path(parent, Some("credentials.vault"));
        cx.spawn_in(window, async move |this, cx| {
            let result = prompt.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                if let Some(settings) = &this.settings_pane {
                    settings.update(cx, |pane, cx| pane.set_busy(false, cx));
                }
                if let Some(backup) = &this.backup_pane {
                    backup.update(cx, |pane, cx| pane.set_busy(false, cx));
                }
                match result {
                    Ok(Ok(Some(target))) => {
                        if target == this.vault.path() {
                            if let Some(backup) = &this.backup_pane {
                                backup.update(cx, |pane, cx| {
                                    pane.finish(
                                        BackupProvider::Local,
                                        "已在使用此保存位置".into(),
                                        None,
                                        window,
                                        cx,
                                    )
                                });
                            }
                            return;
                        }
                        let store = this.vault.clone();
                        let directory = this.directory.clone();
                        this.job(
                            "正在保存并切换本地凭据库位置…",
                            move || {
                                Ok(Outcome::LocalMoved(crate::local_vault::relocate(
                                    &store, &password, &target, &directory,
                                )?))
                            },
                            window,
                            cx,
                        );
                    }
                    Ok(Ok(None)) => {}
                    _ => {
                        if let Some(backup) = &this.backup_pane {
                            backup.update(cx, |pane, cx| {
                                pane.fail("无法打开保存位置选择窗口".into(), cx)
                            });
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn preview_local_open(
        &mut self,
        path: PathBuf,
        entered: Option<Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        let password = entered.or_else(|| self.master.clone()).unwrap_or_default();
        if password.is_empty() {
            if let Some(backup) = &self.backup_pane {
                backup.update(cx, |pane, cx| {
                    pane.fail("请输入所选凭据库的主密码".into(), cx)
                });
            }
            return;
        }
        self.job(
            "正在验证本地凭据库…",
            move || {
                let bytes = VaultStore::open_existing(&path).encrypted_blob()?;
                let data = syncssh_core::vault::decode(&password, &bytes)?;
                Ok(Outcome::RestoreReady(PendingRestore {
                    bytes,
                    password,
                    count: data.servers.len(),
                    local_target: Some(path),
                }))
            },
            window,
            cx,
        );
    }

    fn open_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let last_status = self.error.clone().unwrap_or_else(|| self.status.clone());
        self.settings_pane = None;
        self._settings_subscription = None;
        self.backup_pane = None;
        self._backup_subscription = None;
        self.restore = None;
        self.fields.clear();
        self.error = None;
        match &panel {
            Panel::Settings => {
                let tab = self.tabs.get(self.active);
                let context = SettingsContext {
                    unlocked: self.master.is_some(),
                    has_terminal: tab.is_some(),
                    has_sftp: tab.is_some_and(|tab| tab.sftp.is_some()),
                    has_tunnels: tab.is_some_and(|tab| tab.tunnel_handle.is_some()),
                    auto_reconnect: tab.filter(|tab| tab.remote).map(|tab| tab.auto_reconnect),
                    status: last_status,
                };
                let backup = cx.new(|cx| {
                    BackupPane::new(
                        &self.data,
                        self.master.is_some(),
                        self.vault.path().to_owned(),
                        window,
                        cx,
                    )
                });
                self._backup_subscription = Some(cx.subscribe_in(
                    &backup,
                    window,
                    |this, _, event: &BackupEvent, window, cx| this.backup_event(event, window, cx),
                ));
                self.backup_pane = Some(backup.clone());
                let pane = cx.new(|cx| {
                    SettingsPane::new(self.preferences.clone(), context, backup, window, cx)
                });
                self._settings_subscription = Some(cx.subscribe_in(
                    &pane,
                    window,
                    |this, pane, event: &SettingsEvent, window, cx| match event {
                        SettingsEvent::Close => this.close_panel(window, cx),
                        SettingsEvent::Action(action) => this.settings_action(*action, window, cx),
                        SettingsEvent::Apply(preferences) => {
                            if let Err(error) = preferences.save(&this.directory) {
                                pane.update(cx, |pane, cx| pane.set_error(error.to_string(), cx));
                                return;
                            }
                            this.preferences = preferences.clone();
                            this.preferences_from_disk = true;
                            crate::appearance::apply(preferences.theme, window, cx);
                            for tab in &this.tabs {
                                tab.pane
                                    .update(cx, |pane, cx| pane.apply_preferences(preferences, cx));
                            }
                            this.status = "个性化设置已保存并应用".into();
                            this.close_panel(window, cx);
                            cx.refresh_windows();
                        }
                    },
                ));
                self.settings_pane = Some(pane);
            }
            Panel::Unlock => {
                self.add_field("master", "主密码", "", true, window, cx);
                if self.vault.can_create() {
                    self.add_field(
                        "confirm",
                        "再次输入主密码（至少 12 个字符）",
                        "",
                        true,
                        window,
                        cx,
                    );
                }
            }
            Panel::Server => {
                let existing = self
                    .edit_id
                    .as_ref()
                    .and_then(|id| self.data.servers.iter().find(|s| &s.id == id))
                    .cloned();
                self.add_field(
                    "name",
                    "名称",
                    existing.as_ref().map(|s| s.name.as_str()).unwrap_or(""),
                    false,
                    window,
                    cx,
                );
                self.add_field(
                    "host",
                    "主机地址",
                    existing.as_ref().map(|s| s.host.as_str()).unwrap_or(""),
                    false,
                    window,
                    cx,
                );
                self.add_field(
                    "port",
                    "端口",
                    &existing.as_ref().map(|s| s.port).unwrap_or(22).to_string(),
                    false,
                    window,
                    cx,
                );
                self.add_field(
                    "user",
                    "用户名",
                    existing
                        .as_ref()
                        .map(|s| s.username.as_str())
                        .unwrap_or("root"),
                    false,
                    window,
                    cx,
                );
                self.add_field(
                    "group",
                    "分组",
                    existing
                        .as_ref()
                        .map(|s| s.group.as_str())
                        .unwrap_or("Personal"),
                    false,
                    window,
                    cx,
                );
                self.add_field(
                    "password",
                    "SSH 密码（编辑时留空保留原值）",
                    "",
                    true,
                    window,
                    cx,
                );
                self.add_field(
                    "keypath",
                    "私钥文件（编辑时留空保留）",
                    "",
                    false,
                    window,
                    cx,
                );
                self.add_field(
                    "passphrase",
                    "私钥口令（可选；留空保留）",
                    "",
                    true,
                    window,
                    cx,
                );
                self.add_field(
                    "certpath",
                    "OpenSSH 证书文件（可选；留空保留）",
                    "",
                    false,
                    window,
                    cx,
                );
            }
            Panel::ChangePassword => {
                self.add_field("master", "新主密码（至少 12 个字符）", "", true, window, cx);
                self.add_field("confirm", "再次输入新主密码", "", true, window, cx);
            }
            _ => {}
        }
        for tab in &self.tabs {
            tab.pane
                .update(cx, |pane, cx| pane.set_input_enabled(false, cx));
        }
        self.panel = Some(panel);
        if let Some(settings) = &self.settings_pane {
            settings.update(cx, |pane, cx| pane.focus_first(window, cx));
        } else if let Some(field) = self.fields.first() {
            field.state.update(cx, |state, cx| state.focus(window, cx));
        } else {
            window.focus(&self.modal_focus);
        }
        cx.notify();
    }

    fn job(
        &mut self,
        label: &str,
        work: impl FnOnce() -> Result<Outcome> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        if let Some(settings) = &self.settings_pane {
            settings.update(cx, |pane, cx| pane.set_busy(true, cx));
        }
        if let Some(backup) = &self.backup_pane {
            backup.update(cx, |pane, cx| pane.set_busy(true, cx));
        }
        self.error = None;
        self.status_warning = false;
        self.status = label.into();
        cx.notify();
        let (tx, rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(result) = rx.recv_async().await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.busy = false;
                    if let Some(settings) = &this.settings_pane {
                        settings.update(cx, |pane, cx| pane.set_busy(false, cx));
                    }
                    if let Some(backup) = &this.backup_pane {
                        backup.update(cx, |pane, cx| pane.set_busy(false, cx));
                    }
                    match result {
                        Ok(Outcome::Unlocked(data, password))
                        | Ok(Outcome::PasswordChanged(data, password)) => {
                            this.vault.require_existing();
                            this.master = Some(password);
                            if !this.preferences_from_disk {
                                this.preferences.effects = Effects {
                                    input: data.effects_enabled,
                                    mouse: data.effects_enabled,
                                };
                                if this.preferences.save(&this.directory).is_ok() {
                                    this.preferences_from_disk = true;
                                }
                            }
                            this.data = data;
                            this.apply_effects(cx);
                            this.status = "凭据库已解锁 · 所有本地凭据均加密保存".into();
                            this.close_panel(window, cx);
                            if let Err(error) =
                                crate::local_vault::remember(&this.directory, &this.vault)
                            {
                                this.status_warning = true;
                                this.status = format!("凭据库已解锁，但位置配置未能保存：{error}");
                            }
                        }
                        Ok(Outcome::Saved(data, message)) => {
                            this.data = data;
                            for tab in &this.tabs {
                                if let (Some(pane), Some(profile)) = (
                                    &tab.tunnel_pane,
                                    tab.profile_id.as_ref().and_then(|id| {
                                        this.data.servers.iter().find(|s| &s.id == id)
                                    }),
                                ) {
                                    pane.update(cx, |pane, cx| {
                                        pane.saved(&profile.connection.tunnels, cx)
                                    });
                                }
                            }
                            this.status_warning =
                                message.contains("备份失败") || message.contains("云备份未完成");
                            this.status = message;
                            this.close_panel(window, cx);
                        }
                        Ok(Outcome::BackupSaved(data, provider, message, key)) => {
                            this.data = data;
                            this.status = message.clone();
                            if let Some(backup) = &this.backup_pane {
                                backup.update(cx, |pane, cx| {
                                    pane.finish(provider, message, key, window, cx)
                                });
                            }
                        }
                        Ok(Outcome::LocalMoved(store)) => {
                            this.vault = store;
                            this.status = format!(
                                "凭据库已保存到 {}，后续修改会持续更新此文件。",
                                this.vault.path().display()
                            );
                            if let Some(backup) = &this.backup_pane {
                                backup.update(cx, |pane, cx| {
                                    pane.set_local_path(this.vault.path().to_owned(), cx);
                                    pane.finish(
                                        BackupProvider::Local,
                                        this.status.clone(),
                                        None,
                                        window,
                                        cx,
                                    );
                                });
                            }
                        }
                        Ok(Outcome::Restored(data, password, store)) => {
                            this.cancel_remote_transfers(cx);
                            this.tabs.retain(|tab| !tab.remote);
                            this.active = 0;
                            this.prune_workspaces();
                            this.pending_split = None;
                            let switched = store.is_some();
                            if let Some(store) = store {
                                this.vault = store;
                            }
                            this.vault.require_existing();
                            this.master = Some(password);
                            if !this.preferences_from_disk {
                                this.preferences.effects = Effects {
                                    input: data.effects_enabled,
                                    mouse: data.effects_enabled,
                                };
                                if this.preferences.save(&this.directory).is_ok() {
                                    this.preferences_from_disk = true;
                                }
                            }
                            this.data = data;
                            this.apply_effects(cx);
                            this.status = if switched {
                                "已切换凭据库并解锁"
                            } else {
                                "备份已恢复，凭据库已解锁"
                            }
                            .into();
                            if let Some(backup) = &this.backup_pane {
                                backup.update(cx, |pane, cx| {
                                    pane.set_local_path(this.vault.path().to_owned(), cx);
                                    pane.restored(&this.data, window, cx);
                                });
                            }
                            if let Some(settings) = &this.settings_pane {
                                settings.update(cx, |pane, cx| pane.set_unlocked(cx));
                            }
                            if !switched {
                                if let Err(error) =
                                    crate::local_vault::remember(&this.directory, &this.vault)
                                {
                                    this.status_warning = true;
                                    this.status =
                                        format!("备份已恢复，但位置配置未能保存：{error}");
                                }
                            }
                        }
                        Ok(Outcome::Connected(io, title, profile_id)) => {
                            if let Err(error) =
                                this.install_connection(None, io, title, profile_id, window, cx)
                            {
                                this.status = "终端初始化失败".into();
                                this.error = Some(error.to_string());
                            }
                        }
                        Ok(Outcome::Challenge(key, config, title, profile_id)) => {
                            if this.pending_split.is_some_and(|(target, _)| {
                                !this
                                    .workspaces
                                    .iter()
                                    .any(|workspace| workspace.root.contains(target))
                            }) {
                                this.pending_split = None;
                                this.status = "分屏目标已关闭，连接已取消".into();
                                cx.notify();
                                return;
                            }
                            this.trust = Some((key, config, title, profile_id));
                            this.open_panel(Panel::Trust, window, cx);
                            this.status = "等待确认 SSH 主机指纹".into();
                        }
                        Ok(Outcome::RestoreReady(pending)) => {
                            if let Some(backup) = &this.backup_pane {
                                backup.update(cx, |pane, cx| {
                                    pane.show_restore_preview(pending.count, cx)
                                });
                            }
                            this.restore = Some(pending);
                        }
                        Err(error) => {
                            this.pending_split = None;
                            this.status = "操作未完成".into();
                            if let Some(backup) = &this.backup_pane {
                                backup.update(cx, |pane, cx| pane.fail(error.to_string(), cx));
                            } else {
                                this.error = Some(error.to_string());
                            }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }
    fn new_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.pending_split = None;
        self.spawn_local(window, cx);
    }
    fn spawn_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.job(
            "正在启动本地 Shell…",
            || {
                Ok(Outcome::Connected(
                    local_shell(100, 30)?,
                    "本地 Shell".into(),
                    None,
                ))
            },
            window,
            cx,
        );
    }
    fn unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let password = Zeroizing::new(self.value("master", cx));
        if self.vault.can_create()
            && (password.chars().count() < 12 || password.as_str() != self.value("confirm", cx))
        {
            self.error = Some("请设置至少 12 个字符的主密码，并确认两次输入一致。".into());
            cx.notify();
            return;
        }
        let path = self.vault.clone();
        self.job(
            "正在派生加密密钥并解锁…",
            move || {
                let store = path;
                let data = if store.can_create() {
                    let data = VaultData::default();
                    store.save(&password, &data)?;
                    data
                } else {
                    store.unlock(&password)?
                };
                Ok(Outcome::Unlocked(data, password))
            },
            window,
            cx,
        );
    }
    fn lock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.cancel_remote_transfers(cx);
        self.tabs.retain(|tab| !tab.remote);
        self.active = 0;
        self.prune_workspaces();
        self.pending_split = None;
        self.master = None;
        self.vault = if self.vault.can_create() {
            VaultStore::create_new(self.vault.path())
        } else {
            VaultStore::open_existing(self.vault.path())
        };
        self.data = VaultData::default();
        self.fields.clear();
        self.backup_pane = None;
        self._backup_subscription = None;
        self.trust = None;
        self.restore = None;
        self.status = "凭据库已锁定 · SSH 会话已关闭".into();
        self.open_panel(Panel::Unlock, window, cx);
    }
    fn cancel_remote_transfers(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let Some(files) = &tab.files {
                files.update(cx, |pane, cx| pane.cancel(cx));
            }
        }
    }
    fn apply_effects(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            tab.pane.update(cx, |pane, cx| {
                pane.set_effects(self.preferences.effects, cx)
            });
        }
    }
    fn save_data(&mut self, data: VaultData, window: &mut Window, cx: &mut Context<Self>) {
        let Some(password) = self.master.clone() else {
            return;
        };
        let path = self.vault.clone();
        self.job(
            "正在加密保存…",
            move || {
                let store = path;
                store.save(&password, &data)?;
                let mut results = vec!["已加密保存到本地".to_string()];
                let targets = automatic_backup_targets(&data);
                if !targets.is_empty() {
                    match (store.encrypted_blob(), tokio::runtime::Runtime::new()) {
                        (Ok(bytes), Ok(rt)) => {
                            results.extend(upload_results(&targets, |target| {
                                rt.block_on(target.upload(&bytes))
                            }));
                        }
                        _ => results.push("云备份未完成，请手动重试".into()),
                    }
                }
                let message = results.join("；");
                Ok(Outcome::Saved(data, message))
            },
            window,
            cx,
        );
    }
    fn edit_server(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.master.is_none() {
            self.open_panel(Panel::Unlock, window, cx);
            return;
        }
        self.edit_id = id;
        self.clear_passphrase = false;
        self.clear_certificate = false;
        self.auth = self
            .edit_id
            .as_ref()
            .and_then(|id| self.data.servers.iter().find(|s| &s.id == id))
            .map(|s| match &s.credential {
                Credential::Password { .. } => 0,
                Credential::PrivateKey { .. } => 1,
                Credential::Agent => 1,
            })
            .unwrap_or(0);
        self.open_panel(Panel::Server, window, cx);
    }
    fn save_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let result: Result<ServerProfile> = (|| {
            let old = self
                .edit_id
                .as_ref()
                .and_then(|id| self.data.servers.iter().find(|s| &s.id == id));
            let credential = match self.auth {
                0 => {
                    let mut password = self.value("password", cx);
                    if password.is_empty() {
                        if let Some(ServerProfile {
                            credential: Credential::Password { password: saved },
                            ..
                        }) = old
                        {
                            password = saved.clone();
                        }
                    }
                    Credential::Password { password }
                }
                1 => {
                    let old_key = old.and_then(|s| match &s.credential {
                        Credential::PrivateKey {
                            pem,
                            passphrase,
                            certificate,
                        } => Some((pem, passphrase, certificate)),
                        _ => None,
                    });
                    let keypath = self.value("keypath", cx);
                    let pem = if keypath.is_empty() {
                        old_key
                            .as_ref()
                            .map(|v| v.0.clone())
                            .context("请选择私钥文件")?
                    } else {
                        read_secret_file(&keypath)?
                    };
                    let passphrase = if self.clear_passphrase {
                        None
                    } else {
                        self.optional("passphrase", cx).or_else(|| {
                            if keypath.is_empty() {
                                old_key.as_ref().and_then(|v| v.1.clone())
                            } else {
                                None
                            }
                        })
                    };
                    let certificate = if self.clear_certificate {
                        None
                    } else {
                        match self.optional("certpath", cx) {
                            Some(path) => Some(read_secret_file(&path)?),
                            None => {
                                if keypath.is_empty() {
                                    old_key.and_then(|v| v.2.clone())
                                } else {
                                    None
                                }
                            }
                        }
                    };
                    Credential::PrivateKey {
                        pem,
                        passphrase,
                        certificate,
                    }
                }
                _ => bail!("请选择密码或私钥 / 证书认证"),
            };
            let profile = ServerProfile {
                id: self
                    .edit_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: self.value("name", cx).trim().into(),
                host: self.value("host", cx).trim().into(),
                port: self
                    .value("port", cx)
                    .parse()
                    .context("端口需要是 1–65535 的整数")?,
                username: self.value("user", cx).trim().into(),
                group: self.value("group", cx).trim().into(),
                credential,
                connection: self
                    .edit_id
                    .as_ref()
                    .and_then(|id| self.data.servers.iter().find(|s| &s.id == id))
                    .map(|s| s.connection.clone())
                    .unwrap_or_default(),
            };
            profile.validate()?;
            Ok(profile)
        })();
        match result {
            Ok(profile) => {
                let mut data = self.data.clone();
                if let Some(index) = data.servers.iter().position(|s| s.id == profile.id) {
                    data.servers[index] = profile;
                } else {
                    data.servers.push(profile);
                }
                if let Err(error) = data.validate() {
                    self.error = Some(error.to_string());
                    cx.notify();
                    return;
                }
                self.save_data(data, window, cx);
            }
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
            }
        }
    }
    fn connect(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(profile) = self.data.servers.iter().find(|s| s.id == id) else {
            return;
        };
        let config = match ssh_config(profile) {
            Ok(config) => config,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        self.connect_config(
            config,
            profile.name.clone(),
            profile.id.clone(),
            None,
            window,
            cx,
        );
    }
    fn connect_config(
        &mut self,
        config: SshConfig,
        title: String,
        profile_id: String,
        trust: Option<HostKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = self.directory.join("known_hosts.json");
        self.job(
            "正在验证主机并建立 SSH 会话…",
            move || {
                let mut known = KnownHosts::load(path)?;
                if let Some(key) = trust {
                    known.trust(key)?;
                }
                match connect_ssh(config.clone(), known, 100, 30) {
                    Ok(io) => Ok(Outcome::Connected(io, title, Some(profile_id))),
                    Err(SessionError::UnknownHostKey(key)) => {
                        Ok(Outcome::Challenge(key, config, title, profile_id))
                    }
                    Err(error) => Err(error.into()),
                }
            },
            window,
            cx,
        );
    }
    fn backup(
        &mut self,
        config: CloudConfig,
        automatic: bool,
        upload: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        let Some(password) = self.master.clone() else {
            if let Some(backup) = &self.backup_pane {
                backup.update(cx, |pane, cx| {
                    pane.fail("请先解锁或创建凭据库；恢复备份无需先解锁。".into(), cx)
                });
            }
            return;
        };
        let provider = config.provider();
        let label = provider.label();
        let mut data = self.data.clone();
        config.apply(&mut data, automatic);
        let path = self.vault.clone();
        self.job(
            &format!("正在加密保存 {label} 配置…"),
            move || {
                let store = path;
                store.save(&password, &data)?;
                if !upload {
                    return Ok(Outcome::BackupSaved(
                        data,
                        provider,
                        format!("{label} 配置已加密保存到本地"),
                        None,
                    ));
                }
                // Retain saved settings and report local success even when upload fails.
                let result = store.encrypted_blob().and_then(|bytes| {
                    tokio::runtime::Runtime::new()
                        .map_err(anyhow::Error::from)
                        .and_then(|rt| rt.block_on(config.upload(&bytes)))
                });
                match result {
                    Ok(key) => Ok(Outcome::BackupSaved(
                        data,
                        provider,
                        format!("{label} 加密备份成功 · {key}"),
                        Some(key),
                    )),
                    Err(error) => Ok(Outcome::BackupSaved(
                        data,
                        provider,
                        // Transport errors are sanitized; show only the outer message.
                        format!("{label} 配置已加密保存；上传失败：{error}"),
                        None,
                    )),
                }
            },
            window,
            cx,
        );
    }
    fn preview_restore(
        &mut self,
        config: CloudConfig,
        key: String,
        entered: Option<Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        let password = entered.or_else(|| self.master.clone()).unwrap_or_default();
        if password.is_empty() {
            if let Some(backup) = &self.backup_pane {
                backup.update(cx, |pane, cx| {
                    pane.fail("请输入备份创建时使用的主密码。".into(), cx)
                });
            }
            return;
        }
        let store = self.vault.clone();
        self.job(
            "正在下载并验证加密备份…",
            move || {
                store.observe()?;
                let bytes = tokio::runtime::Runtime::new()?.block_on(config.download(&key))?;
                let data = syncssh_core::vault::decode(&password, &bytes)?;
                Ok(Outcome::RestoreReady(PendingRestore {
                    bytes,
                    password,
                    count: data.servers.len(),
                    local_target: None,
                }))
            },
            window,
            cx,
        );
    }
    fn change_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let password = Zeroizing::new(self.value("master", cx));
        if password.chars().count() < 12 || password.as_str() != self.value("confirm", cx) {
            self.error = Some("新密码至少 12 个字符，且两次输入需要一致。".into());
            cx.notify();
            return;
        }
        let path = self.vault.clone();
        let Some(old) = self.master.clone() else {
            return;
        };
        self.job(
            "正在使用新主密码重新加密…",
            move || {
                let store = path;
                store.change_password(&old, &password)?;
                let data = store.unlock(&password)?;
                Ok(Outcome::PasswordChanged(data, password))
            },
            window,
            cx,
        );
    }

    fn choose_credential_file(
        &mut self,
        key: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.panel != Some(Panel::Server) || self.auth != 1 {
            return;
        }
        let Some(field) = self
            .fields
            .iter()
            .find(|field| field.key == key)
            .map(|field| field.state.clone())
        else {
            return;
        };
        let title = if key == "keypath" {
            "选择 SSH 私钥文件"
        } else {
            "选择 OpenSSH 用户证书文件"
        };
        self.busy = true;
        self.error = None;
        self.status = title.into();
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            // Private keys often have no extension (for example id_ed25519).
            prompt: Some(title.into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = prompt.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                // Never apply a delayed picker result to a different connection form.
                if this.panel != Some(Panel::Server)
                    || !this
                        .fields
                        .iter()
                        .any(|current| current.key == key && current.state == field)
                {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.first() {
                            if let Some(value) = path.to_str() {
                                field.update(cx, |state, cx| {
                                    state.set_value(value.to_owned(), window, cx);
                                    state.focus(window, cx);
                                });
                                this.status = if key == "keypath" {
                                    "已选择 SSH 私钥文件"
                                } else {
                                    "已选择 OpenSSH 证书文件"
                                }
                                .into();
                            } else {
                                this.error =
                                    Some("所选文件路径无法表示为 UTF-8，请使用其他文件位置".into());
                            }
                        }
                    }
                    Ok(Ok(None)) => {} // Cancellation preserves the existing field value.
                    Ok(Err(error)) => this.error = Some(format!("无法选择文件：{error}")),
                    Err(_) => this.error = Some("文件选择窗口未返回结果，请重试".into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn cycle_modal(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(settings) = &self.settings_pane {
            settings.update(cx, |pane, cx| pane.cycle_focus(backwards, window, cx));
            return;
        }
        let visible: Vec<_> = self
            .fields
            .iter()
            .filter(|field| {
                self.panel != Some(Panel::Server)
                    || !((field.key == "password" && self.auth != 0)
                        || (["keypath", "passphrase", "certpath"].contains(&field.key)
                            && self.auth != 1))
            })
            .collect();
        if visible.is_empty() {
            window.focus(&self.modal_focus);
            return;
        }
        let current = visible
            .iter()
            .position(|field| field.state.focus_handle(cx).is_focused(window));
        let next = match current {
            Some(index) if backwards => (index + visible.len() - 1) % visible.len(),
            Some(index) => (index + 1) % visible.len(),
            None => 0,
        };
        visible[next]
            .state
            .update(cx, |state, cx| state.focus(window, cx));
    }

    fn sidebar_footer(&self, collapsed: bool, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::appearance::palette(cx);
        column()
            .p_2()
            .flex_shrink_0()
            .gap_2()
            .border_t_1()
            .border_color(palette.color(LINE))
            .child(
                Button::new("sidebar-settings")
                    .ghost()
                    .small()
                    .icon(IconName::Settings)
                    .when(!collapsed, |button| button.label("设置").w_full())
                    .tooltip("设置 · ⌘, / Ctrl+,")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_panel(Panel::Settings, window, cx)
                    })),
            )
            .child(
                row()
                    .justify_center()
                    .gap_1()
                    .child(
                        div()
                            .size(px(5.))
                            .rounded_full()
                            .bg(palette.color(if self.master.is_some() { MINT } else { MUTED })),
                    )
                    .child(palette.text(
                        concat!("v", env!("CARGO_PKG_VERSION")),
                        MUTED,
                        if collapsed { 8. } else { 10. },
                    )),
            )
            .into_any_element()
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::appearance::palette(cx);
        if self.preferences.sidebar_collapsed {
            return column()
                .w(px(48.))
                .h_full()
                .flex_shrink_0()
                .bg(palette.color(PANEL))
                .border_r_1()
                .border_color(palette.color(LINE))
                .child(
                    column()
                        .p_2()
                        .gap_3()
                        .child(
                            Button::new("expand-sidebar")
                                .ghost()
                                .small()
                                .icon(IconName::PanelLeftOpen)
                                .tooltip("展开侧栏 · ⌘B / Ctrl+Shift+B")
                                .on_click(cx.listener(|this, _, w, cx| this.toggle_sidebar(w, cx))),
                        )
                        .child(
                            Button::new("rail-connections")
                                .ghost()
                                .small()
                                .icon(IconName::Folder)
                                .tooltip("服务器连接")
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.toggle_sidebar(w, cx);
                                    if this.master.is_none() {
                                        this.open_panel(Panel::Unlock, w, cx);
                                    }
                                })),
                        )
                        .child(
                            Button::new("rail-local")
                                .ghost()
                                .small()
                                .icon(IconName::SquareTerminal)
                                .tooltip("新建本地终端")
                                .on_click(cx.listener(|this, _, w, cx| this.new_local(w, cx))),
                        )
                        .child(
                            Button::new("rail-add")
                                .ghost()
                                .small()
                                .icon(IconName::Plus)
                                .tooltip("添加 SSH 连接")
                                .on_click(
                                    cx.listener(|this, _, w, cx| this.edit_server(None, w, cx)),
                                ),
                        ),
                )
                .child(div().flex_1())
                .child(self.sidebar_footer(true, cx))
                .into_any_element();
        }
        let query = self.search.read(cx).value().to_lowercase();
        let mut groups: BTreeMap<String, Vec<ServerProfile>> = BTreeMap::new();
        for server in &self.data.servers {
            if format!("{} {} {}", server.name, server.host, server.group)
                .to_lowercase()
                .contains(&query)
            {
                groups
                    .entry(if server.group.is_empty() {
                        "未分组".into()
                    } else {
                        server.group.clone()
                    })
                    .or_default()
                    .push(server.clone());
            }
        }
        let mut list = column().gap_2();
        if self.master.is_none() {
            list =
                list.child(
                    column()
                        .p_4()
                        .gap_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(palette.color(LINE))
                        .child(palette.text("凭据库已锁定", 0xdbe8fb, 14.))
                        .child(palette.text("用主密码解锁你的服务器与凭据。", MUTED, 12.))
                        .child(Button::new("unlock-side").label("解锁凭据库").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.open_panel(Panel::Unlock, window, cx)
                            }),
                        )),
                );
        } else if self.data.servers.is_empty() {
            list = list.child(
                column()
                    .p_4()
                    .gap_2()
                    .child(palette.text("你的服务器，从这里开始", 0xc1d1e9, 14.))
                    .child(palette.text("添加密码或私钥 / 证书连接。", MUTED, 12.)),
            );
        }
        for (group, servers) in groups {
            list = list.child(palette.text(group.to_uppercase(), MUTED, 10.).px_2());
            for server in servers {
                let connect_id = server.id.clone();
                let edit_id = server.id.clone();
                let split_horizontal_id = server.id.clone();
                let split_vertical_id = server.id.clone();
                let row_id = SharedString::from(format!("server-{}", server.id));
                let edit_btn_id = SharedString::from(format!("edit-{}", server.id));
                let kind = match &server.credential {
                    Credential::Password { .. } => "PASSWORD",
                    Credential::PrivateKey { .. } => "SSH KEY",
                    Credential::Agent => "AGENT",
                };
                list = list.child(
                    column()
                        .id(row_id)
                        .p_3()
                        .gap_2()
                        .rounded_lg()
                        .bg(palette.color(0x142033))
                        .border_1()
                        .border_color(palette.color(0x24344e))
                        .cursor_pointer()
                        .hover(|s| s.bg(palette.color(0x1b2d44)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.connect(&connect_id, window, cx)
                        }))
                        .child(
                            row()
                                .justify_between()
                                .child(palette.text(server.name, 0xe6effe, 13.))
                                .child(
                                    Button::new(edit_btn_id)
                                        .ghost()
                                        .xsmall()
                                        .label("•••")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.edit_server(Some(edit_id.clone()), window, cx);
                                        })),
                                ),
                        )
                        .child(palette.text(
                            format!("{}@{}:{}", server.username, server.host, server.port),
                            MUTED,
                            11.,
                        ))
                        .child(
                            row()
                                .gap_1()
                                .child(palette.text(kind, MINT, 9.).flex_1())
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "split-horizontal-{}",
                                        server.id
                                    )))
                                    .ghost()
                                    .xsmall()
                                    .label("↕")
                                    .tooltip("在当前窗格下方连接此服务器")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        cx.stop_propagation();
                                        this.split_profile(
                                            &split_horizontal_id,
                                            SplitDirection::Horizontal,
                                            w,
                                            cx,
                                        );
                                    })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "split-vertical-{}",
                                        server.id
                                    )))
                                    .ghost()
                                    .xsmall()
                                    .label("↔")
                                    .tooltip("在当前窗格右侧连接此服务器")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        cx.stop_propagation();
                                        this.split_profile(
                                            &split_vertical_id,
                                            SplitDirection::Vertical,
                                            w,
                                            cx,
                                        );
                                    })),
                                ),
                        ),
                );
            }
        }
        column()
            .w(px(244.))
            .flex_shrink_0()
            .h_full()
            .bg(palette.color(PANEL))
            .border_r_1()
            .border_color(palette.color(LINE))
            .child(
                row()
                    .h(px(48.))
                    .px_3()
                    .gap_2()
                    .child(palette.text("SyncSSH", 0xf1f7ff, 17.).flex_1())
                    .child(
                        Button::new("collapse-sidebar")
                            .ghost()
                            .small()
                            .icon(IconName::PanelLeftClose)
                            .tooltip("收起侧栏 · ⌘B / Ctrl+Shift+B")
                            .on_click(cx.listener(|this, _, w, cx| this.toggle_sidebar(w, cx))),
                    ),
            )
            .child(
                column()
                    .px_4()
                    .gap_3()
                    .child(
                        row()
                            .gap_1()
                            .child(
                                Button::new("local-sidebar")
                                    .label("＋ 本地终端")
                                    .on_click(cx.listener(|this, _, w, cx| this.new_local(w, cx))),
                            )
                            .child(
                                Button::new("split-local-horizontal")
                                    .ghost()
                                    .small()
                                    .label("↕")
                                    .tooltip("在当前窗格下方新建本地终端")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.split_local(SplitDirection::Horizontal, w, cx)
                                    })),
                            )
                            .child(
                                Button::new("split-local-vertical")
                                    .ghost()
                                    .small()
                                    .label("↔")
                                    .tooltip("在当前窗格右侧新建本地终端")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.split_local(SplitDirection::Vertical, w, cx)
                                    })),
                            ),
                    )
                    .child(
                        row()
                            .mt_3()
                            .justify_between()
                            .child(palette.text("服务器连接", MUTED, 11.))
                            .child(
                                Button::new("add-server")
                                    .ghost()
                                    .xsmall()
                                    .label("＋")
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.edit_server(None, w, cx)),
                                    ),
                            ),
                    )
                    .child(Input::new(&self.search).small()),
            )
            .child(
                div()
                    .id("servers-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .child(list),
            )
            .child(self.sidebar_footer(false, cx))
            .into_any_element()
    }

    fn settings_view(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(pane) = &self.settings_pane else {
            return div().into_any_element();
        };
        div()
            .occlude()
            .track_focus(&self.modal_focus)
            .key_context("SyncSSHModal")
            .on_action(cx.listener(|this, _: &ModalNext, w, cx| this.cycle_modal(false, w, cx)))
            .on_action(cx.listener(|this, _: &ModalPrevious, w, cx| this.cycle_modal(true, w, cx)))
            .on_action(cx.listener(|this, _: &ModalClose, w, cx| this.close_panel(w, cx)))
            .absolute()
            .inset_0()
            .bg(rgba(0x030811b0))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w((window.viewport_size().width - px(48.)).min(px(960.)))
                    .h((window.viewport_size().height - px(48.)).min(px(780.)))
                    .rounded_xl()
                    .overflow_hidden()
                    .shadow_lg()
                    .child(pane.clone()),
            )
            .into_any_element()
    }

    fn panel_view(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::appearance::palette(cx);
        let Some(panel) = &self.panel else {
            return div().into_any_element();
        };
        let (eyebrow, title, description) = match panel {
            Panel::Settings => return self.settings_view(window, cx),
            Panel::Unlock => (
                "YOUR KEYS. YOUR CONTROL.",
                if !self.vault.can_create() {
                    "欢迎回来"
                } else {
                    "创建你的加密凭据库"
                },
                "主密码保护所有 SSH 密码、私钥与备份，仅接受英文、数字、半角符号和空格，不会保存或上传。",
            ),
            Panel::Server => (
                "CONNECTION DETAILS",
                if self.edit_id.is_some() {
                    "编辑服务器"
                } else {
                    "添加 SSH 服务器"
                },
                "凭据仅在内存中解密，保存后由主密码保护。",
            ),
            Panel::ChangePassword => (
                "ROTATE YOUR MASTER PASSWORD",
                "更换主密码",
                "重新加密本地凭据库。本地恢复副本也会重新加密；已有云端快照仍需旧密码解密。",
            ),
            Panel::Trust => (
                "VERIFY BEFORE CONNECTING",
                "确认 SSH 主机身份",
                "请通过服务器控制台或管理员核对 SHA256 指纹。确认前不会发送登录凭据。",
            ),
            Panel::Delete => (
                "REMOVE CONNECTION",
                "删除这台服务器？",
                "此操作会删除本机保存的连接与凭据，已有云端快照不会被删除。",
            ),
        };
        let mut content = column()
            .flex_shrink_0()
            .min_w_0()
            .gap_3()
            .p_5()
            .child(
                row()
                    .justify_between()
                    .child(palette.text(eyebrow, MINT, 10.))
                    .child(
                        Button::new("dismiss-panel")
                            .ghost()
                            .xsmall()
                            .label("关闭")
                            .on_click(cx.listener(|this, _, w, cx| this.close_panel(w, cx))),
                    ),
            )
            .child(palette.text(title, 0xf1f7ff, 25.))
            .child(palette.text(description, MUTED, 13.));
        let mut form = div().grid().grid_cols(1).gap_3().min_w_0().flex_shrink_0();
        for field in &self.fields {
            if *panel == Panel::Server && field.key == "password" {
                let mut methods = row().gap_4();
                for (index, label) in ["密码", "私钥 / 证书"].into_iter().enumerate() {
                    methods = methods.child(
                        Radio::new(("auth-method", index))
                            .label(label)
                            .checked(self.auth == index)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.auth = index;
                                if let Some(field) = this.fields.iter().find(|field| {
                                    field.key == if index == 0 { "password" } else { "keypath" }
                                }) {
                                    field.state.update(cx, |state, cx| state.focus(window, cx));
                                }
                                cx.notify();
                            })),
                    );
                }
                form = form.child(
                    column()
                        .gap_2()
                        .child(palette.text("认证方法", 0xb6c5db, 11.))
                        .child(methods),
                );
            }
            if *panel == Panel::Server
                && ((field.key == "password" && self.auth != 0)
                    || (["keypath", "passphrase", "certpath"].contains(&field.key)
                        && self.auth != 1))
            {
                continue;
            }
            form = form.child(
                column()
                    .min_w_0()
                    .gap_1()
                    .child(
                        palette
                            .text(field.label, 0xb6c5db, 11.)
                            .whitespace_normal()
                            .line_height(px(16.)),
                    )
                    .child(if matches!(field.key, "master" | "confirm") {
                        crate::ascii_password::input(&field.state, self.busy, false)
                    } else if matches!(field.key, "keypath" | "certpath") {
                        let key = field.key;
                        row()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&field.state).disabled(self.busy)),
                            )
                            .child(
                                Button::new(if key == "keypath" {
                                    "choose-private-key"
                                } else {
                                    "choose-ssh-certificate"
                                })
                                .label("选择文件…")
                                .disabled(self.busy)
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.choose_credential_file(key, window, cx)
                                    },
                                )),
                            )
                            .into_any_element()
                    } else {
                        Input::new(&field.state).into_any_element()
                    }),
            );
        }
        content = content.child(form);
        if *panel == Panel::Server && self.auth == 1 && self.edit_id.is_some() {
            content = content.child(
                row()
                    .gap_2()
                    .child(
                        Button::new("clear-key-pass")
                            .ghost()
                            .small()
                            .label(if self.clear_passphrase {
                                "☑ 清除旧私钥口令"
                            } else {
                                "☐ 清除旧私钥口令"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clear_passphrase = !this.clear_passphrase;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("clear-key-cert")
                            .ghost()
                            .small()
                            .label(if self.clear_certificate {
                                "☑ 移除旧证书"
                            } else {
                                "☐ 移除旧证书"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clear_certificate = !this.clear_certificate;
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(error) = &self.error {
            content = content.child(
                div()
                    .p_3()
                    .rounded_lg()
                    .bg(palette.color(0x3b202b))
                    .child(palette.text(error.clone(), 0xffb2bd, 12.)),
            );
        }
        if self.busy {
            content = content.child(palette.text("◌  正在处理，请稍候…", MINT, 12.));
        }
        let actions = match panel {
            Panel::Settings => row(),
            Panel::Unlock => row()
                .gap_3()
                .child(
                    Button::new("unlock-submit")
                        .primary()
                        .label(if !self.vault.can_create() {
                            "解锁"
                        } else {
                            "创建凭据库"
                        })
                        .on_click(cx.listener(|this, _, w, cx| this.unlock(w, cx))),
                )
                .child(
                    Button::new("unlock-restore")
                        .ghost()
                        .label("打开本地或云端备份")
                        .on_click(cx.listener(|this, _, w, cx| this.open_backup_settings(w, cx))),
                )
                .child(
                    Button::new("skip-vault")
                        .ghost()
                        .label("仅使用本地终端")
                        .on_click(cx.listener(|this, _, w, cx| this.close_panel(w, cx))),
                ),
            Panel::Server => row()
                .gap_3()
                .child(
                    Button::new("save-server")
                        .primary()
                        .label("加密保存")
                        .on_click(cx.listener(|this, _, w, cx| this.save_server(w, cx))),
                )
                .when(self.edit_id.is_some(), |r| {
                    r.child(
                        Button::new("delete-server")
                            .ghost()
                            .label("删除连接")
                            .on_click(
                                cx.listener(|this, _, w, cx| this.open_panel(Panel::Delete, w, cx)),
                            ),
                    )
                }),
            Panel::ChangePassword => row().child(
                Button::new("change-password")
                    .primary()
                    .label("使用新密码重新加密")
                    .on_click(cx.listener(|this, _, w, cx| this.change_password(w, cx))),
            ),
            Panel::Trust => {
                if let Some((key, _, _, _)) = &self.trust {
                    content = content.child(
                        column()
                            .p_4()
                            .gap_2()
                            .rounded_lg()
                            .bg(palette.color(BG))
                            .child(palette.text(
                                format!("{}:{} · {}", key.host, key.port, key.algorithm),
                                0xd6e3f6,
                                14.,
                            ))
                            .child(palette.text(key.fingerprint.clone(), MINT, 12.)),
                    );
                }
                row()
                    .gap_3()
                    .child(
                        Button::new("trust-host")
                            .primary()
                            .label("指纹一致，信任并连接")
                            .on_click(cx.listener(|this, _, w, cx| {
                                if this.busy {
                                    return;
                                }
                                if let Some((key, config, title, profile_id)) = this.trust.take() {
                                    this.close_panel(w, cx);
                                    this.connect_config(
                                        config,
                                        title,
                                        profile_id,
                                        Some(key),
                                        w,
                                        cx,
                                    );
                                }
                            })),
                    )
                    .child(
                        Button::new("reject-host")
                            .label("取消连接")
                            .on_click(cx.listener(|this, _, w, cx| this.close_panel(w, cx))),
                    )
            }
            Panel::Delete => row().child(
                Button::new("confirm-delete")
                    .primary()
                    .label("确认删除")
                    .on_click(cx.listener(|this, _, w, cx| {
                        let mut data = this.data.clone();
                        data.servers
                            .retain(|s| Some(&s.id) != this.edit_id.as_ref());
                        this.save_data(data, w, cx);
                    })),
            ),
        };
        div()
            .occlude()
            .track_focus(&self.modal_focus)
            .key_context("SyncSSHModal")
            .on_action(cx.listener(|this, _: &ModalNext, w, cx| this.cycle_modal(false, w, cx)))
            .on_action(cx.listener(|this, _: &ModalPrevious, w, cx| this.cycle_modal(true, w, cx)))
            .on_action(cx.listener(|this, _: &ModalClose, w, cx| this.close_panel(w, cx)))
            .absolute()
            .inset_0()
            .bg(rgba(0x030811d9))
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .child(
                column()
                    .id("modal-card")
                    .occlude()
                    .w(px(640.))
                    .max_w_full()
                    .max_h((window.viewport_size().height - px(48.)).max(px(200.)))
                    .min_h_0()
                    .overflow_hidden()
                    .bg(palette.color(PANEL))
                    .border_1()
                    .border_color(palette.color(0x30445f))
                    .rounded_xl()
                    .shadow_lg()
                    .child(
                        column()
                            .id(SharedString::from(format!("modal-scroll-{eyebrow}")))
                            .min_h_0()
                            .flex_1()
                            .overflow_y_scroll()
                            .child(content),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .px_5()
                            .py_4()
                            .border_t_1()
                            .border_color(palette.color(LINE))
                            .child(actions),
                    ),
            )
            .into_any_element()
    }

    fn render_layout(
        &self,
        node: &LayoutNode,
        multiple: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::appearance::palette(cx);
        match node {
            LayoutNode::Pane(id) => {
                let id = *id;
                let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
                    return div().into_any_element();
                };
                let selected = self.tabs.get(self.active).is_some_and(|tab| tab.id == id);
                let disconnected =
                    tab.remote && !matches!(tab.session_state, Some(SessionStatus::Connected));
                let icon = if tab.reconnecting {
                    "↻"
                } else if disconnected {
                    "○"
                } else if tab.remote {
                    "↗"
                } else {
                    "⌘"
                };
                let mut pane = column()
                    .id(("workspace-pane", id))
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .capture_any_mouse_down(
                        cx.listener(move |this, _, _, cx| this.select_pane(id, cx)),
                    );
                if multiple {
                    pane = pane
                        .border_1()
                        .border_color(palette.color(if selected { MINT } else { LINE }))
                        .child(
                            row()
                                .id(("pane-title", id))
                                .w_full()
                                .min_w_0()
                                .h(px(28.))
                                .flex_shrink_0()
                                .px_2()
                                .gap_1()
                                .bg(palette.color(if selected { 0x1b2e3e } else { 0x101927 }))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.select_pane(id, cx);
                                    this.focus_terminal(w, cx);
                                }))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(11.))
                                        .text_color(palette.color(if disconnected {
                                            0xf1bd72
                                        } else if selected {
                                            0xecf4ff
                                        } else {
                                            MUTED
                                        }))
                                        .child(format!("{icon} {}", tab.title)),
                                )
                                .child(
                                    Button::new(("close-pane", id))
                                        .ghost()
                                        .xsmall()
                                        .label("×")
                                        .tooltip("关闭此窗格")
                                        .on_click(cx.listener(move |this, _, w, cx| {
                                            cx.stop_propagation();
                                            this.close_panes(&[id], w, cx);
                                        })),
                                ),
                        );
                }
                pane.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .size_full()
                        .child(tab.pane.clone()),
                )
                .into_any_element()
            }
            LayoutNode::Split {
                id,
                direction,
                first,
                second,
            } => {
                let axis = match direction {
                    SplitDirection::Horizontal => Axis::Vertical,
                    SplitDirection::Vertical => Axis::Horizontal,
                };
                let mut group = ResizablePanelGroup::new(("terminal-split", *id)).axis(axis);
                if let Some(state) = self.split_states.get(id) {
                    group = group.with_state(state);
                }
                group
                    .child(
                        resizable_panel()
                            .size_range(px(80.)..Pixels::MAX)
                            .child(self.render_layout(first, multiple, cx)),
                    )
                    .child(
                        resizable_panel()
                            .size_range(px(80.)..Pixels::MAX)
                            .child(self.render_layout(second, multiple, cx)),
                    )
                    .into_any_element()
            }
        }
    }
}

impl Render for SyncSSH {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::appearance::palette(cx);
        let mut tabbar = row()
            .id("workspace-tabs")
            .h(px(38.))
            .flex_shrink_0()
            .min_w_0()
            .overflow_x_scroll()
            .gap_1()
            .px_3()
            .bg(palette.color(0x0e1725))
            .border_b_1()
            .border_color(palette.color(LINE));
        for workspace in &self.workspaces {
            let Some(tab) = self.tabs.iter().find(|tab| tab.id == workspace.focused) else {
                continue;
            };
            let workspace_id = workspace.id;
            let focused = workspace.focused;
            let selected = self
                .active_workspace()
                .is_some_and(|w| w.id == workspace_id);
            let count = workspace.root.panes().len();
            tabbar = tabbar.child(
                row()
                    .id(("tab", workspace_id))
                    .h(px(30.))
                    .flex_shrink_0()
                    .px_3()
                    .gap_4()
                    .rounded_md()
                    .bg(palette.color(if selected { 0x22324a } else { 0x111b2a }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.select_pane(focused, cx);
                        this.focus_terminal(w, cx);
                        cx.notify();
                    }))
                    .child(palette.text(
                        format!(
                            "{}  {}{}",
                            if tab.reconnecting {
                                "↻"
                            } else if tab.remote
                                && !matches!(tab.session_state, Some(SessionStatus::Connected))
                            {
                                "○"
                            } else if tab.remote {
                                "↗"
                            } else {
                                "⌘"
                            },
                            tab.title,
                            if count > 1 {
                                format!(" · {count} 窗格")
                            } else {
                                String::new()
                            }
                        ),
                        if tab.remote
                            && !matches!(tab.session_state, Some(SessionStatus::Connected))
                        {
                            0xf1bd72
                        } else if selected {
                            0xecf4ff
                        } else {
                            MUTED
                        },
                        12.,
                    ))
                    .child(
                        Button::new(("close-tab", workspace_id))
                            .ghost()
                            .xsmall()
                            .label("×")
                            .tooltip("关闭此工作区及其中所有会话")
                            .on_click(cx.listener(move |this, _, w, cx| {
                                cx.stop_propagation();
                                if let Some(workspace) =
                                    this.workspaces.iter().find(|w| w.id == workspace_id)
                                {
                                    let ids = workspace.root.panes();
                                    this.close_panes(&ids, w, cx);
                                }
                            })),
                    ),
            );
        }
        tabbar = tabbar.child(
            Button::new("new-tab")
                .ghost()
                .label("＋")
                .on_click(cx.listener(|this, _, w, cx| this.new_local(w, cx))),
        );
        let layout = self
            .active_workspace()
            .map(|workspace| workspace.root.clone());
        let workspace = layout
            .as_ref()
            .map(|node| self.render_layout(node, node.panes().len() > 1, cx));
        let terminal = if let Some(tab) = self.tabs.get(self.active) {
            let mut area = column().size_full().min_h_0();
            if tab.remote
                && (!matches!(tab.session_state, Some(SessionStatus::Connected))
                    || tab.reconnect_notice.is_some())
            {
                let id = tab.id;
                let connected = matches!(tab.session_state, Some(SessionStatus::Connected));
                let mut label = if let Some(notice) = &tab.reconnect_notice {
                    notice.clone()
                } else {
                    match &tab.session_state {
                        Some(SessionStatus::Connected) => "SSH 已连接".into(),
                        Some(SessionStatus::Exited { exit_code }) => format!(
                            "远端 Shell 已退出{}",
                            exit_code
                                .map(|code| format!("（{code}）"))
                                .unwrap_or_default()
                        ),
                        Some(SessionStatus::Disconnected { reason }) => {
                            format!("连接已断开：{reason}")
                        }
                        _ => "SSH 已关闭".into(),
                    }
                };
                if tab.retry_at.is_some() {
                    label.push_str(&format!(" · 等待重试（{}/5）", tab.retry_count + 1));
                }
                let mut bar = row()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .flex_shrink_0()
                    .bg(palette.color(0x111d2d))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(palette.color(if connected { MINT } else { 0xf1bd72 }))
                            .child(label),
                    )
                    .child(
                        Button::new("auto-reconnect")
                            .xsmall()
                            .ghost()
                            .label(format!(
                                "{} 自动重连",
                                if tab.auto_reconnect { "☑" } else { "☐" }
                            ))
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.toggle_auto_reconnect(id, w, cx)
                            })),
                    );
                if !connected {
                    bar = bar.child(
                        Button::new("reconnect")
                            .small()
                            .label("重新连接")
                            .disabled(tab.reconnecting)
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.reconnect_tab(id, false, w, cx)
                            })),
                    );
                    if tab.reconnecting || tab.retry_at.is_some() {
                        bar =
                            bar.child(
                                Button::new("stop-reconnect")
                                    .small()
                                    .ghost()
                                    .label("停止重试")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.cancel_reconnect(id, cx)
                                    })),
                            );
                    }
                }
                area = area.child(bar);
            } else if tab.pane.read(cx).is_exited() {
                area = area.child(
                    row()
                        .px_3()
                        .py_2()
                        .gap_3()
                        .flex_shrink_0()
                        .child(palette.text("本地 Shell 已退出", MUTED, 12.))
                        .child(
                            Button::new("restart-local")
                                .small()
                                .label("新建本地终端")
                                .on_click(cx.listener(|this, _, w, cx| this.new_local(w, cx))),
                        ),
                );
            }
            let body = row()
                .size_full()
                .flex_1()
                .min_h_0()
                .items_start()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_hidden()
                        .children(workspace),
                )
                .when(tab.files_visible, |r| {
                    if let Some(files) = &tab.files {
                        r.child(
                            div()
                                .w(px(420.))
                                .flex_shrink_0()
                                .h_full()
                                .min_h_0()
                                .border_l_1()
                                .border_color(palette.color(LINE))
                                .child(files.clone()),
                        )
                    } else {
                        r
                    }
                })
                .when(tab.tunnels_visible, |r| {
                    if let Some(tunnels) = &tab.tunnel_pane {
                        r.child(
                            div()
                                .w(px(420.))
                                .flex_shrink_0()
                                .h_full()
                                .min_h_0()
                                .border_l_1()
                                .border_color(palette.color(LINE))
                                .child(tunnels.clone()),
                        )
                    } else {
                        r
                    }
                });
            area.child(body).into_any_element()
        } else {
            column()
                .size_full()
                .items_center()
                .justify_center()
                .gap_4()
                .child(palette.text("◈", MINT, 48.))
                .child(palette.text("SyncSSH", 0xe1edfe, 23.))
                .child(palette.text("开启本地 Shell，或从左侧选择你的服务器。", MUTED, 14.))
                .child(
                    Button::new("empty-new")
                        .primary()
                        .label("打开本地终端")
                        .on_click(cx.listener(|this, _, w, cx| this.new_local(w, cx))),
                )
                .into_any_element()
        };
        let sidebar = self.sidebar(cx);
        let panel = self.panel_view(_window, cx);
        column()
            .relative()
            .size_full()
            .bg(palette.color(BG))
            .text_color(palette.color(0xe1eaf8))
            .font_family(cx.theme().font_family.clone())
            .on_action(cx.listener(|this, _: &NewTab, w, cx| {
                if this.panel.is_none() {
                    this.new_local(w, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &LockVault, w, cx| this.lock(w, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, w, cx| {
                if this.panel.is_none() {
                    this.toggle_sidebar(w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, w, cx| {
                if this.panel.is_none() {
                    this.open_panel(Panel::Settings, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SplitHorizontal, w, cx| {
                this.split_active(SplitDirection::Horizontal, w, cx)
            }))
            .on_action(cx.listener(|this, _: &SplitVertical, w, cx| {
                this.split_active(SplitDirection::Vertical, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ClosePane, w, cx| {
                if let Some(id) = this.tabs.get(this.active).map(|tab| tab.id) {
                    this.close_panes(&[id], w, cx);
                }
            }))
            .child(
                row().flex_1().min_h_0().items_start().child(sidebar).child(
                    column()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .child(tabbar)
                        .child(div().flex_1().min_h_0().overflow_hidden().child(terminal)),
                ),
            )
            .when(
                self.busy || self.error.is_some() || self.status_warning,
                |root| {
                    root.child(
                        row()
                            .h(px(26.))
                            .px_3()
                            .gap_2()
                            .flex_shrink_0()
                            .bg(palette.color(PANEL))
                            .border_t_1()
                            .border_color(palette.color(LINE))
                            .child(
                                palette
                                    .text(
                                        self.error.clone().unwrap_or_else(|| self.status.clone()),
                                        if self.error.is_some() {
                                            0xffb2bd
                                        } else if self.status_warning {
                                            0xf1bd72
                                        } else {
                                            MUTED
                                        },
                                        11.,
                                    )
                                    .flex_1(),
                            )
                            .when(!self.busy, |bar| {
                                bar.child(
                                    Button::new("dismiss-status")
                                        .ghost()
                                        .xsmall()
                                        .label("×")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.error = None;
                                            this.status_warning = false;
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                },
            )
            .when(self.panel.is_some(), |r| r.child(panel))
    }
}

fn ssh_auth(credential: &Credential) -> SshAuth {
    match credential {
        Credential::Password { password } => SshAuth::Password(password.clone()),
        Credential::PrivateKey {
            pem,
            passphrase,
            certificate,
        } => SshAuth::PrivateKey {
            pem: pem.clone(),
            passphrase: passphrase.clone(),
            certificate: certificate.clone(),
        },
        Credential::Agent => SshAuth::Agent,
    }
}
fn ssh_config(profile: &ServerProfile) -> Result<SshConfig> {
    profile.validate()?;
    Ok(SshConfig {
        host: profile.host.clone(),
        port: profile.port,
        username: profile.username.clone(),
        auth: ssh_auth(&profile.credential),
    })
}
fn reconnect_delay(attempt: u32) -> Duration {
    Duration::from_secs((2u64.saturating_pow(attempt.min(4))).max(1))
}
fn retryable_ssh_error(error: &SessionError) -> bool {
    matches!(
        error,
        SessionError::Timeout | SessionError::Transport(_) | SessionError::Io(_)
    )
}

fn row() -> Div {
    div().flex().items_center()
}
fn column() -> Div {
    div().flex().flex_col()
}
fn read_secret_file(path: &str) -> Result<String> {
    let path = std::path::Path::new(path);
    ensure!(path.is_absolute(), "请输入文件的绝对路径");
    let file = std::fs::File::open(path).context("无法读取所选密钥文件")?;
    let metadata = file.metadata().context("无法检查密钥文件")?;
    ensure!(
        metadata.is_file() && metadata.len() <= 1024 * 1024,
        "密钥文件必须是小于 1 MiB 的普通文件"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .context("无法读取密钥文件")?;
    ensure!(bytes.len() <= 1024 * 1024, "密钥文件超过 1 MiB");
    let text = std::str::from_utf8(&bytes).context("密钥文件需要 UTF-8 PEM / OpenSSH 格式")?;
    if text.is_empty() {
        bail!("密钥文件为空");
    }
    Ok(text.to_owned())
}

#[cfg(test)]
mod backup_tests {
    use super::{
        automatic_backup_targets, reconnect_delay, retryable_ssh_error, upload_results, CloudConfig,
    };
    use syncssh_core::{S3Config, S3Provider, VaultData, WebDavConfig};
    use syncssh_session::SessionError;

    #[test]
    fn reconnection_stops_on_identity_and_authentication_failures() {
        assert!(retryable_ssh_error(&SessionError::Timeout));
        assert!(retryable_ssh_error(&SessionError::Transport(
            "network closed".into()
        )));
        assert!(!retryable_ssh_error(&SessionError::Authentication));
        assert!(!retryable_ssh_error(&SessionError::KnownHosts(
            "changed".into()
        )));
        assert!(!retryable_ssh_error(&SessionError::InvalidConfig(
            "bad".into()
        )));
        let delays = (0..5)
            .map(|n| reconnect_delay(n).as_secs())
            .collect::<Vec<_>>();
        assert_eq!(delays, [1, 2, 4, 8, 16]);
        assert_eq!(reconnect_delay(u32::MAX).as_secs(), 16);
    }

    #[test]
    fn saving_one_provider_preserves_the_other_provider_and_automatic_setting() {
        let s3 = S3Config {
            provider: S3Provider::S3,
            bucket: "test-backups".into(),
            access_key: "s3-user".into(),
            secret_key: "s3-secret".into(),
            region: "us-east-1".into(),
            endpoint: None,
            session_token: None,
            prefix: "syncssh/backups".into(),
        };
        let webdav = WebDavConfig {
            endpoint: "https://dav.example.test/files/".into(),
            username: "dav-user".into(),
            password: "dav-secret".into(),
            prefix: "syncssh/backups".into(),
        };
        let mut data = VaultData::default();
        CloudConfig::S3(s3.clone()).apply(&mut data, true);
        CloudConfig::WebDav(webdav.clone()).apply(&mut data, true);
        assert_eq!(data.s3.as_ref(), Some(&s3));
        assert_eq!(data.webdav.as_ref(), Some(&webdav));
        assert!(data.backup_on_save && data.webdav_backup_on_save);
        assert_eq!(automatic_backup_targets(&data).len(), 2);

        CloudConfig::WebDav(webdav.clone()).apply(&mut data, false);
        assert!(data.backup_on_save);
        assert!(!data.webdav_backup_on_save);
        assert_eq!(data.s3.as_ref(), Some(&s3));
        assert_eq!(automatic_backup_targets(&data)[0].provider().label(), "S3");

        CloudConfig::WebDav(webdav.clone()).apply(&mut data, true);
        CloudConfig::S3(s3).apply(&mut data, false);
        assert_eq!(data.webdav.as_ref(), Some(&webdav));
        assert!(data.webdav_backup_on_save);
        assert!(!data.backup_on_save);
        assert_eq!(
            automatic_backup_targets(&data)[0].provider().label(),
            "WebDAV"
        );
    }

    #[test]
    fn a_failed_destination_does_not_skip_or_misreport_the_other_backup() {
        let targets = vec![
            CloudConfig::S3(S3Config::default()),
            CloudConfig::WebDav(WebDavConfig::default()),
        ];
        let mut attempted = Vec::new();
        let reports = upload_results(&targets, |target| {
            attempted.push(target.provider().label());
            match target {
                CloudConfig::S3(_) => anyhow::bail!("sensitive server response"),
                CloudConfig::WebDav(_) => Ok("syncssh/backups/test.vault".into()),
            }
        });
        assert_eq!(attempted, ["S3", "WebDAV"]);
        assert!(reports[0].contains("S3 备份失败"));
        assert!(reports[1].contains("WebDAV 备份成功"));
        assert!(reports[1].contains("test.vault"));
        assert!(!reports.join(" ").contains("sensitive"));
    }

    #[test]
    fn all_cloud_accounts_are_routed_and_one_failure_does_not_skip_later_accounts() {
        let mut data = VaultData::default();
        for provider in S3Provider::ALL {
            CloudConfig::S3(S3Config::for_provider(provider)).apply(&mut data, true);
        }
        CloudConfig::WebDav(WebDavConfig::default()).apply(&mut data, true);
        let targets = automatic_backup_targets(&data);
        let mut attempted = Vec::new();
        let reports = upload_results(&targets, |target| {
            let label = target.provider().label();
            attempted.push(label);
            if label == "阿里云 OSS" {
                anyhow::bail!("test failure")
            }
            Ok("syncssh/backups/snapshot.vault".into())
        });
        assert_eq!(
            attempted,
            [
                "S3",
                "阿里云 OSS",
                "华为云 OBS",
                "腾讯云 COS",
                "火山引擎 TOS",
                "WebDAV"
            ]
        );
        assert_eq!(
            reports
                .iter()
                .filter(|report| report.contains("成功"))
                .count(),
            5
        );
        assert!(reports[1].contains("阿里云 OSS 备份失败"));
    }
}
