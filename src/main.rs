use anyhow::Context;
use std::env;
use std::str::SplitWhitespace;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{tcp::ReadHalf, TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

const EMAIL_TERM: &[u8; 5] = b"\r\n.\r\n";
const MAX_SOCKET_READ_BYTES: usize = 512_000; // 512kb
const MAX_SOCKET_TIMEOUT_MS: u64 = 15 * 1000;
const MAILSERVER_GREET: &[u8; 19] = b"220 rs-mailserver\r\n";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let send_or_recv = env::args().nth(1).unwrap_or_else(|| "recv".to_string());

    let addr = env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1:25".to_string());

    if send_or_recv == "recv" {
        println!("Started in {send_or_recv} {addr}");
        let ctoken = CancellationToken::new();
        let cloned_token = ctoken.clone();

        let listener: TcpListener = TcpListener::bind(addr).await?;
        start_server(cloned_token, listener, MAX_SOCKET_TIMEOUT_MS).await?;
    } else if send_or_recv == "send" {
        println!("Started in {send_or_recv}");
        let mut stream = TcpStream::connect(addr).await?;
        println!("Stream connected");

        let mut buf = vec![0; 1024];

        stream.read(&mut buf).await?;

        // https://en.wikipedia.org/wiki/Simple_Mail_Transfer_Protocol

        stream.write(b"HELO todo\r\n").await?;
        stream.read(&mut buf).await?;

        stream.write(b"MAIL FROM:<bob@example.org>\r\n").await?;
        stream.read(&mut buf).await?;

        stream.write(b"RCPT TO:<alice@example.com>\r\n").await?;
        stream.read(&mut buf).await?;

        stream.write(b"RCPT TO:<theboss@example.com>\r\n").await?;
        stream.read(&mut buf).await?;

        stream.write(b"DATA\r\n").await?;
        stream.read(&mut buf).await?;

        stream
            .write(
                b"From: \"Bob Example\" <bob@example.org>
To: \"Alice Example\" <alice@example.com>
Cc: theboss@example.com
Date: Tue, 15 Jan 2008 16:02:43 -0500
Subject: Test message

Hello Alice.
This is a test message with 5 header fields and 4 lines in the message body.
Your friend,
Bob\r\n.\r\n",
            )
            .await?;
        stream.read(&mut buf).await?;

        stream.write(b"QUIT\r\n").await?;
        stream.read(&mut buf).await?;
    } else {
        anyhow::bail!("Invalid mode {send_or_recv}")
    }

    Ok(())
}

async fn start_server(
    cancel_token: CancellationToken,
    listener: TcpListener,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                break
            }
            Ok((tcp_stream, remote_addr)) = listener.accept() => {
                let remote_ip = remote_addr.ip().to_string();
                println!("Accepted connection from {remote_ip}");

                tokio::spawn(handle_connection(tcp_stream, timeout_ms));
            }
        }
    }

    println!("Exiting server...");
    Ok(())
}

#[derive(Debug)]
enum SocketReadError {
    PayloadTooLargeError,
    NoContentError,
}

impl std::fmt::Display for SocketReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PayloadTooLargeError => write!(f, "socket payload too large"),
            Self::NoContentError => write!(f, "no content received before timeout"),
        }
    }
}

pub async fn read_more(
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
                        return Err(anyhow::anyhow!(SocketReadError::PayloadTooLargeError));
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
            _ = tokio::time::sleep(std::time::Duration::from_millis(current_timeout)) => {
                break;
            }
        };
    }

    Ok(total_read)
}

