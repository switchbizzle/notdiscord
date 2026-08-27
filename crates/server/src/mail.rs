//! Outgoing mail over plain SMTP to a relay that trusts us — in production
//! that's the postfix on this same box (127.0.0.1:25), which accepts from
//! localhost without auth or TLS. Not configured (no NOTDISCORD_SMTP_ADDR)
//! means every email feature politely reports itself as unavailable.

use anyhow::{bail, Context};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// host:port of the SMTP relay, e.g. 127.0.0.1:25.
fn smtp_addr() -> Option<String> {
    std::env::var("NOTDISCORD_SMTP_ADDR").ok().filter(|s| !s.is_empty())
}

/// Bare sender address, e.g. notdiscord@notdiscord.switchbhost.com.
fn from_addr() -> Option<String> {
    std::env::var("NOTDISCORD_MAIL_FROM").ok().filter(|s| !s.is_empty())
}

pub fn configured() -> bool {
    smtp_addr().is_some() && from_addr().is_some()
}

/// Strict enough that an address can't smuggle SMTP or header syntax:
/// ASCII only, no whitespace/control/angle characters, one @ with a dotted
/// domain after it.
pub fn valid_address(s: &str) -> bool {
    if s.len() < 6 || s.len() > 254 {
        return false;
    }
    if !s.chars().all(|c| c.is_ascii_graphic() && !matches!(c, '<' | '>' | ',' | ';' | '"' | '\\')) {
        return false;
    }
    let Some((local, domain)) = s.split_once('@') else { return false };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !domain.contains('@')
}

async fn expect(reader: &mut BufReader<tokio::io::ReadHalf<TcpStream>>, accept: &[u8]) -> anyhow::Result<()> {
    // Responses can span lines ("250-..." then "250 "); the space marks the last.
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.context("smtp read")?;
        if line.len() < 4 {
            bail!("short smtp response: {line:?}");
        }
        if !accept.contains(&line.as_bytes()[0]) {
            bail!("smtp refused: {}", line.trim());
        }
        if line.as_bytes()[3] == b' ' {
            return Ok(());
        }
    }
}

/// Sends one message. Subject and body must be trusted template text
/// (ASCII, no user-controlled content beyond a numeric code).
pub async fn send(to: &str, subject: &str, body: &str) -> anyhow::Result<()> {
    let addr = smtp_addr().context("mail not configured")?;
    let from = from_addr().context("mail not configured")?;
    if !valid_address(to) {
        bail!("invalid recipient address");
    }

    let work = async {
        let stream = TcpStream::connect(&addr).await.context("smtp connect")?;
        let (read, mut write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read);

        expect(&mut reader, b"2").await?; // greeting
        write.write_all(b"EHLO notdiscord\r\n").await?;
        expect(&mut reader, b"2").await?;
        write.write_all(format!("MAIL FROM:<{from}>\r\n").as_bytes()).await?;
        expect(&mut reader, b"2").await?;
        write.write_all(format!("RCPT TO:<{to}>\r\n").as_bytes()).await?;
        expect(&mut reader, b"2").await?;
        write.write_all(b"DATA\r\n").await?;
        expect(&mut reader, b"3").await?;

        // The relay (postfix) adds Date and Message-ID for local submissions.
        let msg = format!(
            "From: NotDiscord <{from}>\r\nTo: <{to}>\r\nSubject: {subject}\r\n\
             MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n.\r\n",
            body.replace('\n', "\r\n"),
        );
        write.write_all(msg.as_bytes()).await?;
        expect(&mut reader, b"2").await?;
        write.write_all(b"QUIT\r\n").await?;
        Ok(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(20), work)
        .await
        .context("smtp timed out")?
}

/// Six random digits, OS entropy.
pub fn new_code() -> String {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("os rng");
    format!("{:06}", u32::from_le_bytes(bytes) % 1_000_000)
}
