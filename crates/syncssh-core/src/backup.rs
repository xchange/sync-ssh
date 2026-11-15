//! S3 transport accepts only the bounded encrypted envelope. Object names are unique snapshots;
//! restoring is explicit, so concurrent clients cannot overwrite one another's latest vault.
use crate::{crypto, S3Config, S3Provider};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use object_store::{
    aws::AmazonS3Builder, path::Path, ClientOptions, ObjectStore, PutMode, PutOptions, RetryConfig,
};
use rand::{rngs::OsRng, RngCore};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct BackupObject {
    pub key: String,
    pub last_modified: DateTime<Utc>,
    pub size: u64,
}

fn client_builder(config: &S3Config) -> Result<AmazonS3Builder> {
    config.validate()?;
    let mut headers = reqwest::header::HeaderMap::new();
    if config.provider == S3Provider::Aliyun {
        headers.insert(
            "x-oss-s3-compat",
            reqwest::header::HeaderValue::from_static("true"),
        );
    }
    let options = ClientOptions::new()
        .with_allow_http(false)
        .with_default_headers(headers)
        .with_timeout(Duration::from_secs(60))
        .with_connect_timeout(Duration::from_secs(15));
    let mut builder = AmazonS3Builder::new()
        .with_bucket_name(&config.bucket)
        .with_region(&config.region)
        .with_access_key_id(&config.access_key)
        .with_secret_access_key(&config.secret_key)
        .with_virtual_hosted_style_request(config.provider != S3Provider::S3)
        .with_unsigned_payload(config.provider == S3Provider::Huawei)
        .with_allow_http(false)
        .with_client_options(options)
        .with_retry(RetryConfig {
            max_retries: 2,
            retry_timeout: Duration::from_secs(90),
            ..Default::default()
        });
    if let Some(endpoint) = config.request_endpoint()? {
        builder = builder.with_endpoint(endpoint);
    }
    if let Some(token) = &config.session_token {
        builder = builder.with_token(token);
    }
    Ok(builder)
}

fn client(config: &S3Config) -> Result<impl ObjectStore> {
    client_builder(config)?
        .build()
        .map_err(|_| anyhow::anyhow!("S3 配置无效，无法创建 HTTPS 客户端"))
}

pub(crate) fn snapshot_key(prefix: &str) -> Result<String> {
    let mut random = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut random)
        .context("操作系统随机数生成失败")?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{prefix}/{}-{suffix}.vault",
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
    ))
}

fn validated_key(config: &S3Config, key: &str) -> Result<Path> {
    ensure!(
        key.starts_with(&format!("{}/", config.prefix))
            && key.ends_with(".vault")
            && key.len() <= 1024,
        "备份对象不在配置的前缀目录内"
    );
    ensure!(
        !key.split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
            && !key.chars().any(char::is_control),
        "备份对象路径无效"
    );
    Path::parse(key).context("备份对象路径无效")
}

/// Uploads a new immutable snapshot. Requires s3:PutObject for the configured prefix.
pub async fn upload(config: &S3Config, encrypted: &[u8]) -> Result<String> {
    let store = client(config)?;
    upload_to(&store, &config.prefix, encrypted).await
}

async fn upload_to(store: &dyn ObjectStore, prefix: &str, encrypted: &[u8]) -> Result<String> {
    crypto::validate_envelope(encrypted)?;
    let key = snapshot_key(prefix)?;
    let options = PutOptions {
        mode: PutMode::Create,
        ..Default::default()
    };
    store
        .put_opts(&Path::parse(&key)?, encrypted.to_vec().into(), options)
        .await
        .map_err(|_| {
            anyhow::anyhow!("S3 上传失败，请检查网络、HTTPS Endpoint、密钥及 PutObject 权限")
        })?;
    Ok(key)
}

/// Download encrypted bytes only; the caller must authenticate with vault::decode/import_encrypted.
pub async fn download(config: &S3Config, key: &str) -> Result<Vec<u8>> {
    let store = client(config)?;
    download_from(&store, &validated_key(config, key)?).await
}

async fn download_from(store: &dyn ObjectStore, key: &Path) -> Result<Vec<u8>> {
    let result = store
        .get(key)
        .await
        .map_err(|_| anyhow::anyhow!("S3 下载失败，请检查对象路径、网络及 GetObject 权限"))?;
    ensure!(
        result.meta.size <= crypto::MAX_ENVELOPE_BYTES as u64,
        "远程备份超过大小上限"
    );
    let mut stream = result.into_stream();
    let mut encrypted = Vec::new();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|_| anyhow::anyhow!("S3 下载中断"))?
    {
        ensure!(
            encrypted.len().saturating_add(chunk.len()) <= crypto::MAX_ENVELOPE_BYTES,
            "远程备份超过大小上限"
        );
        encrypted.extend_from_slice(&chunk);
    }
    crypto::validate_envelope(&encrypted)?;
    Ok(encrypted)
}

