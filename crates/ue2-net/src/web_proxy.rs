//! HTTP proxy in front of the firmware web server for `--net user` (docs/status/network.md, "Web UI proxy").
//!
//! The firmware web UI builds every API URL as `"http://" + serverIP + "/v1/…"` with
//! `var serverIP = window.location.hostname;` (firmware/1541ultimate/html/index.html:12; the same line 12 in the
//! `index.html` the Commodore C64U 1.1.0 updater writes). `hostname` carries no port, so behind a forward from
//! 127.0.0.1:8080 every button calls port 80 of the Mac. The proxy passes HTTP through unchanged, except that in
//! `text/html` and JavaScript bodies it replaces `location.hostname` with `location.host`, which includes the port.
//!
//! The firmware sends static files as `HTTP/1.1 200 OK`, `Connection: close`, a `Content-Type` from the file
//! extension and no length (httpd/c-version/lib/middleware.c:17-32,82-83,111-120), and never compresses. A rewritten
//! body goes out with a `Content-Length`; a compressed one (`Content-Encoding` other than `identity`) is not touched.
//!
//! One thread accepts; each connection gets one thread per direction. The emulation thread only services the
//! libslirp forward the proxy connects to, so a slow client or guest never blocks it.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Guest port of the firmware web server (httpd/c-version/lib/server.h `MHS_PORT`).
pub const WEB_GUEST_PORT: u16 = 80;
/// Host port of the proxy when `--net user` comes without `--hostfwd` and `--web-port`.
pub const DEFAULT_WEB_PORT: u16 = 8080;

/// The rewrite: the page's host name becomes host and port.
const FROM: &[u8] = b"location.hostname";
const TO: &[u8] = b"location.host";
/// Media types whose bodies are rewritten.
const REWRITE_TYPES: [&str; 6] = [
    "text/html",
    "application/javascript",
    "text/javascript",
    "application/x-javascript",
    "application/ecmascript",
    "text/ecmascript",
];
/// Longer heads, longer chunk-size lines and larger rewritable bodies pass through unparsed or unchanged.
const MAX_HEAD: usize = 64 * 1024;
const MAX_LINE: u64 = 8 * 1024;
const MAX_REWRITE: usize = 16 << 20;
/// How long a client may keep its side open after the guest closed the connection.
const LINGER: Duration = Duration::from_secs(10);

