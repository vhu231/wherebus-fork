#[tokio::main]
async fn main() -> anyhow::Result<()> {
    wherebus::runtime::serve().await
}
