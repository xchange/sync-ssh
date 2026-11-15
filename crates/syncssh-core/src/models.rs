use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    Password {
        password: String,
    },
    PrivateKey {
        pem: String,
        passphrase: Option<String>,
        #[serde(default)]
        certificate: Option<String>,
    },
    Agent,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Password { .. } => "Password([REDACTED])",
            Self::PrivateKey { .. } => "PrivateKey([REDACTED])",
            Self::Agent => "Agent",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerProfile {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub group: String,
    pub credential: Credential,
    #[serde(default)]
    pub connection: ConnectionOptions,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionOptions {
    #[serde(default)]
    pub auto_reconnect: bool,
    #[serde(default)]
    pub tunnels: Vec<TunnelRule>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TunnelMode {
    Local,
    Remote,
    Dynamic,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelRule {
    pub id: String,
    pub name: String,
    pub mode: TunnelMode,
    pub listen_host: String,
    pub listen_port: u16,
    #[serde(default)]
    pub target_host: String,
    #[serde(default)]
    pub target_port: u16,
    #[serde(default)]
    pub auto_start: bool,
}

impl TunnelRule {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.id.is_empty() && self.id.len() <= 128, "隧道 ID 无效");
        ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 256,
            "请输入隧道名称"
        );
        let valid_host = |host: &str| {
            !host.is_empty()
                && host.len() <= 253
                && !host
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\' | '@'))
        };
        ensure!(valid_host(&self.listen_host), "请输入有效监听地址");
        if self.mode != TunnelMode::Dynamic {
            ensure!(valid_host(&self.target_host), "请输入有效目标地址");
            ensure!(self.target_port != 0, "目标端口必须介于 1 和 65535 之间");
        }
        Ok(())
    }
}

impl ServerProfile {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.trim().is_empty() && self.id.len() <= 128,
            "服务器 ID 无效"
        );
        ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 256,
            "请输入服务器名称（最多 256 字节）"
        );
        ensure!(
            !self.host.is_empty() && self.host.len() <= 253,
            "请输入有效主机地址"
        );
        ensure!(
            !self.host.starts_with('-')
                && !self
                    .host
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\' | '@')),
            "主机地址不能包含空白、路径或用户名"
        );
        ensure!(self.port > 0, "端口必须介于 1 和 65535 之间");
        ensure!(
            !self.username.is_empty()
                && self.username.len() <= 128
                && !self
                    .username
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace()),
            "请输入有效的 SSH 用户名"
        );
        ensure!(self.group.len() <= 256, "分组名称过长");
        ensure!(
            self.connection.tunnels.len() <= 16,
            "每台服务器最多保存 16 条隧道规则"
        );
        let mut tunnel_ids = HashSet::new();
        for tunnel in &self.connection.tunnels {
            tunnel.validate()?;
            ensure!(tunnel_ids.insert(&tunnel.id), "服务器包含重复隧道 ID");
        }
        match &self.credential {
            Credential::Password { password } => ensure!(
                !password.is_empty() && password.len() <= 65536,
                "SSH 密码不能为空或过长"
            ),
            Credential::PrivateKey {
                pem,
                passphrase,
                certificate,
            } => {
                ensure!(
                    pem.len() <= 1024 * 1024 && pem.contains("PRIVATE KEY-----"),
                    "请输入 PEM 或 OpenSSH 格式的私钥内容"
                );
                ensure!(
                    passphrase.as_ref().is_none_or(|p| p.len() <= 65536),
                    "私钥口令过长"
                );
                ensure!(
                    certificate.as_ref().is_none_or(|c| c.len() <= 1024 * 1024),
                    "SSH 证书过长"
                );
            }
            Credential::Agent => {}
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum S3Provider {
    #[default]
    S3,
    Aliyun,
    Huawei,
    Tencent,
    Volcengine,
}

impl S3Provider {
    pub const ALL: [Self; 5] = [
        Self::S3,
        Self::Aliyun,
        Self::Huawei,
        Self::Tencent,
        Self::Volcengine,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::S3 => "S3",
            Self::Aliyun => "阿里云 OSS",
            Self::Huawei => "华为云 OBS",
            Self::Tencent => "腾讯云 COS",
            Self::Volcengine => "火山引擎 TOS",
        }
    }

    pub fn default_region(self) -> &'static str {
        match self {
            Self::S3 => "us-east-1",
            Self::Aliyun => "cn-hangzhou",
            Self::Huawei => "cn-north-4",
            Self::Tencent => "ap-guangzhou",
            Self::Volcengine => "cn-beijing",
        }
    }

    /// Regional service endpoints. Bucket names are added only by the transport.
    pub fn endpoint(self, region: &str) -> Option<String> {
        match self {
            Self::S3 => None,
            Self::Aliyun => Some(format!("https://s3.oss-{region}.aliyuncs.com")),
            Self::Huawei => Some(format!("https://obs.{region}.myhuaweicloud.com")),
            Self::Tencent => Some(format!("https://cos.{region}.myqcloud.com")),
            Self::Volcengine => Some(format!("https://tos-s3-{region}.volces.com")),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct S3Config {
    #[serde(default)]
    #[zeroize(skip)]
    pub provider: S3Provider,
    pub bucket: String,
    pub region: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    pub access_key: String,
    pub secret_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
    #[serde(default = "default_prefix")]
    pub prefix: String,
}

fn default_prefix() -> String {
    "syncssh/backups".into()
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            provider: S3Provider::S3,
            bucket: String::new(),
            region: "us-east-1".into(),
            endpoint: None,
            access_key: String::new(),
            secret_key: String::new(),
            session_token: None,
            prefix: default_prefix(),
        }
    }
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("S3Config([REDACTED])")
    }
}

