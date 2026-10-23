//! `berth k8s`: a single-node k3s cluster in the VM (agent/src/k8s.rs) with a
//! `berth` context in ~/.kube/config. The API is forwarded to localhost:6443
//! by the daemon (ports.rs) and pod/service IPs are routed like container IPs.

use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::daemon::Request;
use crate::paths::Paths;

pub const CONTEXT: &str = "berth";
pub const API_PORT: u16 = 6443;

fn agent(paths: &Paths, action: &str) -> Result<String> {
    let reply: crate::daemon::K8sReply = crate::request_with_timeout(
        paths,
        &Request::K8s { action: action.to_string() },
        Duration::from_secs(600),
    )?;
    Ok(reply.output)
}

pub fn status(paths: &Paths) -> Result<String> {
    Ok(agent(paths, "status")?.trim().to_string())
}

pub fn enable(paths: &Paths) -> Result<()> {
    println!("Installing and starting Kubernetes (k3s)...");
    let out = agent(paths, "enable")?;
    if let Some(err) = out.strip_prefix("error: ") {
        bail!("{err}");
    }
    let started = Instant::now();
    while status(paths)? != "running" {
        if started.elapsed() > Duration::from_secs(300) {
            bail!("k3s did not come up within 5 minutes; see `berth shell sudo journalctl -u k3s`");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    write_kubeconfig(paths, &agent(paths, "kubeconfig")?)?;
    // The API forward appears with the next docker event; wait until the
    // node answers through it.
    let kubectl = crate::cli_tools::bin_dir(paths).join("kubectl");
    while !Command::new(&kubectl)
        .args(["--context", CONTEXT, "get", "--raw", "/readyz"])
        .output()
        .is_ok_and(|o| o.status.success())
    {
        if started.elapsed() > Duration::from_secs(300) {
            bail!("the Kubernetes API isn't reachable on localhost:{API_PORT}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("Kubernetes is running. kubectl context '{CONTEXT}' is now current.");
    Ok(())
}

pub fn disable(paths: &Paths) -> Result<()> {
    let out = agent(paths, "disable")?;
    if let Some(err) = out.strip_prefix("error: ") {
        bail!("{err}");
    }
    println!("Kubernetes stopped. (The '{CONTEXT}' kubectl context stays for next time.)");
    Ok(())
}

/// Pull the three credentials out of k3s.yaml and set them with kubectl, which
/// merges into the user's existing kubeconfig rather than replacing it.
fn write_kubeconfig(paths: &Paths, k3s_yaml: &str) -> Result<()> {
    let field = |name: &str| {
        k3s_yaml
            .lines()
            .find_map(|l| l.trim().strip_prefix(name)?.strip_prefix(':').map(|v| v.trim().to_string()))
            .with_context(|| format!("{name} missing from k3s kubeconfig"))
    };
    let (ca, cert, key) =
        (field("certificate-authority-data")?, field("client-certificate-data")?, field("client-key-data")?);
    let kubectl = crate::cli_tools::bin_dir(paths).join("kubectl");
    let server = format!("https://127.0.0.1:{API_PORT}");
    let ca_key = format!("clusters.{CONTEXT}.certificate-authority-data");
    let cert_key = format!("users.{CONTEXT}.client-certificate-data");
    let key_key = format!("users.{CONTEXT}.client-key-data");
    let steps: [Vec<&str>; 7] = [
        vec!["config", "set-cluster", CONTEXT, "--server", &server],
        vec!["config", "set", &ca_key, &ca],
        vec!["config", "set-credentials", CONTEXT],
        vec!["config", "set", &cert_key, &cert],
        vec!["config", "set", &key_key, &key],
        vec!["config", "set-context", CONTEXT, "--cluster", CONTEXT, "--user", CONTEXT],
        vec!["config", "use-context", CONTEXT],
    ];
    for args in steps {
        let out = Command::new(&kubectl).args(&args).output().context("running kubectl")?;
        if !out.status.success() {
            bail!("kubectl {}: {}", args[1], String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    Ok(())
}
