//! Telegram 机器人适配层：直接调用 provider，不经过 HTTP 接口。
//!
//! 与网页版一致的原则：没有模拟数据；上游出错时如实告知用户。
//! 用户数据（城市选择、收藏车次、盯车与查询习惯）落盘为单个 JSON 文件。
//! 机器人与网页版同进程启动，见 [`crate::runtime`]。
pub mod app;
pub mod auth;
pub mod console;
pub(crate) mod db;
pub mod miniapp;
pub mod render;
pub mod store;
pub mod telegram;
pub mod watch;

pub use app::{App, start};
