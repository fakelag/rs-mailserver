use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use crate::mailserver::email::Email;

pub async fn start_worker(ct: CancellationToken, mut rx: Receiver<Email>) -> anyhow::Result<()> {
    todo!()
}
