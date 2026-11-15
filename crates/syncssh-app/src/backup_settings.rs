//! Inline local vault and encrypted cloud backup settings.
use anyhow::{ensure, Result};
use gpui::{prelude::*, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputState},
    Disableable, Sizable,
};
use std::path::PathBuf;
use syncssh_core::{S3Config, S3Provider, VaultData, WebDavConfig};
use zeroize::Zeroizing;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupProvider {
    Local,
    S3,
    Aliyun,
    Huawei,
    Tencent,
    Volcengine,
    WebDav,
}
impl BackupProvider {
    pub(crate) const ALL: [Self; 7] = [
        Self::Local,
        Self::S3,
        Self::Aliyun,
        Self::Huawei,
        Self::Tencent,
        Self::Volcengine,
        Self::WebDav,
    ];

    pub(crate) fn index(self) -> usize {
        self as usize
    }

    pub(crate) fn s3_provider(self) -> Option<S3Provider> {
        match self {
            Self::S3 => Some(S3Provider::S3),
            Self::Aliyun => Some(S3Provider::Aliyun),
            Self::Huawei => Some(S3Provider::Huawei),
            Self::Tencent => Some(S3Provider::Tencent),
            Self::Volcengine => Some(S3Provider::Volcengine),
            Self::Local | Self::WebDav => None,
        }
    }

    pub(crate) fn from_s3(provider: S3Provider) -> Self {
        match provider {
            S3Provider::S3 => Self::S3,
            S3Provider::Aliyun => Self::Aliyun,
            S3Provider::Huawei => Self::Huawei,
            S3Provider::Tencent => Self::Tencent,
            S3Provider::Volcengine => Self::Volcengine,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        if self == Self::Local {
            return "本地";
        }
        self.s3_provider()
            .map(S3Provider::label)
            .unwrap_or("WebDAV")
    }
}

#[derive(Clone)]
pub(crate) enum CloudConfig {
    S3(S3Config),
    WebDav(WebDavConfig),
}
impl CloudConfig {
    pub(crate) fn provider(&self) -> BackupProvider {
        match self {
            Self::S3(config) => BackupProvider::from_s3(config.provider),
            Self::WebDav(_) => BackupProvider::WebDav,
        }
    }
    pub(crate) fn apply(&self, data: &mut VaultData, automatic: bool) {
        match self {
            Self::S3(config) => {
                data.set_object_storage(config.clone(), automatic);
            }
            Self::WebDav(config) => {
                data.webdav = Some(config.clone());
                data.webdav_backup_on_save = automatic;
            }
        }
    }
    pub(crate) async fn upload(&self, bytes: &[u8]) -> Result<String> {
        match self {
            Self::S3(config) => syncssh_core::backup::upload(config, bytes).await,
            Self::WebDav(config) => syncssh_core::webdav::upload(config, bytes).await,
        }
    }
    pub(crate) async fn download(&self, key: &str) -> Result<Vec<u8>> {
        match self {
            Self::S3(config) => syncssh_core::backup::download(config, key).await,
            Self::WebDav(config) => syncssh_core::webdav::download(config, key).await,
        }
    }
}

pub(crate) enum BackupEvent {
    ChooseLocalSave,
    OpenLocal {
        path: PathBuf,
        password: Option<Zeroizing<String>>,
    },
    ReloadLocal,
    Save {
        config: CloudConfig,
        automatic: bool,
        upload: bool,
    },
    Restore {
        config: CloudConfig,
        key: String,
        password: Option<Zeroizing<String>>,
    },
    ConfirmRestore,
    CancelRestore,
}

struct BackupField {
    key: &'static str,
    label: &'static str,
    state: Entity<InputState>,
}

struct ProviderForm {
    fields: Vec<BackupField>,
    automatic: bool,
    feedback: Option<String>,
    error: Option<String>,
}

impl ProviderForm {
    fn new(
        provider: BackupProvider,
        data: &VaultData,
        window: &mut Window,
        cx: &mut Context<BackupPane>,
    ) -> Self {
        let mut fields = Vec::new();
        let mut add = |key, label, value: &str, secret| {
            let state = cx.new(|cx| {
                let mut state = if key == "restore_password" {
                    crate::ascii_password::state(window, cx)
                } else {
                    InputState::new(window, cx).masked(secret)
                };
                state.set_value(value.to_owned(), window, cx);
                state
            });
            if key == "region" {
                cx.observe(&state, |_, _, cx| cx.notify()).detach();
            }
            fields.push(BackupField { key, label, state });
        };
        if provider == BackupProvider::Local {
            add(
                "restore_password",
                "所选凭据库主密码（已解锁时可留空；仅 ASCII）",
                "",
                true,
            );
            return Self {
                fields,
                automatic: false,
                feedback: None,
                error: None,
            };
        }
        let automatic;
        if let Some(s3_provider) = provider.s3_provider() {
            let config = data
                .object_storage(s3_provider)
                .cloned()
                .unwrap_or_else(|| S3Config::for_provider(s3_provider));
            automatic = data.object_storage_automatic(s3_provider);
            add(
                "bucket",
                if s3_provider == S3Provider::Tencent {
                    "Bucket（包含 APPID）"
                } else {
                    "Bucket"
                },
                &config.bucket,
                false,
            );
            add("region", "Region", &config.region, false);
            add(
                "endpoint",
                "Endpoint（HTTPS；留空自动选择）",
                config.endpoint.as_deref().unwrap_or(""),
                false,
            );
            add("access", "Access Key ID", &config.access_key, true);
            add("secret", "Secret Access Key", &config.secret_key, true);
            add(
                "token",
                "Session Token（可选）",
                config.session_token.as_deref().unwrap_or(""),
                true,
            );
            add("prefix", "对象前缀", &config.prefix, false);
        } else {
            let config = data.webdav.clone().unwrap_or_default();
            automatic = data.webdav_backup_on_save;
            add(
                "endpoint",
                "WebDAV 根目录 URL（HTTPS）",
                &config.endpoint,
                false,
            );
            add("username", "WebDAV 用户名", &config.username, false);
            add(
                "password",
                "WebDAV 密码 / 应用专用密码",
                &config.password,
                true,
            );
            add("prefix", "备份子目录（相对根目录）", &config.prefix, false);
        }
        add("object", "恢复对象 Key（包含前缀）", "", false);
        add("restore_password", "备份主密码（已解锁时可留空）", "", true);
        Self {
            fields,
            automatic,
            feedback: None,
            error: None,
        }
    }

