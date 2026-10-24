//! `berth import orbstack|docker-desktop`: copy another engine's networks,
//! images, volumes and containers into berth, through both Docker APIs.
//! The other engine must be running; nothing in it is changed (helper
//! containers made there to read volumes are removed again).
//!
//! - Networks: user-defined ones, by name and driver; berth picks subnets.
//! - Images: tagged ones and those containers use, streamed from
//!   `images/get` into `images/load`; ones berth already has are skipped.
//! - Volumes: local ones, contents copied with the archive API through
//!   containers that are created but never started.
//! - Containers: recreated (stopped) from their configuration, labels
//!   included, so `docker compose up` treats them as its own.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::paths::Paths;

/// Things skipped because they failed: reported, then the import fails.
static FAILED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn failed(what: &str, err: &anyhow::Error) {
    eprintln!("  {what} skipped: {err:#}");
    FAILED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

pub struct Options {
    pub images: bool,
    pub volumes: bool,
    pub containers: bool,
    pub dry_run: bool,
    /// Only this Compose project: its containers and what they use.
    pub project: Option<String>,
}

/// With --project, what may be imported besides the containers.
struct Scope {
    images: HashSet<String>,
    volumes: HashSet<String>,
    networks: HashSet<String>,
}

impl Scope {
    fn of(containers: &[Value]) -> Self {
        let mut scope = Scope { images: HashSet::new(), volumes: HashSet::new(), networks: HashSet::new() };
        for c in containers {
            for key in [&c["Config"]["Image"], &c["Image"]] {
                if let Some(i) = key.as_str() {
                    scope.images.insert(i.to_string());
                }
            }
            for m in c["Mounts"].as_array().into_iter().flatten() {
                if m["Type"] == "volume"
                    && let Some(n) = m["Name"].as_str()
                {
                    scope.volumes.insert(n.to_string());
                }
            }
            if let Some(nets) = c["NetworkSettings"]["Networks"].as_object() {
                scope.networks.extend(nets.keys().cloned());
            }
        }
        scope
    }
}

/// The other engine's Docker socket.
pub fn source_socket(paths: &Paths, from: &str, socket: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(socket) = socket {
        return Ok(socket);
    }
    let (path, name, start) = match from {
        "orbstack" => (paths.home.join(".orbstack/run/docker.sock"), "OrbStack", "orb start"),
        "docker-desktop" => (paths.home.join(".docker/run/docker.sock"), "Docker Desktop", "open -a Docker"),
        other => bail!("unknown source {other}: orbstack or docker-desktop"),
    };
    if UnixStream::connect(&path).is_err() {
        bail!("{name} isn't running ({} doesn't answer). Start it ({start}), then import again.", path.display());
    }
    Ok(path)
}

pub fn run(paths: &Paths, from: &Path, opts: Options) -> Result<()> {
    let to = paths.docker_socket();
    let src = Api(from.to_path_buf());
    let dst = Api(to);
    let version = src.json("GET", "/version", None)?;
    println!(
        "Importing from Docker {} ({}){}",
        version["Version"].as_str().unwrap_or("?"),
        from.display(),
        if opts.dry_run { " — dry run, nothing is changed" } else { "" }
    );

    let containers: Vec<Value> = src
        .json("GET", "/containers/json?all=1", None)?
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|c| c["Id"].as_str())
        .filter_map(|id| src.json("GET", &format!("/containers/{id}/json"), None).inspect_err(|e| failed(&format!("container {id}"), e)).ok())
        // Kubernetes' containers belong to its cluster, not to Docker users.
        .filter(|c| !c["Config"]["Labels"].as_object().is_some_and(|l| l.contains_key("io.kubernetes.pod.name")))
        .filter(|c| opts.project.as_ref().is_none_or(|p| c["Config"]["Labels"]["com.docker.compose.project"] == p.as_str()))
        .collect();
    if let Some(p) = &opts.project
        && containers.is_empty()
    {
        bail!("no containers of Compose project {p}");
    }
    let scope = opts.project.as_ref().map(|_| Scope::of(&containers));
    let scope = scope.as_ref();

    if opts.containers {
        networks(&src, &dst, scope, opts.dry_run)?;
    }
    if opts.images {
        images(&src, &dst, &containers, scope, opts.dry_run)?;
    }
    if opts.volumes {
        volumes(&src, &dst, scope, opts.dry_run)?;
    }
    if opts.containers {
        recreate(&dst, &containers, opts.dry_run)?;
    }
    match FAILED.load(std::sync::atomic::Ordering::SeqCst) {
        0 => {
            println!("Done.");
            Ok(())
        }
        n => bail!("{n} item(s) couldn't be imported (above); the rest was"),
    }
}

