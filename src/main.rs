use std::env;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod mailserver;
mod redis;

const MAX_SOCKET_TIMEOUT_MS: u64 = 15 * 1000;

#[tokio::main]
#[cfg(not(tarpaulin_include))]
async fn main() -> anyhow::Result<()> {
    let domain = env::args().nth(1).expect("domain required");

    let addr = env::args()
        .nth(2)
        .unwrap_or_else(|| "0.0.0.0:25".to_string());

    let (tx, mut rx) = mpsc::channel::<mailserver::email::Email>(10);

    let ctoken = CancellationToken::new();

    let listener: TcpListener = TcpListener::bind(addr.to_string()).await?;

    let mut set = tokio::task::JoinSet::new();

    set.spawn(redis::redis_storage::start_worker(ctoken.clone(), rx));

    set.spawn(mailserver::smtp_server::start_server(
        ctoken.clone(),
        tx,
        domain.clone(),
        listener,
        MAX_SOCKET_TIMEOUT_MS,
    ));

    while let Some(join_res) = set.join_next().await {
        if let Ok(res) = join_res {
            println!("Handle completed with {:?}", res);
        } else if let Err(err) = join_res {
            println!("Handle errored with {:?}", err);
        }
    }

    Ok(())
}
