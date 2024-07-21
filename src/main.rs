use anyhow::Context;
use std::env;
use std::error::Error;
use std::str::SplitWhitespace;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{tcp::ReadHalf, TcpListener, TcpStream},
};

const EMAIL_TERM: &[u8; 5] = b"\r\n.\r\n";
const MAX_SOCKET_READ_BYTES: usize = 1_000_000;

#[tokio::main]
async fn main() -> anyhow::Result<(), Box<dyn Error>> {
    let send_or_recv = env::args().nth(1).unwrap_or_else(|| "recv".to_string());

    let addr = env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1:25".to_string());

    println!("Started in {send_or_recv}");

    if send_or_recv == "recv" {
        let listener = TcpListener::bind(addr).await?;

        loop {
            let (tcp_stream, remote_addr) = listener.accept().await?;

            let remote_ip = remote_addr.ip().to_string();
            println!("Accepted connection from {remote_ip}");

            tokio::spawn(handle_connection(tcp_stream));
        }
    } else if send_or_recv == "send" {
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

        Ok(())
    } else {
        Err(format!("Invalid mode {send_or_recv}").into())
    }
}

#[derive(Debug)]
enum SocketReadError {
    PayloadTooLargeError,
}

impl std::fmt::Display for SocketReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PayloadTooLargeError => write!(f, "socket payload too large"),
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
                    println!("Error reading from socket: {read_err}");
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
        n += read_more(reader, timeout_ms, MAX_SOCKET_READ_BYTES, to).await?;

        if to.ends_with(b"\r\n") {
            break;
        }
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
    SmtpStateAwaitData,
}

impl std::fmt::Display for SmtpState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SmtpStateAwaitGreet => write!(f, "SmtpStateAwaitGreet"),
            Self::SmtpStateAwaitFrom => write!(f, "SmtpStateAwaitFrom"),
            Self::SmtpStateAwaitRcpt => write!(f, "SmtpStateAwaitRcpt"),
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

async fn handle_connection(mut tcp_stream: TcpStream) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(64);

    let client_addr = tcp_stream.peer_addr().map_err(|err| {
        eprintln!("Unable to get connected client peer address: {err}");
        return err;
    })?;

    let client_ip = client_addr.to_string();
    let (mut reader, mut writer) = tcp_stream.split();

    println!("[{client_ip}] Greeting new client from");

    writer.write(b"220 rs-mailserver\n").await.map_err(|err| {
        eprintln!("[{client_ip}] Failed to greet client {err}");
        return err;
    })?;

    let mut state = SmtpState::SmtpStateAwaitGreet;

    loop {
        let mut iter = read_command(&mut reader, 5 * 1000, &mut buf)
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
                writer.write(b"250 Ok").await?;
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
                writer.write(b"250 Ok").await?;
                state = SmtpState::SmtpStateAwaitRcpt;
            }
            (SmtpState::SmtpStateAwaitRcpt, "RCPT") => {
                let to = iter
                    .next()
                    .context("expected non-empty RCPT TO command")
                    .map_err(|err| command_error_handler(err, state, &client_ip, command))?
                    .strip_prefix("TO:")
                    .context("expected non-empty RCPT TO command")
                    .map_err(|err| command_error_handler(err, state, &client_ip, command))?;

                println!("received RCPT TO {to}");
                writer.write(b"250 Ok").await?;
            }
            (SmtpState::SmtpStateAwaitRcpt, "DATA") => {
                println!("received DATA command");

                writer
                    .write(b"354 End data with <CR><LF>.<CR><LF>\n")
                    .await
                    .map_err(|err| {
                        eprintln!(
                            "[{client_ip}] Failed to send data to client in state {state}: {err}"
                        );
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
                        writer.write(b"250 Ok").await?;
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
                writer.write(b"221 Bye").await?;
                let _ = writer.shutdown().await;
                break;
            }
            _ => {
                eprintln!(
                    "[{client_ip}] Received unknown/invalid command \"{command}\" in state {state}"
                );
                writer
                    .write(b"500 Command not recognized\n")
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

    println!("[{client_ip}] Connection closed");

    Ok(())
}
