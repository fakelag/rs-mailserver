use crate::mpsc::error::SendTimeoutError;
use anyhow::Context;
use email_address::EmailAddress;
use std::str::{FromStr, SplitWhitespace};
use tokio::sync::mpsc::Sender;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{tcp::ReadHalf, tcp::WriteHalf, TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

use super::email::Email;

const MAX_SOCKET_READ_BYTES: usize = 512_000; // 512kb

pub const EMAIL_TERM: &[u8; 5] = b"\r\n.\r\n";
pub const RESP_MAILSERVER_GREET: &[u8] = b"220 rs-mailserver\r\n";
pub const RESP_END_DATA_WITH: &[u8] = b"354 End data with <CR><LF>.<CR><LF>\r\n";
pub const RESP_OK: &[u8] = b"250 Ok\r\n";
pub const RESP_AUTH_OK: &[u8] = b"235 Ok\n";
pub const RESP_BYE: &[u8] = b"221 Bye\r\n";

pub const RESP_UNKNOWN_COMMAND: &[u8] = b"500 Command not recognized\r\n";
pub const RESP_SYNTAX_ERROR: &[u8] = b"500 Syntax error\r\n";
pub const RESP_BUSY_ERROR: &[u8] = b"450 mailbox unavailable\r\n";
pub const RESP_SERVICE_UNAVAILABLE: &[u8] =
    b"421 Service not available, closing transmission channel\r\n";

pub async fn start_server(
    ct: CancellationToken,
    tx: Sender<Email>,
    domain: String,
    listener: TcpListener,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    println!(
        "SMTP Server started. Listening to {addr} ({domain})",
        addr = listener.local_addr()?
    );

    loop {
        tokio::select! {
            _ = ct.cancelled() => {
                break
            }
            Ok((tcp_stream, remote_addr)) = listener.accept() => {
                let remote_ip = remote_addr.ip().to_string();
                println!("Accepted connection from {remote_ip}");

                tokio::spawn(handle_connection(ct.clone(), tx.clone(), tcp_stream, timeout_ms, domain.clone()));
            }
        }
    }

    println!("Exiting SMTP server...");
    Ok(())
}

#[derive(Debug)]
enum SmtpError {
    CommandParsingError(String),
    NoContentError,
    PayloadTooLargeError,
    RateLimitError,
    ServerShutdown,
}

impl std::fmt::Display for SmtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CommandParsingError(command_name) => {
                write!(f, "invalid syntax for command \"{command_name}\"",)
            }
            Self::NoContentError => write!(f, "no content received before timeout"),
            Self::PayloadTooLargeError => write!(f, "socket payload too large"),
            Self::RateLimitError => write!(f, "rate limited, channel is too busy"),
            Self::ServerShutdown => write!(f, "server shutting down"),
        }
    }
}

async fn read_more(
    ct: CancellationToken,
    reader: &mut ReadHalf<'_>,
    timeout_ms: u64,
    max_read_size: usize,
    to: &mut Vec<u8>,
) -> anyhow::Result<usize> {
    const TMP_BUFFER_SIZE: usize = 4096;

    let mut total_read = 0;
    let mut current_timeout = timeout_ms;

    loop {
        let mut buf = vec![0; TMP_BUFFER_SIZE];
        tokio::select! {
            read_result = reader.read(&mut buf) => {
                if let Ok(num_bytes) = read_result {
                    if num_bytes == 0 {
                        break;
                    }

                    if total_read + num_bytes + to.len() >= max_read_size {
                        return Err(anyhow::anyhow!(SmtpError::PayloadTooLargeError));
                    }

                    total_read += num_bytes;

                    buf.truncate(num_bytes);
                    to.extend_from_slice(&buf);

                    // Set timeout to 10ms while draining
                    current_timeout = 10;
                } else if let Err(read_err) = read_result {
                    return Err(read_err.into());
                }
            }
            _ = ct.cancelled() => {
                return Err(anyhow::anyhow!(SmtpError::ServerShutdown));
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(current_timeout)) => {
                break;
            }
        };
    }

    Ok(total_read)
}

