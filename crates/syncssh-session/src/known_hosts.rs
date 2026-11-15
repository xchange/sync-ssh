use crate::SessionError;
use russh::keys::{HashAlg, PublicKey};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAX_FILE_SIZE: u64 = 1024 * 1024;

/// A pinned public host key. It contains no private data or login credential.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    /// Canonical OpenSSH public-key text (algorithm and base64, no comment).
    pub key: String,
    pub fingerprint: String,
}

impl HostKey {
    pub fn from_openssh(host: &str, port: u16, openssh: &str) -> Result<Self, SessionError> {
        let key = PublicKey::from_openssh(openssh)
            .map_err(|_| SessionError::KnownHosts("invalid public key".into()))?;
        Self::from_public_key(host, port, &key)
    }

    pub(crate) fn from_public_key(
        host: &str,
        port: u16,
        key: &PublicKey,
    ) -> Result<Self, SessionError> {
        if host.is_empty() || host.len() > 253 || host.chars().any(char::is_control) || port == 0 {
            return Err(SessionError::KnownHosts("invalid host or port".into()));
        }
        let encoded = key
            .to_openssh()
            .map_err(|_| SessionError::KnownHosts("invalid public key".into()))?;
        // Remove comments before comparing or persisting keys.
        let canonical = encoded
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        Ok(Self {
            host: host.trim_end_matches('.').to_ascii_lowercase(),
            port,
            algorithm: key.algorithm().to_string(),
            key: canonical,
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        })
    }

    fn checked(&self) -> Result<Self, SessionError> {
        let parsed = Self::from_openssh(&self.host, self.port, &self.key)?;
        if parsed.algorithm != self.algorithm || parsed.fingerprint != self.fingerprint {
            return Err(SessionError::KnownHosts(
                "inconsistent public key metadata".into(),
            ));
        }
        Ok(parsed)
    }
}

impl fmt::Display for HostKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{} {} {}",
            self.host, self.port, self.algorithm, self.fingerprint
        )
    }
}

#[derive(Clone, Default)]
pub struct KnownHosts {
    path: Option<PathBuf>,
    keys: Vec<HostKey>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileData {
    version: u8,
    keys: Vec<HostKey>,
}

impl KnownHosts {
    pub fn memory() -> Self {
        Self::default()
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let path = path.as_ref().to_path_buf();
        let mut result = Self {
            path: Some(path.clone()),
            keys: Vec::new(),
        };
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(result),
            Err(err) => return Err(err.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_FILE_SIZE + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_SIZE {
            return Err(SessionError::KnownHosts("file exceeds 1 MiB".into()));
        }
        let data: FileData = serde_json::from_slice(&bytes)
            .map_err(|_| SessionError::KnownHosts("malformed file".into()))?;
        if data.version != 1 || data.keys.len() > 4096 {
            return Err(SessionError::KnownHosts(
                "unsupported version or too many keys".into(),
            ));
        }
        for key in data.keys {
            let key = key.checked()?;
            if result
                .keys
                .iter()
                .any(|old| old.host == key.host && old.port == key.port)
            {
                return Err(SessionError::KnownHosts("duplicate host entry".into()));
            }
            result.keys.push(key);
        }
        Ok(result)
    }

    pub fn entries(&self) -> &[HostKey] {
        &self.keys
    }

    /// No implicit first-use acceptance: the caller must display the challenge
    /// and call this only after the user verifies its fingerprint.
    /// Existing different keys cannot be overwritten through this API.
    pub fn trust(&mut self, key: HostKey) -> Result<(), SessionError> {
        let key = key.checked()?;
        let mut next = if let Some(path) = &self.path {
            Self::load(path)?
        } else {
            self.clone()
        };
        match next.verify(&key) {
            Ok(()) => {
                *self = next;
                return Ok(());
            }
            Err(SessionError::UnknownHostKey(_)) => {}
            Err(error) => return Err(error),
        }
        if next.keys.len() >= 4096 {
            return Err(SessionError::KnownHosts("too many keys".into()));
        }
        next.keys.push(key);
        if let Some(path) = &next.path {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            if !parent.exists() {
                fs::create_dir_all(parent)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
                }
            }
            let bytes = serde_json::to_vec_pretty(&FileData {
                version: 1,
                keys: next.keys.clone(),
            })
            .map_err(|_| SessionError::KnownHosts("serialization failed".into()))?;
            if bytes.len() as u64 > MAX_FILE_SIZE {
                return Err(SessionError::KnownHosts("file exceeds 1 MiB".into()));
            }
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temp.as_file()
                    .set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            temp.write_all(&bytes)?;
            temp.as_file().sync_all()?;
            temp.persist(path).map_err(|error| error.error)?;
            #[cfg(unix)]
            fs::File::open(parent)?.sync_all()?;
        }
        *self = next;
        Ok(())
    }

