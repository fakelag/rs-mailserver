use anyhow::Context;
use std::env;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod mailsender;
mod mailserver;
mod redis;

const MAX_SOCKET_TIMEOUT_MS: u64 = 15 * 1000;

#[tokio::main]
#[cfg(not(tarpaulin_include))]
async fn main() -> anyhow::Result<()> {
    let program = env::args().nth(0).expect("program expected");
    let send_usage = format!(
        "Usage:\n\t{program} send <mail_from> <mail_to> <mail_content>
\t{program} send from@yourdomain.com to@yourdomain.com \"email contents\""
    );

    let recv_usage = format!(
        "Usage:\n\t{program} recv <server_domain> [redis_addr] [listen_address]
\t{program} recv yourdomain.com redis://localhost:6379 0.0.0.0:25"
    );

    let mode = env::args()
        .nth(1)
        .expect("expected argument help/send/recv");

    if mode == "help" {
        println!("Starting SMTP client:\n{send_usage}\nStarting SMTP server:\n{recv_usage}");
        return Ok(());
    } else if mode == "send" {
        let addr = env::args().nth(2).expect("server address required");
        let mail_from = env::args()
            .nth(3)
            .context(format!("missing <mail_from> argument\n{send_usage}"))?;

        let mail_to = env::args()
            .nth(4)
            .context(format!("missing <mail_to> argument\n{send_usage}"))?;

        let mail_content = env::args()
            .nth(5)
            .context(format!("missing <mail_content> argument\n{send_usage}"))?;

        mailsender::smtp_sender::send(addr, mail_from, mail_to, mail_content).await?;
        println!("Ok");
        return Ok(());
    } else if mode != "recv" {
        panic!("Invalid mode \"{mode}\". Expected help/send/recv")
    }

    let domain = env::args()
        .nth(2)
        .context(format!("missing <server_domain> argument\n{recv_usage}"))?;

    let redis_addr = env::args()
        .nth(3)
        .unwrap_or("redis://localhost:6379".to_string());

    let addr = env::args().nth(4).unwrap_or("0.0.0.0:25".to_string());

    let (tx, rx) = mpsc::channel::<mailserver::email::Email>(10);

    let ctoken = CancellationToken::new();

    let listener: TcpListener = TcpListener::bind(addr.to_string()).await?;

    let mut set = tokio::task::JoinSet::new();

    set.spawn(redis::redis_storage::start_worker(
        ctoken.clone(),
        redis_addr,
        rx,
    ));

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
