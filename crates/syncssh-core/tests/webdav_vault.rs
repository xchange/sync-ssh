//! Compatibility and persistence checks for adding WebDAV beside an existing S3 vault.
use syncssh_core::{crypto, vault, S3Config, S3Provider, VaultData, VaultStore, WebDavConfig};

const MASTER: &str = "a test-only master passphrase";

fn webdav() -> WebDavConfig {
    WebDavConfig {
        endpoint: "https://cloud.example.test/remote.php/dav/files/dav-user/".into(),
        username: "dav-user-private".into(),
        password: "dav-application-password-private".into(),
        prefix: "syncssh/备份 folder".into(),
    }
}

fn s3() -> S3Config {
    let mut config = S3Config::default();
    config.bucket = "syncssh-test-vault".into();
    config.access_key = "s3-access-private".into();
    config.secret_key = "s3-secret-private".into();
    config
}

#[test]
fn encrypted_legacy_vault_opens_without_changing_s3_auto_backup() {
    // This JSON is the v0.1.1 on-disk shape, intentionally without either new field.
    let mut legacy = serde_json::json!({
        "schema_version": 1,
        "servers": [],
        "s3": s3(),
        "effects_enabled": false,
        "backup_on_save": true
    });
    legacy["s3"].as_object_mut().unwrap().remove("provider");
    let bytes = crypto::encrypt(MASTER, &serde_json::to_vec(&legacy).unwrap()).unwrap();
    let data = vault::decode(MASTER, &bytes).unwrap();
    assert_eq!(data.s3, Some(s3()));
    assert!(data.backup_on_save);
    assert!(!data.effects_enabled);
    assert_eq!(data.webdav, None);
    assert!(!data.webdav_backup_on_save);
    assert!(data.object_stores.is_empty());
}

#[test]
fn all_object_storage_accounts_survive_encrypted_save_and_restore_independently() {
    let directory = tempfile::tempdir().unwrap();
    let store = VaultStore::new(directory.path().join("credentials.vault"));
    let mut data = VaultData::default();
    for provider in S3Provider::ALL {
        let mut config = S3Config::for_provider(provider);
        config.bucket = "syncssh-backups-1250000000".into();
        config.access_key = format!("private-access-{provider:?}");
        config.secret_key = format!("private-secret-{provider:?}");
        data.set_object_storage(config, true);
    }
    data.webdav = Some(webdav());
    data.webdav_backup_on_save = true;
    data.validate().unwrap();
    store.save(MASTER, &data).unwrap();
    let encrypted = store.encrypted_blob().unwrap();
    let ciphertext_text = String::from_utf8_lossy(&encrypted);
    assert!(!ciphertext_text.contains("private-access"));
    assert!(!ciphertext_text.contains("private-secret"));
    let restored = vault::decode(MASTER, &encrypted).unwrap();
    assert_eq!(restored, data);
    let mut updated = restored.clone();
    let mut aliyun = updated.object_storage(S3Provider::Aliyun).unwrap().clone();
    aliyun.secret_key = "new-private-secret".into();
    updated.set_object_storage(aliyun, false);
    assert!(!updated.object_storage_automatic(S3Provider::Aliyun));
    for provider in [
        S3Provider::S3,
        S3Provider::Huawei,
        S3Provider::Tencent,
        S3Provider::Volcengine,
    ] {
        assert_eq!(
            updated.object_storage(provider),
            restored.object_storage(provider)
        );
        assert!(updated.object_storage_automatic(provider));
    }
    assert_eq!(updated.webdav, restored.webdav);
    assert!(updated.webdav_backup_on_save);
    updated
        .object_stores
        .get_mut(&S3Provider::Huawei)
        .unwrap()
        .config
        .provider = S3Provider::Tencent;
    assert!(updated.validate().is_err());
}