async fn read_command<'a>(
    ct: CancellationToken,
    reader: &mut ReadHalf<'_>,
    timeout_ms: u64,
    to: &'a mut Vec<u8>,
) -> anyhow::Result<SplitWhitespace<'a>> {
    let mut n: usize = 0;

    loop {
        let curr_read_bytes =
            read_more(ct.clone(), reader, timeout_ms, MAX_SOCKET_READ_BYTES, to).await?;

        if curr_read_bytes == 0 {
            break;
        }

        n += curr_read_bytes;

        if to.ends_with(b"\r\n") {
            break;
        }
    }

    if n == 0 {
        return Err(anyhow::anyhow!(SmtpError::NoContentError));
    }

    let content_iter = match std::str::from_utf8(to) {
        Ok(v) => v[0..n].split_whitespace(),
        Err(e) => panic!("Invalid UTF-8 sequence: {}", e),
    };

    Ok(content_iter)
}

#[derive(Copy, Clone)]
enum SmtpState {
    SmtpStateAwaitGreet,
    SmtpStateAwaitFrom,
    SmtpStateAwaitRcpt,
    SmtpStateAwaitDataOrRcpt,
    SmtpStateAwaitData,
}

impl std::fmt::Display for SmtpState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SmtpStateAwaitGreet => write!(f, "SmtpStateAwaitGreet"),
            Self::SmtpStateAwaitFrom => write!(f, "SmtpStateAwaitFrom"),
            Self::SmtpStateAwaitRcpt => write!(f, "SmtpStateAwaitRcpt"),
            Self::SmtpStateAwaitDataOrRcpt => write!(f, "SmtpStateAwaitDataOrRcpt"),
            Self::SmtpStateAwaitData => write!(f, "SmtpStateAwaitData"),
        }
    }
}

fn command_parsing_error_handler(
    err: anyhow::Error,
    state: SmtpState,
    client_ip: &str,
    command: &str,
) -> anyhow::Error {
    eprintln!("[{client_ip}] Failed to decode {command} command in state {state}: {err}");
    err
}

async fn send_email_channel<'a>(
    ct: CancellationToken,
    tx: &'a Sender<Email>,
    email: Email,
) -> anyhow::Result<(), SmtpError> {
    tokio::select! {
        send_result = tx.send_timeout(email, std::time::Duration::from_millis(100)) => {
            if let Err(err) = send_result {
               match err {
                    SendTimeoutError::Timeout(_) => {
                        eprintln!("ERROR: Email processing channel is too busy (capacity={}, max={}). Rejecting", tx.capacity(), tx.max_capacity());
                        return Err(SmtpError::RateLimitError);
                    }
                    SendTimeoutError::Closed(_) => {
                        eprintln!("ERROR: Email processing channel is closed. Rejecting");
                        return Err(SmtpError::ServerShutdown);
                    }
                }
            } else {
                return Ok(());
            }
        },
        _ = ct.cancelled() => {
            return Err(SmtpError::ServerShutdown);
        }
    }
}

async fn handle_email<'a>(
    ct: CancellationToken,
    client_ip: &str,
    writer: &mut WriteHalf<'a>,
    tx: &'a Sender<Email>,
    email: Email,
    ok_resp: &[u8],
) -> anyhow::Result<()> {
    if !email.is_valid() {
        writer.write(ok_resp).await.map_err(|err| {
            eprintln!("[{client_ip}] Failed to write to stream: {err}");
            err
        })?;
        return Ok(());
    }

    if let Err(err) = send_email_channel(ct, &tx, email).await {
        match err {
            SmtpError::RateLimitError => {
                writer.write(RESP_BUSY_ERROR).await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to write to stream: {err}");
                    err
                })?;
                return Ok(());
            }
            _ => {
                eprintln!("[{client_ip}] Unrecoverable error occurred when writing to email channel: {err}");
                writer
                    .write(RESP_SERVICE_UNAVAILABLE)
                    .await
                    .map_err(|err| {
                        eprintln!("[{client_ip}] Failed to write to stream: {err}");
                        err
                    })?;
                return Err(anyhow::anyhow!(err));
            }
        }
    }

    writer.write(ok_resp).await.map_err(|err| {
        eprintln!("[{client_ip}] Failed to write to stream: {err}");
        err
    })?;
    Ok(())
}