const BUILTIN_NETWORKS: [&str; 3] = ["bridge", "host", "none"];

fn networks(src: &Api, dst: &Api, scope: Option<&Scope>, dry_run: bool) -> Result<()> {
    let have: HashSet<String> = names(&dst.json("GET", "/networks", None)?, "Name");
    let mut wanted: Vec<Value> = Vec::new();
    for net in src.json("GET", "/networks", None)?.as_array().cloned().unwrap_or_default() {
        let name = net["Name"].as_str().unwrap_or_default();
        if BUILTIN_NETWORKS.contains(&name)
            || net["Driver"] != "bridge"
            || have.contains(name)
            || scope.is_some_and(|s| !s.networks.contains(name))
        {
            continue;
        }
        wanted.push(net);
    }
    for net in wanted {
        let name = net["Name"].as_str().unwrap_or_default();
        println!("network {name}");
        if dry_run {
            continue;
        }
        // Not its subnet: the other engine's pool may clash with berth's.
        let body = json!({
            "Name": name,
            "Driver": "bridge",
            "Labels": net["Labels"],
            "Internal": net["Internal"],
            "Attachable": net["Attachable"],
            "EnableIPv6": net["EnableIPv6"],
            "Options": net["Options"],
        });
        if let Err(err) = dst.json("POST", "/networks/create", Some(&body)) {
            failed("network", &err);
        }
    }
    Ok(())
}

