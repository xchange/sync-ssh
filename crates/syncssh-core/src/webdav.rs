//! WebDAV transport for the same authenticated encrypted envelope used by S3.
//! Credentials are sent only to the configured HTTPS origin; redirects are never followed.
use crate::{backup::BackupObject, crypto, WebDavConfig};
use anyhow::{bail, ensure, Result};
use chrono::{DateTime, Utc};
use percent_encoding::percent_decode_str;
use reqwest::{header, redirect::Policy, Client, Method, Response, StatusCode};
use std::time::Duration;
use url::Url;

const MAX_LIST_BYTES: usize = 4 * 1024 * 1024;
const MAX_LIST_OBJECTS: usize = 10_000;
const DAV: &str = "DAV:";
const PROPERTIES: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>";

struct Transport<'a> {
    config: &'a WebDavConfig,
    client: Client,
    root: Url,
}

fn http_client() -> Result<Client> {
    Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("SyncSSH/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| anyhow::anyhow!("无法创建 WebDAV HTTPS 客户端"))
}

impl<'a> Transport<'a> {
    fn new(config: &'a WebDavConfig) -> Result<Self> {
        config.validate()?;
        let mut root =
            Url::parse(&config.endpoint).map_err(|_| anyhow::anyhow!("WebDAV 地址无效"))?;
        if !root.path().ends_with('/') {
            let path = format!("{}/", root.path());
            root.set_path(&path);
        }
        Ok(Self {
            config,
            client: http_client()?,
            root,
        })
    }

    fn url(&self, key: &str, collection: bool) -> Result<Url> {
        let mut url = self.root.clone();
        let mut parts = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("WebDAV 地址无效"))?;
        parts.pop_if_empty();
        for part in key.split('/') {
            ensure!(safe_segment(part), "WebDAV 备份路径无效");
            parts.push(part);
        }
        if collection {
            parts.push("");
        }
        drop(parts);
        Ok(url)
    }

    fn request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .basic_auth(&self.config.username, Some(&self.config.password))
    }

    async fn propfind(&self, url: Url, depth: &'static str) -> Result<Response> {
        self.request(Method::from_bytes(b"PROPFIND").unwrap(), url)
            .header("Depth", depth)
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(PROPERTIES)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("WebDAV 查询失败，请检查网络、HTTPS 地址及权限"))
    }

    async fn ensure_collections(&self) -> Result<()> {
        let mut prefix = String::new();
        for part in self.config.prefix.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let url = self.url(&prefix, true)?;
            let response = self
                .request(Method::from_bytes(b"MKCOL").unwrap(), url.clone())
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("WebDAV 创建目录失败，请检查网络及写入权限"))?;
            if response.status() == StatusCode::CREATED {
                continue;
            }
            if response.status() == StatusCode::METHOD_NOT_ALLOWED {
                // 405 is also returned for non-collections. Confirm the exact resource type.
                let check = self.propfind(url.clone(), "0").await?;
                check_status(check.status(), &[StatusCode::MULTI_STATUS], "确认目录")?;
                let bytes = bounded_body(check, MAX_LIST_BYTES).await?;
                ensure!(
                    is_collection(&bytes, &url)?,
                    "WebDAV 备份路径已被文件占用，或不是可访问目录"
                );
                continue;
            }
            check_status(response.status(), &[StatusCode::CREATED], "创建目录")?;
        }
        Ok(())
    }

    async fn upload(&self, encrypted: &[u8]) -> Result<String> {
        crypto::validate_envelope(encrypted)?;
        let key = crate::backup::snapshot_key(&self.config.prefix)?;
        self.ensure_collections().await?;
        let response = self
            .request(Method::PUT, self.url(&key, false)?)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::IF_NONE_MATCH, "*")
            .body(encrypted.to_vec())
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("WebDAV 上传失败，请检查网络及写入权限"))?;
        check_status(
            response.status(),
            &[StatusCode::CREATED, StatusCode::NO_CONTENT, StatusCode::OK],
            "上传",
        )?;
        Ok(key)
    }

    async fn download(&self, key: &str) -> Result<Vec<u8>> {
        validated_key(self.config, key)?;
        let response = self
            .request(Method::GET, self.url(key, false)?)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("WebDAV 下载失败，请检查网络及读取权限"))?;
        check_status(response.status(), &[StatusCode::OK], "下载")?;
        let encrypted = bounded_body(response, crypto::MAX_ENVELOPE_BYTES).await?;
        crypto::validate_envelope(&encrypted)?;
        Ok(encrypted)
    }

    async fn list(&self) -> Result<Vec<BackupObject>> {
        let collection = self.url(&self.config.prefix, true)?;
        let response = self.propfind(collection.clone(), "1").await?;
        check_status(response.status(), &[StatusCode::MULTI_STATUS], "列出备份")?;
        let bytes = bounded_body(response, MAX_LIST_BYTES).await?;
        parse_list(&bytes, &collection, self.config)
    }
}

