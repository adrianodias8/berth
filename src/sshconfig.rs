//! `ssh berth` and `ssh <machine>.berth` from the Mac, and so VS Code's
//! Remote-SSH: ~/.berth/ssh/config, written on each start and included from
//! the top of ~/.ssh/config (an Include further down would only apply
//! inside the Host block before it).

use std::path::Path;

use anyhow::Result;

use crate::paths::Paths;

const INCLUDE: &str = "Include ~/.berth/ssh/config";

/// Write berth's SSH config; returns true if it added the Include.
pub fn install(paths: &Paths) -> Result<bool> {
    let user = std::env::var("USER")?;
    let key = paths.ssh_key();
    let berth = std::env::current_exe()?;
    let common = format!(
        "  User {user}\n  IdentityFile \"{}\"\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  LogLevel ERROR\n",
        key.display()
    );
    let config = format!(
        "# Written by berth on each start; put your own settings in ~/.ssh/config.\n\
         Host berth\n  HostName berth\n  ProxyCommand nc -U \"{}\"\n{common}\n\
         # Linux machines: ssh <machine>.berth\n\
         Host *.berth\n  ProxyCommand \"{}\" machine proxy %h\n{common}",
        paths.ssh_socket().display(),
        berth.display()
    );
    let ours = paths.ssh().join("config");
    if std::fs::read_to_string(&ours).ok().as_deref() != Some(config.as_str()) {
        write_private(&ours, &config)?;
    }
    let dir = paths.home.join(".ssh");
    let user_config = dir.join("config");
    let existing = std::fs::read_to_string(&user_config).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == INCLUDE) {
        return Ok(false);
    }
    if !dir.exists() {
        std::fs::create_dir(&dir)?;
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    }
    write_private(&user_config, &format!("{INCLUDE}\n\n{existing}"))?;
    Ok(true)
}

/// Replace a file atomically, readable only by the user.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("berth-tmp");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