pub async fn read_command<'a>(
    reader: &mut ReadHalf<'_>,
    timeout_ms: u64,
    to: &'a mut Vec<u8>,
) -> anyhow::Result<SplitWhitespace<'a>> {
    let mut n: usize = 0;

    loop {
        let curr_read_bytes = read_more(reader, timeout_ms, MAX_SOCKET_READ_BYTES, to).await?;

        if curr_read_bytes == 0 {
            break;
        }

        n += curr_read_bytes;

        if to.ends_with(b"\r\n") {
            break;
        }
    }

    if n == 0 {
        return Err(anyhow::anyhow!(SocketReadError::NoContentError));
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

fn command_error_handler(
    err: anyhow::Error,
    state: SmtpState,
    client_ip: &str,
    command: &str,
) -> anyhow::Error {
    eprintln!("[{client_ip}] Failed to decode {command} command in state {state}: {err}");
    err
}

async fn smtp_loop(
    tcp_stream: &mut TcpStream,
    client_ip: &str,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = tcp_stream.split();

    let mut buf = Vec::with_capacity(64);
    let mut state = SmtpState::SmtpStateAwaitGreet;

    loop {
        let mut iter = read_command(&mut reader, timeout_ms, &mut buf)
            .await
            .map_err(|err| {
                eprintln!("[{client_ip}] Failed to read command in state {state}: {err}");
                err
            })?;

        let command = iter
            .next()
            .context("received empty command")
            .map_err(|err| {
                eprintln!("[{client_ip}] Failed to decode next command in state {state}: {err}");
                return err;
            })?;

        match (state, command) {
            (SmtpState::SmtpStateAwaitGreet, "HELO") => {
                let fqdn = iter
                    .next()
                    .context("expected fqdn or address")
                    .map_err(|err| command_error_handler(err, state, &client_ip, command))?;

                println!("received HELO {fqdn}");

                writer.write(b"250 Ok\r\n").await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                    err
                })?;
                state = SmtpState::SmtpStateAwaitFrom;
            }
            (SmtpState::SmtpStateAwaitFrom, "MAIL") => {
                let from = iter
                    .next()
                    .context("expected non-empty MAIL FROM command")
                    .map_err(|err| command_error_handler(err, state, &client_ip, command))?
                    .strip_prefix("FROM:")
                    .context("expected non-empty MAIL FROM command")
                    .map_err(|err| command_error_handler(err, state, &client_ip, command))?;

                println!("received MAIL FROM {from}");
                writer.write(b"250 Ok\r\n").await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                    err
                })?;
                state = SmtpState::SmtpStateAwaitRcpt;
            }
            (SmtpState::SmtpStateAwaitRcpt | SmtpState::SmtpStateAwaitDataOrRcpt, "RCPT") => {
                let to = iter
                    .next()
                    .context("expected non-empty RCPT TO command")
                    .map_err(|err| command_error_handler(err, state, &client_ip, command))?
                    .strip_prefix("TO:")
                    .context("expected non-empty RCPT TO command")
                    .map_err(|err| command_error_handler(err, state, &client_ip, command))?;

                println!("received RCPT TO {to}");
                writer.write(b"250 Ok\r\n").await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                    err
                })?;
                state = SmtpState::SmtpStateAwaitDataOrRcpt;
            }
            (SmtpState::SmtpStateAwaitDataOrRcpt, "DATA") => {
                println!("received DATA command");

                writer
                    .write(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                    .await
                    .map_err(|err| {
                        eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                        err
                    })?;

                state = SmtpState::SmtpStateAwaitData;

                let mut data_buf = vec![0; 0];
                let mut ok = false;
                loop {
                    let n = read_more(&mut reader, 5 * 1000, MAX_SOCKET_READ_BYTES, &mut data_buf)
                        .await
                        .map_err(|err| {
                            eprintln!("[{client_ip}] Failed to read DATA from client in state {state}: {err}");
                            err
                        })?;

                    if n == 0 {
                        eprintln!("[{client_ip}] Failed to read DATA from client in state {state} (read 0 bytes)");
                        break;
                    }

                    if data_buf.ends_with(EMAIL_TERM) {
                        data_buf.truncate(data_buf.len() - EMAIL_TERM.len());
                        ok = true;
                        writer.write(b"250 Ok\r\n").await.map_err(|err| {
                            eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                            err
                        })?;
                        break;
                    }
                }

                if ok {
                    match std::str::from_utf8(&data_buf) {
                        Ok(v) => println!("DATA:\n{v}"),
                        Err(err) => eprintln!(
                            "[{client_ip}] Received invalid UTF-8 DATA from client: {err}"
                        ),
                    };
                } else {
                    println!("[{client_ip}] Received invalid DATA from client");
                }
            }
            (_, "QUIT") => {
                writer.write(b"221 Bye\r\n").await.map_err(|err| {
                    eprintln!("[{client_ip}] Failed to write to stream in {state}: {err}");
                    err
                })?;
                break;
            }
            _ => {
                eprintln!(
                    "[{client_ip}] Received unknown/invalid command \"{command}\" in state {state}"
                );
                writer
                    .write(b"500 Command not recognized\r\n")
                    .await
                    .map_err(|err| {
                        eprintln!("[{client_ip}] Failed to send message to client {err}");
                        return err;
                    })?;
                // anyhow::bail!("Unexpected message received: {command}",),
            }
        }

        buf.clear();
    }

    Ok(())
}

