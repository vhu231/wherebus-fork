//! 最小 Telegram Bot API 客户端：长轮询拉取更新 + 发送/编辑消息。
//!
//! 只依赖仓库已有的 reqwest / serde，不引入机器人框架。
use std::time::Duration;

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

const DEFAULT_API_HOST: &str = "https://api.telegram.org";
/// 长轮询挂起时长（秒），需小于 HTTP 超时。
pub const POLL_TIMEOUT_SECS: u32 = 25;
/// Telegram 单条消息上限 4096 字符，留出安全余量。
const MAX_TEXT_CHARS: usize = 3800;

#[derive(Debug)]
pub enum TgError {
    Network(String),
    Api {
        code: i64,
        description: String,
        retry_after: Option<u64>,
    },
}

impl TgError {
    /// 「消息内容未变化」不是真正的失败，刷新时应忽略。
    pub fn is_not_modified(&self) -> bool {
        matches!(self, Self::Api { description, .. } if description.contains("message is not modified"))
    }

    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            Self::Network(_) => None,
        }
    }
}

impl std::fmt::Display for TgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(e) => write!(f, "网络错误: {e}"),
            Self::Api {
                code, description, ..
            } => write!(f, "Telegram 接口错误 {code}: {description}"),
        }
    }
}

impl std::error::Error for TgError {}

pub type TgResult<T> = Result<T, TgError>;

// ─── 更新对象（只反序列化用得到的字段） ───

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub location: Option<Location>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(default, rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    // Option 字段缺失时 serde 自动填 None，无需 default（否则会要求 T: Default）
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    parameters: Option<ResponseParameters>,
}

#[derive(Deserialize)]
struct ResponseParameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

// ─── 客户端 ───

pub struct Telegram {
    client: reqwest::Client,
    base: String,
}

impl Telegram {
    pub fn new(token: &str) -> TgResult<Self> {
        let root_store =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        let client = reqwest::Client::builder()
            // 必须长于长轮询挂起时间，否则每次轮询都会超时报错
            .timeout(Duration::from_secs(POLL_TIMEOUT_SECS as u64 + 20))
            .use_preconfigured_tls(tls)
            .build()
            .map_err(|e| TgError::Network(e.to_string()))?;

        let host = std::env::var("WHEREBUS_BOT_API")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API_HOST.to_string());

        Ok(Self {
            client,
            base: format!("{}/bot{}", host.trim_end_matches('/'), token),
        })
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, body: Value) -> TgResult<T> {
        let response = self
            .client
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await
            .map_err(|e| TgError::Network(e.to_string()))?;

        let payload: Envelope<T> = response
            .json()
            .await
            .map_err(|e| TgError::Network(format!("响应解析失败: {e}")))?;

        match (payload.ok, payload.result) {
            (true, Some(result)) => Ok(result),
            (true, None) => Err(TgError::Api {
                code: 0,
                description: "接口返回成功但缺少 result".into(),
                retry_after: None,
            }),
            (false, _) => Err(TgError::Api {
                code: payload.error_code.unwrap_or(0),
                description: payload.description.unwrap_or_else(|| "未知错误".into()),
                retry_after: payload.parameters.and_then(|p| p.retry_after),
            }),
        }
    }

    pub async fn get_me(&self) -> TgResult<User> {
        self.call("getMe", json!({})).await
    }

    pub async fn get_updates(&self, offset: i64) -> TgResult<Vec<Update>> {
        self.call(
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": POLL_TIMEOUT_SECS,
                "allowed_updates": ["message", "callback_query"],
            }),
        )
        .await
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        markup: Option<Value>,
    ) -> TgResult<Message> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": clamp(text),
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        if let Some(markup) = markup {
            body["reply_markup"] = markup;
        }
        self.call("sendMessage", body).await
    }

    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        markup: Option<Value>,
    ) -> TgResult<Value> {
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": clamp(text),
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        if let Some(markup) = markup {
            body["reply_markup"] = markup;
        }
        self.call("editMessageText", body).await
    }

    /// 删除一条消息（用于清理请求位置的临时提示）。
    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> TgResult<Value> {
        self.call(
            "deleteMessage",
            json!({"chat_id": chat_id, "message_id": message_id}),
        )
        .await
    }

    pub async fn answer_callback_query(&self, id: &str, text: &str, alert: bool) -> TgResult<Value> {
        self.call(
            "answerCallbackQuery",
            json!({
                "callback_query_id": id,
                "text": text.chars().take(190).collect::<String>(),
                "show_alert": alert,
            }),
        )
        .await
    }

    /// 主动推送的提醒：带通知声音，与静默的卡片更新区分开。
    pub async fn send_alert(&self, chat_id: i64, text: &str) -> TgResult<Message> {
        self.call(
            "sendMessage",
            json!({
                "chat_id": chat_id,
                "text": clamp(text),
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
                "disable_notification": false,
            }),
        )
        .await
    }

    pub async fn set_my_commands(&self, commands: &[(&str, &str)]) -> TgResult<Value> {
        let commands: Vec<Value> = commands
            .iter()
            .map(|(command, description)| json!({"command": command, "description": description}))
            .collect();
        self.call("setMyCommands", json!({ "commands": commands }))
            .await
    }
}

/// 超长文本按字符截断，避免触发 Telegram 的 4096 限制。
fn clamp(text: &str) -> String {
    if text.chars().count() <= MAX_TEXT_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_TEXT_CHARS).collect();
    out.push_str("\n…（内容过长已截断）");
    out
}

// ─── 键盘构造 ───

/// 行内键盘：每个按钮是 (文案, callback_data)。
pub fn inline_keyboard(rows: Vec<Vec<(String, String)>>) -> Value {
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|(text, data)| json!({"text": text, "callback_data": data}))
                .collect::<Vec<Value>>()
                .into()
        })
        .collect();
    json!({ "inline_keyboard": rows })
}

/// 请求用户共享位置的普通键盘（仅私聊可用）。
pub fn location_keyboard() -> Value {
    json!({
        "keyboard": [[{"text": "📍 发送我的位置", "request_location": true}]],
        "resize_keyboard": true,
        "one_time_keyboard": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_truncated_with_notice() {
        let text = "站".repeat(MAX_TEXT_CHARS + 100);
        let out = clamp(&text);
        assert!(out.chars().count() <= MAX_TEXT_CHARS + 20);
        assert!(out.ends_with("（内容过长已截断）"));
        assert_eq!(clamp("短文本"), "短文本");
    }

    #[test]
    fn not_modified_error_is_recognised() {
        let error = TgError::Api {
            code: 400,
            description: "Bad Request: message is not modified".into(),
            retry_after: None,
        };
        assert!(error.is_not_modified());
        assert!(!TgError::Network("boom".into()).is_not_modified());
    }

    #[test]
    fn keyboard_shape_matches_bot_api() {
        let markup = inline_keyboard(vec![vec![("刷新".into(), "m:home".into())]]);
        assert_eq!(markup["inline_keyboard"][0][0]["text"], "刷新");
        assert_eq!(markup["inline_keyboard"][0][0]["callback_data"], "m:home");
    }
}
