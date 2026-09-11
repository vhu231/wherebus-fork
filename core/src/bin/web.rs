#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let address = std::env::var("WHEREBUS_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("WhereBus: http://{}", listener.local_addr()?);
    axum::serve(listener, wherebus::web::router())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
