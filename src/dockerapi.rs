//! The Docker (and Podman compat) socket bridge, with one change to what
//! clients send: bind-mount sources under /tmp and /var/folders become
//! /private/tmp and /private/var/folders. On the Mac those are symlinks the
//! CLI passes through unresolved, and in the VM /tmp is the VM's own; the
//! real folders are shared at their /private paths (virtiofs::EXTRA_SHARES).
//! So `docker run -v /tmp/x:/x` and `$TMPDIR` mounts (testcontainers) see
//! the Mac's files, as with OrbStack.
//!
//! Only container-create requests are parsed. Everything else, bodies
//! included, streams through; an upgraded connection (attach, exec,
//! BuildKit sessions) becomes a plain pipe after its request.

use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::daemon::{guest_stream, is_disconnect};

/// Requests bigger than this are passed on unread.
const MAX_CREATE_BODY: u64 = 8 << 20;
/// Request headers, as dockerd (Go's 1 MiB): `docker build` sends every
/// registry login in X-Registry-Config.
const HEAD_LIMIT: usize = 1 << 20;

pub fn bridge(listener: UnixListener, target: &'static str, name: &'static str) {
    tokio::spawn(async move {
        loop {
            let client = match listener.accept().await {
                Ok((client, _)) => client,
                Err(err) => {
                    // Out of descriptors, typically: back off, don't stop serving.
                    eprintln!("{name} bridge: accept: {err}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
            tokio::spawn(async move {
                if let Err(err) = connection(client, target).await
                    && !is_disconnect(&err)
                {
                    eprintln!("{name} bridge: {err:#}");
                }
            });
        }
    });
}

async fn connection(client: UnixStream, target: &str) -> Result<()> {
    let guest = guest_stream(target).await?;
    let (from_guest, mut to_guest) = guest.into_split();
    let (from_client, mut to_client) = client.into_split();
    let replies = tokio::spawn(async move {
        let mut from_guest = from_guest;
        let _ = tokio::io::copy_buf(&mut BufReader::with_capacity(crate::BUF, &mut from_guest), &mut to_client).await;
        let _ = to_client.shutdown().await;
    });
    let result = requests(BufReader::with_capacity(crate::BUF, from_client), &mut to_guest).await;
    let _ = to_guest.shutdown().await;
    let _ = replies.await;
    result
}

/// Pass requests on one at a time, rewriting container creates.
async fn requests<R, W>(mut client: BufReader<R>, guest: &mut W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let Some(head) = read_head(&mut client).await? else { return Ok(()) };
        let text = String::from_utf8_lossy(&head).into_owned();
        let mut lines = text.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split(' ');
        let (method, path) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let header = |name: &str| {
            text.split("\r\n").skip(1).find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
            })
        };
        let length = header("content-length").and_then(|v| v.parse::<u64>().ok());
        let chunked = header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
        let upgrade = header("upgrade").is_some() || header("connection").is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));

        if method == "POST" && is_create(path) && !chunked && length.is_some_and(|n| n <= MAX_CREATE_BODY) {
            let mut body = vec![0u8; length.unwrap_or(0) as usize];
            client.read_exact(&mut body).await?;
            match rewrite_create(&body) {
                Some(new) => {
                    guest.write_all(&with_length(&text, new.len()).into_bytes()).await?;
                    guest.write_all(&new).await?;
                }
                None => {
                    guest.write_all(&head).await?;
                    guest.write_all(&body).await?;
                }
            }
        } else {
            guest.write_all(&head).await?;
            if let Some(n) = length {
                // copy_buf: through our big buffer (copy() uses 8 KiB).
                tokio::io::copy_buf(&mut (&mut client).take(n), guest).await?;
            } else if chunked {
                copy_chunked(&mut client, guest).await?;
            }
        }
        guest.flush().await?;
        if upgrade {
            // From here on it isn't HTTP.
            tokio::io::copy_buf(&mut client, guest).await?;
            return Ok(());
        }
    }
}

/// The request line and headers, through the blank line; None at EOF.
async fn read_head<R: tokio::io::AsyncRead + Unpin>(client: &mut BufReader<R>) -> Result<Option<Vec<u8>>> {
    let mut head = Vec::new();
    loop {
        let start = head.len();
        if (&mut *client).take(HEAD_LIMIT as u64).read_until(b'\n', &mut head).await? == 0 {
            if head.is_empty() {
                return Ok(None);
            }
            anyhow::bail!("connection ended inside a request");
        }
        if head.len() > HEAD_LIMIT {
            anyhow::bail!("request headers too long");
        }
        if &head[start..] == b"\r\n" && start > 0 {
            return Ok(Some(head));
        }
    }
}

async fn copy_chunked<R, W>(client: &mut BufReader<R>, guest: &mut W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let mut line = Vec::new();
        client.read_until(b'\n', &mut line).await?;
        guest.write_all(&line).await?;
        let size = u64::from_str_radix(String::from_utf8_lossy(&line).trim().split(';').next().unwrap_or(""), 16)?;
        if size == 0 {
            // Trailers, then the blank line.
            loop {
                let mut trailer = Vec::new();
                client.read_until(b'\n', &mut trailer).await?;
                guest.write_all(&trailer).await?;
                if trailer == b"\r\n" || trailer.is_empty() {
                    return Ok(());
                }
            }
        }
        tokio::io::copy_buf(&mut (&mut *client).take(size + 2), guest).await?;
    }
}