async fn smtp_loop(
    ct: CancellationToken,
    tx: Sender<Email>,
    tcp_stream: &mut TcpStream,
    client_ip: &str,
    timeout_ms: u64,
    srv_domain: &str,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = tcp_stream.split();

    let mut buf = Vec::with_capacity(64);
    let mut state = SmtpState::SmtpStateAwaitGreet;
    let mut email = Email::new();

    loop {
        let mut iter = read_command(ct.clone(), &mut reader, timeout_ms, &mut buf)
            .await
            .map_err(|err| {
                eprintln!("[{client_ip}] Failed to read command in state {state}: {err}");
                err
            })?;

        let command_result = iter
            .next()
            .context("received empty command")
            .map_err(|err| command_parsing_error_handler(err, state, &client_ip, "<empty>"));

        let command = match command_result {
            Ok(command) => command,
            Err(e) => {
                writer.write(RESP_SYNTAX_ERROR).await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                    err
                })?;
                return Err(e);
            }
        };

        match (state, command) {
            (SmtpState::SmtpStateAwaitGreet, "HELO" | "EHLO") => {
                let fqdn_result = iter
                    .next()
                    .context("expected fqdn or address")
                    .map_err(|err| command_parsing_error_handler(err, state, &client_ip, command));

                if let Ok(fqdn) = fqdn_result {
                    println!("[{client_ip}] Identified as {fqdn} with {command}");

                    if command == "EHLO" {
                        let resp =
                            format!("250-{srv_domain} Hello {fqdn}\r\n250 AUTH PLAIN LOGIN\r\n");

                        writer.write(resp.as_bytes()).await.map_err(|err| {
                            eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                            err
                        })?;
                    } else {
                        writer.write(RESP_OK).await.map_err(|err| {
                            eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                            err
                        })?;
                    }
                    state = SmtpState::SmtpStateAwaitFrom;
                } else {
                    writer.write(RESP_SYNTAX_ERROR).await.map_err(|err| {
                        eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                        err
                    })?;
                }
            }
            (_, "AUTH") => {
                writer.write(RESP_AUTH_OK).await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                    err
                })?;
            }
            (_, "NOOP" | "HELP" | "INFO" | "VRFY" | "EXPN") => {
                println!("Received command {command}");
                writer.write(RESP_OK).await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                    err
                })?;
            }
            (SmtpState::SmtpStateAwaitFrom, "MAIL") => {
                let from_result = iter
                    .next()
                    .map_or("", |x| x)
                    .strip_prefix("FROM:")
                    .and_then(|val| if val.len() > 0 { Some(val) } else { None })
                    .ok_or(anyhow::anyhow!(SmtpError::CommandParsingError(
                        "MAIL FROM".to_string()
                    )))
                    .map_err(|err| command_parsing_error_handler(err, state, &client_ip, command));

                if let Ok(from) = from_result {
                    let from_addr_result = EmailAddress::from_str(from.trim_matches(['<', '>']))
                        .map_err(|err| {
                            eprintln!("[{client_ip}]: Failed to parse MAIL FROM:<email>: {err}");
                            err
                        });
                    if let Ok(from_addr) = from_addr_result {
                        email.mail_from = from_addr.to_string();
                    } else {
                        writer.write(RESP_SYNTAX_ERROR).await.map_err(|err| {
                            eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                            err
                        })?;
                        continue;
                    }

                    writer.write(RESP_OK).await.map_err(|err| {
                        eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                        err
                    })?;
                    state = SmtpState::SmtpStateAwaitRcpt;
                } else {
                    writer.write(RESP_SYNTAX_ERROR).await.map_err(|err| {
                        eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                        err
                    })?;
                }
            }
            (SmtpState::SmtpStateAwaitRcpt | SmtpState::SmtpStateAwaitDataOrRcpt, "RCPT") => {
                let to_result = iter
                    .next()
                    .map_or("", |x| x)
                    .strip_prefix("TO:")
                    .and_then(|val| if val.len() > 0 { Some(val) } else { None })
                    .ok_or(anyhow::anyhow!(SmtpError::CommandParsingError(
                        "RCPT TO".to_string()
                    )))
                    .map_err(|err| command_parsing_error_handler(err, state, &client_ip, command));

                if let Ok(to) = to_result {
                    if email.mail_to.len() == 0 {
                        let to_addr_result = EmailAddress::from_str(to.trim_matches(['<', '>']))
                            .map_err(|err| {
                                eprintln!("[{client_ip}]: Failed to parse RCPT TO:<email>: {err}");
                                err
                            });
                        if let Ok(to_addr) = to_addr_result {
                            email.mail_to = to_addr.to_string();
                        } else {
                            writer.write(RESP_SYNTAX_ERROR).await.map_err(|err| {
                                eprintln!(
                                    "[{client_ip}] Failed to write to stream in {state}: {err}"
                                );
                                err
                            })?;
                            continue;
                        }
                    } else {
                        // TODO: Support multiple recipients
                        println!("[{client_ip}] Skipping recipient {to} since multiple recipients are not supported")
                    }

                    writer.write(RESP_OK).await.map_err(|err| {
                        eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                        err
                    })?;
                    state = SmtpState::SmtpStateAwaitDataOrRcpt;
                } else {
                    writer.write(RESP_SYNTAX_ERROR).await.map_err(|err| {
                        eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                        err
                    })?;
                }
            }
            (SmtpState::SmtpStateAwaitDataOrRcpt, "DATA") => {
                writer.write(RESP_END_DATA_WITH).await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                    err
                })?;

                state = SmtpState::SmtpStateAwaitData;

                let mut data_buf = vec![0; 0];
                let mut ok = false;
                loop {
                    let n = read_more(
                        ct.clone(),
                        &mut reader,
                        5 * 1000,
                        MAX_SOCKET_READ_BYTES,
                        &mut data_buf,
                    )
                    .await
                    .map_err(|err| {
                        eprintln!(
                            "[{client_ip}] Failed to read DATA from client in state {state}: {err}"
                        );
                        err
                    })?;

                    if n == 0 {
                        eprintln!("[{client_ip}] Failed to read DATA from client in state {state} (read 0 bytes)");
                        break;
                    }

                    if data_buf.ends_with(EMAIL_TERM) {
                        data_buf.truncate(data_buf.len() - EMAIL_TERM.len());
                        ok = true;
                        break;
                    }
                }

                if ok {
                    match std::str::from_utf8(&data_buf) {
                        Ok(content) => {
                            email.mail_content = content.to_string();
                            handle_email(
                                ct.clone(),
                                client_ip,
                                &mut writer,
                                &tx,
                                email.clone(),
                                RESP_OK,
                            )
                            .await?;
                        }
                        Err(err) => eprintln!(
                            "[{client_ip}] Received invalid UTF-8 DATA from client: {err}"
                        ),
                    };
                } else {
                    println!("[{client_ip}] Received invalid DATA from client");
                    writer.write(RESP_SYNTAX_ERROR).await.map_err(|err| {
                        eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                        err
                    })?;
                }
                email = Email::new();
                state = SmtpState::SmtpStateAwaitGreet;
            }
            (_, "RSET") => {
                email = Email::new();
                state = SmtpState::SmtpStateAwaitGreet;
                writer.write(RESP_OK).await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                    err
                })?;
            }
            (_, "QUIT") => {
                handle_email(
                    ct.clone(),
                    client_ip,
                    &mut writer,
                    &tx,
                    email.clone(),
                    RESP_BYE,
                )
                .await?;
                break;
            }
            _ => {
                eprintln!(
                    "[{client_ip}] Received unknown/invalid command \"{command}\" in state {state}"
                );
                writer.write(RESP_UNKNOWN_COMMAND).await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to send message to client {err}");
                    return err;
                })?;
                // anyhow::bail!("Unexpected message received: {command}",),
            }
        }

        buf.clear();
    }

    if email.is_valid() {
        println!("Valid email received: {:?}", email)
    }

    Ok(())
}