/// Lists the newest 1,000 snapshots (s3:ListBucket required).
/// Refuses prefixes with over 10,000 objects instead of silently showing stale snapshots.
pub async fn list(config: &S3Config) -> Result<Vec<BackupObject>> {
    let store = client(config)?;
    let prefix = Path::parse(&config.prefix)?;
    let mut stream = store.list(Some(&prefix)).take(10001);
    let mut scanned = 0;
    let mut snapshots = Vec::new();
    while let Some(meta) = stream
        .try_next()
        .await
        .map_err(|_| anyhow::anyhow!("无法列出 S3 备份，请检查 ListBucket 权限及网络"))?
    {
        scanned += 1;
        ensure!(
            scanned <= 10000,
            "备份目录超过 10,000 个对象，请在 S3 设置生命周期规则后重试"
        );
        let key = meta.location.to_string();
        if validated_key(config, &key).is_ok() && meta.size <= crypto::MAX_ENVELOPE_BYTES as u64 {
            snapshots.push(BackupObject {
                key,
                last_modified: meta.last_modified,
                size: meta.size,
            });
        }
    }
    snapshots.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    snapshots.truncate(1000);
    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::client::{
        HttpClient, HttpConnector, HttpError, HttpRequest, HttpResponse, HttpService,
    };
    use object_store::memory::InMemory;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug)]
    struct RecordingTransport {
        requests: Arc<Mutex<Vec<HttpRequest>>>,
        encrypted: Vec<u8>,
    }

    impl HttpConnector for RecordingTransport {
        fn connect(&self, _: &ClientOptions) -> object_store::Result<HttpClient> {
            Ok(HttpClient::new(self.clone()))
        }
    }

    #[async_trait::async_trait]
    impl HttpService for RecordingTransport {
        async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.requests.lock().unwrap().push(request);
            let mut response = HttpResponse::new(self.encrypted.clone().into());
            response.headers_mut().insert(
                "content-length",
                self.encrypted.len().to_string().parse().unwrap(),
            );
            response.headers_mut().insert(
                "last-modified",
                "Sat, 14 Nov 2026 00:00:00 GMT".parse().unwrap(),
            );
            response
                .headers_mut()
                .insert("etag", "\"test-etag\"".parse().unwrap());
            Ok(response)
        }
    }

    #[tokio::test]
    async fn every_provider_signs_and_routes_encrypted_snapshots() {
        let encrypted =
            crypto::encrypt("a unique master password", b"private SSH credentials").unwrap();
        for provider in S3Provider::ALL {
            let mut config = S3Config::for_provider(provider);
            config.bucket = "syncssh-test-1250000000".into();
            config.access_key = "test-access-key".into();
            config.secret_key = "test-secret-key".into();
            config.session_token = Some("test-session-token".into());
            let transport = RecordingTransport {
                requests: Arc::default(),
                encrypted: encrypted.clone(),
            };
            let store = client_builder(&config)
                .unwrap()
                .with_http_connector(transport.clone())
                .build()
                .unwrap();
            let key = upload_to(&store, &config.prefix, &encrypted).await.unwrap();
            assert_eq!(
                download_from(&store, &validated_key(&config, &key).unwrap())
                    .await
                    .unwrap(),
                encrypted
            );
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let expected_host = match provider {
                S3Provider::S3 => "s3.us-east-1.amazonaws.com".into(),
                _ => format!(
                    "{}.{}",
                    config.bucket,
                    url::Url::parse(&config.service_endpoint().unwrap())
                        .unwrap()
                        .host_str()
                        .unwrap()
                ),
            };
            for request in requests.iter() {
                assert_eq!(request.uri().scheme_str(), Some("https"));
                assert_eq!(request.uri().host(), Some(expected_host.as_str()));
                let expected_path = if provider == S3Provider::S3 {
                    format!("/{}/{key}", config.bucket)
                } else {
                    format!("/{key}")
                };
                assert_eq!(request.uri().path(), expected_path);
                let authorization = request.headers()["authorization"].to_str().unwrap();
                assert!(authorization.starts_with("AWS4-HMAC-SHA256 "));
                assert!(authorization.contains(&format!("/{}/s3/aws4_request", config.region)));
                assert_eq!(
                    request.headers()["x-amz-security-token"],
                    "test-session-token"
                );
                if provider == S3Provider::Huawei {
                    assert_eq!(
                        request.headers()["x-amz-content-sha256"],
                        "UNSIGNED-PAYLOAD"
                    );
                }
            }
            assert_eq!(requests[0].method(), "PUT");
            assert_eq!(requests[0].headers()["if-none-match"], "*");
            assert_eq!(requests[0].body().content_length(), encrypted.len());
            assert_eq!(requests[1].method(), "GET");
        }
    }
    #[tokio::test]
    async fn snapshot_transport_roundtrip_is_immutable_and_ciphertext_only() {
        let store = InMemory::new();
        let bytes = crypto::encrypt("a unique master password", b"ssh-password-secret").unwrap();
        let first = upload_to(&store, "test/snapshots", &bytes).await.unwrap();
        let second = upload_to(&store, "test/snapshots", &bytes).await.unwrap();
        assert_ne!(first, second);
        assert_eq!(
            download_from(&store, &Path::parse(&first).unwrap())
                .await
                .unwrap(),
            bytes
        );
        assert_eq!(store.list(None).collect::<Vec<_>>().await.len(), 2);
        assert!(upload_to(&store, "test/snapshots", b"plaintext-secret")
            .await
            .is_err());
    }
    #[test]
    fn restore_cannot_escape_prefix() {
        let config = S3Config::default();
        assert!(validated_key(&config, "other/data.vault").is_err());
        assert!(validated_key(&config, "syncssh/backups/../evil.vault").is_err());
        assert!(validated_key(&config, "syncssh/backups/20260101.vault").is_ok());
    }
}