/// Upload a distinct encrypted snapshot; the configured endpoint collection must already exist.
pub async fn upload(config: &WebDavConfig, encrypted: &[u8]) -> Result<String> {
    // Validate before any network traffic, including directory creation.
    crypto::validate_envelope(encrypted)?;
    Transport::new(config)?.upload(encrypted).await
}

/// Download encrypted bytes. Authenticate with vault::decode before presenting a restore preview.
pub async fn download(config: &WebDavConfig, key: &str) -> Result<Vec<u8>> {
    Transport::new(config)?.download(key).await
}

/// List up to 1,000 newest snapshots. Refuse oversized listings instead of silently truncating input.
pub async fn list(config: &WebDavConfig) -> Result<Vec<BackupObject>> {
    Transport::new(config)?.list().await
}

fn safe_segment(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && !part
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | '%' | '?' | '#'))
}

fn validated_key(config: &WebDavConfig, key: &str) -> Result<()> {
    let prefix = format!("{}/", config.prefix);
    let name = key.strip_prefix(&prefix).unwrap_or("");
    ensure!(
        key.len() <= 1024 && name.ends_with(".vault") && safe_segment(name),
        "WebDAV 备份文件必须位于配置目录内，且使用 .vault 后缀"
    );
    Ok(())
}

fn check_status(status: StatusCode, accepted: &[StatusCode], action: &str) -> Result<()> {
    if accepted.contains(&status) {
        return Ok(());
    }
    let reason = match status.as_u16() {
        301..=399 => "服务器返回重定向；请填写最终 HTTPS WebDAV 地址",
        401 => "用户名或密码错误",
        403 => "没有访问权限",
        404 => "地址或备份文件不存在",
        409 => "父目录不存在，请确认 WebDAV 根地址",
        412 => "同名备份已存在，未覆盖原文件",
        423 => "目标被锁定，请稍后重试",
        507 => "服务器存储空间不足",
        _ => "服务器未完成操作，请检查地址、权限和服务状态",
    };
    bail!("WebDAV {action}失败（HTTP {}）：{reason}", status.as_u16())
}

async fn bounded_body(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        response.content_length().is_none_or(|n| n <= limit as u64),
        "WebDAV 响应超过大小上限"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("WebDAV 下载中断"))?
    {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= limit,
            "WebDAV 响应超过大小上限"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn document(bytes: &[u8]) -> Result<roxmltree::Document<'_>> {
    ensure!(bytes.len() <= MAX_LIST_BYTES, "WebDAV 目录响应过大");
    let text =
        std::str::from_utf8(bytes).map_err(|_| anyhow::anyhow!("WebDAV 目录不是有效 UTF-8 XML"))?;
    // roxmltree's default disables DTDs and entity declarations.
    let doc =
        roxmltree::Document::parse(text).map_err(|_| anyhow::anyhow!("WebDAV 目录 XML 无效"))?;
    ensure!(
        doc.root_element().has_tag_name((DAV, "multistatus")),
        "WebDAV 目录缺少 DAV multistatus"
    );
    Ok(doc)
}