fn images(src: &Api, dst: &Api, containers: &[Value], scope: Option<&Scope>, dry_run: bool) -> Result<()> {
    // By tag: with containerd's image store an image's ID can change on the
    // way (index vs. manifest), and a tag berth already has is left alone.
    let mut have: HashSet<String> = HashSet::new();
    for image in dst.json("GET", "/images/json", None)?.as_array().into_iter().flatten() {
        have.extend(image["Id"].as_str().map(str::to_string));
        have.extend(image["RepoTags"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(str::to_string)));
    }
    let mut names: Vec<String> = Vec::new();
    let mut size = 0u64;
    let used: HashSet<&str> = containers.iter().filter_map(|c| c["Image"].as_str()).collect();
    for image in src.json("GET", "/images/json", None)?.as_array().cloned().unwrap_or_default() {
        let id = image["Id"].as_str().unwrap_or_default();
        if have.contains(id) {
            continue;
        }
        let tags: Vec<String> = image["RepoTags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str())
            .filter(|t| *t != "<none>:<none>")
            .map(str::to_string)
            .collect();
        let tagged = !tags.is_empty();
        let tags: Vec<String> = tags.into_iter().filter(|t| !have.contains(t)).collect();
        if tagged && tags.is_empty() {
            continue;
        }
        if tags.is_empty() && !used.contains(id) {
            continue; // dangling and unused
        }
        let tags: Vec<String> = match scope {
            Some(s) if s.images.contains(id) => tags,
            Some(s) => tags.into_iter().filter(|t| s.images.contains(t)).collect(),
            None => tags,
        };
        if scope.is_some() && tags.is_empty() && !used.contains(id) {
            continue;
        }
        size += image["Size"].as_u64().unwrap_or(0);
        if tags.is_empty() {
            names.push(id.to_string());
        } else {
            names.extend(tags);
        }
    }
    if names.is_empty() {
        println!("images: nothing new");
        return Ok(());
    }
    println!("images: {} ({:.1} GB): {}", names.len(), size as f64 / 1e9, names.join(", "));
    if dry_run {
        return Ok(());
    }
    // One export shares layers between images.
    let query: Vec<String> = names.iter().map(|n| format!("names={}", urlencode(n))).collect();
    let mut export = src.stream("GET", &format!("/images/get?{}", query.join("&")))?;
    let started = std::time::Instant::now();
    let reply = dst.upload("POST", "/images/load?quiet=1", &mut export, "application/x-tar")?;
    for line in reply.lines() {
        if let Ok(msg) = serde_json::from_str::<Value>(line)
            && let Some(err) = msg["error"].as_str()
        {
            bail!("loading images: {err}");
        }
    }
    println!("  loaded in {:.0?}", started.elapsed());
    Ok(())
}

fn volumes(src: &Api, dst: &Api, scope: Option<&Scope>, dry_run: bool) -> Result<()> {
    let have: HashSet<String> = names(&dst.json("GET", "/volumes", None)?["Volumes"], "Name");
    let list = src.json("GET", "/volumes", None)?["Volumes"].as_array().cloned().unwrap_or_default();
    let todo: Vec<&Value> = list
        .iter()
        .filter(|v| v["Driver"] == "local" && !have.contains(v["Name"].as_str().unwrap_or_default()))
        .filter(|v| scope.is_none_or(|s| s.volumes.contains(v["Name"].as_str().unwrap_or_default())))
        .collect();
    if todo.is_empty() {
        println!("volumes: nothing new");
        return Ok(());
    }
    if dry_run {
        for v in &todo {
            println!("volume {}", v["Name"].as_str().unwrap_or_default());
        }
        return Ok(());
    }
    // Any image will do for containers that are never started.
    let (src_image, dst_image) = (helper_image(src)?, helper_image(dst)?);
    for v in todo {
        let name = v["Name"].as_str().unwrap_or_default();
        print!("volume {name}");
        std::io::stdout().flush()?;
        let started = std::time::Instant::now();
        let body = json!({ "Name": name, "Driver": "local", "Labels": v["Labels"], "DriverOpts": v["Options"] });
        // A volume that is a folder on the Mac (o=bind, device=...) or a
        // network share: the same data on both sides, so only its definition.
        let elsewhere = v["Options"].as_object().is_some_and(|o| o.contains_key("device") || o.contains_key("type"));
        let copied = dst.json("POST", "/volumes/create", Some(&body)).and_then(|_| {
            if elsewhere {
                return Ok(());
            }
            let reader = Helper::create(src, &src_image, name)?;
            let writer = Helper::create(dst, &dst_image, name)?;
            // The archive is the folder /berth-volume itself: unpacking it at
            // / lands in the new volume, owners and modes kept.
            let mut tar = src.stream("GET", &format!("/containers/{}/archive?path=/berth-volume", reader.id))?;
            dst.upload("PUT", &format!("/containers/{}/archive?path=/", writer.id), &mut tar, "application/x-tar").map(|_| ())
        });
        match copied {
            Ok(()) if elsewhere => println!(" (points outside Docker: definition only)"),
            Ok(()) => println!(" ({:.0?})", started.elapsed()),
            Err(err) => {
                println!();
                // Half a volume would be skipped as present next time.
                let _ = dst.json("DELETE", &format!("/volumes/{}", urlencode(name)), None);
                failed(&format!("volume {name}"), &err);
            }
        }
    }
    Ok(())
}

/// A container with a volume at /berth-volume, removed when dropped.
struct Helper<'a> {
    api: &'a Api,
    id: String,
}

impl<'a> Helper<'a> {
    fn create(api: &'a Api, image: &str, volume: &str) -> Result<Self> {
        let body = json!({
            "Image": image,
            "Entrypoint": ["/berth-import-never-runs"],
            "Labels": { "dev.berth.import": "helper" },
            "HostConfig": { "Binds": [format!("{volume}:/berth-volume")] },
        });
        let reply = api.json("POST", "/containers/create", Some(&body))?;
        let id = reply["Id"].as_str().context("no container id")?.to_string();
        Ok(Helper { api, id })
    }
}

impl Drop for Helper<'_> {
    fn drop(&mut self) {
        // v=1: an image's VOLUMEs would leave anonymous volumes behind.
        let _ = self.api.json("DELETE", &format!("/containers/{}?force=1&v=1", self.id), None);
    }
}

/// An image the engine already has, else busybox, pulled.
fn helper_image(api: &Api) -> Result<String> {
    let images = api.json("GET", "/images/json", None)?;
    if let Some(id) = images.as_array().and_then(|a| a.first()).and_then(|i| i["Id"].as_str()) {
        return Ok(id.to_string());
    }
    let mut pull = api.stream("POST", "/images/create?fromImage=busybox&tag=latest")?;
    std::io::copy(&mut pull, &mut std::io::sink())?;
    Ok("busybox:latest".into())
}