impl S3Config {
    pub fn for_provider(provider: S3Provider) -> Self {
        let mut config = Self::default();
        config.provider = provider;
        config.region = provider.default_region().into();
        config
    }

    pub fn service_endpoint(&self) -> Option<String> {
        self.endpoint
            .clone()
            .or_else(|| self.provider.endpoint(&self.region))
    }

    /// object_store expects an already bucket-qualified endpoint in virtual-hosted mode.
    pub fn request_endpoint(&self) -> Result<Option<String>> {
        self.validate()?;
        let Some(endpoint) = self.service_endpoint() else {
            return Ok(None);
        };
        if self.provider == S3Provider::S3 {
            return Ok(Some(endpoint));
        }
        let mut url = url::Url::parse(&endpoint)?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("对象存储 Endpoint 缺少主机名"))?;
        let host = format!("{}.{}", self.bucket, host);
        url.set_host(Some(&host))?;
        Ok(Some(url.to_string().trim_end_matches('/').into()))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (3..=63).contains(&self.bucket.len())
                && self.bucket.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'-'
                    || b == b'.')
                && !self.bucket.starts_with(['-', '.'])
                && !self.bucket.ends_with(['-', '.']),
            "S3 bucket 名称无效"
        );
        ensure!(
            !self.region.is_empty()
                && self.region.len() <= 128
                && self
                    .region
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "S3 region 无效"
        );
        ensure!(
            !self.access_key.trim().is_empty() && self.access_key.len() <= 1024,
            "请输入 S3 Access Key"
        );
        ensure!(
            !self.secret_key.is_empty() && self.secret_key.len() <= 4096,
            "请输入 S3 Secret Key"
        );
        ensure!(
            self.session_token.as_ref().is_none_or(|v| v.len() <= 16384),
            "S3 Session Token 过长"
        );
        ensure!(
            self.prefix.len() <= 512
                && !self.prefix.starts_with('/')
                && !self.prefix.ends_with('/')
                && !self.prefix.chars().any(|c| c.is_control())
                && !self
                    .prefix
                    .split('/')
                    .any(|p| p == "." || p == ".." || p.is_empty()),
            "备份前缀必须是非空相对路径，不可包含 . 或 .."
        );
        if let Some(endpoint) = &self.endpoint {
            let parsed = url::Url::parse(endpoint)
                .map_err(|_| anyhow::anyhow!("S3 Endpoint 不是有效 URL"))?;
            ensure!(
                parsed.scheme() == "https" && parsed.host_str().is_some(),
                "S3 Endpoint 必须使用 HTTPS"
            );
            ensure!(
                parsed.username().is_empty()
                    && parsed.password().is_none()
                    && parsed.query().is_none()
                    && parsed.fragment().is_none(),
                "S3 Endpoint 不可包含凭据、查询参数或片段"
            );
            ensure!(
                parsed.path() == "/" || parsed.path().is_empty(),
                "S3 Endpoint 只能包含协议、主机和可选端口"
            );
            if self.provider != S3Provider::S3 {
                ensure!(
                    !parsed
                        .host_str()
                        .unwrap_or_default()
                        .starts_with(&format!("{}.", self.bucket)),
                    "请填写服务 Endpoint，不要包含 Bucket 名称"
                );
            }
        }
        if self.provider != S3Provider::S3 {
            ensure!(
                !self.bucket.contains('.'),
                "此供应商的 Bucket 名称不能包含点号"
            );
        }
        if self.provider == S3Provider::Tencent {
            ensure!(
                self.bucket
                    .rsplit_once('-')
                    .is_some_and(|(name, app_id)| !name.is_empty()
                        && !app_id.is_empty()
                        && app_id.bytes().all(|b| b.is_ascii_digit())),
                "腾讯云 Bucket 需要包含 APPID，例如 syncssh-1250000000"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectStorageBackup {
    pub config: S3Config,
    #[serde(default)]
    pub backup_on_save: bool,
}

/// WebDAV credentials are serialized only inside the encrypted vault.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct WebDavConfig {
    /// An existing HTTPS collection, e.g. https://cloud.example/remote.php/dav/files/user/.
    pub endpoint: String,
    pub username: String,
    pub password: String,
    #[serde(default = "default_prefix")]
    pub prefix: String,
}

impl Default for WebDavConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            username: String::new(),
            password: String::new(),
            prefix: default_prefix(),
        }
    }
}

