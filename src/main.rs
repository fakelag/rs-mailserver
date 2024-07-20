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
    const TMP_BUFFER_SIZE: usize = 64;

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

                    if total_read + num_bytes > max_read_size {
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
    buf: &'a mut Vec<u8>,
) -> anyhow::Result<SplitWhitespace<'a>> {
    // let mut buf = Vec::with_capacity(64);
    let n = read_more(reader, timeout_ms, MAX_SOCKET_READ_BYTES, buf).await?;

    let content_iter = match std::str::from_utf8(buf) {
        Ok(v) => v[0..n].split_whitespace(),
        Err(e) => panic!("Invalid UTF-8 sequence: {}", e),
    };

    Ok(content_iter)
}

async fn handle_connection(mut tcp_stream: TcpStream) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(64);

    let (mut reader, mut writer) = tcp_stream.split();

    loop {
        writer.write(b"220 rs-mailserver\n").await?;

        let mut iter = read_command(&mut reader, 5 * 1000, &mut buf).await?;

        let command = iter.next().context("received empty command")?;

        match command {
            "HELO" => {
                let fqdn = iter.next().context("expected fqdn or address")?;
                println!("received HELO {fqdn}");
                writer.write(b"250 Ok").await?;
            }
            "MAIL" => {
                let from = iter
                    .next()
                    .context("expected non-empty MAIL FROM command")?
                    .strip_prefix("FROM:")
                    .context("expected non-empty MAIL FROM command")?;

                println!("received MAIL FROM {from}");
                writer.write(b"250 Ok").await?;
            }
            "RCPT" => {
                let to = iter
                    .next()
                    .context("expected non-empty RCPT TO command")?
                    .strip_prefix("TO:")
                    .context("expected non-empty RCPT TO command")?;

                println!("received RCPT TO {to}");
                writer.write(b"250 Ok").await?;
            }
            "DATA" => {
                println!("received DATA command");

                writer
                    .write(b"354 End data with <CR><LF>.<CR><LF>\n")
                    .await?;

                let mut data_buf = vec![0; 0];
                let mut ok = false;
                loop {
                    let n = read_more(&mut reader, 5 * 1000, MAX_SOCKET_READ_BYTES, &mut data_buf)
                        .await?;

                    if n == 0 {
                        // stream ended abruptly
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
                        Err(e) => panic!("Received invalid UTF-8 DATA from client: {}", e),
                    };
                } else {
                    println!("Received invalid DATA from client");
                }
            }
            _ => {
                writer.write(b"500 Command not recognized\n").await?;
                // anyhow::bail!("Unexpected message received: {command}",),
            }
        }

        // println!("CMD: {command}");
        // while let Some(msg) = iter.next() {
        //     println!("{msg}")
        // }

        buf.clear();
    }

    // todo
    println!("Connection closed");

    Ok(())
}
