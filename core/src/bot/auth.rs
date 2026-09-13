//! Telegram Mini App 登录：校验 initData 签名，签发会话令牌。
//!
//! 校验流程按 Telegram 官方规范：
//! `secret = HMAC_SHA256(key = "WebAppData", msg = bot_token)`，
//! `hash = HMAC_SHA256(key = secret, msg = 按 key 排序的 "k=v" 用 \n 连接)`。
//! 服务端只信任这个签名，不信任前端传来的任何用户身份字段。
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use hmac::{Hmac, KeyInit, Mac};
use parking_lot::Mutex;
use serde::Serialize;
use sha2::Sha256;

use crate::bot::store::now_secs;

type HmacSha256 = Hmac<Sha256>;

/// initData 的有效期：超过这个时间的签名不再接受。
const MAX_AUTH_AGE_SECS: u64 = 24 * 3600;
/// 会话令牌有效期。
const SESSION_TTL_SECS: u64 = 12 * 3600;

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Malformed(&'static str),
    BadSignature,
    Expired,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "登录数据格式不正确：{what}"),
            Self::BadSignature => write!(f, "登录数据签名校验失败"),
            Self::Expired => write!(f, "登录数据已过期，请重新打开小程序"),
        }
    }
}

impl std::error::Error for AuthError {}

/// 从 initData 中解析出的 Telegram 用户。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TelegramUser {
    pub id: i64,
    pub name: String,
    pub username: Option<String>,
}

