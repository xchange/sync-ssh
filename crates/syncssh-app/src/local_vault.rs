//! A local pointer to the active encrypted vault; never included in cloud backups.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use syncssh_core::{VaultData, VaultStore};

#[derive(Serialize, Deserialize)]
struct Location {
    path: PathBuf,
}

pub fn load(directory: &Path) -> Result<VaultStore> {
    let file = match fs::File::open(directory.join("vault-location.json")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let path = directory.join("credentials.vault");
            return Ok(if path.exists() {
                VaultStore::open_existing(path)
            } else {
                VaultStore::create_new(path)
            });
        }
        Err(error) => return Err(error).context("无法读取凭据库位置配置"),
    };
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16 * 1024, "凭据库位置配置过大");
    let location: Location = serde_json::from_slice(&bytes).context("凭据库位置配置无效")?;
    ensure!(location.path.is_absolute(), "凭据库位置必须为绝对路径");
    Ok(VaultStore::open_existing(location.path))
}

pub fn remember(directory: &Path, store: &VaultStore) -> Result<()> {
    let path = fs::canonicalize(store.path()).context("无法确认凭据库文件位置")?;
    fs::create_dir_all(directory)?;
    let mut pending = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(&mut pending, &Location { path })?;
    pending.write_all(b"\n")?;
    pending.as_file().sync_all()?;
    pending
        .persist(directory.join("vault-location.json"))
        .context("无法保存凭据库位置；原文件与原位置配置已保留")?;
    Ok(())
}

pub fn relocate(
    source: &VaultStore,
    password: &str,
    target: &Path,
    directory: &Path,
) -> Result<VaultStore> {
    ensure!(target.is_absolute(), "请选择绝对文件路径");
    ensure!(
        !target.exists(),
        "目标位置已有文件，请使用“打开已有凭据库”，避免覆盖现有数据"
    );
    let bytes = source.encrypted_blob()?;
    let mut store = VaultStore::create_new(target);
    store.import_encrypted(password, &bytes)?;
    source.observe()?;
    store.require_existing();
    remember(directory, &store)?;
    Ok(store)
}

pub fn open_verified(
    target: &Path,
    expected: &[u8],
    password: &str,
    directory: &Path,
) -> Result<(VaultStore, VaultData)> {
    let store = VaultStore::open_existing(target);
    let data = store.unlock_matching(password, expected)?;
    remember(directory, &store)?;
    Ok((store, data))
}

#[cfg(test)]
mod tests {
    use super::{load, open_verified, relocate, remember};
    use syncssh_core::{VaultData, VaultStore};
    const PASSWORD: &str = "temporary ASCII test password";

    #[test]
    fn relocating_preserves_ciphertext_and_persists_a_continuously_updated_location() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join("settings");
        let original = VaultStore::create_new(settings.join("credentials.vault"));
        original.save(PASSWORD, &VaultData::default()).unwrap();
        remember(&settings, &original).unwrap();
        let bytes = original.encrypted_blob().unwrap();
        let destination = temp.path().join("any-sync-folder/credentials.vault");
        let moved = relocate(&original, PASSWORD, &destination, &settings).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        assert_eq!(std::fs::read(original.path()).unwrap(), bytes);
        assert!(!moved.can_create());
        let loaded = load(&settings).unwrap();
        assert_eq!(loaded.path(), std::fs::canonicalize(&destination).unwrap());
        let mut data = loaded.unlock(PASSWORD).unwrap();
        data.effects_enabled = false;
        loaded.save(PASSWORD, &data).unwrap();
        assert_eq!(
            VaultStore::open_existing(&destination)
                .unlock(PASSWORD)
                .unwrap(),
            data
        );
        assert_eq!(std::fs::read(original.path()).unwrap(), bytes);
    }

    #[test]
    fn wrong_password_changed_preview_or_existing_destination_keep_original_location() {
        let temp = tempfile::tempdir().unwrap();
        let original = load(temp.path()).unwrap();
        original.save(PASSWORD, &VaultData::default()).unwrap();
        remember(temp.path(), &original).unwrap();
        let config = std::fs::read(temp.path().join("vault-location.json")).unwrap();
        let other = VaultStore::create_new(temp.path().join("other.vault"));
        other.save(PASSWORD, &VaultData::default()).unwrap();
        let preview = other.encrypted_blob().unwrap();
        assert!(open_verified(other.path(), &preview, "wrong password", temp.path()).is_err());
        assert!(relocate(&original, PASSWORD, other.path(), temp.path()).is_err());
        let mut changed = VaultData::default();
        changed.effects_enabled = false;
        other.save(PASSWORD, &changed).unwrap();
        assert!(open_verified(other.path(), &preview, PASSWORD, temp.path()).is_err());
        assert_eq!(
            std::fs::read(temp.path().join("vault-location.json")).unwrap(),
            config
        );
        let current = other.encrypted_blob().unwrap();
        let (opened, data) = open_verified(other.path(), &current, PASSWORD, temp.path()).unwrap();
        assert_eq!(data, changed);
        assert_eq!(std::fs::read(other.path()).unwrap(), current);
        assert_eq!(
            load(temp.path()).unwrap().path(),
            std::fs::canonicalize(opened.path()).unwrap()
        );
    }

    #[test]
    fn missing_selected_files_and_stale_sources_cannot_produce_empty_replacements() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join("settings");
        let source = VaultStore::create_new(temp.path().join("sync/credentials.vault"));
        source.save(PASSWORD, &VaultData::default()).unwrap();
        remember(&settings, &source).unwrap();
        let other = VaultStore::open_existing(source.path());
        other.unlock(PASSWORD).unwrap();
        other.save(PASSWORD, &VaultData::default()).unwrap();
        let target = temp.path().join("new.vault");
        assert!(relocate(&source, PASSWORD, &target, &settings).is_err());
        assert!(!target.exists());
        std::fs::remove_file(source.path()).unwrap();
        let selected = load(&settings).unwrap();
        assert!(!selected.can_create());
        assert!(selected.save(PASSWORD, &VaultData::default()).is_err());
        assert!(!selected.path().exists());
    }
}
