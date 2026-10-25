//! IPv6 for containers, opt-in: `"ipv6": true` in ~/.berth/config.json (the
//! app's Settings writes it), applied when the agent starts.
//!
//! Docker then gives its default bridge a ULA subnet (`DOCKER_SUBNET`) and,
//! through `default-network-opts`, every new bridge network (Compose ones
//! included) an IPv6 subnet from its own ULA pool. Docker's ip6tables rules
//! masquerade container traffic leaving the VM (NAT66), so containers use the
//! VM's own IPv6 address outside.
//!
//! What the VM has to offer: Apple's NAT (VZNATNetworkDeviceAttachment)
//! advertises a ULA /64 on the VM network, so the guest always has an IPv6
//! address there and reaches the Mac on the Mac's address in that prefix. A
//! default route comes only with a non-zero router lifetime in those
//! advertisements; on a Mac without IPv6 internet the lifetime is 0 and IPv6
//! stays limited to containers, the VM and the Mac (see docs/LEARNINGS.md).
//!
//! Enabling IPv6 networks turns on IPv6 forwarding, and systemd-networkd
//! stops accepting router advertisements on forwarding interfaces unless told
//! to, which would cost the VM its route out. A drop-in for the uplink's
//! .network file keeps them.

use std::io;
use std::process::Command;

use serde_json::{Map, Value, json};

/// Docker's default bridge. fd62:6572:7468 is "berth" in ASCII: fixed rather
/// than random so the subnet is recognisable, and as unlikely as any other
/// ULA to meet one of the Mac's networks.
pub const DOCKER_SUBNET: &str = "fd62:6572:7468:1::/64";
const ENABLE_OPT: &str = "com.docker.network.enable_ipv6";
const DROPIN: &str = "berth-ipv6.conf";

/// The `ipv6` setting in berth's config.json.
pub fn enabled() -> bool {
    crate::setting("ipv6").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Our IPv6 keys in daemon.json. Turning the setting off removes only what we
/// wrote (recognised by our subnet), so IPv6 configured by hand stays.
pub fn apply(config: &mut Map<String, Value>, enabled: bool) {
    if enabled {
        config.insert("ipv6".into(), Value::Bool(true));
        config.insert("fixed-cidr-v6".into(), json!(DOCKER_SUBNET));
        let opts = config.entry("default-network-opts").or_insert_with(|| json!({}));
        if let Some(opts) = opts.as_object_mut() {
            let bridge = opts.entry("bridge").or_insert_with(|| json!({}));
            if let Some(bridge) = bridge.as_object_mut() {
                bridge.insert(ENABLE_OPT.into(), json!("true"));
            }
        }
        return;
    }
    if config.get("fixed-cidr-v6").and_then(Value::as_str) != Some(DOCKER_SUBNET) {
        return;
    }
    config.remove("fixed-cidr-v6");
    config.remove("ipv6");
    if let Some(opts) = config.get_mut("default-network-opts").and_then(Value::as_object_mut) {
        if let Some(bridge) = opts.get_mut("bridge").and_then(Value::as_object_mut) {
            bridge.remove(ENABLE_OPT);
            if bridge.is_empty() {
                opts.remove("bridge");
            }
        }
        if opts.is_empty() {
            config.remove("default-network-opts");
        }
    }
}

/// Keep accepting router advertisements on the uplink once IPv6 forwarding
/// is on (or stop overriding networkd when IPv6 is off).
pub fn configure_network(enabled: bool) -> io::Result<()> {
    let ifname = crate::domains::uplink_name()?;
    let out = Command::new("networkctl").args(["--json=short", "status", &ifname]).output()?;
    let status: Value = serde_json::from_slice(&out.stdout).map_err(io::Error::other)?;
    let Some(file) = status["NetworkFile"].as_str() else { return Ok(()) };
    let Some(name) = std::path::Path::new(file).file_name().and_then(|n| n.to_str()) else { return Ok(()) };
    let dir = format!("/etc/systemd/network/{name}.d");
    let path = format!("{dir}/{DROPIN}");
    let content = "# berth: IPv6 for containers (agent/src/ipv6.rs)\n[Network]\nIPv6AcceptRA=yes\n";
    let changed = if enabled {
        if std::fs::read_to_string(&path).is_ok_and(|c| c == content) {
            false
        } else {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&path, content)?;
            true
        }
    } else {
        std::fs::remove_file(&path).is_ok()
    };
    if changed {
        eprintln!("berth-agent: ipv6: {} router advertisements on {ifname}", if enabled { "accepting" } else { "default handling of" });
        Command::new("networkctl").arg("reload").status()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn enable_keeps_other_settings() {
        let mut c = config(json!({"dns": ["198.19.248.253"], "default-network-opts": {"bridge": {"com.docker.network.driver.mtu": "1400"}}}));
        apply(&mut c, true);
        assert_eq!(
            Value::Object(c.clone()),
            json!({
                "dns": ["198.19.248.253"],
                "ipv6": true,
                "fixed-cidr-v6": DOCKER_SUBNET,
                "default-network-opts": {"bridge": {"com.docker.network.driver.mtu": "1400", "com.docker.network.enable_ipv6": "true"}}
            })
        );
        let before = c.clone();
        apply(&mut c, true);
        assert_eq!(c, before, "idempotent, so dockerd isn't restarted");
        apply(&mut c, false);
        assert_eq!(
            Value::Object(c),
            json!({"dns": ["198.19.248.253"], "default-network-opts": {"bridge": {"com.docker.network.driver.mtu": "1400"}}})
        );
    }

    #[test]
    fn disable_removes_only_ours() {
        let mut c = config(json!({"dns": ["x"]}));
        apply(&mut c, true);
        apply(&mut c, false);
        assert_eq!(Value::Object(c), json!({"dns": ["x"]}));
        let mine = json!({"ipv6": true, "fixed-cidr-v6": "2001:db8:1::/64"});
        let mut c = config(mine.clone());
        apply(&mut c, false);
        assert_eq!(Value::Object(c), mine);
    }
}
