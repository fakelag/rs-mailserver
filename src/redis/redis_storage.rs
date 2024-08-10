use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use crate::mailserver::email::Email;

pub async fn start_worker(ct: CancellationToken, mut rx: Receiver<Email>) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            _ = ct.cancelled() => {
                break;
            }
            recv_result = rx.recv() => {
                if let Some(email) = recv_result {
                    let mail_from = email.mail_from;
                    break;

                } else {
                    println!("Channel is dropped.");
                    break;
                }
            }
        }
    }

    println!("Exiting redis worker...");
    Ok(())
}
