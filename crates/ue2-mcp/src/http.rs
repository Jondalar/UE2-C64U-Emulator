//! Minimal HTTP/1.1 client for the firmware's web server behind the emulator's web UI proxy (`--web-port`).

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

pub struct Request<'a> {
    pub port: u16,
    pub method: &'a str,
    pub path: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub attempts: u32,
}

enum Failure {
    /// Nothing came back (connection refused/reset or closed before any byte): the guest service is not up yet.
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

/// Send `req`, retrying while the guest does not answer at all, until `timeout` (wall clock).
pub async fn send(req: &Request<'_>, timeout: Duration) -> Result<Response> {
    let deadline = Instant::now() + timeout;
    let mut attempts = 0;
    loop {
        attempts += 1;
        match tokio::time::timeout_at(deadline, once(req)).await {
            Err(_) => bail!(
                "HTTP {} {} on 127.0.0.1:{}: no complete response within {} ms ({attempts} attempt(s))",
                req.method,
                req.path,
                req.port,
                timeout.as_millis()
            ),
            Ok(Ok(mut r)) => {
                r.attempts = attempts;
                return Ok(r);
            }
            Ok(Err(Failure::Fatal(e))) => return Err(e),
            Ok(Err(Failure::Retry(e))) => {
                if deadline.saturating_duration_since(Instant::now()) < Duration::from_millis(600) {
                    return Err(e.context(format!("gave up after {attempts} attempt(s)")));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

async fn once(req: &Request<'_>) -> Result<Response, Failure> {
    let mut s = TcpStream::connect(("127.0.0.1", req.port))
        .await
        .map_err(|e| Failure::Retry(anyhow!("connect 127.0.0.1:{}: {e}", req.port)))?;
    let has = |name: &str| req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    let mut head = format!("{} {} HTTP/1.1\r\n", req.method, req.path);
    if !has("host") {
        head.push_str(&format!("Host: 127.0.0.1:{}\r\n", req.port));
    }
    if !has("connection") {
        head.push_str("Connection: close\r\n");
    }
    if !has("user-agent") {
        head.push_str("User-Agent: ue2-mcp\r\n");
    }
    for (k, v) in req.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    let wants_length = !req.body.is_empty() || matches!(req.method, "POST" | "PUT" | "PATCH");
    if wants_length && !has("content-length") {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(req.body);
    s.write_all(&out).await.map_err(|e| Failure::Retry(anyhow!("send request: {e}")))?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        let n = match s.read(&mut chunk).await {
            Ok(n) => n,
            Err(e) if buf.is_empty() => return Err(Failure::Retry(anyhow!("read response: {e}"))),
            Err(e) => return Err(Failure::Fatal(anyhow!("read response after {} bytes: {e}", buf.len()))),
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(r) = parse(&buf, req.method, false).map_err(Failure::Fatal)? {
            return Ok(r);
        }
    }
    if buf.is_empty() {
        return Err(Failure::Retry(anyhow!(
            "127.0.0.1:{} closed the connection without a response (web server not up yet?)",
            req.port
        )));
    }
    parse(&buf, req.method, true)
        .map_err(Failure::Fatal)?
        .ok_or_else(|| Failure::Fatal(anyhow!("incomplete HTTP response ({} bytes)", buf.len())))
}

/// Parse a response. `Ok(None)`: more bytes are needed.
fn parse(buf: &[u8], method: &str, eof: bool) -> Result<Option<Response>> {
    let Some(header_end) = find(buf, b"\r\n\r\n") else {
        if eof {
            bail!("no HTTP header in {} bytes: {:?}", buf.len(), String::from_utf8_lossy(&buf[..buf.len().min(200)]));
        }
        return Ok(None);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    if !parts.next().is_some_and(|v| v.starts_with("HTTP/")) {
        bail!("not an HTTP response: {status_line:?}");
    }
    let status: u16 =
        parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("bad status line {status_line:?}"))?;
    let reason = parts.next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect();
    let header = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    let rest = &buf[header_end + 4..];
    let no_body = method.eq_ignore_ascii_case("HEAD") || status == 204 || status == 304 || status < 200;
    let chunked = header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let length = header("content-length").and_then(|v| v.parse::<usize>().ok());
    let body = if no_body {
        Some(Vec::new())
    } else if chunked {
        dechunk(rest)?
    } else if let Some(len) = length {
        (rest.len() >= len).then(|| rest[..len].to_vec())
    } else if eof {
        Some(rest.to_vec())
    } else {
        None
    };
    match body {
        Some(body) => Ok(Some(Response { status, reason, headers, body, attempts: 0 })),
        None if eof => bail!("HTTP body truncated ({} bytes after the header)", rest.len()),
        None => Ok(None),
    }
}

/// Decode a chunked body. `Ok(None)`: incomplete.
fn dechunk(mut data: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut body = Vec::new();
    loop {
        let Some(eol) = find(data, b"\r\n") else { return Ok(None) };
        let size_text = String::from_utf8_lossy(&data[..eol]);
        let size_hex = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| anyhow!("bad chunk size {size_hex:?}"))?;
        data = &data[eol + 2..];
        if size == 0 {
            return Ok(Some(body));
        }
        if data.len() < size + 2 {
            return Ok(None);
        }
        body.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn parses_length_chunked_and_eof_bodies() {
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello", "GET", false).unwrap().unwrap();
        assert_eq!((r.status, r.reason.as_str(), r.body.as_slice()), (200, "OK", &b"hello"[..]));
        assert!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel", "GET", false).unwrap().is_none());
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=1\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse(chunked, "GET", false).unwrap().unwrap().body, b"abcde");
        assert!(parse(&chunked[..chunked.len() - 8], "GET", false).unwrap().is_none());
        assert_eq!(parse(b"HTTP/1.0 404 Not Found\r\n\r\nnope", "GET", true).unwrap().unwrap().body, b"nope");
        assert!(parse(b"HTTP/1.0 404 Not Found\r\n\r\nnope", "GET", false).unwrap().is_none());
        assert!(parse(b"garbage\r\n\r\n", "GET", true).is_err());
    }

    #[tokio::test]
    async fn retries_until_the_server_answers() {
        // Reserve a port, then start listening on it only after the first attempts were refused.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            let l = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}").await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let req = Request { port, method: "POST", path: "/v1/x", headers: &[], body: b"ab" };
        let r = send(&req, Duration::from_secs(5)).await.unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"{}"[..]));
        assert!(r.attempts >= 2, "attempts {}", r.attempts);
        let seen = server.await.unwrap();
        assert!(seen.starts_with("POST /v1/x HTTP/1.1\r\n") && seen.contains("Content-Length: 2\r\n"), "{seen}");
        assert!(seen.ends_with("\r\n\r\nab"), "{seen}");
    }
}
