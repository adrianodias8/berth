//! Minimal Docker Engine API client over /run/docker.sock (HTTP/1.0, so no
//! chunked encoding to deal with).

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;

use serde::Deserialize;

const SOCKET: &str = "/run/docker.sock";

#[derive(Deserialize, Debug)]
pub struct Container {
    #[serde(rename = "Names", default)]
    pub names: Vec<String>,
    #[serde(rename = "Labels", default)]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "NetworkSettings")]
    pub network: Option<NetworkSettings>,
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
    let mut stream = UnixStream::connect(SOCKET)?;
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

pub fn containers() -> io::Result<Vec<Container>> {
    let mut body = String::new();
    get("/containers/json")?.read_to_string(&mut body)?;
    serde_json::from_str(&body).map_err(io::Error::other)
}

pub fn networks() -> io::Result<Vec<Network>> {
    let mut body = String::new();
    get("/networks")?.read_to_string(&mut body)?;
    serde_json::from_str(&body).map_err(io::Error::other)
}
