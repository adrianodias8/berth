//! Minimal Docker Engine API client over /run/docker.sock (HTTP/1.0, so no
//! chunked encoding to deal with). Podman's Docker-compatible API answers the
//! same calls, so containers and networks are read from both engines.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;

use serde::Deserialize;

const SOCKET: &str = "/run/docker.sock";

/// Engine sockets to read: Docker always, Podman when it has containers
/// running (asking it otherwise starts its service for nothing).
fn sockets() -> Vec<&'static str> {
    let mut sockets = vec![SOCKET];
    if crate::engines::podman_running() {
        sockets.push(crate::engines::PODMAN_SOCKET);
    }
    sockets
}

#[derive(Deserialize, Debug)]
pub struct Container {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Names", default)]
    pub names: Vec<String>,
    #[serde(rename = "Labels", default)]
    pub labels: BTreeMap<String, String>,
    /// Empty for nerdctl's containers.
    #[serde(rename = "ImageID", default)]
    pub image_id: String,
    #[serde(rename = "NetworkSettings")]
    pub network: Option<NetworkSettings>,
    #[serde(rename = "HostConfig")]
    pub host_config: Option<HostConfigSummary>,
}

#[derive(Deserialize, Debug)]
pub struct HostConfigSummary {
    #[serde(rename = "NetworkMode", default)]
    pub network_mode: String,
}

#[derive(Deserialize, Debug)]
pub struct NetworkSettings {
    #[serde(rename = "Networks", default)]
    pub networks: BTreeMap<String, Endpoint>,
}

#[derive(Deserialize, Debug)]
pub struct Endpoint {
    #[serde(rename = "IPAddress", default)]
    pub ip: String,
}

#[derive(Deserialize, Debug)]
pub struct Network {
    #[serde(rename = "Driver", default)]
    pub driver: String,
    #[serde(rename = "IPAM")]
    pub ipam: Option<Ipam>,
}

#[derive(Deserialize, Debug)]
pub struct Ipam {
    #[serde(rename = "Config", default)]
    pub config: Option<Vec<IpamConfig>>,
}

#[derive(Deserialize, Debug)]
pub struct IpamConfig {
    #[serde(rename = "Subnet", default)]
    pub subnet: String,
}

impl Network {
    /// IPv4 subnets of a bridge network; other drivers have no routable subnet.
    pub fn ipv4_subnets(&self) -> impl Iterator<Item = &str> {
        let bridge = self.driver == "bridge";
        self.ipam
            .iter()
            .flat_map(|i| i.config.iter().flatten())
            .map(|c| c.subnet.as_str())
            .filter(move |s| bridge && !s.contains(':') && !s.is_empty())
    }
}

impl Container {
    pub fn name(&self) -> Option<&str> {
        self.names.first().map(|n| n.trim_start_matches('/'))
    }

    /// First network address, in network-name order; None for host or none
    /// networking.
    pub fn ip(&self) -> Option<std::net::Ipv4Addr> {
        self.network.as_ref()?.networks.values().find_map(|e| e.ip.parse().ok())
    }
}

/// Send a GET and return a reader positioned at the body.
pub fn get(path: &str) -> io::Result<BufReader<UnixStream>> {
    get_from(SOCKET, path)
}

pub fn get_from(socket: &str, path: &str) -> io::Result<BufReader<UnixStream>> {
    let mut stream = UnixStream::connect(socket)?;
    write!(stream, "GET {path} HTTP/1.0\r\nHost: docker\r\n\r\n")?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if !status.contains(" 200 ") {
        return Err(io::Error::other(format!("docker {path}: {}", status.trim())));
    }
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            return Ok(reader);
        }
    }
}

/// Podman's running containers as its Docker-compatible API lists them,
/// for the host's port forwarding; `[]` without asking when it has none.
pub fn podman_containers() -> String {
    if !crate::engines::podman_running() {
        return "[]".into();
    }
    let mut body = String::new();
    match get_bounded(crate::engines::PODMAN_SOCKET, "/containers/json").and_then(|mut r| r.read_to_string(&mut body)) {
        Ok(_) => body,
        Err(_) => "[]".into(),
    }
}

/// A one-shot request (not an event stream): bounded, so a hung engine
/// (podman.service, say) can't stall the domain sync.
fn get_bounded(socket: &str, path: &str) -> io::Result<BufReader<UnixStream>> {
    let reader = get_from(socket, path)?;
    reader.get_ref().set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    Ok(reader)
}

fn list<T: serde::de::DeserializeOwned>(path: &str) -> io::Result<Vec<T>> {
    let mut all = Vec::new();
    for (i, socket) in sockets().into_iter().enumerate() {
        let read = || -> io::Result<Vec<T>> {
            let mut body = String::new();
            get_bounded(socket, path)?.read_to_string(&mut body)?;
            serde_json::from_str(&body).map_err(io::Error::other)
        };
        match read() {
            Ok(items) => all.extend(items),
            // Docker is required; a Podman socket that's still starting isn't.
            Err(err) if i == 0 => return Err(err),
            Err(_) => {}
        }
    }
    Ok(all)
}

pub fn containers() -> io::Result<Vec<Container>> {
    let mut all: Vec<Container> = list("/containers/json")?;
    all.extend(crate::engines::nerdctl_containers().into_iter().filter_map(|c| serde_json::from_value(c).ok()));
    Ok(all)
}

pub fn networks() -> io::Result<Vec<Network>> {
    list("/networks")
}

/// An image's own labels (`Config.Labels`), from whichever engine has it;
/// None when none does.
pub fn image_labels(id: &str) -> Option<BTreeMap<String, String>> {
    sockets().into_iter().find_map(|socket| {
        let mut body = String::new();
        get_bounded(socket, &format!("/images/{id}/json")).and_then(|mut r| r.read_to_string(&mut body)).ok()?;
        let image: serde_json::Value = serde_json::from_str(&body).ok()?;
        Some(serde_json::from_value(image["Config"]["Labels"].clone()).unwrap_or_default())
    })
}

/// `GET /containers/{id}/json`, untyped, from whichever engine has it.
pub fn inspect(id: &str) -> io::Result<serde_json::Value> {
    let mut last = io::Error::other("no engine");
    for socket in sockets() {
        let mut body = String::new();
        match get_bounded(socket, &format!("/containers/{id}/json")).and_then(|mut r| r.read_to_string(&mut body)) {
            Ok(_) => return serde_json::from_str(&body).map_err(io::Error::other),
            Err(err) => last = err,
        }
    }
    crate::engines::nerdctl_inspect(id).ok_or(last)
}