/// 校验 Mini App 的 initData，成功后返回其中的用户。
pub fn verify_init_data(
    init_data: &str,
    bot_token: &str,
    now: u64,
) -> Result<TelegramUser, AuthError> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut hash: Option<String> = None;

    for chunk in init_data.split('&').filter(|chunk| !chunk.is_empty()) {
        let (key, value) = chunk
            .split_once('=')
            .ok_or(AuthError::Malformed("参数缺少 = 分隔"))?;
        let key = percent_decode(key);
        let value = percent_decode(value);
        match key.as_str() {
            "hash" => hash = Some(value),
            // signature 属于另一套 Ed25519 校验流程，不参与 HMAC 计算
            "signature" => {}
            _ => pairs.push((key, value)),
        }
    }

    let hash = hash.ok_or(AuthError::Malformed("缺少 hash"))?;
    let auth_date: u64 = pairs
        .iter()
        .find(|(key, _)| key == "auth_date")
        .and_then(|(_, value)| value.parse().ok())
        .ok_or(AuthError::Malformed("缺少 auth_date"))?;
    // 允许少量时钟偏差，但过期的签名一律拒绝
    if now.saturating_sub(auth_date) > MAX_AUTH_AGE_SECS {
        return Err(AuthError::Expired);
    }

    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let data_check_string = pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");

    let expected = decode_hex(&hash).ok_or(AuthError::Malformed("hash 不是十六进制"))?;
    let secret = hmac(b"WebAppData", bot_token.as_bytes());
    let mut mac =
        HmacSha256::new_from_slice(&secret).map_err(|_| AuthError::Malformed("密钥长度非法"))?;
    mac.update(data_check_string.as_bytes());
    // verify_slice 是常数时间比较
    mac.verify_slice(&expected)
        .map_err(|_| AuthError::BadSignature)?;

    let user = pairs
        .iter()
        .find(|(key, _)| key == "user")
        .map(|(_, value)| value.as_str())
        .ok_or(AuthError::Malformed("缺少 user"))?;
    let user: serde_json::Value =
        serde_json::from_str(user).map_err(|_| AuthError::Malformed("user 不是合法 JSON"))?;
    let id = user
        .get("id")
        .and_then(|value| value.as_i64())
        .ok_or(AuthError::Malformed("user.id 缺失"))?;
    let first = user
        .get("first_name")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let last = user
        .get("last_name")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let name = format!("{first} {last}").trim().to_string();

    Ok(TelegramUser {
        id,
        name: if name.is_empty() {
            format!("用户 {id}")
        } else {
            name
        },
        username: user
            .get("username")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

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

/// initData 是 URL 编码的查询串，这里做最小实现的百分号解码。
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match u8::from_str_radix(&raw[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ─── 会话 ───

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub user: TelegramUser,
    pub issued_at: u64,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub fn issue(&self, user: TelegramUser) -> (String, Session) {
        let session = Session {
            user,
            issued_at: now_secs(),
        };
        let token = random_token();
        let mut sessions = self.inner.lock();
        let now = now_secs();
        sessions.retain(|_, existing| now.saturating_sub(existing.issued_at) < SESSION_TTL_SECS);
        sessions.insert(token.clone(), session.clone());
        (token, session)
    }

    pub fn lookup(&self, token: &str) -> Option<Session> {
        let session = self.inner.lock().get(token).cloned()?;
        if now_secs().saturating_sub(session.issued_at) >= SESSION_TTL_SECS {
            self.inner.lock().remove(token);
            return None;
        }
        Some(session)
    }

    pub fn revoke(&self, token: &str) {
        self.inner.lock().remove(token);
    }

    /// 用户被停用或注销后，作废其全部会话。
    pub fn revoke_user(&self, user_id: i64) {
        self.inner.lock().retain(|_, s| s.user.id != user_id);
    }

    pub fn active(&self) -> usize {
        self.inner.lock().len()
    }
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

    const TOKEN: &str = "123456:TEST-TOKEN";

    /// 按官方算法生成一份合法 initData，用于测试。
    fn sign(pairs: &[(&str, &str)]) -> String {
        let mut sorted: Vec<(&str, &str)> = pairs.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let check = sorted
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");
        let secret = hmac(b"WebAppData", TOKEN.as_bytes());
        let digest = hmac(&secret, check.as_bytes());
        let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let encoded = sorted
            .iter()
            .map(|(key, value)| format!("{key}={}", percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{encoded}&hash={hash}")
    }

    fn percent_encode(raw: &str) -> String {
        raw.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (byte as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect()
    }

    #[test]
    fn accepts_a_correctly_signed_init_data() {
        let user = r#"{"id":42,"first_name":"小明","last_name":"王","username":"xiaoming"}"#;
        let init_data = sign(&[("auth_date", "1000"), ("query_id", "AAA"), ("user", user)]);
        let parsed = verify_init_data(&init_data, TOKEN, 1200).unwrap();
        assert_eq!(
            parsed,
            TelegramUser {
                id: 42,
                name: "小明 王".into(),
                username: Some("xiaoming".into()),
            }
        );
    }

    #[test]
    fn rejects_tampered_or_stale_data() {
        let user = r#"{"id":42,"first_name":"小明"}"#;
        let init_data = sign(&[("auth_date", "1000"), ("user", user)]);

        // 改动任何字段都会让签名失效
        let tampered = init_data.replace("42", "43");
        assert_eq!(
            verify_init_data(&tampered, TOKEN, 1200),
            Err(AuthError::BadSignature)
        );
        // 换一个 bot token 也无法通过
        assert_eq!(
            verify_init_data(&init_data, "999:OTHER", 1200),
            Err(AuthError::BadSignature)
        );
        // 超过有效期
        assert_eq!(
            verify_init_data(&init_data, TOKEN, 1000 + MAX_AUTH_AGE_SECS + 1),
            Err(AuthError::Expired)
        );
        // 缺 hash
        assert!(matches!(
            verify_init_data("auth_date=1000", TOKEN, 1000),
            Err(AuthError::Malformed(_))
        ));
    }

    #[test]
    fn percent_decoding_handles_utf8_and_plus() {
        assert_eq!(percent_decode("%E5%85%AC%E4%BA%A4"), "公交");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
    }

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
        assert_eq!(sessions.active(), 2);
        sessions.revoke_all();
        assert_eq!(sessions.active(), 0);
    }

    #[test]
    fn sessions_are_unique_per_login() {
        let sessions = Sessions::default();
        let user = TelegramUser {
            id: 42,
            name: "小明".into(),
            username: None,
        };
        let (token_a, session) = sessions.issue(user.clone());
        let (token_b, _) = sessions.issue(user.clone());
        assert_ne!(token_a, token_b);
        assert_eq!(token_a.len(), 64);
        assert_eq!(session.user.id, 42);
        assert_eq!(sessions.lookup(&token_a).unwrap().user.id, 42);
        assert!(sessions.lookup("不存在").is_none());

        sessions.revoke(&token_a);
        assert!(sessions.lookup(&token_a).is_none());
        sessions.revoke_user(42);
        assert!(sessions.lookup(&token_b).is_none());
        assert_eq!(sessions.active(), 0);
    }
}
