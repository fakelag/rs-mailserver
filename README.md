## Rust SMTP mailserver

SMTP server written in rust. Server receives emails via [SMTP](https://en.wikipedia.org/wiki/Simple_Mail_Transfer_Protocol) and publishes them to redis [pubsub](https://redis.io/docs/latest/develop/interact/pubsub/) where they can then be processed by other services. Connections from SMTP clients are handled concurrently by spawning [tokio](https://tokio.rs/) green tasks for each new connection. After an SMTP task has completed a successful exchange with its client, the final email is sent to a [mpsc](https://doc.rust-lang.org/nightly/std/sync/mpsc/index.html) channel for processing. A redis worker task consumes messages from the channel and publishes them. If redis is unreachable, the message will be dropped.

![System graph](https://github.com/fakelag/rs-mailserver/blob/master/img/rsms.png)

### Usage
Compiled binary can be used in client or server mode:

```bash
$ rs-mailserver help
Starting SMTP client:
Usage:
        rs-mailserver send <mail_from> <mail_to> <mail_content>
        rs-mailserver send from@yourdomain.com to@yourdomain.com "email contents"
Starting SMTP server:
Usage:
        rs-mailserver recv <server_domain> [redis_addr] [listen_address]
        rs-mailserver recv yourdomain.com redis://localhost:6379 0.0.0.0:25
```

#### Starting SMTP as server

```bash
$ rs-mailserver recv yourdomain.com
SMTP Server started. Listening to 0.0.0.0:25 (yourdomain.com)
```

#### Sending an email to your server
```bash
$ rs-mailserver send localhost:25 from@yourdomain.com to@yourdomain.com "email contents to send"
Ok
```
