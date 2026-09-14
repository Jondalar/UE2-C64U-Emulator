//! Client for ue2emu's TCP control protocol (crates/ue2emu/src/control.rs): one command per line, answered by
//! result lines and then `ok`, or by `error line <n>: <message>`. `screen` output sits between
//! `--- screen ---` markers, so an `ok` inside that block is screen text, not the terminator.

use anyhow::{bail, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

pub const SCREEN_MARK: &str = "--- screen ---";

pub struct CtlConn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl CtlConn {
    pub async fn connect(port: u16) -> Result<CtlConn> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await?;
        stream.set_nodelay(true)?;
        let (r, w) = stream.into_split();
        Ok(CtlConn { reader: BufReader::new(r), writer: w })
    }

    /// Send one command. Outer `Err`: the connection failed. Inner `Err`: the emulator's error message.
    pub async fn command(&mut self, line: &str) -> Result<Result<Vec<String>, String>> {
        if line.contains(['\n', '\r']) {
            bail!("a control command must be a single line");
        }
        self.writer.write_all(format!("{line}\n").as_bytes()).await?;
        self.writer.flush().await?;
        let mut out = Vec::new();
        let mut in_screen = false;
        let mut raw = Vec::new();
        loop {
            raw.clear();
            if self.reader.read_until(b'\n', &mut raw).await? == 0 {
                bail!("the emulator closed the control connection");
            }
            let l = String::from_utf8_lossy(&raw).trim_end_matches(['\n', '\r']).to_string();
            if l == SCREEN_MARK {
                in_screen = !in_screen;
                out.push(l);
                continue;
            }
            if !in_screen {
                if l == "ok" {
                    return Ok(Ok(out));
                }
                if let Some(rest) = l.strip_prefix("error line ") {
                    return Ok(Err(rest.split_once(": ").map_or(rest, |(_, m)| m).to_string()));
                }
            }
            out.push(l);
        }
    }
}

/// The text between the first pair of screen markers.
pub fn screen_text(lines: &[String]) -> String {
    let mut text = String::new();
    let mut inside = false;
    for l in lines {
        if l == SCREEN_MARK {
            if inside {
                break;
            }
            inside = true;
            continue;
        }
        if inside {
            text.push_str(l);
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Fake control server answering like control.rs: an `ok` row inside the screen block must not end the reply.
    #[tokio::test]
    async fn parses_results_errors_and_screen_blocks() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            let mut n = 0;
            while let Some(line) = lines.next_line().await.unwrap() {
                n += 1;
                let reply = match line.as_str() {
                    "wait 1" => "ok\n".to_string(),
                    "screen" => "--- screen ---\nok\n MENU\n--- screen ---\nok\n".to_string(),
                    other => format!("error line {n}: unknown command '{other}'\n"),
                };
                w.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let mut c = CtlConn::connect(port).await.unwrap();
        assert_eq!(c.command("wait 1").await.unwrap(), Ok(vec![]));
        assert_eq!(c.command("bogus").await.unwrap(), Err("unknown command 'bogus'".to_string()));
        let screen = c.command("screen").await.unwrap().unwrap();
        assert_eq!(screen_text(&screen), "ok\n MENU\n");
        assert!(c.command("x\ny").await.is_err());
        drop(c);
        server.await.unwrap();
    }
}
