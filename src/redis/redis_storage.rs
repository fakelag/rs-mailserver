use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use crate::mailserver::email::Email;

pub async fn start_worker(
    ct: CancellationToken,
    redis_addr: String,
    mut rx: Receiver<Email>,
) -> anyhow::Result<()> {
    let mut con = redis::Client::open(redis_addr)?
        .get_multiplexed_async_connection()
        .await?;

    loop {
        tokio::select! {
            _ = ct.cancelled() => {
                break;
            }
            recv_result = rx.recv() => {
                if let Some(email) = recv_result {
                    let serialize_result = serde_json::to_string(&email);

                    if let Ok(email_json) = serialize_result {
                        let redis_res = redis::cmd("PUBLISH")
                            .arg(email.mail_to.clone())
                            .arg(email_json)
                            .exec_async(&mut con)
                            .await;

                        if let Ok(_res) = redis_res {
                            println!("Wrote email from {from} -> {to} of {size} bytes to redis",
                                from = email.mail_from,
                                to=email.mail_to,
                                size=email.mail_content.len(),
                            )
                        } else if let Err(err) = redis_res {
                            eprintln!("ERROR: Writing to redis failed: {err}")
                        }
                    } else if let Err(err) = serialize_result {
                        eprintln!("ERROR: Serializing email to JSON failed: {err}")
                    }

                    continue;
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
