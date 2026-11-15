//! Versioned envelope. The fixed v1 KDF parameters cannot be supplied by an untrusted backup.
//! Layout: magic[8], version[1], KDF[1], salt[16], nonce[24], plaintext_len_le[4], AEAD ciphertext.
//! The complete 54-byte header is authenticated as additional data.
use anyhow::{ensure, Context, Result};
use argon2::{Algorithm, Argon2, Block, Params, Version};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use rand::{rngs::OsRng, RngCore};
use zeroize::Zeroizing;

pub const MAX_PLAINTEXT_BYTES: usize = 16 * 1024 * 1024;
pub const HEADER_LEN: usize = 54;
pub const MAX_ENVELOPE_BYTES: usize = HEADER_LEN + MAX_PLAINTEXT_BYTES + 16;
const MAGIC: &[u8; 8] = b"AURVAULT";
const KDF_ID: u8 = 1; // Argon2id v0x13, 65536 KiB, t=3, p=1, 32-byte output.

pub fn validate_new_password(password: &str) -> Result<()> {
    ensure!(
        password.bytes().all(|b| (0x20..=0x7e).contains(&b)),
        "主密码只能包含普通 ASCII 字符（空格、英文字母、数字及半角符号）"
    );
    ensure!(
        password.chars().count() >= 12,
        "主密码至少需要 12 个字符；建议使用较长的独特口令"
    );
    ensure!(password.len() <= 1024, "主密码不能超过 1024 字节");
    Ok(())
}

fn derive_key(password: &str, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    ensure!(!password.is_empty() && password.len() <= 1024, "主密码无效");
    let params = Params::new(65536, 3, 1, Some(32)).map_err(|_| anyhow::anyhow!("加密参数无效"))?;
    let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
    let kdf = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    kdf.hash_password_into_with_memory(
        password.as_bytes(),
        salt,
        key.as_mut(),
        memory.as_mut_slice(),
    )
    .map_err(|_| anyhow::anyhow!("密钥派生失败"))?;
    Ok(key)
}

/// Checks bounds and structural validity without deriving a key or trusting its contents.
pub fn validate_envelope(blob: &[u8]) -> Result<()> {
    ensure!(
        (HEADER_LEN + 16..=MAX_ENVELOPE_BYTES).contains(&blob.len()),
        "加密文件大小无效（上限 16 MiB）"
    );
    ensure!(
        &blob[..8] == MAGIC && blob[8] == 1 && blob[9] == KDF_ID,
        "不是受支持的 SyncSSH 加密凭据库"
    );
    let declared_len = u32::from_le_bytes(blob[50..54].try_into().context("文件头无效")?) as usize;
    ensure!(
        declared_len <= MAX_PLAINTEXT_BYTES && blob.len() == HEADER_LEN + declared_len + 16,
        "加密文件已截断或长度无效"
    );
    Ok(())
}

pub fn encrypt(password: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    validate_new_password(password)?;
    ensure!(
        plaintext.len() <= MAX_PLAINTEXT_BYTES,
        "凭据库过大（上限 16 MiB）"
    );
    let mut header = [0u8; HEADER_LEN];
    header[..8].copy_from_slice(MAGIC);
    header[8] = 1;
    header[9] = KDF_ID;
    OsRng
        .try_fill_bytes(&mut header[10..50])
        .context("操作系统随机数生成失败")?;
    header[50..54].copy_from_slice(&(plaintext.len() as u32).to_le_bytes());
    let key = derive_key(password, &header[10..26])?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref())
        .map_err(|_| anyhow::anyhow!("密钥初始化失败"))?;
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&header[26..50]),
            Payload {
                msg: plaintext,
                aad: &header,
            },
        )
        .map_err(|_| anyhow::anyhow!("凭据库加密失败"))?;
    let mut blob = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    blob.extend_from_slice(&header);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

pub fn decrypt(password: &str, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    validate_envelope(blob)?;
    let key = derive_key(password, &blob[10..26])?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref())
        .map_err(|_| anyhow::anyhow!("密钥初始化失败"))?;
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&blob[26..50]),
            Payload {
                msg: &blob[HEADER_LEN..],
                aad: &blob[..HEADER_LEN],
            },
        )
        .map_err(|_| anyhow::anyhow!("主密码错误，或加密文件已损坏/被修改"))?;
    Ok(Zeroizing::new(plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;
    const PASSWORD: &str = "a long unique test passphrase";
    #[test]
    fn new_master_passwords_only_accept_printable_ascii() {
        assert!(validate_new_password("ASCII letters 123 !@#$%^&*() ").is_ok());
        assert!(validate_new_password(&"a".repeat(1024)).is_ok());
        for invalid in [
            "短密码 short password",
            "fullwidth Ａ password",
            "newline password\n",
            "control password\u{7f}",
            "short",
        ] {
            assert!(validate_new_password(invalid).is_err());
        }
        assert!(validate_new_password(&"a".repeat(1025)).is_err());
    }
    #[test]
    fn round_trip_and_randomized_ciphertext() {
        let first = encrypt(PASSWORD, b"credential-secret").unwrap();
        let second = encrypt(PASSWORD, b"credential-secret").unwrap();
        assert_ne!(first, second);
        assert_eq!(&**decrypt(PASSWORD, &first).unwrap(), b"credential-secret");
        assert!(!first.windows(17).any(|w| w == b"credential-secret"));
    }
    #[test]
    fn wrong_password_and_tampering_fail() {
        let blob = encrypt(PASSWORD, b"sensitive").unwrap();
        assert!(decrypt("different password", &blob).is_err());
        for index in [10, 26, HEADER_LEN, blob.len() - 1] {
            let mut changed = blob.clone();
            changed[index] ^= 1;
            assert!(decrypt(PASSWORD, &changed).is_err());
        }
    }
    #[test]
    fn malformed_headers_rejected_before_kdf() {
        for bad in [&[][..], b"AURVAULT", &[0u8; HEADER_LEN + 16]] {
            assert!(validate_envelope(bad).is_err());
        }
        let mut header = vec![0u8; HEADER_LEN + 16];
        header[..8].copy_from_slice(MAGIC);
        header[8] = 1;
        header[9] = 1;
        validate_envelope(&header).unwrap();
        header[8] = 2;
        assert!(validate_envelope(&header).is_err());
        header[8] = 1;
        header[50..54].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(validate_envelope(&header).is_err());
    }
    #[test]
    fn short_password_rejected_at_creation() {
        assert!(encrypt("weak", b"x").is_err());
    }
}