    pub fn verify(&self, key: &HostKey) -> Result<(), SessionError> {
        let key = key.checked()?;
        match self
            .keys
            .iter()
            .find(|old| old.host == key.host && old.port == key.port)
        {
            None => Err(SessionError::UnknownHostKey(key)),
            Some(old) if old.key == key.key => Ok(()),
            Some(old) => Err(SessionError::HostKeyChanged {
                expected: old.clone(),
                received: key,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    fn sample(host: &str, port: u16, byte: u8) -> HostKey {
        let mut wire = Vec::new();
        wire.extend_from_slice(&11_u32.to_be_bytes());
        wire.extend_from_slice(b"ssh-ed25519");
        wire.extend_from_slice(&32_u32.to_be_bytes());
        wire.extend_from_slice(&[byte; 32]);
        let text = format!(
            "ssh-ed25519 {} fixture",
            base64::engine::general_purpose::STANDARD.encode(&wire)
        );
        let key = HostKey::from_openssh(host, port, &text).unwrap();
        let expected = format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(&wire))
        );
        assert_eq!(key.fingerprint, expected);
        key
    }

    #[test]
    fn first_use_requires_explicit_trust_and_key_changes_are_rejected() {
        let mut hosts = KnownHosts::memory();
        let key = sample("Example.COM.", 22, 1);
        assert_eq!(key.host, "example.com");
        assert!(matches!(
            hosts.verify(&key),
            Err(SessionError::UnknownHostKey(_))
        ));
        hosts.trust(key.clone()).unwrap();
        hosts.verify(&key).unwrap();
        let changed = sample("example.com", 22, 2);
        assert!(matches!(
            hosts.verify(&changed),
            Err(SessionError::HostKeyChanged { .. })
        ));
        assert!(matches!(
            hosts.trust(changed),
            Err(SessionError::HostKeyChanged { .. })
        ));
        hosts.verify(&key).unwrap();
        assert!(matches!(
            hosts.verify(&sample("example.com", 2222, 1)),
            Err(SessionError::UnknownHostKey(_))
        ));
    }

    #[test]
    fn known_hosts_persists_and_permissions_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/hosts.json");
        let mut hosts = KnownHosts::load(&path).unwrap();
        let key = sample("host", 22, 1);
        hosts.trust(key.clone()).unwrap();
        KnownHosts::load(&path).unwrap().verify(&key).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        // A second reader's persisted trust is retained in a stale object.
        let mut stale = KnownHosts::load(&path).unwrap();
        let other = sample("other", 22, 2);
        hosts.trust(other.clone()).unwrap();
        stale.trust(other.clone()).unwrap();
        stale.verify(&other).unwrap();
    }

    #[test]
    fn corrupt_fingerprint_cannot_be_trusted_or_loaded() {
        let mut key = sample("host", 22, 1);
        key.fingerprint = "SHA256:forged".into();
        assert!(KnownHosts::memory().trust(key.clone()).is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        fs::write(
            &path,
            serde_json::to_vec(&FileData {
                version: 1,
                keys: vec![key],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(KnownHosts::load(path).is_err());
    }

    #[test]
    fn duplicate_hosts_and_oversized_files_are_rejected() {
        let key = sample("host", 22, 1);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        fs::write(
            &path,
            serde_json::to_vec(&FileData {
                version: 1,
                keys: vec![key.clone(), key],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(KnownHosts::load(&path).is_err());
        fs::write(&path, vec![b' '; MAX_FILE_SIZE as usize + 1]).unwrap();
        assert!(KnownHosts::load(&path).is_err());
    }
}