fn recreate(dst: &Api, containers: &[Value], dry_run: bool) -> Result<()> {
    let have: HashSet<String> = dst
        .json("GET", "/containers/json?all=1", None)?
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|c| c["Names"].as_array().cloned().unwrap_or_default())
        .filter_map(|n| n.as_str().map(|n| n.trim_start_matches('/').to_string()))
        .collect();
    let mut made = 0;
    for c in containers {
        let name = c["Name"].as_str().unwrap_or_default().trim_start_matches('/').to_string();
        if name.is_empty() || have.contains(&name) {
            continue;
        }
        if c["Config"]["Labels"]["dev.berth.import"] == "helper" {
            continue;
        }
        println!("container {name}");
        if dry_run {
            continue;
        }
        match create_container(dst, c, &name) {
            Ok(()) => made += 1,
            Err(err) => failed(&format!("container {name}"), &err),
        }
    }
    if made > 0 {
        println!("  {made} containers created, stopped: start them with `docker start`, or `docker compose up -d` in their project");
    }
    Ok(())
}

fn create_container(dst: &Api, c: &Value, name: &str) -> Result<()> {
    let mut config = c["Config"].clone();
    let id = c["Id"].as_str().unwrap_or_default();
    let obj = config.as_object_mut().context("no Config")?;
    // The old engine's defaults, not the user's settings.
    if obj.get("Hostname").and_then(Value::as_str).is_some_and(|h| id.starts_with(h)) {
        obj.remove("Hostname");
    }
    obj.remove("MacAddress");
    let mut host = c["HostConfig"].clone();
    if let Some(h) = host.as_object_mut() {
        for key in ["ContainerIDFile", "CgroupParent", "Runtime"] {
            h.remove(key);
        }
        // Volumes that aren't in Binds or Mounts (an image's VOLUME, e.g.
        // postgres' data) were imported under their names: reattach them,
        // or the container would get a new empty one.
        let declared: HashSet<String> = h
            .get("Binds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|b| b.as_str()?.split(':').nth(1).map(str::to_string))
            .chain(h.get("Mounts").and_then(Value::as_array).into_iter().flatten().filter_map(|m| m["Target"].as_str().map(str::to_string)))
            .collect();
        let extra: Vec<Value> = c["Mounts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["Type"] == "volume" && m["Destination"].as_str().is_some_and(|d| !declared.contains(d)))
            .map(|m| json!({ "Type": "volume", "Source": m["Name"], "Target": m["Destination"], "ReadOnly": m["RW"] == false }))
            .collect();
        // Compose lists anonymous volumes in Mounts with no Source: name the
        // imported one, or Docker makes a new empty volume there too.
        let actual = |target: &str| {
            c["Mounts"].as_array().into_iter().flatten().find(|m| m["Type"] == "volume" && m["Destination"] == target).map(|m| m["Name"].clone())
        };
        if let Some(list) = h.get_mut("Mounts").and_then(Value::as_array_mut) {
            for m in list.iter_mut().filter(|m| m["Type"] == "volume" && m["Source"].as_str().is_none_or(str::is_empty)) {
                if let Some(name) = m["Target"].as_str().and_then(actual) {
                    m["Source"] = name;
                }
            }
        }
        if !extra.is_empty() {
            let mounts = h.entry("Mounts").or_insert_with(|| json!([]));
            if let Some(list) = mounts.as_array_mut() {
                list.extend(extra);
            } else {
                *mounts = Value::Array(extra);
            }
        }
    }
    obj.insert("HostConfig".into(), host);
    // The first network at create, the others connected after.
    let networks: Vec<(String, Value)> = c["NetworkSettings"]["Networks"]
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let endpoint = |v: &Value| {
        // Docker adds the container's ID as an alias; not the user's.
        let aliases: Vec<Value> = v["Aliases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a.as_str().is_some_and(|a| !id.starts_with(a)))
            .cloned()
            .collect();
        json!({ "Aliases": aliases })
    };
    if let Some((net, v)) = networks.first() {
        obj.insert("NetworkingConfig".into(), json!({ "EndpointsConfig": { net: endpoint(v) } }));
    }
    let reply = dst.json("POST", &format!("/containers/create?name={}", urlencode(name)), Some(&config))?;
    let new_id = reply["Id"].as_str().context("no container id")?;
    for (net, v) in networks.iter().skip(1) {
        let body = json!({ "Container": new_id, "EndpointConfig": endpoint(v) });
        if let Err(err) = dst.json("POST", &format!("/networks/{}/connect", urlencode(net)), Some(&body)) {
            eprintln!("  network {net}: {err:#}");
        }
    }
    Ok(())
}

fn names(list: &Value, key: &str) -> HashSet<String> {
    list.as_array().into_iter().flatten().filter_map(|v| v[key].as_str().map(str::to_string)).collect()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A Docker API on a unix socket: plain HTTP/1.1, one request per connection.
struct Api(PathBuf);

impl Api {
    fn send(&self, method: &str, path: &str, headers: &str) -> Result<UnixStream> {
        let mut sock = UnixStream::connect(&self.0).with_context(|| format!("connecting to {}", self.0.display()))?;
        write!(sock, "{method} {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n{headers}\r\n")?;
        Ok(sock)
    }

    /// The response body, after checking the status.
    fn response(sock: UnixStream, what: &str) -> Result<Box<dyn Read>> {
        let mut reader = BufReader::with_capacity(1 << 20, sock);
        let mut status = String::new();
        reader.read_line(&mut status)?;
        let code: u16 = status.split_whitespace().nth(1).and_then(|c| c.parse().ok()).context("bad HTTP response")?;
        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let body: Box<dyn Read> = if headers.get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
            Box::new(Chunked { inner: reader, left: 0, done: false })
        } else if let Some(len) = headers.get("content-length").and_then(|v| v.parse::<u64>().ok()) {
            Box::new(Exact { inner: reader.take(len), left: len })
        } else {
            Box::new(reader)
        };
        if code >= 300 {
            let mut text = String::new();
            let _ = body.take(64 * 1024).read_to_string(&mut text);
            let msg = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["message"].as_str().map(str::to_string)).unwrap_or(text);
            bail!("{what}: {code} {}", msg.trim());
        }
        Ok(body)
    }

    fn json(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        let headers = if body.is_some() { format!("Content-Type: application/json\r\nContent-Length: {}\r\n", payload.len()) } else { String::new() };
        let mut sock = self.send(method, path, &headers)?;
        sock.write_all(payload.as_bytes())?;
        let mut text = String::new();
        Self::response(sock, &format!("{method} {path}"))?.read_to_string(&mut text)?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }

    fn stream(&self, method: &str, path: &str) -> Result<Box<dyn Read>> {
        let sock = self.send(method, path, "")?;
        Self::response(sock, &format!("{method} {path}"))
    }

    /// Send `body` (chunked, it may be gigabytes) and return the reply.
    fn upload(&self, method: &str, path: &str, body: &mut dyn Read, content_type: &str) -> Result<String> {
        let mut sock = self.send(method, path, &format!("Content-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n"))?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            write!(sock, "{n:x}\r\n")?;
            sock.write_all(&buf[..n])?;
            sock.write_all(b"\r\n")?;
        }
        sock.write_all(b"0\r\n\r\n")?;
        let mut text = String::new();
        Self::response(sock, &format!("{method} {path}"))?.read_to_string(&mut text)?;
        Ok(text)
    }
}

/// A body of known length that must not end early (a short read would
/// pass for all of it).
struct Exact<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && self.left > 0 && !buf.is_empty() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// An HTTP chunked body.
struct Chunked<R> {
    inner: R,
    left: usize,
    done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            self.inner.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| std::io::Error::other("bad chunk size"))?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.left = size;
        }
        let want = self.left.min(buf.len());
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n;
        if self.left == 0 {
            let mut crlf = [0u8; 2];
            self.inner.read_exact(&mut crlf)?;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_bodies() {
        let raw = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\n\r\n";
        let mut body = String::new();
        Chunked { inner: &raw[..], left: 0, done: false }.read_to_string(&mut body).unwrap();
        assert_eq!(body, "Wikipedia");
    }

    #[test]
    fn short_bodies_are_errors() {
        let mut body = String::new();
        assert!(Exact { inner: &b"abc"[..], left: 5 }.read_to_string(&mut body).is_err());
        let mut body = String::new();
        Exact { inner: &b"abcde"[..], left: 5 }.read_to_string(&mut body).unwrap();
        assert_eq!(body, "abcde");
    }

    #[test]
    fn query_escaping() {
        assert_eq!(urlencode("ghcr.io/a/b:1.0"), "ghcr.io%2Fa%2Fb%3A1.0");
    }
}