async fn handle_connection(mut tcp_stream: TcpStream, timeout_ms: u64) {
    let client_addr_result = tcp_stream.peer_addr().map_err(|err| {
        eprintln!("Unable to get connected client peer address: {err}");
    });

    let client_ip: String;
    if let Ok(client_addr) = client_addr_result {
        client_ip = client_addr.to_string().to_string();
    } else {
        return;
    }

    println!("[{client_ip}] Greeting new client");

    let greet_write_result = tcp_stream.write(MAILSERVER_GREET).await.map_err(|err| {
        eprintln!("[{client_ip}] Failed to greet client {err}");
        return err;
    });

    if let Err(_) = greet_write_result {
        return;
    }

    let result = smtp_loop(&mut tcp_stream, client_ip.as_str(), timeout_ms).await;

    let _ = tcp_stream.shutdown().await;

    if let Ok(_) = result {
        println!("[{client_ip}] Exchange completed successfully");
    } else {
        println!("[{client_ip}] Connection closed due to error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // type ServerThreadHandle = tokio::task::JoinHandle<Result<(), anyhow::Error>>;

    async fn setup(ctoken: CancellationToken, timeout_ms: Option<u64>) -> anyhow::Result<String> {
        let addr = "localhost:0";
        let listener: TcpListener = TcpListener::bind(addr).await?;
        let listen_addr = listener
            .local_addr()
            .expect("local address unavailable")
            .to_string();

        tokio::spawn(start_server(
            ctoken.clone(),
            listener,
            timeout_ms.unwrap_or(MAX_SOCKET_TIMEOUT_MS),
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

        let expect_resp =
            String::from_utf8(MAILSERVER_GREET.to_vec()).expect("Unable to decode greet buffer");
        let src_resp =
            std::str::from_utf8(&recv_buf[0..n]).expect("Got unexpected non-utf8 data from server");
        assert_eq!(src_resp, expect_resp, "Expected server to greet on connect",);
    }

    async fn smtp_send_and_recv(
        stream: &mut TcpStream,
        recv_buf: &mut Vec<u8>,
        command: &[u8],
        expect: &str,
    ) {
        stream
            .write(command)
            .await
            .expect("Failed to write to tcpstream");

        let n = stream
            .read(recv_buf)
            .await
            .expect("Failed to read from tcpstream");

        let src_resp =
            std::str::from_utf8(&recv_buf[0..n]).expect("Got unexpected non-utf8 data from server");
        assert_eq!(
            src_resp, expect,
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
            b"HELO rs-mailserver-tester\r\n",
            "250 Ok\r\n",
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"MAIL FROM:<bob@example.org>\r\n",
            "250 Ok\r\n",
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"RCPT TO:<alice@example.com>\r\n",
            "250 Ok\r\n",
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"RCPT TO:<theboss@example.com>\r\n",
            "250 Ok\r\n",
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"DATA\r\n",
            "354 End data with <CR><LF>.<CR><LF>\r\n",
        )
        .await;

        let mail_data = b"From: \"Bob Example\" <bob@example.org>
To: \"Alice Example\" <alice@example.com>
Cc: theboss@example.com
Date: Tue, 15 Jan 2008 16:02:43 -0500
Subject: Test message

Hello Alice.
This is a test message with 5 header fields and 4 lines in the message body.
Your friend,
Bob\r\n.\r\n";

        smtp_send_and_recv(&mut stream, &mut buf, mail_data, "250 Ok\r\n").await;

        smtp_send_and_recv(&mut stream, &mut buf, b"QUIT\r\n", "221 Bye\r\n").await;

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

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"FOO 123\r\n",
            "500 Command not recognized\r\n",
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
            "500 Command not recognized\r\n",
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"HELO rs-mailserver-tester\r\n",
            "250 Ok\r\n",
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"MAIL FROM:<bob@example.org>\r\n",
            "250 Ok\r\n",
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"DATA\r\n",
            "500 Command not recognized\r\n",
        )
        .await;

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
            "250 Ok\r\n",
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
            "250 Ok\r\n",
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"MAIL FROM:<bob@example.org>\r\n",
            "250 Ok\r\n",
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"RCPT TO:<alice@example.com>\r\n",
            "250 Ok\r\n",
        )
        .await;

        smtp_send_and_recv(
            &mut stream,
            &mut buf,
            b"DATA\r\n",
            "354 End data with <CR><LF>.<CR><LF>\r\n",
        )
        .await;

        stream
            .write(garbage_buffer.as_slice())
            .await
            .expect("Failed to write to tcpstream");

        smtp_expect_conn_close(&mut stream, &mut buf).await;

        teardown(ctoken)
            .await
            .expect("Test teardown did not complete successfully");
    }
}
