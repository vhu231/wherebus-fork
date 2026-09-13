#[tokio::main]
async fn main() -> anyhow::Result<()> {
    wherebus::bot::run().await
}