async fn handle_connection(
    ct: CancellationToken,
    tx: Sender<Email>,
    mut tcp_stream: TcpStream,
    timeout_ms: u64,
    srv_domain: String,
) {
    let client_addr_result = tcp_stream.peer_addr().map_err(|err| {
        eprintln!("Unable to get connected client peer address: {err}");
    });

    let client_ip: String;
    if let Ok(client_addr) = client_addr_result {
        client_ip = client_addr.to_string()
    } else {
        return;
    }

    println!("[{client_ip}] Greeting new client");

    let greet_write_result = tcp_stream
        .write(RESP_MAILSERVER_GREET)
        .await
        .map_err(|err| {
            eprintln!("[{client_ip}] Failed to greet client {err}");
            return err;
        });

    if let Err(_) = greet_write_result {
        return;
    }

    let result = smtp_loop(
        ct,
        tx,
        &mut tcp_stream,
        client_ip.as_str(),
        timeout_ms,
        srv_domain.as_str(),
    )
    .await;

    let _ = tcp_stream.shutdown().await;

    if let Ok(_) = result {
        println!("[{client_ip}] Exchange completed successfully");
    } else {
        println!("[{client_ip}] Connection closed due to error");
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;

    // type ServerThreadHandle = tokio::task::JoinHandle<Result<(), anyhow::Error>>;

    async fn setup(ctoken: CancellationToken, timeout_ms: Option<u64>) -> anyhow::Result<String> {
        let (tx, mut rx) = mpsc::channel::<Email>(10);

        let ctoken_copy = ctoken.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    recv_result = rx.recv() => {
                        let mut ok = false;
                        if let Some(email) = recv_result {
                            ok = email.is_valid();
                        }
                        if !ok {
                            panic!("ERROR: Error receiving from email channel");
                        }
                    }
                    _ = ctoken_copy.cancelled() => {
                        break;
                    }
                }
            }
        });
        setup_with_receiver(ctoken, tx, timeout_ms).await
    }

    async fn setup_with_receiver(
        ctoken: CancellationToken,
        tx: Sender<Email>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<String> {
        let addr = "localhost:0";
        let listener: TcpListener = TcpListener::bind(addr).await?;
        let listen_addr = listener
            .local_addr()
            .expect("local address unavailable")
            .to_string();

        tokio::spawn(start_server(
            ctoken.clone(),
            tx,
            "testserver".to_string(),
            listener,
            timeout_ms.unwrap_or(15 * 1000),
        ));
        Ok(listen_addr)
    }

    async fn teardown(ctoken: CancellationToken) -> anyhow::Result<()> {
        ctoken.cancel();
        Ok(())
    }

    async fn smtp_expect_greet(stream: &mut TcpStream, recv_buf: &mut Vec<u8>) {
        let n = stream
            .read(recv_buf)
            .await
            .expect("Failed to read server greeting from tcpstream");

        let expect_resp = String::from_utf8(RESP_MAILSERVER_GREET.to_vec())
            .expect("Unable to decode greet buffer");
        let src_resp =
            std::str::from_utf8(&recv_buf[0..n]).expect("Got unexpected non-utf8 data from server");
        assert_eq!(src_resp, expect_resp, "Expected server to greet on connect",);
    }

    async fn smtp_send_and_recv(
        stream: &mut TcpStream,
        recv_buf: &mut Vec<u8>,
        command: &[u8],
        expect: &[u8],
    ) {
        stream
            .write(command)
            .await
            .expect("Failed to write to tcpstream");

        let n = stream
            .read(recv_buf)
            .await
            .expect("Failed to read from tcpstream");

        let exp_resp = std::str::from_utf8(expect).expect("Unexpected non-utf8 expect content");
        let src_resp =
            std::str::from_utf8(&recv_buf[0..n]).expect("Got unexpected non-utf8 data from server");
        assert_eq!(
            src_resp, exp_resp,
            "Expected server to reply with specific response"
        );
    }

    async fn smtp_expect_conn_close(stream: &mut TcpStream, recv_buf: &mut Vec<u8>) {
        tokio::select! {
            read_res = stream
            .read(recv_buf) => {
                if let Ok(read_bytes) = read_res {
                    if read_bytes > 0 {
                        panic!("Socket returned > 0 bytes unexpectedly");
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(300)) => {
                panic!("Connection was not timed out in expected time");
            }
        }
    }

    #[tokio::test]
    async fn it_connects_to_smtp_server_and_completes_mail_exchange() {
        let ctoken = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel::<Email>(10);

        let ctoken_copy = ctoken.clone();
        let listener_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    recv_result = rx.recv() => {
                        if let Some(email) = recv_result {
                            return Some(email);
                        }
                    }
                    _ = ctoken_copy.cancelled() => {
                        break;
                    }
                }
            }
            None
        });

        let srv_addr = setup_with_receiver(ctoken.clone(), tx, None)
            .await
            .expect("Test setup did not complete successfully");

        let mut stream = TcpStream::connect(srv_addr)
            .await
            .expect("Test setup did not complete successfully");

        let mut buf = vec![0; 1024];

        smtp_expect_greet(&mut stream, &mut buf).await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"HELO rs-mailserver-tester\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"MAIL FROM:<bob@example.org>\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"RCPT TO:<alice@example.com>\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"RCPT TO:<theboss@example.com>\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(&mut stream, &mut buf, b"DATA\r\n", RESP_END_DATA_WITH).await;

        let mail_data = b"From: \"Bob Example\" <bob@example.org>
