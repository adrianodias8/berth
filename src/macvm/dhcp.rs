//! A macOS guest's IP address, for `berth mac shell`. VZ's NAT network is
//! served by macOS's own DHCP server (bootpd), which records every lease in
//! `/var/db/dhcpd_leases`, readable by any user:
//!
//! ```text
//! {
//!     name=berths-Virtual-Machine
//!     ip_address=192.168.64.7
//!     hw_address=1,a2:b3:4:5:6:7
//!     identifier=1,a2:b3:4:5:6:7
//!     lease=0x6a1b2c3d
//! }
//! ```
//!
//! bootpd writes MAC octets without leading zeros, so they're compared as
//! numbers. Clients that identify by a DUID (`hw_address=ff,…`, as
//! systemd-networkd in berth's Linux VM does) have no MAC and are skipped;
//! macOS identifies by its MAC. The guest has no agent to ask yet
//! (berth-gui-mac comes later).

use std::net::Ipv4Addr;

pub const LEASES: &str = "/var/db/dhcpd_leases";

#[derive(Debug, PartialEq, Eq)]
pub struct Lease {
    pub name: String,
    pub ip: Ipv4Addr,
    pub hw: [u8; 6],
    /// Expiry, Unix seconds.
    pub lease: u64,
}

/// Every well-formed entry; others (and unknown keys) are skipped.
pub fn parse(text: &str) -> Vec<Lease> {
    let mut leases = Vec::new();
    let (mut name, mut ip, mut hw, mut lease) = (String::new(), None, None, 0u64);
    for line in text.lines().map(str::trim) {
        match line {
            "{" => (name, ip, hw, lease) = (String::new(), None, None, 0),
            "}" => {
                if let (Some(ip), Some(hw)) = (ip.take(), hw.take()) {
                    leases.push(Lease { name: std::mem::take(&mut name), ip, hw, lease });
                }
            }
            _ => match line.split_once('=') {
                Some(("name", v)) => name = v.to_string(),
                Some(("ip_address", v)) => ip = v.parse().ok(),
                // "1," is the hardware type (Ethernet).
                Some(("hw_address", v)) => hw = parse_mac(v.split_once(',').map_or(v, |(_, mac)| mac)),
                Some(("lease", v)) => lease = u64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(0),
                _ => {}
            },
        }
    }
    leases
}

/// `a2:b3:4:5:06:07` (any case, leading zeros optional).
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = s.trim().split(':');
    for byte in &mut out {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// The address of the newest lease for `mac`.
pub fn ip_for(text: &str, mac: &str) -> Option<Ipv4Addr> {
    let mac = parse_mac(mac)?;
    parse(text).into_iter().filter(|l| l.hw == mac).max_by_key(|l| l.lease).map(|l| l.ip)
}

/// `ip_for` on the Mac's leases file.
pub fn lookup(mac: &str) -> Option<Ipv4Addr> {
    ip_for(&std::fs::read_to_string(LEASES).ok()?, mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "{
	name=berths-Virtual-Machine
	ip_address=192.168.64.7
	hw_address=1,a2:b3:4:5:6:7
	identifier=1,a2:b3:4:5:6:7
	lease=0x6a1b2c3d
}
{
	name=debian
	ip_address=192.168.64.2
	hw_address=1,ee:1:2:3:4:5
	identifier=ff,a:b:c
	lease=0x6a000000
}
{
	name=berths-Virtual-Machine
	ip_address=192.168.64.9
	hw_address=1,a2:b3:4:5:6:7
	lease=0x6a1b2c3e
}
{
	name=berth
	ip_address=192.168.64.3
	hw_address=ff,f1:f5:dd:7f:0:2:0:0:ab:11:d1:84:a9:78:49:47:43:98
	identifier=ff,f1:f5:dd:7f:0:2:0:0:ab:11:d1:84:a9:78:49:47:43:98
	lease=0x6ae85718
}
{
	name=broken
	ip_address=not-an-ip
	hw_address=1,1:2:3:4:5:6
}
";

    #[test]
    fn parses_entries() {
        let leases = parse(SAMPLE);
        assert_eq!(leases.len(), 3);
        assert_eq!(leases[0].name, "berths-Virtual-Machine");
        assert_eq!(leases[0].ip, Ipv4Addr::new(192, 168, 64, 7));
        assert_eq!(leases[0].hw, [0xa2, 0xb3, 4, 5, 6, 7]);
        assert_eq!(leases[0].lease, 0x6a1b2c3d);
        assert_eq!(leases[1].hw, [0xee, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn newest_lease_wins_and_zeros_dont_matter() {
        // VZ writes MACs with leading zeros, bootpd without.
        assert_eq!(ip_for(SAMPLE, "A2:B3:04:05:06:07"), Some(Ipv4Addr::new(192, 168, 64, 9)));
        assert_eq!(ip_for(SAMPLE, "ee:01:02:03:04:05"), Some(Ipv4Addr::new(192, 168, 64, 2)));
        assert_eq!(ip_for(SAMPLE, "01:02:03:04:05:06"), None);
        assert_eq!(ip_for(SAMPLE, "nonsense"), None);
        assert_eq!(ip_for("", "a2:b3:04:05:06:07"), None);
    }

    #[test]
    fn macs() {
        assert_eq!(parse_mac("a2:b3:4:5:6:7"), Some([0xa2, 0xb3, 4, 5, 6, 7]));
        assert_eq!(parse_mac("a2:b3:4:5:6"), None);
        assert_eq!(parse_mac("a2:b3:4:5:6:7:8"), None);
        assert_eq!(parse_mac("a2:b3:4:5:6:777"), None);
        assert_eq!(parse_mac("a2::4:5:6:7"), None);
    }
}
