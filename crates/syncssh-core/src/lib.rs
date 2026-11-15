//! Encrypted, portable vault and client-side encrypted S3 / WebDAV snapshots.
//! No password, private key, or cloud secret is written in plaintext by this crate.
pub mod backup;
pub mod crypto;
pub mod models;
pub mod vault;
pub mod webdav;

pub use models::{
    ConnectionOptions, Credential, ObjectStorageBackup, S3Config, S3Provider, ServerProfile,
    TunnelMode, TunnelRule, VaultData, WebDavConfig,
};
pub use vault::VaultStore;
