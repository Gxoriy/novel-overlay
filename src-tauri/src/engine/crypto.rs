//! 暴露给 JS 的加密原语。
//!
//! 设计取舍：高频签名（MD5/SHA/HMAC/AES）用 Rust 实现，不依赖 JS 侧再带一份
//! crypto-js；只有站点自己那套拼装逻辑才交给 JS 跑。

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::{Aes128, Aes192, Aes256};
use anyhow::{anyhow, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use hmac::{Hmac, Mac};
use md5::{Digest as Md5Digest, Md5};
use sha1::Sha1;
use sha2::Sha256;

pub fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn sha1_hex(input: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn hmac_sha1_hex(key: &str, message: &str) -> Result<String> {
    // `Mac::new_from_slice` 与 `KeyInit::new_from_slice` 同名，需限定 trait 消歧。
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(key.as_bytes())
        .map_err(|error| anyhow!("HMAC-SHA1 密钥非法: {error}"))?;
    mac.update(message.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

pub fn hmac_sha256_hex(key: &str, message: &str) -> Result<String> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_bytes())
        .map_err(|error| anyhow!("HMAC-SHA256 密钥非法: {error}"))?;
    mac.update(message.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

pub fn base64_encode(input: &str) -> String {
    BASE64.encode(input.as_bytes())
}

pub fn base64_decode(input: &str) -> Result<String> {
    let bytes = BASE64
        .decode(input.trim())
        .map_err(|error| anyhow!("base64 解码失败: {error}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn hex_encode(input: &str) -> String {
    hex::encode(input.as_bytes())
}

pub fn hex_decode(input: &str) -> Result<String> {
    let bytes = hex::decode(input.trim()).map_err(|error| anyhow!("hex 解码失败: {error}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// AES 加密，返回 base64。
///
/// - `mode`：`ECB` 或 `CBC`（大小写不敏感）
/// - `key`：16 / 24 / 32 字节，决定 AES-128 / 192 / 256
/// - `iv`：仅 CBC 需要，16 字节
/// - 填充：PKCS7
pub fn aes_encrypt(mode: &str, key: &str, iv: &str, data: &str) -> Result<String> {
    let cipher_text = match mode.trim().to_ascii_uppercase().as_str() {
        "ECB" => ecb_encrypt(key.as_bytes(), data.as_bytes())?,
        "CBC" => cbc_encrypt(key.as_bytes(), iv.as_bytes(), data.as_bytes())?,
        other => return Err(anyhow!("不支持的 AES 模式 `{other}`，只支持 ECB / CBC")),
    };
    Ok(BASE64.encode(cipher_text))
}

/// AES 解密。`data` 为 base64 密文，返回 UTF-8 文本。
pub fn aes_decrypt(mode: &str, key: &str, iv: &str, data: &str) -> Result<String> {
    let raw = BASE64
        .decode(data.trim())
        .map_err(|error| anyhow!("密文不是合法 base64: {error}"))?;
    let plain = match mode.trim().to_ascii_uppercase().as_str() {
        "ECB" => ecb_decrypt(key.as_bytes(), &raw)?,
        "CBC" => cbc_decrypt(key.as_bytes(), iv.as_bytes(), &raw)?,
        other => return Err(anyhow!("不支持的 AES 模式 `{other}`，只支持 ECB / CBC")),
    };
    Ok(String::from_utf8_lossy(&plain).into_owned())
}

fn key_error() -> anyhow::Error {
    anyhow!("AES 密钥长度必须为 16 / 24 / 32 字节")
}

fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let padding = 16 - (data.len() % 16);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(padding as u8).take(padding));
    out
}

fn pkcs7_unpad(mut data: Vec<u8>) -> Vec<u8> {
    if let Some(&last) = data.last() {
        let padding = last as usize;
        if padding > 0 && padding <= 16 && padding <= data.len() {
            data.truncate(data.len() - padding);
        }
    }
    data
}

fn ecb_encrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut buffer = pkcs7_pad(data);
    match key.len() {
        16 => {
            let cipher = Aes128::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                cipher.encrypt_block(GenericArray::from_mut_slice(block));
            }
        }
        24 => {
            let cipher = Aes192::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                cipher.encrypt_block(GenericArray::from_mut_slice(block));
            }
        }
        32 => {
            let cipher = Aes256::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                cipher.encrypt_block(GenericArray::from_mut_slice(block));
            }
        }
        _ => return Err(key_error()),
    }
    Ok(buffer)
}

fn ecb_decrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        return Err(anyhow!("ECB 密文长度必须是 16 的整数倍"));
    }
    let mut buffer = data.to_vec();
    match key.len() {
        16 => {
            let cipher = Aes128::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                cipher.decrypt_block(GenericArray::from_mut_slice(block));
            }
        }
        24 => {
            let cipher = Aes192::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                cipher.decrypt_block(GenericArray::from_mut_slice(block));
            }
        }
        32 => {
            let cipher = Aes256::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                cipher.decrypt_block(GenericArray::from_mut_slice(block));
            }
        }
        _ => return Err(key_error()),
    }
    Ok(pkcs7_unpad(buffer))
}

