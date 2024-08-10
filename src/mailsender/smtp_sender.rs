use tokio::{io::AsyncReadExt, io::AsyncWriteExt, net::TcpStream};

use crate::mailserver::smtp_server;

pub async fn send(
    srv_addr: String,
    mail_from: String,
    mail_to: String,
    mail_content: String,
) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(srv_addr)
        .await
        .expect("Unable to connect to server");

    smtp_read_greet(&mut stream).await?;

    smtp_send_and_recv(
        &mut stream,
        b"HELO rs-mailserver-sender\r\n",
        smtp_server::RESP_OK,
    )
    .await?;

    smtp_send_and_recv(
        &mut stream,
        format!("MAIL FROM:<{mail_from}>\r\n").as_bytes(),
        smtp_server::RESP_OK,
    )
    .await?;

    smtp_send_and_recv(
        &mut stream,
        format!("RCPT TO:<{mail_to}>\r\n").as_bytes(),
        smtp_server::RESP_OK,
    )
    .await?;

    smtp_send_and_recv(&mut stream, b"DATA\r\n", smtp_server::RESP_END_DATA_WITH).await?;

    let mut mail_data = Vec::new();
    mail_data.extend_from_slice(mail_content.as_bytes());
    mail_data.extend_from_slice(smtp_server::EMAIL_TERM);

    smtp_send_and_recv(&mut stream, &mail_data, smtp_server::RESP_OK).await?;

    smtp_send_and_recv(&mut stream, b"QUIT\r\n", smtp_server::RESP_BYE).await?;

    Ok(())
}

async fn smtp_read_greet(stream: &mut TcpStream) -> anyhow::Result<()> {
    let mut buf = vec![0; 1024];
    let _ = stream.read(&mut buf).await?;
    Ok(())
}

async fn smtp_send_and_recv(
    stream: &mut TcpStream,
    command: &[u8],
    expect: &[u8],
) -> anyhow::Result<()> {
    let mut buf = vec![0; 1024];
    stream
        .write(command)
        .await
        .expect("Failed to write to tcpstream");

    let n = stream
        .read(&mut buf)
        .await
        .expect("Failed to read from tcpstream");

    let exp_resp = std::str::from_utf8(expect).expect("Unexpected non-utf8 expect content");
    let src_resp =
        std::str::from_utf8(&buf[0..n]).expect("Got unexpected non-utf8 data from server");

    if exp_resp != src_resp {
        anyhow::bail!("Unexpected response from server: {src_resp}")
    }

    Ok(())
}