fn success_props<'a, 'i>(
    response: roxmltree::Node<'a, 'i>,
) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> {
    response
        .children()
        .filter(|n| n.has_tag_name((DAV, "propstat")))
        .filter_map(|ps| {
            let status = ps
                .children()
                .find(|n| n.has_tag_name((DAV, "status")))?
                .text()?;
            let mut parts = status.split_whitespace();
            let protocol = parts.next()?;
            let code = parts.next()?.parse::<u16>().ok()?;
            (protocol.starts_with("HTTP/") && (200..300).contains(&code))
                .then(|| ps.children().find(|n| n.has_tag_name((DAV, "prop"))))
                .flatten()
        })
}

fn href_url(response: roxmltree::Node<'_, '_>, collection: &Url) -> Option<Url> {
    let href = response
        .children()
        .find(|n| n.has_tag_name((DAV, "href")))?
        .text()?
        .trim();
    let url = collection.join(href).ok()?;
    (url.origin() == collection.origin()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

fn is_collection(bytes: &[u8], expected: &Url) -> Result<bool> {
    let doc = document(bytes)?;
    Ok(doc
        .root_element()
        .children()
        .filter(|n| n.has_tag_name((DAV, "response")))
        .any(|response| {
            href_url(response, expected).is_some_and(|u| {
                u.path().trim_end_matches('/') == expected.path().trim_end_matches('/')
            }) && success_props(response).any(|prop| {
                prop.children()
                    .filter(|n| n.has_tag_name((DAV, "resourcetype")))
                    .any(|n| n.children().any(|n| n.has_tag_name((DAV, "collection"))))
            })
        }))
}

fn parse_list(bytes: &[u8], collection: &Url, config: &WebDavConfig) -> Result<Vec<BackupObject>> {
    let doc = document(bytes)?;
    let mut snapshots = std::collections::BTreeMap::new();
    for (index, response) in doc
        .root_element()
        .children()
        .filter(|n| n.has_tag_name((DAV, "response")))
        .enumerate()
    {
        ensure!(
            index <= MAX_LIST_OBJECTS,
            "WebDAV 目录超过 10,000 个对象，请清理旧备份后重试"
        );
        let Some(url) = href_url(response, collection) else {
            continue;
        };
        let Some(encoded_name) = url.path().strip_prefix(collection.path()) else {
            continue;
        };
        let Ok(name) = percent_decode_str(encoded_name).decode_utf8() else {
            continue;
        };
        let key = format!("{}/{}", config.prefix, name);
        if validated_key(config, &key).is_err() {
            continue;
        }
        let mut is_dir = false;
        let mut length = None;
        let mut modified = None;
        for prop in success_props(response) {
            for node in prop.children() {
                if node.has_tag_name((DAV, "resourcetype")) {
                    is_dir |= node.children().any(|n| n.has_tag_name((DAV, "collection")));
                } else if node.has_tag_name((DAV, "getcontentlength")) {
                    length = node.text().and_then(|s| s.trim().parse::<u64>().ok());
                } else if node.has_tag_name((DAV, "getlastmodified")) {
                    modified = node
                        .text()
                        .and_then(|s| DateTime::parse_from_rfc2822(s.trim()).ok())
                        .map(|d| d.with_timezone(&Utc));
                }
            }
        }
        if let (false, Some(size), Some(last_modified)) = (is_dir, length, modified) {
            if size <= crypto::MAX_ENVELOPE_BYTES as u64 {
                snapshots.insert(
                    key.clone(),
                    BackupObject {
                        key,
                        last_modified,
                        size,
                    },
                );
            }
        }
    }
    let mut snapshots: Vec<_> = snapshots.into_values().collect();
    snapshots.sort_by(|a, b| {
        b.last_modified
            .cmp(&a.last_modified)
            .then_with(|| b.key.cmp(&a.key))
    });
    snapshots.truncate(1000);
    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::{BTreeMap, HashSet},
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex, OnceLock,
        },
        thread,
    };

    #[derive(Clone, Debug)]
    struct Request {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }
    struct Reply {
        status: u16,
        body: Vec<u8>,
        extra_headers: String,
    }
    impl Reply {
        fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                body: body.into(),
                extra_headers: String::new(),
            }
        }
    }
    struct Mock {
        endpoint: String,
        requests: Arc<Mutex<Vec<Request>>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Mock {
        fn new(mut handler: impl FnMut(&Request) -> Reply + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}/dav/users/test/", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let thread = thread::spawn(move || {
                while !stopped.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let request = read_request(&mut stream);
                            let reply = handler(&request);
                            recorded.lock().unwrap().push(request);
                            let length = if reply
                                .extra_headers
                                .to_ascii_lowercase()
                                .contains("content-length:")
                                || reply
                                    .extra_headers
                                    .to_ascii_lowercase()
                                    .contains("transfer-encoding:")
                            {
                                String::new()
                            } else {
                                format!("Content-Length: {}\r\n", reply.body.len())
                            };
                            let head = format!(
                                "HTTP/1.1 {} Response\r\nConnection: close\r\n{length}{}\r\n",
                                reply.status, reply.extra_headers
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(&reply.body);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("mock accept: {e}"),
                    }
                }
            });
            Self {
                endpoint,
                requests,
                stop,
                thread: Some(thread),
            }
        }

        fn transport<'a>(&self, config: &'a WebDavConfig) -> Transport<'a> {
            // Only unit tests can inject HTTP loopback. Public APIs always call config.validate().
            Transport {
                config,
                root: Url::parse(&self.endpoint).unwrap(),
                client: Client::builder()
                    .no_proxy()
                    .redirect(Policy::none())
                    .timeout(Duration::from_secs(20))
                    .build()
                    .unwrap(),
            }
        }
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let result = self.thread.take().unwrap().join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
    fn read_request(stream: &mut TcpStream) -> Request {
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut byte = [0u8; 1];
        while !bytes.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            assert!(bytes.len() <= 65536);
        }
        let text = std::str::from_utf8(&bytes).unwrap();
        let mut lines = text.split("\r\n");
        let first: Vec<_> = lines.next().unwrap().split_whitespace().collect();
        let headers: BTreeMap<_, _> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        let length: usize = headers
            .get("content-length")
            .map(|s| s.parse().unwrap())
            .unwrap_or(0);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        Request {
            method: first[0].into(),
            path: first[1].into(),
            headers,
            body,
        }
    }
    fn config() -> WebDavConfig {
        WebDavConfig {
            endpoint: "https://example.invalid/dav/users/test/".into(),
            username: "user".into(),
            password: "pass".into(),
            prefix: "SyncSSH 备份/snapshots".into(),
        }
    }
    fn ciphertext() -> &'static [u8] {
        static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
        BYTES.get_or_init(|| {
            crypto::encrypt("webdav master password", b"private-ssh-credential").unwrap()
        })
    }
    fn multistatus(responses: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\">{responses}</d:multistatus>"
        )
    }
    fn collection_response(path: &str) -> String {
        format!("<d:response><d:href>{path}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>")
    }
    fn file_response(path: &str, length: usize) -> String {
        format!("<d:response><d:href>{path}</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>{length}</d:getcontentlength><d:getlastmodified>Fri, 06 Nov 2026 01:00:00 GMT</d:getlastmodified></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_http_roundtrip_uses_basic_auth_conditional_writes_and_only_ciphertext() {
        let mut directories = HashSet::<String>::new();
        let mut files = BTreeMap::<String, Vec<u8>>::new();
        let mock = Mock::new(move |req| {
            if req.headers.get("authorization").map(String::as_str) != Some("Basic dXNlcjpwYXNz") {
                return Reply::new(401, "");
            }
            match req.method.as_str() {
                "MKCOL" => Reply::new(
                    if directories.insert(req.path.clone()) {
                        201
                    } else {
                        405
                    },
                    "",
                ),
                "PUT" => {
                    assert_eq!(
                        req.headers.get("if-none-match").map(String::as_str),
                        Some("*")
                    );
                    if files.contains_key(&req.path) {
                        return Reply::new(412, "");
                    }
                    files.insert(req.path.clone(), req.body.clone());
                    Reply::new(201, "")
                }
                "GET" => files
                    .get(&req.path)
                    .map(|body| Reply::new(200, body.clone()))
                    .unwrap_or_else(|| Reply::new(404, "")),
                "PROPFIND" => {
                    assert_eq!(
                        req.headers.get("content-type").map(String::as_str),
                        Some("application/xml; charset=utf-8")
                    );
                    let mut responses = collection_response(&req.path);
                    if req.headers.get("depth").map(String::as_str) == Some("1") {
                        for (path, body) in &files {
                            responses.push_str(&file_response(path, body.len()));
                        }
                    }
                    Reply::new(207, multistatus(&responses))
                }
                _ => panic!("unexpected method"),
            }
        });
        let config = config();
        let transport = mock.transport(&config);
        let first = transport.upload(ciphertext()).await.unwrap();
        let second = transport.upload(ciphertext()).await.unwrap();
        assert_ne!(first, second);
        let bytes = transport.download(&first).await.unwrap();
        assert_eq!(bytes, ciphertext());
        let list = transport.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|item| item.key == first));
        assert_eq!(
            &**crypto::decrypt("webdav master password", &bytes).unwrap(),
            b"private-ssh-credential"
        );
        let requests = mock.requests.lock().unwrap();
        assert!(requests.iter().all(|r| !r.path.contains(' ')));
        for req in requests.iter().filter(|r| r.method == "PUT") {
            assert!(!req.body.windows(22).any(|w| w == b"private-ssh-credential"));
        }
    }

    #[tokio::test]
    async fn authentication_missing_file_and_redirect_errors_are_safe_and_not_followed() {
        for status in [401, 404, 302] {
            let mock = Mock::new(move |_| {
                let mut reply = Reply::new(status, "private-server-detail user:pass");
                if status == 302 {
                    reply.extra_headers = "Location: https://example.invalid/leak\r\n".into();
                }
                reply
            });
            let config = config();
            let error = mock
                .transport(&config)
                .download(&format!("{}/one.vault", config.prefix))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(&status.to_string()));
            assert!(!error.contains("private-server-detail"));
            assert!(!error.contains("pass"));
            assert_eq!(mock.requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn advertised_and_chunked_responses_are_bounded_and_plaintext_is_refused() {
        let mock = Mock::new(|_| {
            let mut reply = Reply::new(200, "");
            reply.extra_headers = format!("Content-Length: {}\r\n", crypto::MAX_ENVELOPE_BYTES + 1);
            reply
        });
        let config = config();
        assert!(mock
            .transport(&config)
            .download(&format!("{}/one.vault", config.prefix))
            .await
            .unwrap_err()
            .to_string()
            .contains("大小上限"));
        let chunked = Mock::new(|_| {
            let mut reply = Reply::new(200, "4\r\n1234\r\n0\r\n\r\n");
            reply.extra_headers = "Transfer-Encoding: chunked\r\n".into();
            reply
        });
        let response = chunked
            .transport(&config)
            .client
            .get(&chunked.endpoint)
            .send()
            .await
            .unwrap();
        assert!(bounded_body(response, 3).await.is_err());
        let plaintext = Mock::new(|_| Reply::new(200, "unprotected password"));
        let client = plaintext.transport(&config);
        assert!(client
            .download(&format!("{}/one.vault", config.prefix))
            .await
            .is_err());
        assert!(client.upload(b"unprotected password").await.is_err());
        assert_eq!(plaintext.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn existing_file_cannot_masquerade_as_directory_and_collision_is_not_overwritten() {
        let mock = Mock::new(|req| match req.method.as_str() {
            "MKCOL" => Reply::new(405, ""),
            "PROPFIND" => Reply::new(207, multistatus(&file_response(&req.path, 10))),
            _ => panic!("must not upload into non-collection"),
        });
        let config = config();
        assert!(mock.transport(&config).upload(ciphertext()).await.is_err());
        let collision =
            Mock::new(|req| Reply::new(if req.method == "PUT" { 412 } else { 201 }, ""));
        assert!(collision
            .transport(&config)
            .upload(ciphertext())
            .await
            .unwrap_err()
            .to_string()
            .contains("未覆盖"));
    }

    #[test]
    fn endpoint_and_prefix_encoding_preserve_existing_paths() {
        let mut config = config();
        config.endpoint = "https://example.invalid/dav/users/alice%20smith/中文".into();
        let transport = Transport::new(&config).unwrap();
        assert_eq!(
            transport.root.as_str(),
            "https://example.invalid/dav/users/alice%20smith/%E4%B8%AD%E6%96%87/"
        );
        let target = transport
            .url(&format!("{}/one.vault", config.prefix), false)
            .unwrap();
        assert_eq!(target.as_str(), "https://example.invalid/dav/users/alice%20smith/%E4%B8%AD%E6%96%87/SyncSSH%20%E5%A4%87%E4%BB%BD/snapshots/one.vault");
        assert!(!target.as_str().contains("%2520"));
    }

    #[test]
    fn invalid_keys_never_escape_the_configured_collection() {
        let config = config();
        for suffix in [
            "../file.vault",
            "nested/file.vault",
            "%2e%2e.vault",
            "%252f.vault",
            "file.vault?x=1",
            "file.vault#fragment",
            "file\\.vault",
            "/file.vault",
            "\nfile.vault",
        ] {
            assert!(
                validated_key(&config, &format!("{}/{suffix}", config.prefix)).is_err(),
                "{suffix}"
            );
        }
        assert!(validated_key(&config, "other/file.vault").is_err());
        assert!(validated_key(&config, &format!("{}/good.vault", config.prefix)).is_ok());
        let mut insecure = config.clone();
        insecure.endpoint = "http://127.0.0.1:9000/dav/".into();
        assert!(Transport::new(&insecure).is_err());
    }

    #[test]
    fn listing_filters_foreign_origins_directories_failed_properties_and_path_attacks() {
        let config = config();
        let transport = Transport::new(&config).unwrap();
        let collection = transport.url(&config.prefix, true).unwrap();
        let good = format!("{}good.vault", collection.path());
        let mut responses = file_response(&good, 100);
        responses.push_str(&file_response(&format!("https://evil.invalid{good}"), 100));
        responses.push_str(&file_response("/outside/other.vault", 100));
        responses.push_str(&file_response(
            &format!("{}%2Fescape.vault", collection.path()),
            100,
        ));
        responses.push_str(&file_response(
            &format!("{}%252e%252e.vault", collection.path()),
            100,
        ));
        responses.push_str(&file_response(
            &format!("{}huge.vault", collection.path()),
            crypto::MAX_ENVELOPE_BYTES + 1,
        ));
        responses.push_str(
            &file_response(&format!("{}failed.vault", collection.path()), 100)
                .replace("200 OK", "404 Not Found"),
        );
        responses.push_str(&collection_response(&format!(
            "{}dir.vault",
            collection.path()
        )));
        let snapshots =
            parse_list(multistatus(&responses).as_bytes(), &collection, &config).unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].key, format!("{}/good.vault", config.prefix));
        assert!(document(
            b"<!DOCTYPE x [<!ENTITY x 'secret'>]><multistatus xmlns='DAV:'>&x;</multistatus>"
        )
        .is_err());
    }
}