To: \"Alice Example\" <alice@example.com>
Cc: theboss@example.com
Date: Tue, 15 Jan 2008 16:02:43 -0500
Subject: Test message

Hello Alice.
This is a test message with 5 header fields and 4 lines in the message body.
Your friend,
Bob\r\n.\r\n";

        smtp_send_and_recv(&mut stream, &mut buf, mail_data, RESP_OK).await;

        smtp_send_and_recv(&mut stream, &mut buf, b"QUIT\r\n", RESP_BYE).await;

        tokio::select! {
            listen_result = listener_handle => {
                let received_email = listen_result.expect("Error occurred in receiver task").expect("Did not receive any emails in receiver task");

                assert_eq!(
                    received_email.is_valid(), true,
                    "Expected receiver to receive a valid email"
                );
                assert_eq!(
                    received_email.mail_from, "bob@example.org",
                    "Expected receiver to have correct mail_from"
                );
                assert_eq!(
                    received_email.mail_to, "alice@example.com",
                    "Expected receiver to have correct mail_to"
                );
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                panic!("Listener handle did not complete in time");
            }
        }

        teardown(ctoken)
            .await
            .expect("Test teardown did not complete successfully");
    }

    #[tokio::test]
    async fn it_connects_to_smtp_server_and_uses_esmtp() {
        let ctoken = CancellationToken::new();
        let srv_addr = setup(ctoken.clone(), None)
            .await
            .expect("Test setup did not complete successfully");

        let mut stream = TcpStream::connect(srv_addr)
            .await
            .expect("Test setup did not complete successfully");

        let mut buf = vec![0; 1024];

        smtp_expect_greet(&mut stream, &mut buf).await;

        let ehlo_resp =
            format!("250-testserver Hello rs-mailserver-tester\r\n250 AUTH PLAIN LOGIN\r\n");

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"EHLO rs-mailserver-tester\r\n",
            ehlo_resp.as_bytes(),
        )
        .await;

        smtp_send_and_recv(&mut stream, &mut buf, b"AUTH\r\n", RESP_AUTH_OK).await;

        smtp_send_and_recv(&mut stream, &mut buf, b"RSET\r\n", RESP_OK).await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"MAIL FROM:<bob@example.org>\r\n",
            RESP_UNKNOWN_COMMAND,
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"EHLO rs-mailserver-tester\r\n",
            ehlo_resp.as_bytes(),
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"MAIL FROM:<bob@example.org>\r\n",
            RESP_OK,
        )
        .await;

        let commands = ["NOOP\r\n", "HELP\r\n", "INFO\r\n", "VRFY\r\n", "EXPN\r\n"];

        for cmd in commands {
            smtp_send_and_recv(&mut stream, &mut buf, cmd.as_bytes(), RESP_OK).await;
        }

        teardown(ctoken)
            .await
            .expect("Test teardown did not complete successfully");
    }

    #[tokio::test]
    async fn it_responds_correctly_to_unknown_command() {
        let ctoken = CancellationToken::new();
        let srv_addr = setup(ctoken.clone(), None)
            .await
            .expect("Test setup did not complete successfully");

        let mut stream = TcpStream::connect(srv_addr)
            .await
            .expect("Test setup did not complete successfully");

        let mut buf = vec![0; 1024];

        smtp_expect_greet(&mut stream, &mut buf).await;

        smtp_send_and_recv(&mut stream, &mut buf, b"FOO 123\r\n", RESP_UNKNOWN_COMMAND).await;

        teardown(ctoken)
            .await
            .expect("Test teardown did not complete successfully");
    }

    #[tokio::test]
    async fn it_responds_correctly_to_empty_command() {
        let ctoken = CancellationToken::new();
        let srv_addr = setup(ctoken.clone(), None)
            .await
            .expect("Test setup did not complete successfully");

        let mut stream = TcpStream::connect(srv_addr)
            .await
            .expect("Test setup did not complete successfully");

        let mut buf = vec![0; 1024];

        smtp_expect_greet(&mut stream, &mut buf).await;

        stream
            .write(b"\r\n")
            .await
            .expect("Failed to write to tcpstream");

        smtp_send_and_recv(&mut stream, &mut buf, b"\r\n", RESP_SYNTAX_ERROR).await;

        teardown(ctoken)
            .await
            .expect("Test teardown did not complete successfully");
    }

    #[tokio::test]
    async fn it_responds_correctly_to_missing_command_args() {
        let ctoken = CancellationToken::new();
        let srv_addr = setup(ctoken.clone(), None)
            .await
            .expect("Test setup did not complete successfully");

        let mut stream = TcpStream::connect(srv_addr)
            .await
            .expect("Test setup did not complete successfully");

        let mut buf = vec![0; 1024];

        smtp_expect_greet(&mut stream, &mut buf).await;

        smtp_send_and_recv(&mut stream, &mut buf, b"HELO\r\n", RESP_SYNTAX_ERROR).await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"HELO rs-mailserver-tester\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(&mut stream, &mut buf, b"MAIL\r\n", RESP_SYNTAX_ERROR).await;

        smtp_send_and_recv(&mut stream, &mut buf, b"MAIL FROM:\r\n", RESP_SYNTAX_ERROR).await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"MAIL FROM:<bob@example.org>\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(&mut stream, &mut buf, b"RCPT\r\n", RESP_SYNTAX_ERROR).await;

        smtp_send_and_recv(&mut stream, &mut buf, b"RCPT TO:\r\n", RESP_SYNTAX_ERROR).await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"RCPT TO:<alice@example.com>\r\n",
            RESP_OK,
        )
        .await;

        teardown(ctoken)
            .await
            .expect("Test teardown did not complete successfully");
    }

    #[tokio::test]
    async fn it_responds_correctly_to_out_of_order_commands() {
        let ctoken = CancellationToken::new();
        let srv_addr = setup(ctoken.clone(), None)
            .await
            .expect("Test setup did not complete successfully");

        let mut stream = TcpStream::connect(srv_addr)
            .await
            .expect("Test setup did not complete successfully");

        let mut buf = vec![0; 1024];

        smtp_expect_greet(&mut stream, &mut buf).await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"MAIL FROM:<bob@example.org>\r\n",
            RESP_UNKNOWN_COMMAND,
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"HELO rs-mailserver-tester\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"MAIL FROM:<bob@example.org>\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(&mut stream, &mut buf, b"DATA\r\n", RESP_UNKNOWN_COMMAND).await;

        teardown(ctoken)
            .await
            .expect("Test teardown did not complete successfully");
    }

    #[tokio::test]
    async fn it_timeouts_idle_connection() {
        let ctoken = CancellationToken::new();
        let srv_addr = setup(ctoken.clone(), Some(100))
            .await
            .expect("Test setup did not complete successfully");

        let mut stream = TcpStream::connect(srv_addr.as_str())
            .await
            .expect("Test setup did not complete successfully");

        let mut buf = vec![0; 1024];

        smtp_expect_greet(&mut stream, &mut buf).await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"HELO rs-mailserver-tester\r\n",
            RESP_OK,
        )
        .await;

        smtp_expect_conn_close(&mut stream, &mut buf).await;

        stream = TcpStream::connect(srv_addr.as_str())
            .await
            .expect("Test setup did not complete successfully");

        smtp_expect_greet(&mut stream, &mut buf).await;

        smtp_expect_conn_close(&mut stream, &mut buf).await;

        teardown(ctoken)
            .await
            .expect("Test teardown did not complete successfully");
    }

    #[tokio::test]
    async fn it_aborts_connection_on_too_large_payload() {
        let ctoken = CancellationToken::new();
        let srv_addr = setup(ctoken.clone(), None)
            .await
            .expect("Test setup did not complete successfully");

        let mut stream = TcpStream::connect(srv_addr.as_str())
            .await
            .expect("Test setup did not complete successfully");

        let mut buf = vec![0; 1024];
        let mut garbage_buffer = vec![b'A'; MAX_SOCKET_READ_BYTES + 1];

        let helo = b"HELO ";

        for (index, byte) in helo.iter().enumerate() {
            garbage_buffer[index] = *byte;
        }

        let gb_len = garbage_buffer.len();
        garbage_buffer[gb_len - 2] = b'\r';
        garbage_buffer[gb_len - 1] = b'\n';

        smtp_expect_greet(&mut stream, &mut buf).await;

        stream
            .write(garbage_buffer.as_slice())
            .await
            .expect("Failed to write to tcpstream");

        smtp_expect_conn_close(&mut stream, &mut buf).await;

        stream = TcpStream::connect(srv_addr.as_str())
            .await
            .expect("Test setup did not complete successfully");

        smtp_expect_greet(&mut stream, &mut buf).await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"HELO rs-mailserver-tester\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"MAIL FROM:<bob@example.org>\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"RCPT TO:<alice@example.com>\r\n",
            RESP_OK,
        )
        .await;

        smtp_send_and_recv(&mut stream, &mut buf, b"DATA\r\n", RESP_END_DATA_WITH).await;

        stream
            .write(garbage_buffer.as_slice())
            .await
            .expect("Failed to write to tcpstream");

        smtp_expect_conn_close(&mut stream, &mut buf).await;

        teardown(ctoken)
            .await
            .expect("Test teardown did not complete successfully");
    }

    #[tokio::test]
    async fn it_closes_server_with_cancellation_token() {
        let ctoken = CancellationToken::new();
        let srv_addr = setup(ctoken.clone(), None)
            .await
            .expect("Test setup did not complete successfully");

        let mut stream = TcpStream::connect(srv_addr.as_str())
            .await
            .expect("Test setup did not complete successfully");

        let mut buf = vec![0; 1024];
        smtp_expect_greet(&mut stream, &mut buf).await;

        teardown(ctoken)
            .await
            .expect("Test teardown did not complete successfully");

        smtp_expect_conn_close(&mut stream, &mut buf).await;
    }
}