    fn value(&self, key: &str, cx: &App) -> String {
        self.fields
            .iter()
            .find(|field| field.key == key)
            .map(|field| field.state.read(cx).value().to_string())
            .unwrap_or_default()
    }
}

fn config_from_values(
    provider: BackupProvider,
    value: impl Fn(&str) -> String,
) -> Result<CloudConfig> {
    ensure!(provider != BackupProvider::Local, "本地文件无需云服务配置");
    let optional = |key| {
        let text = value(key);
        (!text.is_empty()).then_some(text)
    };
    if let Some(provider) = provider.s3_provider() {
        let config = S3Config {
            provider,
            bucket: value("bucket").trim().into(),
            region: value("region").trim().into(),
            endpoint: optional("endpoint").map(|endpoint| endpoint.trim().to_owned()),
            access_key: value("access"),
            secret_key: value("secret"),
            session_token: optional("token"),
            prefix: value("prefix").trim().into(),
        };
        config.validate()?;
        Ok(CloudConfig::S3(config))
    } else {
        let config = WebDavConfig {
            endpoint: value("endpoint").trim().into(),
            username: value("username"),
            password: value("password"),
            prefix: value("prefix").trim().into(),
        };
        config.validate()?;
        Ok(CloudConfig::WebDav(config))
    }
}

pub(crate) struct BackupPane {
    provider: BackupProvider,
    forms: [ProviderForm; 7],
    local_path: PathBuf,
    local_candidate: Option<PathBuf>,
    unlocked: bool,
    busy: bool,
    restore_count: Option<usize>,
}

impl EventEmitter<BackupEvent> for BackupPane {}

impl BackupPane {
    pub(crate) fn new(
        data: &VaultData,
        unlocked: bool,
        local_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            provider: BackupProvider::Local,
            local_path,
            local_candidate: None,
            forms: std::array::from_fn(|index| {
                ProviderForm::new(BackupProvider::ALL[index], data, window, cx)
            }),
            unlocked,
            busy: false,
            restore_count: None,
        }
    }