impl fmt::Debug for WebDavConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebDavConfig([REDACTED])")
    }
}

impl WebDavConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.endpoint.len() <= 4096
                && !self
                    .endpoint
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace()),
            "WebDAV 地址过长或包含空白，请对地址中的空格进行 URL 编码"
        );
        // Validate before URL normalization can collapse literal/encoded dot segments.
        let raw_endpoint = self.endpoint.to_ascii_lowercase();
        ensure!(
            !["%2e", "%2f", "%5c", "%00", "%25"]
                .iter()
                .any(|s| raw_endpoint.contains(s))
                && !self.endpoint.contains('\\')
                && !self.endpoint.split('/').any(|s| s == "." || s == ".."),
            "WebDAV 地址不可包含编码的路径分隔符或相对路径"
        );
        let parsed = url::Url::parse(&self.endpoint)
            .map_err(|_| anyhow::anyhow!("请输入有效的 WebDAV 集合地址"))?;
        ensure!(
            parsed.scheme() == "https" && parsed.host_str().is_some(),
            "WebDAV 必须使用 HTTPS"
        );
        ensure!(
            parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "WebDAV 地址不可包含凭据、查询参数或片段"
        );
        ensure!(
            !self.username.is_empty()
                && self.username.len() <= 1024
                && !self.username.chars().any(|c| c.is_control() || c == ':'),
            "请输入 WebDAV 用户名（不可包含冒号或控制字符）"
        );
        ensure!(
            !self.password.is_empty() && self.password.len() <= 16384,
            "请输入 WebDAV 密码或应用专用密码"
        );
        ensure!(
            self.prefix.len() <= 512
                && !self
                    .prefix
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '\\' | '%' | '?' | '#'))
                && !self
                    .prefix
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == ".."),
            "WebDAV 备份目录必须是非空相对路径，不可包含 .、..、反斜杠或 URL 转义字符"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultData {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub servers: Vec<ServerProfile>,
    #[serde(default)]
    pub s3: Option<S3Config>,
    #[serde(default)]
    pub object_stores: BTreeMap<S3Provider, ObjectStorageBackup>,
    #[serde(default)]
    pub webdav: Option<WebDavConfig>,
    #[serde(default = "enabled")]
    pub effects_enabled: bool,
    #[serde(default)]
    pub backup_on_save: bool,
    #[serde(default)]
    pub webdav_backup_on_save: bool,
}
fn schema_version() -> u32 {
    1
}
fn enabled() -> bool {
    true
}
impl Default for VaultData {
    fn default() -> Self {
        Self {
            schema_version: 1,
            servers: Vec::new(),
            s3: None,
            object_stores: BTreeMap::new(),
            webdav: None,
            effects_enabled: true,
            backup_on_save: false,
            webdav_backup_on_save: false,
        }
    }
}
impl VaultData {
    pub fn object_storage(&self, provider: S3Provider) -> Option<&S3Config> {
        if provider == S3Provider::S3 {
            self.s3.as_ref()
        } else {
            self.object_stores.get(&provider).map(|entry| &entry.config)
        }
    }

    pub fn object_storage_automatic(&self, provider: S3Provider) -> bool {
        if provider == S3Provider::S3 {
            self.backup_on_save
        } else {
            self.object_stores
                .get(&provider)
                .is_some_and(|entry| entry.backup_on_save)
        }
    }

