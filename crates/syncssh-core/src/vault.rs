use crate::{crypto, VaultData};
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

#[derive(Clone, Debug)]
pub struct VaultStore {
    path: PathBuf,
    require_existing: bool,
    // None: not observed; Some(None): must be absent; Some(Some(hash)): unchanged.
    revision: Arc<Mutex<Option<Option<[u8; 32]>>>>,
}

impl VaultStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            require_existing: false,
            revision: Arc::new(Mutex::new(None)),
        }
    }
    pub fn open_existing(path: impl Into<PathBuf>) -> Self {
        Self {
            require_existing: true,
            ..Self::new(path)
        }
    }
    pub fn create_new(path: impl Into<PathBuf>) -> Self {
        Self {
            revision: Arc::new(Mutex::new(Some(None))),
            ..Self::new(path)
        }
    }
    pub fn require_existing(&mut self) {
        self.require_existing = true;
    }
    pub fn can_create(&self) -> bool {
        !self.require_existing && !self.exists()
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn exists(&self) -> bool {
        self.path.exists()
    }
    pub fn previous_path(&self) -> PathBuf {
        self.path.with_extension("vault.previous")
    }

    pub fn unlock(&self, password: &str) -> Result<VaultData> {
        let mut revision = self
            .revision
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据库状态不可用"))?;
        let bytes = read_bounded(&self.path)?;
        let data = decode(password, &bytes)?;
        let expected = Some(fingerprint(&bytes));
        self.check_revision(expected)?;
        *revision = Some(expected);
        Ok(data)
    }

    /// Open exactly the bytes the user has already previewed, without rewriting
    /// the file or adopting a newer external version between preview and confirm.
    pub fn unlock_matching(&self, password: &str, bytes: &[u8]) -> Result<VaultData> {
        let mut revision = self
            .revision
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据库状态不可用"))?;
        let data = decode(password, bytes)?;
        let expected = Some(fingerprint(bytes));
        self.check_revision(expected)?;
        *revision = Some(expected);
        Ok(data)
    }

    pub fn save(&self, password: &str, data: &VaultData) -> Result<()> {
        let mut revision = self
            .revision
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据库状态不可用"))?;
        let expected = self.expected_revision(*revision)?;
        data.validate()?;
        let plaintext = Zeroizing::new(serde_json::to_vec(data).context("凭据库序列化失败")?);
        let encrypted = crypto::encrypt(password, &plaintext)?;
        self.replace(&encrypted, expected, false)?;
        *revision = Some(Some(fingerprint(&encrypted)));
        Ok(())
    }

    /// Re-encrypt both local copies; existing cloud snapshots still require their original password.
    pub fn change_password(&self, old_password: &str, new_password: &str) -> Result<()> {
        crypto::validate_new_password(new_password)?;
        let mut revision = self
            .revision
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据库状态不可用"))?;
        let expected = self.expected_revision(*revision)?;
        self.check_revision(expected)?;
        let data = decode(old_password, &read_bounded(&self.path)?)?;
        let plaintext = Zeroizing::new(serde_json::to_vec(&data).context("凭据库序列化失败")?);
        let encrypted = crypto::encrypt(new_password, &plaintext)?;
        // Publish the new-password recovery copy first. If the current-file write fails,
        // the original current vault remains available under the old password.
        self.replace(&encrypted, expected, true)?;
        *revision = Some(Some(fingerprint(&encrypted)));
        Ok(())
    }

    pub fn encrypted_blob(&self) -> Result<Vec<u8>> {
        let revision = self
            .revision
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据库状态不可用"))?;
        let blob = read_bounded(&self.path)?;
        crypto::validate_envelope(&blob)?;
        if let Some(expected) = *revision {
            ensure!(
                Some(fingerprint(&blob)) == expected,
                "凭据库文件已被外部修改，请锁定后重新解锁再试"
            );
        }
        Ok(blob)
    }

    /// Capture the current destination before a restore preview/download starts.
    pub fn observe(&self) -> Result<()> {
        let mut revision = self
            .revision
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据库状态不可用"))?;
        let expected = self.expected_revision(*revision)?;
        self.check_revision(expected)?;
        *revision = Some(expected);
        Ok(())
    }

    fn current_revision(&self) -> Result<Option<[u8; 32]>> {
        Ok(read_optional(&self.path)?.map(|bytes| fingerprint(&bytes)))
    }
    fn expected_revision(&self, recorded: Option<Option<[u8; 32]>>) -> Result<Option<[u8; 32]>> {
        let expected = match recorded {
            Some(value) => value,
            None => self.current_revision()?,
        };
        ensure!(
            !self.require_existing || expected.is_some(),
            "选定的凭据库文件不可用，请确认文件已同步到本机、磁盘已挂载且路径未移动"
        );
        Ok(expected)
    }
    fn check_revision(&self, expected: Option<[u8; 32]>) -> Result<()> {
        ensure!(
            self.current_revision()? == expected,
            "凭据库文件已被外部修改或移除，请锁定后重新解锁再试；原文件未被覆盖"
        );
        Ok(())
    }

    /// Authenticate and validate the downloaded vault before replacing any local data.
    /// This replaces the complete vault; it never silently merges conflicting server entries.
    pub fn import_encrypted(&self, password: &str, blob: &[u8]) -> Result<VaultData> {
        let mut revision = self
            .revision
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据库状态不可用"))?;
        let expected = self.expected_revision(*revision)?;
        let data = decode(password, blob)?;
        self.replace(blob, expected, false)?;
        *revision = Some(Some(fingerprint(blob)));
        Ok(data)
    }

    fn replace(&self, blob: &[u8], expected: Option<[u8; 32]>, rekey_previous: bool) -> Result<()> {
        crypto::validate_envelope(blob)?;
        self.check_revision(expected)?;
        let directory = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if !directory.exists() {
            fs::create_dir_all(directory).context("无法创建凭据库目录")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                    .context("无法保护凭据库目录")?;
            }
        }
        // The existing ciphertext is preserved before the new vault is atomically published.
        // A failed save leaves either the old or the new complete vault, never a partial file.
        if let Some(old) = read_optional(&self.path)? {
            ensure!(
                Some(fingerprint(&old)) == expected,
                "凭据库文件已被外部修改，请重新解锁"
            );
            atomic_write(
                &self.previous_path(),
                if rekey_previous { blob } else { &old },
            )?;
        }
        atomic_write_guarded(&self.path, blob, || self.check_revision(expected))
    }
}

fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn decode(password: &str, blob: &[u8]) -> Result<VaultData> {
    let plaintext = crypto::decrypt(password, blob)?;
    let data: VaultData = serde_json::from_slice(&plaintext)
        .map_err(|_| anyhow::anyhow!("解密成功，但凭据库数据格式无效"))?;
    data.validate()?;
    Ok(data)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    read_optional(path)?.context("无法打开加密凭据库：文件不存在或尚未同步到本机")
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("无法打开加密凭据库"),
    };
    ensure!(
        file.metadata()?.len() <= crypto::MAX_ENVELOPE_BYTES as u64,
        "加密凭据库超过大小上限"
    );
    let mut bytes = Vec::new();
    file.take(crypto::MAX_ENVELOPE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("无法读取加密凭据库")?;
    ensure!(
        bytes.len() <= crypto::MAX_ENVELOPE_BYTES,
        "加密凭据库超过大小上限"
    );
    Ok(Some(bytes))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_guarded(path, bytes, || Ok(()))
}

fn atomic_write_guarded(
    path: &Path,
    bytes: &[u8],
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut pending = NamedTempFile::new_in(directory).context("无法创建临时加密文件")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        pending
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("无法保护加密文件")?;
    }
    pending.write_all(bytes).context("无法写入加密凭据库")?;
    pending.as_file().sync_all().context("无法同步加密凭据库")?;
    before_publish()?;
    pending
        .persist(path)
        .map_err(|_| anyhow::anyhow!("无法原子替换凭据库文件"))?;
    #[cfg(unix)]
    File::open(directory)?
        .sync_all()
        .context("无法同步凭据库目录")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Credential, ServerProfile};
    const PASSWORD: &str = "test master password long";
    #[test]
    fn save_unlock_previous_and_private_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let store = VaultStore::new(temp.path().join("private/vault.bin"));
        let mut data = VaultData::default();
        store.save(PASSWORD, &data).unwrap();
        assert_eq!(store.unlock(PASSWORD).unwrap(), data);
        let first = store.encrypted_blob().unwrap();
        data.servers.push(ServerProfile {
            id: "id".into(),
            name: "Production".into(),
            host: "example.com".into(),
            port: 22,
            username: "deploy".into(),
            group: "Work".into(),
            credential: Credential::Password {
                password: "sensitive-ssh".into(),
            },
            connection: Default::default(),
        });
        store.save(PASSWORD, &data).unwrap();
        assert_eq!(fs::read(store.previous_path()).unwrap(), first);
        assert_eq!(store.unlock(PASSWORD).unwrap(), data);
        assert!(!store
            .encrypted_blob()
            .unwrap()
            .windows(13)
            .any(|b| b == b"sensitive-ssh"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(store.path().parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }
    #[test]
    fn password_rotation_rekeys_both_local_copies() {
        let temp = tempfile::tempdir().unwrap();
        let store = VaultStore::new(temp.path().join("vault"));
        store.save(PASSWORD, &VaultData::default()).unwrap();
        let replacement = "another unique long master password";
        store.change_password(PASSWORD, replacement).unwrap();
        assert!(store.unlock(PASSWORD).is_err());
        store.unlock(replacement).unwrap();
        let previous = fs::read(store.previous_path()).unwrap();
        assert!(decode(PASSWORD, &previous).is_err());
        decode(replacement, &previous).unwrap();
    }

    #[test]
    fn valid_restore_recovers_a_corrupted_local_file() {
        let temp = tempfile::tempdir().unwrap();
        let store = VaultStore::new(temp.path().join("vault"));
        store.save(PASSWORD, &VaultData::default()).unwrap();
        let valid = store.encrypted_blob().unwrap();
        fs::write(store.path(), b"truncated corrupt data").unwrap();
        assert!(store.import_encrypted(PASSWORD, &valid).is_err());
        // Lock/reopen explicitly acknowledges the externally corrupted file.
        let store = VaultStore::open_existing(store.path());
        store.import_encrypted(PASSWORD, &valid).unwrap();
        store.unlock(PASSWORD).unwrap();
        assert_eq!(
            fs::read(store.previous_path()).unwrap(),
            b"truncated corrupt data"
        );
    }

    #[test]
    fn invalid_restore_never_replaces_local_vault() {
        let temp = tempfile::tempdir().unwrap();
        let store = VaultStore::new(temp.path().join("vault"));
        store.save(PASSWORD, &VaultData::default()).unwrap();
        let original = store.encrypted_blob().unwrap();
        assert!(store.import_encrypted("wrong password", &original).is_err());
        assert_eq!(store.encrypted_blob().unwrap(), original);
        let invalid_data = crypto::encrypt(PASSWORD, b"not JSON").unwrap();
        assert!(store.import_encrypted(PASSWORD, &invalid_data).is_err());
        assert_eq!(store.encrypted_blob().unwrap(), original);
    }

    #[test]
    fn stale_sessions_cannot_overwrite_external_updates_or_rotate_their_password() {
        let temp = tempfile::tempdir().unwrap();
        let first = VaultStore::create_new(temp.path().join("credentials.vault"));
        let original = VaultData::default();
        first.save(PASSWORD, &original).unwrap();
        let second = VaultStore::open_existing(first.path());
        second.unlock(PASSWORD).unwrap();
        let mut updated = original.clone();
        updated.effects_enabled = false;
        first.save(PASSWORD, &updated).unwrap();
        let ciphertext = fs::read(first.path()).unwrap();
        assert!(second.save(PASSWORD, &original).is_err());
        assert!(second
            .change_password(PASSWORD, "another long ASCII password")
            .is_err());
        assert!(second
            .import_encrypted(
                PASSWORD,
                &crypto::encrypt(PASSWORD, &serde_json::to_vec(&original).unwrap()).unwrap()
            )
            .is_err());
        assert_eq!(fs::read(first.path()).unwrap(), ciphertext);
        assert_eq!(second.unlock(PASSWORD).unwrap(), updated);
        second.save(PASSWORD, &updated).unwrap();
    }

    #[test]
    fn deleted_or_unsynced_vaults_are_never_recreated_and_new_targets_never_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let store = VaultStore::create_new(temp.path().join("credentials.vault"));
        store.save(PASSWORD, &VaultData::default()).unwrap();
        let bytes = fs::read(store.path()).unwrap();
        let collision = VaultStore::create_new(store.path());
        assert!(collision.import_encrypted(PASSWORD, &bytes).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), bytes);
        fs::remove_file(store.path()).unwrap();
        assert!(store.save(PASSWORD, &VaultData::default()).is_err());
        assert!(!store.exists());
        let missing = VaultStore::open_existing(temp.path().join("unmounted/credentials.vault"));
        assert!(!missing.can_create());
        assert!(missing.save(PASSWORD, &VaultData::default()).is_err());
        assert!(!missing.path().parent().unwrap().exists());
    }

    #[test]
    fn confirming_an_old_preview_cannot_open_changed_data() {
        let temp = tempfile::tempdir().unwrap();
        let writer = VaultStore::create_new(temp.path().join("credentials.vault"));
        writer.save(PASSWORD, &VaultData::default()).unwrap();
        let preview = writer.encrypted_blob().unwrap();
        let mut changed = VaultData::default();
        changed.effects_enabled = false;
        writer.save(PASSWORD, &changed).unwrap();
        let reader = VaultStore::open_existing(writer.path());
        assert!(reader.unlock_matching(PASSWORD, &preview).is_err());
        assert_eq!(reader.unlock(PASSWORD).unwrap(), changed);
    }
}
