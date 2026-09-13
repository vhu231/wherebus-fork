//! 单进程启动：网页版、管理控制台与 Telegram 机器人一起跑，共用同一个端口和同一个 SQLite 库。
//!
//! 配置来自环境变量，启动时会先读取当前目录的 `.env`（真实环境变量优先）。
//! 没有配置 `TELEGRAM_BOT_TOKEN` 时不启动机器人，网页版与管理控制台照常可用。
use std::{path::Path, sync::Arc};

use crate::bot::{console, miniapp, store::Store};

pub async fn serve() -> anyhow::Result<()> {
    // 先读 .env（已存在的环境变量优先），再解析各项配置
    let env_file = std::env::var("WHEREBUS_ENV_FILE").unwrap_or_else(|_| ".env".into());
    let loaded = crate::support::dotenv::load(&env_file);
    if !loaded.is_empty() {
        println!("已从 {env_file} 载入 {} 个配置项", loaded.len());
    }
    if std::env::var_os("WHEREBUS_BOT_TOKEN").is_some()
        && std::env::var_os("TELEGRAM_BOT_TOKEN").is_none()
    {
        eprintln!("提示：WHEREBUS_BOT_TOKEN 已更名为 TELEGRAM_BOT_TOKEN，请更新配置");
    }

    let address = std::env::var("WHEREBUS_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let db_path = std::env::var("WHEREBUS_DB").unwrap_or_else(|_| "wherebus.db".into());

    let store = Arc::new(Store::open(&db_path)?);
    // 旧版本的 JSON 数据文件：库里还没有用户时自动导入一次，原文件保留作备份
    let legacy = std::env::var("WHEREBUS_BOT_DATA").unwrap_or_else(|_| "wherebus-bot-data.json".into());
    match store.import_legacy_json(Path::new(&legacy)) {
        Ok(0) => {}
        Ok(count) => println!("已从 {legacy} 导入 {count} 个用户到 {db_path}"),
        Err(error) => eprintln!("导入 {legacy} 失败（已忽略）：{error}"),
    }

    let bot = crate::bot::start(Arc::clone(&store)).await?;
    let mut router = crate::web::router().merge(console::router(
        Arc::clone(&store),
        bot.clone(),
    ));
    if let Some(bot) = &bot {
        router = router.merge(miniapp::router(Arc::clone(bot)));
    }

    let listener = tokio::net::TcpListener::bind(&address).await?;
    let address = listener.local_addr()?;
    println!("WhereBus: http://{address}");
    println!("管理控制台：http://{address}/admin · 数据库 {db_path}");
    match &bot {
        Some(_) => println!("Mini App（用户面板）：http://{address}/miniapp"),
        None => println!("未设置 TELEGRAM_BOT_TOKEN，本次不启动机器人（网页与控制台照常可用）"),
    }

    let server = axum::serve(listener, router).with_graceful_shutdown(shutdown());
    match bot {
        Some(bot) => {
            // 同一进程：任一方退出（Ctrl-C）都结束整个服务
            tokio::select! {
                result = server => result?,
                _ = bot.poll_updates(shutdown()) => {}
            }
            println!("\n正在退出…");
        }
        None => server.await?,
    }
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