/// `/containers/create`, with or without an API version, any query; also
/// Podman's own `/vN/libpod/containers/create`.
fn is_create(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or_default();
    let rest = match path.strip_prefix("/v") {
        Some(versioned) => match versioned.split_once('/') {
            Some((version, rest)) if version.bytes().all(|b| b.is_ascii_digit() || b == b'.') => rest,
            _ => return false,
        },
        None => path.strip_prefix('/').unwrap_or(path),
    };
    rest == "containers/create" || rest == "libpod/containers/create"
}

/// The head with its Content-Length replaced.
fn with_length(head: &str, length: usize) -> String {
    let mut out = String::new();
    for line in head.split_inclusive("\r\n") {
        if line.split_once(':').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("content-length")) {
            out.push_str(&format!("Content-Length: {length}\r\n"));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// The Mac path a bind source names in the VM, if it differs.
fn map_source(source: &str) -> Option<String> {
    for prefix in ["/tmp", "/var/folders"] {
        if source == prefix || source.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')) {
            return Some(format!("/private{source}"));
        }
    }
    None
}

/// The create body with bind sources mapped; None if nothing changed.
fn rewrite_create(body: &[u8]) -> Option<Vec<u8>> {
    let mut json: Value = serde_json::from_slice(body).ok()?;
    let mut changed = false;
    // Podman's libpod spec: "mounts": [{"type": "bind", "source": ...}].
    if let Some(mounts) = json.get_mut("mounts").and_then(Value::as_array_mut) {
        for mount in mounts.iter_mut().filter(|m| m["type"] == "bind") {
            if let Some(mapped) = mount["source"].as_str().and_then(map_source) {
                mount["source"] = Value::String(mapped);
                changed = true;
            }
        }
    }
    let Some(host) = json.get_mut("HostConfig") else {
        return changed.then(|| serde_json::to_vec(&json).ok()).flatten();
    };
    if let Some(binds) = host.get_mut("Binds").and_then(Value::as_array_mut) {
        for bind in binds {
            let Some(spec) = bind.as_str() else { continue };
            let (source, rest) = spec.split_once(':').unwrap_or((spec, ""));
            if let Some(mapped) = map_source(source) {
                *bind = Value::String(if rest.is_empty() { mapped } else { format!("{mapped}:{rest}") });
                changed = true;
            }
        }
    }
    if let Some(mounts) = host.get_mut("Mounts").and_then(Value::as_array_mut) {
        for mount in mounts.iter_mut().filter(|m| m["Type"] == "bind") {
            if let Some(mapped) = mount["Source"].as_str().and_then(map_source) {
                mount["Source"] = Value::String(mapped);
                changed = true;
            }
        }
    }
    changed.then(|| serde_json::to_vec(&json).ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_paths() {
        assert!(is_create("/containers/create"));
        assert!(is_create("/v1.47/containers/create?name=web"));
        assert!(!is_create("/v1.47/containers/web/start"));
        assert!(!is_create("/vx/containers/create"));
        assert!(is_create("/v4.0.0/libpod/containers/create"));
        assert!(!is_create("/v4.0.0/libpod/containers/json"));
        let libpod = br#"{"image":"a","mounts":[{"type":"bind","source":"/tmp/p","destination":"/p"}]}"#;
        let out: Value = serde_json::from_slice(&rewrite_create(libpod).unwrap()).unwrap();
        assert_eq!(out["mounts"][0]["source"], "/private/tmp/p");
    }

    #[test]
    fn temp_folders_map_to_their_private_paths() {
        assert_eq!(map_source("/tmp/x").as_deref(), Some("/private/tmp/x"));
        assert_eq!(map_source("/tmp").as_deref(), Some("/private/tmp"));
        assert_eq!(map_source("/var/folders/ab/T/tc").as_deref(), Some("/private/var/folders/ab/T/tc"));
        assert_eq!(map_source("/tmpfoo"), None);
        assert_eq!(map_source("/Users/me/p"), None);
        let body = br#"{"Image":"a","HostConfig":{"Binds":["/tmp/x:/x:ro","/Users/me:/m","vol:/v"],"Mounts":[{"Type":"bind","Source":"/var/folders/q/T","Target":"/t"},{"Type":"volume","Source":"/tmp","Target":"/w"}]}}"#;
        let out: Value = serde_json::from_slice(&rewrite_create(body).unwrap()).unwrap();
        assert_eq!(out["HostConfig"]["Binds"], serde_json::json!(["/private/tmp/x:/x:ro", "/Users/me:/m", "vol:/v"]));
        assert_eq!(out["HostConfig"]["Mounts"][0]["Source"], "/private/var/folders/q/T");
        assert_eq!(out["HostConfig"]["Mounts"][1]["Source"], "/tmp", "volumes aren't paths");
        assert!(rewrite_create(br#"{"HostConfig":{"Binds":["/Users/me:/m"]}}"#).is_none());
    }

    #[tokio::test]
    async fn requests_stream_through() {
        let body = br#"{"HostConfig":{"Binds":["/tmp/a:/a"]}}"#;
        let input = [
            format!("POST /v1.47/containers/create HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes(),
            body.to_vec(),
            b"POST /build HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n".to_vec(),
            b"POST /exec/1/start HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: 2\r\n\r\n{}raw bytes".to_vec(),
        ]
        .concat();
        let mut out = Vec::new();
        requests(BufReader::new(&input[..]), &mut out).await.unwrap();
        let out = String::from_utf8(out).unwrap();
        let rewritten = r#"{"HostConfig":{"Binds":["/private/tmp/a:/a"]}}"#;
        assert!(out.contains(&format!("Content-Length: {}\r\n\r\n{rewritten}", rewritten.len())), "{out}");
        assert!(out.contains("3\r\nabc\r\n0\r\n\r\n"));
        assert!(out.ends_with("{}raw bytes"));
    }
}