#[test]
fn both_providers_and_independent_switches_survive_encrypted_save_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let store = VaultStore::new(dir.path().join("both.vault"));
    let mut data = VaultData {
        s3: Some(s3()),
        webdav: Some(webdav()),
        backup_on_save: true,
        webdav_backup_on_save: true,
        ..Default::default()
    };
    store.save(MASTER, &data).unwrap();
    assert_eq!(store.unlock(MASTER).unwrap(), data);
    let encrypted = store.encrypted_blob().unwrap();
    let dav = data.webdav.as_ref().unwrap();
    for secret in [&dav.endpoint, &dav.username, &dav.password] {
        assert!(
            !encrypted
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "WebDAV configuration leaked into the serialized envelope"
        );
    }

    // Disabling one target must not change the other provider's configuration or switch.
    data.webdav_backup_on_save = false;
    store.save(MASTER, &data).unwrap();
    let current = store.unlock(MASTER).unwrap();
    assert!(current.backup_on_save);
    assert!(!current.webdav_backup_on_save);
    assert_eq!(current.s3, Some(s3()));
    assert_eq!(current.webdav, Some(webdav()));

    let restored = VaultStore::new(dir.path().join("restored.vault"));
    let imported = restored.import_encrypted(MASTER, &encrypted).unwrap();
    assert!(imported.backup_on_save && imported.webdav_backup_on_save);
    assert_eq!(restored.unlock(MASTER).unwrap(), imported);
}

#[test]
fn invalid_webdav_restore_leaves_both_local_copies_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let store = VaultStore::new(dir.path().join("local.vault"));
    let mut data = VaultData {
        webdav: Some(webdav()),
        ..Default::default()
    };
    store.save(MASTER, &data).unwrap();
    data.webdav_backup_on_save = true;
    store.save(MASTER, &data).unwrap();
    let original = std::fs::read(store.path()).unwrap();
    let previous = std::fs::read(store.previous_path()).unwrap();

    assert!(store
        .import_encrypted("the wrong test master password", &original)
        .is_err());
    let mut altered = original.clone();
    *altered.last_mut().unwrap() ^= 1;
    assert!(store.import_encrypted(MASTER, &altered).is_err());
    assert_eq!(std::fs::read(store.path()).unwrap(), original);
    assert_eq!(std::fs::read(store.previous_path()).unwrap(), previous);
    assert_eq!(store.unlock(MASTER).unwrap(), data);
}

#[test]
fn webdav_configuration_debug_does_not_expose_credentials_or_endpoint() {
    let config = webdav();
    let debug = format!("{config:?}");
    let vault_debug = format!(
        "{:?}",
        VaultData {
            webdav: Some(config.clone()),
            ..Default::default()
        }
    );
    for text in [&debug, &vault_debug] {
        assert!(text.contains("[REDACTED]"));
        for secret in [&config.endpoint, &config.username, &config.password] {
            assert!(!text.contains(secret));
        }
    }
}

#[test]
fn webdav_rejects_unsafe_transport_and_path_configuration() {
    let valid = webdav();
    valid.validate().unwrap();
    for endpoint in [
        "http://cloud.example.test/dav/",
        "https://user:secret@cloud.example.test/dav/",
        "https://cloud.example.test/dav/?token=secret",
        "https://cloud.example.test/dav/#fragment",
        "https://cloud.example.test/dav/%2fprivate/",
        "https://cloud.example.test/dav/%252e%252e/",
        "https://cloud.example.test/dav/../private/",
        "https://cloud.example.test/dav/./private/",
        "https://cloud.example.test/dav/%2e%2e/private/",
        "https://cloud.example.test/dav/.%2E/private/",
        "https://cloud.example.test/dav/%2e./private/",
        "https://cloud.example.test/dav/\\private/",
        "https://cloud.example.test/dav/\nprivate/",
    ] {
        let mut config = valid.clone();
        config.endpoint = endpoint.into();
        assert!(config.validate().is_err(), "accepted endpoint {endpoint:?}");
    }
    for prefix in [
        "",
        "/absolute",
        "trailing/",
        "double//slash",
        "../escape",
        "folder/../escape",
        "./folder",
        "folder\\escape",
        "folder/%2e%2e",
        "folder?query",
        "folder#fragment",
        "folder\0control",
    ] {
        let mut config = valid.clone();
        config.prefix = prefix.into();
        assert!(config.validate().is_err(), "accepted prefix {prefix:?}");
    }
    let mut config = valid.clone();
    config.username = "name:injected".into();
    assert!(config.validate().is_err());
    config = valid;
    config.password.clear();
    assert!(config.validate().is_err());
}
