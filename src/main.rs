use std::env;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub mod smtp_server;

const MAX_SOCKET_TIMEOUT_MS: u64 = 15 * 1000;

#[tokio::main]
#[cfg(not(tarpaulin_include))]
async fn main() -> anyhow::Result<()> {
    let domain = env::args().nth(1).expect("domain required");

    let addr = env::args()
        .nth(2)
        .unwrap_or_else(|| "0.0.0.0:25".to_string());

    let ctoken = CancellationToken::new();
    let cloned_token = ctoken.clone();

    let listener: TcpListener = TcpListener::bind(addr.to_string()).await?;

    println!("Server started. Listening to {addr} ({domain})");
    smtp_server::start_server(
        cloned_token,
        domain.as_str(),
        listener,
        MAX_SOCKET_TIMEOUT_MS,
    )
    .await?;

    Ok(())
}