    pub(crate) fn set_local_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.local_path = path;
        cx.notify();
    }

    fn choose_local_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择已有的加密凭据库（.vault）".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = prompt.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.busy = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        this.local_candidate = paths.into_iter().next();
                        this.forms[BackupProvider::Local.index()].error = None;
                        this.forms[BackupProvider::Local.index()].feedback = None;
                    }
                    Ok(Ok(None)) => {}
                    _ => {
                        this.forms[BackupProvider::Local.index()].error =
                            Some("无法打开文件选择窗口".into())
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn request_local_open(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(path) = self.local_candidate.clone() else {
            return;
        };
        let password =
            Zeroizing::new(self.forms[BackupProvider::Local.index()].value("restore_password", cx));
        if !self.unlocked && password.is_empty() {
            self.fail("请输入所选凭据库的主密码".into(), cx);
            return;
        }
        self.busy = true;
        cx.emit(BackupEvent::OpenLocal {
            path,
            password: (!password.is_empty()).then_some(password),
        });
        cx.notify();
    }

    pub(crate) fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        cx.notify();
    }

    pub(crate) fn finish(
        &mut self,
        provider: BackupProvider,
        message: String,
        key: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = &mut self.forms[provider.index()];
        form.feedback = Some(message);
        form.error = None;
        if let Some(key) = key {
            if let Some(field) = form.fields.iter().find(|field| field.key == "object") {
                field
                    .state
                    .update(cx, |state, cx| state.set_value(key, window, cx));
            }
        }
        self.busy = false;
        cx.notify();
    }

    pub(crate) fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.forms[self.provider.index()].error = Some(message);
        self.restore_count = None;
        self.busy = false;
        cx.notify();
    }

    pub(crate) fn show_restore_preview(&mut self, count: usize, cx: &mut Context<Self>) {
        self.restore_count = Some(count);
        self.busy = false;
        cx.notify();
    }

    pub(crate) fn restored(
        &mut self,
        data: &VaultData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.forms = std::array::from_fn(|index| {
            ProviderForm::new(BackupProvider::ALL[index], data, window, cx)
        });
        self.unlocked = true;
        self.local_candidate = None;
        self.restore_count = None;
        self.busy = false;
        self.forms[self.provider.index()].feedback = Some(format!(
            "{} {} 台服务器，凭据库已解锁。",
            if self.provider == BackupProvider::Local {
                "已打开"
            } else {
                "已恢复"
            },
            data.servers.len()
        ));
        cx.notify();
    }

    fn request(&mut self, upload: Option<bool>, cx: &mut Context<Self>) {
        if self.busy || self.restore_count.is_some() {
            return;
        }
        let form = &self.forms[self.provider.index()];
        let result = (|| -> Result<BackupEvent> {
            let config = config_from_values(self.provider, |key| form.value(key, cx))?;
            if let Some(upload) = upload {
                ensure!(self.unlocked, "请先解锁或创建凭据库；恢复备份无需先解锁。");
                Ok(BackupEvent::Save {
                    config,
                    automatic: form.automatic,
                    upload,
                })
            } else {
                let key = form.value("object", cx).trim().to_owned();
                ensure!(
                    !key.is_empty(),
                    "请输入要恢复的 {} 对象 Key。",
                    self.provider.label()
                );
                let password = Zeroizing::new(form.value("restore_password", cx));
                ensure!(
                    self.unlocked || !password.is_empty(),
                    "请输入备份创建时使用的主密码。"
                );
                Ok(BackupEvent::Restore {
                    config,
                    key,
                    password: (!password.is_empty()).then_some(password),
                })
            }
        })();
        match result {
            Ok(event) => {
                let form = &mut self.forms[self.provider.index()];
                form.error = None;
                form.feedback = None;
                self.busy = true;
                cx.emit(event);
            }
            Err(error) => self.forms[self.provider.index()].error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn fields_view(&self, restore: bool) -> AnyElement {
        let mut fields = div().grid().grid_cols(2).gap_3().min_w_0();
        for field in &self.forms[self.provider.index()].fields {
            if ["object", "restore_password"].contains(&field.key) != restore {
                continue;
            }
            fields = fields.child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .gap_1()
                    .child(div().text_xs().child(field.label))
                    .child(if field.key == "restore_password" {
                        crate::ascii_password::input(&field.state, self.busy, true)
                    } else {
                        Input::new(&field.state)
                            .small()
                            .disabled(self.busy)
                            .into_any_element()
                    }),
            );
        }
        fields.into_any_element()
    }

    fn local_view(
        &self,
        providers: AnyElement,
        status: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::appearance::palette(cx);
        let mut body = div().flex().flex_col().gap_4()
            .child(div().text_sm().child("当前凭据库文件"))
            .child(div().p_3().rounded_md().bg(rgb(palette.bg)).text_sm().child(self.local_path.display().to_string()))
            .child(div().text_xs().text_color(rgb(palette.muted)).child("修改连接、主密码或云备份配置后，会自动保存到同一个加密文件。可选择本地磁盘、移动硬盘或网盘同步目录；上传与同步由网盘客户端负责。"));
        let mut actions = div().flex().flex_wrap().gap_2();
        if let Some(count) = self.restore_count {
            if let Some(target) = &self.local_candidate {
                body = body.child(div().text_sm().child("即将打开的文件")).child(
                    div()
                        .p_3()
                        .rounded_md()
                        .bg(rgb(palette.bg))
                        .text_sm()
                        .child(target.display().to_string()),
                );
            }
            body = body.child(div().text_sm().child(format!("已验证所选文件，包含 {count} 台服务器。确认后切换凭据库并关闭当前 SSH 会话，原文件保留。")));
            actions = actions
                .child(
                    Button::new("confirm-local-open")
                        .small()
                        .primary()
                        .label("确认切换凭据库")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.busy = true;
                            cx.emit(BackupEvent::ConfirmRestore);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("cancel-local-open")
                        .small()
                        .label("取消")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.restore_count = None;
                            cx.emit(BackupEvent::CancelRestore);
                            cx.notify();
                        })),
                );
        } else {
            body = body.child(div().mt_2().text_sm().child("打开已有凭据库"))
                .child(Button::new("choose-local-vault").small().label("选择已有的 .vault 文件").disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.choose_local_file(window, cx))))
                .when_some(self.local_candidate.clone(), |body, path| body.child(div().text_sm().child(path.display().to_string())))
                .child(self.fields_view(true))
                .child(div().text_xs().text_color(rgb(palette.muted)).child("首次使用请先创建主密码；打开已有文件可直接验证。若文件被其他设备修改，请重新加载后再编辑。所选位置无法访问时不会创建空库。"));
            actions = actions
                .child(
                    Button::new("local-save-as")
                        .small()
                        .primary()
                        .label("选择保存位置并使用")
                        .disabled(self.busy || !self.unlocked)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(BackupEvent::ChooseLocalSave))),
                )
                .child(
                    Button::new("verify-local-open")
                        .small()
                        .label("验证并打开所选文件")
                        .disabled(self.busy || self.local_candidate.is_none())
                        .on_click(cx.listener(|this, _, _, cx| this.request_local_open(cx))),
                )
                .child(
                    Button::new("reload-local-vault")
                        .small()
                        .label("重新加载当前文件")
                        .disabled(self.busy)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(BackupEvent::ReloadLocal))),
                );
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(div().flex_shrink_0().px_6().pb_3().child(providers))
            .child(div().flex_shrink_0().px_6().pb_2().child(status))
            .child(
                div()
                    .id("local-backup-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .pb_4()
                    .child(body),
            )
            .child(
                div()
                    .px_6()
                    .py_3()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(rgb(palette.line))
                    .child(actions),
            )
            .into_any_element()
    }
}