/// A running proxy. Dropping it stops accepting; open connections finish on their own.
pub struct WebProxy {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl WebProxy {
    /// Accept HTTP clients on `listener` and forward them to `upstream`.
    pub fn start(listener: TcpListener, upstream: SocketAddr) -> io::Result<WebProxy> {
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        thread::Builder::new().name("web-proxy".into()).spawn(move || accept(&listener, upstream, &flag))?;
        Ok(WebProxy { addr, stop })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for WebProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
    }
}

fn accept(listener: &TcpListener, upstream: SocketAddr, stop: &AtomicBool) {
    for conn in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match conn {
            Ok(client) => {
                let spawned =
                    thread::Builder::new().name("web-proxy-conn".into()).spawn(move || serve(client, upstream));
                if spawned.is_err() {
                    thread::sleep(Duration::from_millis(50));
                }
            }
            // Out of descriptors and the like: the client retries.
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// One client connection: requests go to the guest as they are, responses come back through `pump_responses`.
fn serve(client: TcpStream, upstream: SocketAddr) {
    // A refused upstream closes the client without a response, like a plain forward would.
    let Ok(server) = TcpStream::connect(upstream) else { return };
    let _ = client.set_nodelay(true);
    let _ = server.set_nodelay(true);
    let (Ok(client_in), Ok(client_out), Ok(server_in), Ok(server_out)) =
        (client.try_clone(), client.try_clone(), server.try_clone(), server.try_clone())
    else {
        return;
    };
    let (head_tx, head_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let requests = thread::Builder::new().name("web-proxy-req".into()).spawn(move || {
        let _ = pump_requests(BufReader::new(client_in), server_out, &head_tx);
        drop(done_tx);
    });
    let _ = pump_responses(BufReader::new(server_in), client_out, &head_rx);
    // The guest side is finished: FIN to the client, then give it a moment to close before cutting it off (which
    // also wakes the request pump).
    let _ = client.shutdown(Shutdown::Write);
    if let Ok(requests) = requests {
        if done_rx.recv_timeout(LINGER) == Err(mpsc::RecvTimeoutError::Timeout) {
            let _ = client.shutdown(Shutdown::Both);
        }
        let _ = requests.join();
    }
    let _ = server.shutdown(Shutdown::Both);
}

/// Client to guest. Every request is forwarded byte for byte; the pump only follows the framing, so that it can tell
/// the response pump which requests were `HEAD` (their responses have no body).
fn pump_requests(mut from: BufReader<TcpStream>, mut to: TcpStream, head_requests: &Sender<bool>) -> io::Result<()> {
    loop {
        let head = match read_head(&mut from)? {
            Head::Eof => {
                let _ = to.shutdown(Shutdown::Write);
                return Ok(());
            }
            Head::Raw(bytes) => {
                to.write_all(&bytes)?;
                return copy_raw(&mut from, &mut to);
            }
            Head::Complete(bytes) => bytes,
        };
        let fields = Fields::parse(&head);
        let method = fields.start.split(' ').next().unwrap_or_default().to_ascii_uppercase();
        let framing = match fields.framing(Framing::Empty) {
            Some(framing @ (Framing::Empty | Framing::Length(_) | Framing::Chunked)) => framing,
            // A request body without a usable length: the rest of the connection is copied as it is.
            _ => {
                to.write_all(&head)?;
                return copy_raw(&mut from, &mut to);
            }
        };
        let _ = head_requests.send(method == "HEAD");
        to.write_all(&head)?;
        let mut body = Body::new(framing);
        while let Some((piece, _)) = body.next(&mut from)? {
            to.write_all(&piece)?;
        }
        if body.desynced || method == "CONNECT" || fields.get("upgrade").is_some() {
            return copy_raw(&mut from, &mut to);
        }
    }
}

/// Guest to client: responses pass through, rewritable ones are rewritten.
fn pump_responses(mut from: BufReader<TcpStream>, mut to: TcpStream, head_requests: &Receiver<bool>) -> io::Result<()> {
    loop {
        let head = match read_head(&mut from)? {
            Head::Eof => return Ok(()),
            Head::Raw(bytes) => {
                to.write_all(&bytes)?;
                return copy_raw(&mut from, &mut to);
            }
            Head::Complete(bytes) => bytes,
        };
        let fields = Fields::parse(&head);
        let Some(status) = fields.status() else {
            to.write_all(&head)?;
            return copy_raw(&mut from, &mut to);
        };
        if status == 101 {
            to.write_all(&head)?;
            return copy_raw(&mut from, &mut to);
        }
        if (100..200).contains(&status) {
            // Interim response (100 Continue): the final one follows for the same request.
            to.write_all(&head)?;
            continue;
        }
        let head_request = head_requests.try_recv().unwrap_or(false);
        let framing = if head_request || status == 204 || status == 304 {
            Some(Framing::Empty)
        } else {
            fields.framing(Framing::UntilClose)
        };
        let Some(framing) = framing else {
            to.write_all(&head)?;
            return copy_raw(&mut from, &mut to);
        };
        let mut body = Body::new(framing);
        let small = !matches!(framing, Framing::Length(n) if n > MAX_REWRITE as u64);
        if framing != Framing::Empty && small && fields.rewritable() {
            forward_rewritten(&head, &mut body, &mut from, &mut to)?;
        } else {
            to.write_all(&head)?;
            while let Some((piece, _)) = body.next(&mut from)? {
                to.write_all(&piece)?;
            }
        }
        if body.desynced || framing == Framing::UntilClose {
            return copy_raw(&mut from, &mut to);
        }
    }
}

/// Collect a rewritable body; send it rewritten with a `Content-Length`, or exactly as received when it has no
/// match, grows beyond `MAX_REWRITE`, loses its framing or ends early.
fn forward_rewritten(
    head: &[u8],
    body: &mut Body,
    from: &mut BufReader<TcpStream>,
    to: &mut TcpStream,
) -> io::Result<()> {
    let mut wire = Vec::new();
    let mut payload = Vec::new();
    let collected = loop {
        match body.next(from) {
            Ok(Some((piece, is_payload))) => {
                if is_payload {
                    payload.extend_from_slice(&piece);
                }
                wire.extend_from_slice(&piece);
                if wire.len() > MAX_REWRITE {
                    break Ok(false);
                }
            }
            Ok(None) => break Ok(!body.desynced),
            Err(e) => break Err(e),
        }
    };
    match collected {
        Ok(true) => match rewrite(&payload) {
            Some(new) => {
                to.write_all(&with_length(head, new.len()))?;
                to.write_all(&new)
            }
            None => {
                to.write_all(head)?;
                to.write_all(&wire)
            }
        },
        Ok(false) => {
            to.write_all(head)?;
            to.write_all(&wire)?;
            while let Some((piece, _)) = body.next(from)? {
                to.write_all(&piece)?;
            }
            Ok(())
        }
        Err(e) => {
            to.write_all(head)?;
            to.write_all(&wire)?;
            Err(e)
        }
    }
}

/// Replace `location.hostname` with `location.host` where it is not part of a longer identifier. `None` when the
/// body has no such occurrence.
pub fn rewrite(body: &[u8]) -> Option<Vec<u8>> {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut out = Vec::with_capacity(body.len());
    let (mut copied, mut at) = (0, 0);
    while let Some(pos) = body[at..].windows(FROM.len()).position(|w| w == FROM) {
        let start = at + pos;
        let end = start + FROM.len();
        if (start == 0 || !ident(body[start - 1])) && body.get(end).is_none_or(|&b| !ident(b)) {
            out.extend_from_slice(&body[copied..start]);
            out.extend_from_slice(TO);
            copied = end;
            at = end;
        } else {
            at = start + 1;
        }
    }
    (copied > 0).then(|| {
        out.extend_from_slice(&body[copied..]);
        out
    })
}

/// `head` without its `Content-Length` and `Transfer-Encoding` fields, with `Content-Length: len` as the last field.
fn with_length(head: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(head.len() + 24);
    for (i, line) in head.split_inclusive(|&b| b == b'\n').enumerate() {
        if line == b"\r\n" || line == b"\n" {
            out.extend_from_slice(format!("Content-Length: {len}\r\n").as_bytes());
            out.extend_from_slice(line);
            continue;
        }
        let name = line.split(|&b| b == b':').next().unwrap_or_default().trim_ascii();
        if i > 0 && (name.eq_ignore_ascii_case(b"content-length") || name.eq_ignore_ascii_case(b"transfer-encoding")) {
            continue;
        }
        out.extend_from_slice(line);
    }
    out
}

fn copy_raw(from: &mut BufReader<TcpStream>, to: &mut TcpStream) -> io::Result<()> {
    io::copy(from, to)?;
    let _ = to.shutdown(Shutdown::Write);
    Ok(())
}

enum Head {
    /// Start line and fields up to and including the empty line.
    Complete(Vec<u8>),
    /// The peer closed between messages.
    Eof,
    /// Not a head this proxy parses (cut off by EOF or longer than `MAX_HEAD`): passed on raw.
    Raw(Vec<u8>),
}

fn read_head(from: &mut impl BufRead) -> io::Result<Head> {
    let mut head = Vec::new();
    loop {
        let start = head.len();
        let limit = (MAX_HEAD + 1).saturating_sub(start) as u64;
        if from.by_ref().take(limit).read_until(b'\n', &mut head)? == 0 {
            return Ok(if head.is_empty() { Head::Eof } else { Head::Raw(head) });
        }
        let line = &head[start..];
        if !line.ends_with(b"\n") || head.len() > MAX_HEAD {
            return Ok(Head::Raw(head));
        }
        if line == b"\r\n" || line == b"\n" {
            if start == 0 {
                // Empty lines before a message are ignored (RFC 9112 §2.2).
                head.clear();
                continue;
            }
            return Ok(Head::Complete(head));
        }
    }
}

/// Start line and fields of a head. Decoded lossily: only ASCII fields are interpreted, the bytes forwarded are
/// always the original ones.
struct Fields {
    start: String,
    /// Names in lower case.
    fields: Vec<(String, String)>,
}

impl Fields {
    fn parse(head: &[u8]) -> Fields {
        let text = String::from_utf8_lossy(head);
        let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
        let start = lines.next().unwrap_or_default().to_string();
        let fields = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        Fields { start, fields }
    }

    /// All values of field `name` (lower case) as one comma-separated list.
    fn get(&self, name: &str) -> Option<String> {
        let values: Vec<&str> = self.fields.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect();
        (!values.is_empty()).then(|| values.join(", "))
    }

    /// Status code of a response head.
    fn status(&self) -> Option<u16> {
        let mut parts = self.start.split(' ');
        parts.next().filter(|v| v.starts_with("HTTP/"))?;
        parts.next()?.parse().ok()
    }

    /// Body framing from `Transfer-Encoding` and `Content-Length` (RFC 9112 §6.3), `otherwise` without either;
    /// `None` for a `Content-Length` that is not one number.
    fn framing(&self, otherwise: Framing) -> Option<Framing> {
        if let Some(te) = self.get("transfer-encoding") {
            let last = te.rsplit(',').next().unwrap_or_default().trim().to_ascii_lowercase();
            return Some(if last == "chunked" { Framing::Chunked } else { Framing::UntilClose });
        }
        let Some(value) = self.get("content-length") else { return Some(otherwise) };
        let mut lengths = value.split(',').map(|v| v.trim().parse::<u64>().ok());
        let first = lengths.next()??;
        lengths.all(|n| n == Some(first)).then_some(Framing::Length(first))
    }

    /// HTML or JavaScript, not compressed, framed by length, chunks or the end of the connection.
    fn rewritable(&self) -> bool {
        let media =
            self.get("content-type").map(|v| v.split(';').next().unwrap_or_default().trim().to_ascii_lowercase());
        let encoded = self
            .get("content-encoding")
            .is_some_and(|v| !v.split(',').all(|c| c.trim().eq_ignore_ascii_case("identity")));
        let transfer = self.get("transfer-encoding").is_none_or(|v| v.trim().eq_ignore_ascii_case("chunked"));
        media.is_some_and(|m| REWRITE_TYPES.contains(&m.as_str())) && !encoded && transfer
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    Empty,
    Length(u64),
    Chunked,
    UntilClose,
}

#[derive(Clone, Copy)]
enum Chunk {
    Size,
    Data(u64),
    DataEnd,
    Trailer,
}

/// Reads one message body piece by piece, as it is on the wire.
struct Body {
    framing: Framing,
    chunk: Chunk,
    done: bool,
    /// Chunk framing this proxy could not follow; the rest of the connection is read until it closes.
    desynced: bool,
}

impl Body {
    fn new(framing: Framing) -> Body {
        Body { framing, chunk: Chunk::Size, done: false, desynced: false }
    }

    /// The next piece and whether it is payload (not chunk framing); `None` at the end of the body.
    fn next(&mut self, from: &mut impl BufRead) -> io::Result<Option<(Vec<u8>, bool)>> {
        if self.done {
            return Ok(None);
        }
        match self.framing {
            Framing::Empty | Framing::Length(0) => {
                self.done = true;
                Ok(None)
            }
            Framing::Length(left) => {
                let piece = available(from, left)?;
                self.framing = Framing::Length(left - piece.len() as u64);
                Ok(Some((piece, true)))
            }
            Framing::UntilClose => {
                let piece = match available(from, u64::MAX) {
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        self.done = true;
                        return Ok(None);
                    }
                    other => other?,
                };
                Ok(Some((piece, true)))
            }
            Framing::Chunked => self.next_chunked(from),
        }
    }

    fn next_chunked(&mut self, from: &mut impl BufRead) -> io::Result<Option<(Vec<u8>, bool)>> {
        if let Chunk::Data(left) = self.chunk {
            let piece = available(from, left)?;
            let left = left - piece.len() as u64;
            self.chunk = if left == 0 { Chunk::DataEnd } else { Chunk::Data(left) };
            return Ok(Some((piece, true)));
        }
        let mut line = Vec::new();
        from.by_ref().take(MAX_LINE).read_until(b'\n', &mut line)?;
        if line.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let blank = line == b"\r\n" || line == b"\n";
        if !line.ends_with(b"\n") {
            return Ok(Some(self.desync(line)));
        }
        match self.chunk {
            Chunk::Size => {
                let size = String::from_utf8_lossy(&line);
                let size = size.split(';').next().unwrap_or_default().trim();
                match u64::from_str_radix(size, 16) {
                    Ok(0) => self.chunk = Chunk::Trailer,
                    Ok(n) => self.chunk = Chunk::Data(n),
                    Err(_) => return Ok(Some(self.desync(line))),
                }
            }
            Chunk::DataEnd if blank => self.chunk = Chunk::Size,
            Chunk::DataEnd => return Ok(Some(self.desync(line))),
            Chunk::Trailer => self.done = blank,
            Chunk::Data(_) => unreachable!("handled above"),
        }
        Ok(Some((line, false)))
    }

    fn desync(&mut self, line: Vec<u8>) -> (Vec<u8>, bool) {
        self.desynced = true;
        self.framing = Framing::UntilClose;
        (line, false)
    }
}

/// Up to `max` bytes that are already buffered or arrive next; `UnexpectedEof` at the end of the stream.
fn available(from: &mut impl BufRead, max: u64) -> io::Result<Vec<u8>> {
    let buf = loop {
        match from.fill_buf() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => break other?,
        }
    };
    if buf.is_empty() {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let n = buf.len().min(usize::try_from(max).unwrap_or(usize::MAX));
    let piece = buf[..n].to_vec();
    from.consume(n);
    Ok(piece)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_only_the_property() {
        assert_eq!(
            rewrite(b"var serverIP = window.location.hostname;").unwrap(),
            b"var serverIP = window.location.host;"
        );
        assert_eq!(
            rewrite(b"location.hostname+document.location.hostname").unwrap(),
            b"location.host+document.location.host"
        );
        assert_eq!(rewrite(b"geolocation.hostname location.hostnames x.location.host"), None);
        assert_eq!(rewrite(b"no match"), None);
    }

    /// `request` through a fresh proxy to an upstream that reads `upstream_reads` bytes, answers `reply` and closes.
    /// Returns what the upstream received and what the client received.
    fn exchange(request: &[u8], upstream_reads: usize, reply: &'static [u8]) -> (Vec<u8>, Vec<u8>) {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = upstream.accept().unwrap();
            let mut seen = vec![0; upstream_reads];
            s.read_exact(&mut seen).unwrap();
            s.write_all(reply).unwrap();
            seen
        });
        let proxy = WebProxy::start(TcpListener::bind("127.0.0.1:0").unwrap(), upstream_addr).unwrap();
        let mut client = TcpStream::connect(proxy.local_addr()).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        client.write_all(request).unwrap();
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        (server.join().unwrap(), got)
    }

    const GET: &[u8] = b"GET / HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n\r\n";

    #[test]
    fn firmware_page_is_rewritten_and_gets_a_length() {
        // As the firmware sends a static file: close-delimited (middleware.c:82-83).
        let reply = b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n\
            <script>\nvar serverIP = window.location.hostname;\n</script>";
        let (seen, got) = exchange(GET, GET.len(), reply);
        assert_eq!(seen, GET);
        let body = b"<script>\nvar serverIP = window.location.host;\n</script>";
        let head = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/html\r\nContent-Length: ";
        let want = [format!("{head}{}\r\n\r\n", body.len()).as_bytes(), body].concat();
        assert_eq!(String::from_utf8_lossy(&got), String::from_utf8_lossy(&want));
    }

    #[test]
    fn chunked_javascript_is_rewritten_across_chunks() {
        let reply = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\
            Content-Type: application/javascript; charset=utf-8\r\n\r\n\
            9\r\nlocation.\r\n8;x=y\r\nhostname\r\n0\r\nX-Trailer: 1\r\n\r\n";
        let (_, got) = exchange(GET, GET.len(), reply);
        let want = b"HTTP/1.1 200 OK\r\nContent-Type: application/javascript; charset=utf-8\r\n\
            Content-Length: 13\r\n\r\nlocation.host";
        assert_eq!(String::from_utf8_lossy(&got), String::from_utf8_lossy(want));
    }

    #[test]
    fn other_bodies_pass_through_unchanged() {
        let cases: [&'static [u8]; 4] = [
            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 20\r\n\
              Connection: close\r\n\r\n\xff\x00location.hostname\x80",
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\n\r\nlocation.hostname",
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html>no match</html>",
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n\
              11\r\nlocation.hostname\r\n0\r\n\r\n",
        ];
        for reply in cases {
            let (_, got) = exchange(GET, GET.len(), reply);
            assert_eq!(got, reply, "{}", String::from_utf8_lossy(reply));
        }
    }

    #[test]
    fn request_bodies_arrive_exactly() {
        let mut upload = b"POST /v1/drives/a:mount?type=d64 HTTP/1.1\r\nHost: x\r\n\
            Content-Type: application/octet-stream\r\nContent-Length: 300000\r\nExpect: 100-continue\r\n\r\n"
            .to_vec();
        upload.extend((0..300_000u32).map(|i| (i * 7 % 251) as u8));
        let reply = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nConnection: close\r\n\
            Content-Type: application/json\r\nContent-Length: 17\r\n\r\n{ \"errors\": [] }\n";
        let (seen, got) = exchange(&upload, upload.len(), reply);
        assert!(seen == upload, "upload changed");
        assert_eq!(got, reply);

        let chunked =
            b"PUT /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nab\r\n\r\n0\r\n\r\nGET /y HTTP/1.1\r\n\r\n";
        let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n";
        let (seen, got) = exchange(chunked, chunked.len(), reply);
        assert_eq!((seen.as_slice(), got.as_slice()), (&chunked[..], &reply[..]));
    }

    #[test]
    fn keep_alive_follows_head_requests() {
        let requests = b"HEAD / HTTP/1.1\r\nHost: x\r\n\r\nGET / HTTP/1.1\r\nHost: x\r\n\r\n";
        // The HEAD response announces a length but has no body; the next response starts right after it.
        let reply = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 17\r\n\r\n\
            HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 17\r\n\r\nlocation.hostname";
        let (_, got) = exchange(requests, requests.len(), reply);
        let want = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 17\r\n\r\n\
            HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 13\r\n\r\nlocation.host";
        assert_eq!(String::from_utf8_lossy(&got), String::from_utf8_lossy(want));
    }

    #[test]
    fn a_guest_that_closes_at_once_gives_no_response() {
        let (_, got) = exchange(GET, GET.len(), b"");
        assert!(got.is_empty());
    }
}
