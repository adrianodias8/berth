//! The wire side of docs/gui-protocol.md: reach a guest's GUI agent and
//! make one request on that connection.
//!
//! macOS VMs: ~/.berth/mac/<vm>/agent.sock, which the VM's own process
//! (`berth mac run`) forwards to vsock port 2401 in the guest.
//! Linux machines: the daemon's control socket with Request::Gui; after its
//! reply line the daemon has opened `gui <machine>` to berth-agent and
//! carries bytes both ways, so from then on it's the same as a macOS VM.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::daemon::{GuiReply, Request as DaemonRequest};
use crate::paths::Paths;

pub const VERSION: u32 = 1;

/// Where `berth gui` sends its requests.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Mac(String),
    Linux(String),
}

impl Target {
    pub fn parse(s: &str) -> Result<Target> {
        let (kind, name) = s.split_once(':').with_context(|| format!("target {s:?} is mac:<vm> or linux:<machine>"))?;
        if name.is_empty() || name.starts_with('.') || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
            bail!("bad name in target {s:?}");
        }
        match kind {
            "mac" => Ok(Target::Mac(name.into())),
            "linux" => Ok(Target::Linux(name.into())),
            _ => bail!("target {s:?} is mac:<vm> or linux:<machine>"),
        }
    }

    pub fn is_mac(&self) -> bool {
        matches!(self, Target::Mac(_))
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Mac(n) => write!(f, "mac:{n}"),
            Target::Linux(n) => write!(f, "linux:{n}"),
        }
    }
}

pub fn mac_dir(paths: &Paths, vm: &str) -> PathBuf {
    paths.root.join("mac").join(vm)
}

#[derive(Serialize, Debug)]
pub struct Request {
    pub v: u32,
    /// null outside a session.
    pub session: Option<String>,
    pub verb: String,
    pub args: Value,
    /// Bytes that follow the request line (push's tar).
    #[serde(skip_serializing_if = "is_zero")]
    pub payload_len: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Deserialize, Debug, Default)]
pub struct Reply {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub payload_len: u64,
    #[serde(default)]
    pub hit: Option<Value>,
}

/// A connection to the target's GUI agent, ready for the request line.
pub fn connect(paths: &Paths, target: &Target, timeout: Duration) -> Result<UnixStream> {
    let stream = match target {
        Target::Mac(vm) => {
            let sock = mac_dir(paths, vm).join("agent.sock");
            UnixStream::connect(&sock).with_context(|| format!("macOS VM {vm} isn't running (berth mac start {vm})"))?
        }
        Target::Linux(machine) => {
            let mut stream = UnixStream::connect(paths.control_socket()).context("berth is not running (berth start)")?;
            no_sigpipe(&stream);
            stream.set_read_timeout(Some(Duration::from_secs(30)))?;
            writeln!(stream, "{}", serde_json::to_string(&DaemonRequest::Gui { machine: machine.clone() })?)?;
            // Byte at a time: what follows the line is the guest's.
            let line = read_short_line(&mut stream)?;
            let reply: GuiReply = serde_json::from_str(&line).with_context(|| format!("unexpected daemon reply {line:?}"))?;
            if !reply.ok {
                bail!("{}", reply.error.unwrap_or_else(|| format!("can't reach machine {machine}")));
            }
            stream
        }
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    no_sigpipe(&stream);
    Ok(stream)
}

/// main() restores SIGPIPE's default (quiet death on `| head`): a guest
/// that hangs up mid-push must be an error, not the end of the process.
fn no_sigpipe(stream: &UnixStream) {
    let one: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            std::os::fd::AsRawFd::as_raw_fd(stream),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&raw const one).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
}

fn read_short_line(stream: &mut impl Read) -> Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8];
    while line.len() < 4096 {
        if stream.read(&mut byte)? == 0 {
            bail!("the daemon closed the connection");
        }
        if byte[0] == b'\n' {
            return Ok(String::from_utf8_lossy(&line).into_owned());
        }
        line.push(byte[0]);
    }
    bail!("the daemon's reply is too long")
}

/// Send one request (and its payload) and read the reply and its payload.
pub fn exchange<S: Read + Write>(mut stream: S, request: &Request, payload: Option<&mut dyn Read>) -> Result<(Reply, Vec<u8>)> {
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    if let Some(payload) = payload {
        let sent = std::io::copy(&mut payload.take(request.payload_len), &mut stream)?;
        if sent != request.payload_len {
            bail!("the payload ended after {sent} of {} bytes", request.payload_len);
        }
    }
    stream.flush()?;
    // One request per connection, so the reader may read ahead freely.
    let mut reader = BufReader::new(stream);
    let mut text = String::new();
    if reader.read_line(&mut text).map_err(timeout_message)? == 0 {
        bail!("the guest agent closed the connection without replying");
    }
    let reply: Reply = serde_json::from_str(&text).with_context(|| format!("bad reply from the guest agent: {:?}", text.trim()))?;
    let mut bytes = vec![0u8; usize::try_from(reply.payload_len)?];
    reader.read_exact(&mut bytes).context("the guest's payload was cut short")?;
    Ok((reply, bytes))
}

fn timeout_message(err: std::io::Error) -> anyhow::Error {
    if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) {
        anyhow::anyhow!("the guest agent didn't answer in time")
    } else {
        err.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse() {
        assert_eq!(Target::parse("mac:test").unwrap(), Target::Mac("test".into()));
        assert_eq!(Target::parse("linux:dev-1").unwrap().to_string(), "linux:dev-1");
        for bad in ["test", "win:x", "mac:", "mac:../x", "mac:a/b", "linux:a b", "mac:.hidden"] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_request_line_and_payloads_both_ways() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let guest = std::thread::spawn(move || {
            let mut reader = BufReader::new(theirs.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let mut payload = vec![0u8; req["payload_len"].as_u64().unwrap() as usize];
            reader.read_exact(&mut payload).unwrap();
            theirs.write_all(b"{\"ok\":true,\"result\":{\"n\":1},\"payload_len\":3}\nPNG").unwrap();
            (req, payload)
        });
        let request = Request { v: 1, session: None, verb: "push".into(), args: serde_json::json!({}), payload_len: 5 };
        let (reply, bytes) = exchange(&ours, &request, Some(&mut &b"hello"[..])).unwrap();
        assert!(reply.ok);
        assert_eq!(reply.result["n"], 1);
        assert_eq!(bytes, b"PNG");
        let (req, payload) = guest.join().unwrap();
        assert_eq!(req["v"], 1);
        assert_eq!(req["session"], Value::Null);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn a_guest_that_hangs_up_is_an_error() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        let request = Request { v: 1, session: None, verb: "hello".into(), args: serde_json::json!({}), payload_len: 0 };
        assert!(exchange(&ours, &request, None).is_err());
    }
}