impl Render for BackupPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::appearance::palette(cx);
        let mut providers = div().flex().flex_wrap().gap_2();
        for provider in BackupProvider::ALL {
            providers = providers.child(
                Button::new(("backup-provider", provider.index()))
                    .small()
                    .label(provider.label())
                    .when(self.provider == provider, |button| button.primary())
                    .disabled(self.busy || self.restore_count.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.provider = provider;
                        cx.notify();
                    })),
            );
        }
        let form = &self.forms[self.provider.index()];
        let mut status = div().flex().flex_col().gap_2();
        if self.busy {
            status = status.child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.accent))
                    .child("正在处理，请稍候…"),
            );
        } else if let Some(error) = &form.error {
            status = status.child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.error))
                    .child(error.clone()),
            );
        } else if let Some(feedback) = &form.feedback {
            status = status
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(palette.accent))
                        .child(feedback.clone()),
                )
                .child(
                    Button::new("copy-backup-result")
                        .ghost()
                        .xsmall()
                        .label("复制备份结果")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(feedback) = &this.forms[this.provider.index()].feedback {
                                cx.write_to_clipboard(ClipboardItem::new_string(feedback.clone()));
                            }
                        })),
                );
        }
        let mut body = div().flex().flex_col().gap_4();
        if self.provider == BackupProvider::Local {
            return self.local_view(providers.into_any_element(), status.into_any_element(), cx);
        }
        if let Some(count) = self.restore_count {
            body = body
                .child(div().text_lg().child("备份验证成功"))
                .child(
                    div()
                        .text_sm()
                        .child(format!("已验证包含 {count} 台服务器的完整加密备份。")),
                )
                .child(div().text_sm().text_color(rgb(palette.warning)).child(
                    "确认后将替换本机凭据库并关闭当前 SSH 会话，原本地密文会保留为恢复副本。",
                ));
        } else {
            if !self.unlocked {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(rgb(palette.muted))
                        .child("解锁凭据库后可保存配置与上传备份；锁定时可填写原主密码恢复备份。"),
                );
            }
            if let Some(provider) = self.provider.s3_provider() {
                if let Some(endpoint) = provider.endpoint(form.value("region", cx).trim()) {
                    body = body.child(
                        div()
                            .text_xs()
                            .text_color(rgb(palette.muted))
                            .child(format!("自动 Endpoint：{endpoint}（可填写自定义地址覆盖）")),
                    );
                }
            } else {
                body =
                    body.child(div().text_xs().text_color(rgb(palette.muted)).child(
                        "填写 HTTPS WebDAV 集合地址，使用用户名和密码 / 应用专用密码认证。",
                    ));
            }
            body = body
                .child(self.fields_view(false))
                .child(
                    Checkbox::new("auto-backup")
                        .label("保存连接后自动加密备份到此供应商")
                        .checked(form.automatic)
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, checked, _, cx| {
                            this.forms[this.provider.index()].automatic = *checked;
                            cx.notify();
                        })),
                )
                .child(div().mt_2().text_sm().child("从备份恢复"))
                .child(self.fields_view(true));
        }
        let actions = if self.restore_count.is_some() {
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    Button::new("confirm-restore")
                        .primary()
                        .small()
                        .label("确认替换并恢复")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.busy {
                                this.busy = true;
                                cx.emit(BackupEvent::ConfirmRestore);
                                cx.notify();
                            }
                        })),
                )
                .child(
                    Button::new("cancel-restore")
                        .ghost()
                        .small()
                        .label("取消恢复")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.restore_count = None;
                            cx.emit(BackupEvent::CancelRestore);
                            cx.notify();
                        })),
                )
        } else {
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    Button::new("backup-submit")
                        .primary()
                        .small()
                        .label("保存并备份")
                        .disabled(self.busy || !self.unlocked)
                        .on_click(cx.listener(|this, _, _, cx| this.request(Some(true), cx))),
                )
                .child(
                    Button::new("backup-save")
                        .small()
                        .label("仅保存配置")
                        .disabled(self.busy || !self.unlocked)
                        .on_click(cx.listener(|this, _, _, cx| this.request(Some(false), cx))),
                )
                .child(
                    Button::new("restore-preview")
                        .small()
                        .label("下载并验证恢复")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| this.request(None, cx))),
                )
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(div().flex_shrink_0().px_6().pb_3().child(providers))
            .child(div().flex_shrink_0().px_6().pb_2().child(status))
            .child(
                div()
                    .id(("cloud-settings-scroll", self.provider.index()))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .pb_4()
                    .child(body),
            )
            .child(
                div()
                    .px_6()
                    .py_3()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(rgb(palette.line))
                    .child(actions),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{config_from_values, BackupProvider, CloudConfig};
    use syncssh_core::S3Provider;

    #[test]
    fn inline_forms_parse_each_provider_without_trimming_credentials() {
        for provider in BackupProvider::ALL {
            if provider == BackupProvider::Local {
                continue;
            }
            let config = config_from_values(provider, |key| match key {
                "bucket" => "  syncssh-backups-1250000000  ".into(),
                "region" => provider
                    .s3_provider()
                    .map(S3Provider::default_region)
                    .unwrap_or("unused")
                    .into(),
                "endpoint" if provider == BackupProvider::WebDav => {
                    " https://dav.example.test/backups/ ".into()
                }
                "endpoint" => String::new(),
                "access" => "test-access".into(),
                "secret" | "password" => " secret with spaces ".into(),
                "username" => "test-user".into(),
                "token" => "temporary-test-token".into(),
                "prefix" => " syncssh/backups ".into(),
                _ => unreachable!(),
            })
            .unwrap();
            assert_eq!(config.provider().label(), provider.label());
            match &config {
                CloudConfig::S3(config) => {
                    assert_eq!(Some(config.provider), provider.s3_provider());
                    assert_eq!(config.bucket, "syncssh-backups-1250000000");
                    assert_eq!(config.secret_key, " secret with spaces ");
                    assert_eq!(
                        config.session_token.as_deref(),
                        Some("temporary-test-token")
                    );
                    assert_eq!(config.prefix, "syncssh/backups");
                }
                CloudConfig::WebDav(config) => {
                    assert_eq!(config.endpoint, "https://dav.example.test/backups/");
                    assert_eq!(config.password, " secret with spaces ");
                    assert_eq!(config.prefix, "syncssh/backups");
                }
            }
        }
    }

    #[test]
    fn incomplete_provider_forms_are_rejected_before_emitting_requests() {
        for provider in BackupProvider::ALL {
            assert!(config_from_values(provider, |_| String::new()).is_err());
        }
    }
}