    pub fn set_object_storage(&mut self, config: S3Config, backup_on_save: bool) {
        if config.provider == S3Provider::S3 {
            self.s3 = Some(config);
            self.backup_on_save = backup_on_save;
        } else {
            self.object_stores.insert(
                config.provider,
                ObjectStorageBackup {
                    config,
                    backup_on_save,
                },
            );
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "不支持的凭据库版本");
        ensure!(self.servers.len() <= 10000, "服务器数量超过上限");
        let mut ids = HashSet::new();
        for server in &self.servers {
            server.validate()?;
            if !ids.insert(&server.id) {
                bail!("凭据库包含重复服务器 ID");
            }
        }
        if let Some(s3) = &self.s3 {
            ensure!(s3.provider == S3Provider::S3, "S3 备份配置的供应商不匹配");
            s3.validate()?;
        }
        for (provider, entry) in &self.object_stores {
            ensure!(
                *provider != S3Provider::S3 && entry.config.provider == *provider,
                "对象存储配置的供应商不匹配"
            );
            entry.config.validate()?;
        }
        if let Some(webdav) = &self.webdav {
            webdav.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> ServerProfile {
        ServerProfile {
            id: "one".into(),
            name: "example".into(),
            host: "example.com".into(),
            port: 22,
            username: "root".into(),
            group: String::new(),
            credential: Credential::Password {
                password: "super-secret".into(),
            },
            connection: ConnectionOptions::default(),
        }
    }
    #[test]
    fn invalid_profile_and_duplicate_ids_are_rejected() {
        let mut p = profile();
        p.validate().unwrap();
        p.host = "-oProxyCommand=anything".into();
        assert!(p.validate().is_err());
        p = profile();
        p.port = 0;
        assert!(p.validate().is_err());
        let v = VaultData {
            servers: vec![profile(), profile()],
            ..Default::default()
        };
        assert!(v.validate().is_err());
    }
    #[test]
    fn old_profiles_load_with_safe_defaults_and_tunnel_settings_roundtrip() {
        let mut old = serde_json::to_value(profile()).unwrap();
        old.as_object_mut().unwrap().remove("connection");
        let mut p: ServerProfile = serde_json::from_value(old).unwrap();
        assert_eq!(p.connection, ConnectionOptions::default());
        p.connection.auto_reconnect = true;
        p.connection.tunnels.push(TunnelRule {
            id: "rule".into(),
            name: "数据库".into(),
            mode: TunnelMode::Local,
            listen_host: "127.0.0.1".into(),
            listen_port: 0,
            target_host: "db.internal".into(),
            target_port: 5432,
            auto_start: true,
        });
        p.validate().unwrap();
        let encoded = serde_json::to_vec(&p).unwrap();
        assert_eq!(
            serde_json::from_slice::<ServerProfile>(&encoded).unwrap(),
            p
        );
        p.connection.tunnels.push(p.connection.tunnels[0].clone());
        assert!(p.validate().is_err());
        p.connection.tunnels.pop();
        p.connection.tunnels[0].target_port = 0;
        assert!(p.validate().is_err());
    }
    #[test]
    fn secrets_are_not_in_debug_output() {
        assert!(!format!("{:?}", profile()).contains("super-secret"));
        let mut config = S3Config::default();
        config.secret_key = "cloud-secret".into();
        assert!(!format!("{config:?}").contains("cloud-secret"));
    }
    #[test]
    fn endpoint_requires_https_and_no_embedded_credentials() {
        let mut config = S3Config::default();
        config.bucket = "my-backup".into();
        config.access_key = "a".into();
        config.secret_key = "s".into();
        config.validate().unwrap();
        for url in [
            "http://localhost:9000",
            "https://user:pass@example.com",
            "https://example.com/?token=x",
        ] {
            config.endpoint = Some(url.into());
            assert!(config.validate().is_err());
        }
        config.endpoint = Some("https://s3.example.com:9443".into());
        config.validate().unwrap();
    }

    #[test]
    fn provider_endpoints_use_the_selected_region_and_reject_bucket_qualified_overrides() {
        for (provider, region, expected) in [
            (
                S3Provider::Aliyun,
                "cn-shanghai",
                "https://backup-test-1250000000.s3.oss-cn-shanghai.aliyuncs.com",
            ),
            (
                S3Provider::Huawei,
                "cn-east-3",
                "https://backup-test-1250000000.obs.cn-east-3.myhuaweicloud.com",
            ),
            (
                S3Provider::Tencent,
                "ap-shanghai",
                "https://backup-test-1250000000.cos.ap-shanghai.myqcloud.com",
            ),
            (
                S3Provider::Volcengine,
                "cn-shanghai",
                "https://backup-test-1250000000.tos-s3-cn-shanghai.volces.com",
            ),
        ] {
            let mut config = S3Config::for_provider(provider);
            config.bucket = "backup-test-1250000000".into();
            config.access_key = "test-access".into();
            config.secret_key = "test-secret".into();
            config.region = region.into();
            assert_eq!(
                config.request_endpoint().unwrap().as_deref(),
                Some(expected)
            );
            config.endpoint = Some("https://storage.example.test:9443".into());
            assert_eq!(
                config.request_endpoint().unwrap().as_deref(),
                Some("https://backup-test-1250000000.storage.example.test:9443")
            );
            config.endpoint = Some(expected.into());
            assert!(config.validate().is_err());
            config.endpoint = Some("http://storage.example.test".into());
            assert!(config.validate().is_err());
        }
        let mut cos = S3Config::for_provider(S3Provider::Tencent);
        cos.bucket = "missing-app-id".into();
        cos.access_key = "test-access".into();
        cos.secret_key = "test-secret".into();
        assert!(cos.validate().is_err());
    }
}
