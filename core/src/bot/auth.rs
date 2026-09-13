//! 管理控制台的口令与会话。
//!
//! 口令用 PBKDF2-HMAC-SHA256 哈希后存库，会话令牌只存在内存里。
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use hmac::{Hmac, KeyInit, Mac};
use parking_lot::Mutex;
use sha2::Sha256;

use crate::bot::store::now_secs;

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    if raw.len() % 2 != 0 {
        return None;
    }
    (0..raw.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&raw[index..index + 2], 16).ok())
        .collect()
}

// ─── 管理控制台口令（网页端，与 Telegram 身份无关） ───

/// PBKDF2-HMAC-SHA256 迭代次数。
const PBKDF2_ITERATIONS: u32 = 200_000;
/// 管理控制台会话有效期。
const ADMIN_SESSION_TTL_SECS: u64 = 8 * 3600;

/// 把口令哈希成 `pbkdf2$sha256$<迭代次数>$<盐>$<哈希>`，存进数据库。
pub fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 16];
    fill_random(&mut salt);
    let derived = pbkdf2(password.as_bytes(), &salt, PBKDF2_ITERATIONS);
    format!(
        "pbkdf2$sha256${PBKDF2_ITERATIONS}${}${}",
        hex(&salt),
        hex(&derived)
    )
}

/// 常数时间校验口令。
pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    let (Some("pbkdf2"), Some("sha256"), Some(iterations), Some(salt), Some(expected)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let (Ok(iterations), Some(salt), Some(expected)) = (
        iterations.parse::<u32>(),
        decode_hex(salt),
        decode_hex(expected),
    ) else {
        return false;
    };
    let derived = pbkdf2(password.as_bytes(), &salt, iterations);
    constant_time_eq(&derived, &expected)
}

fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    // PBKDF2 的第一个（也是唯一一个）输出块：U1 = HMAC(password, salt || 0x00000001)
    let mut block = salt.to_vec();
    block.extend_from_slice(&1u32.to_be_bytes());
    let mut current = hmac(password, &block);
    let mut output = [0u8; 32];
    output.copy_from_slice(&current);
    for _ in 1..iterations.max(1) {
        current = hmac(password, &current);
        for (slot, byte) in output.iter_mut().zip(current.iter()) {
            *slot ^= byte;
        }
    }
    output
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 管理控制台的会话（登录后签发，仅存在内存里）。
#[derive(Default)]
pub struct AdminSessions {
    inner: Mutex<HashMap<String, u64>>,
}

impl AdminSessions {
    pub fn issue(&self) -> String {
        let token = random_token();
        let now = now_secs();
        let mut sessions = self.inner.lock();
        sessions.retain(|_, issued| now.saturating_sub(*issued) < ADMIN_SESSION_TTL_SECS);
        sessions.insert(token.clone(), now);
        token
    }

    pub fn valid(&self, token: &str) -> bool {
        match self.inner.lock().get(token) {
            Some(issued) => now_secs().saturating_sub(*issued) < ADMIN_SESSION_TTL_SECS,
            None => false,
        }
    }

    pub fn revoke(&self, token: &str) {
        self.inner.lock().remove(token);
    }

    /// 改口令后作废所有会话。
    pub fn revoke_all(&self) {
        self.inner.lock().clear();
    }

    pub fn active(&self) -> usize {
        self.inner.lock().len()
    }
}

/// 用 rustls 依赖里已有的 CSPRNG 生成会话令牌。
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes);
    hex(&bytes)
}

/// 复用 rustls 依赖里已有的 CSPRNG。
fn fill_random(bytes: &mut [u8]) {
    static PROVIDER: OnceLock<Arc<rustls::crypto::CryptoProvider>> = OnceLock::new();
    let provider =
        PROVIDER.get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    provider
        .secure_random
        .fill(bytes)
        .expect("系统随机数不可用");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip_and_rejects_wrong_input() {
        let stored = hash_password("正确的口令");
        assert!(stored.starts_with("pbkdf2$sha256$"));
        assert!(verify_password("正确的口令", &stored));
        assert!(!verify_password("错误的口令", &stored));
        assert!(!verify_password("", &stored));
        // 每次哈希都用新的盐
        assert_ne!(stored, hash_password("正确的口令"));
        // 存储格式损坏时一律不通过
        assert!(!verify_password("正确的口令", "乱七八糟"));
        assert!(!verify_password("正确的口令", "pbkdf2$sha256$x$y$z"));
    }

    #[test]
    fn admin_sessions_expire_and_revoke() {
        let sessions = AdminSessions::default();
        let token = sessions.issue();
        assert!(sessions.valid(&token));
        assert!(!sessions.valid("别的令牌"));
        sessions.revoke(&token);
        assert!(!sessions.valid(&token));

        let first = sessions.issue();
        let second = sessions.issue();
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(sessions.active(), 2);
        sessions.revoke_all();
        assert_eq!(sessions.active(), 0);
    }
}
