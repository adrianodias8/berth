//! Keep the guest on Docker's own engine packages (docker-ce) rather than
//! Debian's older docker.io. Runs at agent start: fresh VMs get docker-ce on
//! first boot, VMs provisioned with docker.io are migrated once. Images,
//! containers and volumes in /var/lib/docker are kept, as is daemon.json.

use std::process::Command;

const SCRIPT: &str = r#"
set -e
export DEBIAN_FRONTEND=noninteractive
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
. /etc/os-release
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian $VERSION_CODENAME stable" > /etc/apt/sources.list.d/docker.list
apt-get update -q
# docker.io conflicts with docker-ce; removing it keeps /var/lib/docker.
apt-get remove -y -q docker.io docker-cli docker-buildx containerd runc || true
# confnew: Debian's containerd leaves a 1.x config.toml behind that
# containerd 2 can't load. daemon.json is not a package conffile, so ours stays.
opts="-o Dpkg::Options::=--force-confnew"
apt-get install -y -q $opts docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin \
    || { dpkg --force-confnew --configure -a && apt-get install -y -q $opts -f; }
systemctl enable --now docker
"#;

fn installed(package: &str) -> bool {
    Command::new("dpkg-query")
        .args(["-W", "-f=${Status}", package])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("install ok installed"))
}

pub fn ensure_docker_ce() {
    if installed("docker-ce") {
        return;
    }
    eprintln!("berth-agent: engine: installing Docker Engine (docker-ce)");
    match Command::new("sh").args(["-c", SCRIPT]).output() {
        Ok(out) if out.status.success() => eprintln!("berth-agent: engine: docker-ce installed"),
        Ok(out) => eprintln!(
            "berth-agent: engine: install failed: {}",
            String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("")
        ),
        Err(err) => eprintln!("berth-agent: engine: {err}"),
    }
}