/// CBC 加密：手写链接（前一密文块异或进当前明文块），与 ECB 保持同一套原语。
///
/// 不用 `cbc::Encryptor::encrypt_padded_vec_mut` 的原因：那条路径要求 `alloc` 特性
/// 且把填充策略绑死在类型参数上；自己实现只有 20 行，反而更可控。
fn cbc_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if iv.len() != 16 {
        return Err(anyhow!("CBC 的 iv 必须 16 字节，当前 {}", iv.len()));
    }
    let mut buffer = pkcs7_pad(data);
    let mut prev = iv.to_vec();
    match key.len() {
        16 => {
            let cipher = Aes128::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                for (byte, chain) in block.iter_mut().zip(prev.iter()) {
                    *byte ^= *chain;
                }
                cipher.encrypt_block(GenericArray::from_mut_slice(block));
                prev.copy_from_slice(block);
            }
        }
        24 => {
            let cipher = Aes192::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                for (byte, chain) in block.iter_mut().zip(prev.iter()) {
                    *byte ^= *chain;
                }
                cipher.encrypt_block(GenericArray::from_mut_slice(block));
                prev.copy_from_slice(block);
            }
        }
        32 => {
            let cipher = Aes256::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                for (byte, chain) in block.iter_mut().zip(prev.iter()) {
                    *byte ^= *chain;
                }
                cipher.encrypt_block(GenericArray::from_mut_slice(block));
                prev.copy_from_slice(block);
            }
        }
        _ => return Err(key_error()),
    }
    Ok(buffer)
}

/// CBC 解密：先解密，再异或回上一段密文。
fn cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if iv.len() != 16 {
        return Err(anyhow!("CBC 的 iv 必须 16 字节，当前 {}", iv.len()));
    }
    if data.is_empty() || data.len() % 16 != 0 {
        return Err(anyhow!("CBC 密文长度必须是 16 的整数倍"));
    }
    let mut buffer = data.to_vec();
    let mut prev = iv.to_vec();
    match key.len() {
        16 => {
            let cipher = Aes128::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                let mut cipher_block = [0u8; 16];
                cipher_block.copy_from_slice(block);
                cipher.decrypt_block(GenericArray::from_mut_slice(block));
                for (byte, chain) in block.iter_mut().zip(prev.iter()) {
                    *byte ^= *chain;
                }
                prev.copy_from_slice(&cipher_block);
            }
        }
        24 => {
            let cipher = Aes192::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                let mut cipher_block = [0u8; 16];
                cipher_block.copy_from_slice(block);
                cipher.decrypt_block(GenericArray::from_mut_slice(block));
                for (byte, chain) in block.iter_mut().zip(prev.iter()) {
                    *byte ^= *chain;
                }
                prev.copy_from_slice(&cipher_block);
            }
        }
        32 => {
            let cipher = Aes256::new_from_slice(key).map_err(|_| key_error())?;
            for block in buffer.chunks_exact_mut(16) {
                let mut cipher_block = [0u8; 16];
                cipher_block.copy_from_slice(block);
                cipher.decrypt_block(GenericArray::from_mut_slice(block));
                for (byte, chain) in block.iter_mut().zip(prev.iter()) {
                    *byte ^= *chain;
                }
                prev.copy_from_slice(&cipher_block);
            }
        }
        _ => return Err(key_error()),
    }
    Ok(pkcs7_unpad(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hashes() {
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn base64_roundtrip() {
        let encoded = base64_encode("斗破苍穹");
        assert_eq!(base64_decode(&encoded).unwrap(), "斗破苍穹");
    }

    #[test]
    fn aes_cbc_roundtrip() {
        let key = "0123456789abcdef";
        let iv = "abcdef0123456789";
        let cipher_text = aes_encrypt("CBC", key, iv, "sign=abc&t=1700000000").unwrap();
        assert_ne!(cipher_text, "sign=abc&t=1700000000");
        assert_eq!(
            aes_decrypt("CBC", key, iv, &cipher_text).unwrap(),
            "sign=abc&t=1700000000"
        );
    }

    #[test]
    fn aes_ecb_roundtrip() {
        let key = "0123456789abcdef";
        let cipher_text = aes_encrypt("ECB", key, "", "hello world").unwrap();
        assert_eq!(
            aes_decrypt("ECB", key, "", &cipher_text).unwrap(),
            "hello world"
        );
    }

    #[test]
    fn rejects_bad_key_length() {
        assert!(aes_encrypt("CBC", "short", "0123456789abcdef", "x").is_err());
    }
}
