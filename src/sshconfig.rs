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
    let mut user_config = dir.join("config");
    // Only a missing file is empty: one we can't read (not UTF-8, no
    // permission) must not be replaced by just our line.
    let existing = match std::fs::read(&user_config) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => anyhow::bail!("reading ~/.ssh/config: {e}; add `{INCLUDE}` at its top yourself"),
    };
    if String::from_utf8_lossy(&existing).lines().any(|l| l.trim() == INCLUDE) {
        return Ok(false);
    }
    if !dir.exists() {
        std::fs::create_dir(&dir)?;
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    }
    // A link (dotfile managers) is edited where it points, not replaced.
    if std::fs::symlink_metadata(&user_config).is_ok_and(|m| m.file_type().is_symlink()) {
        user_config = std::fs::canonicalize(&user_config)?;
    }
    if !existing.is_empty() {
        let backup = user_config.with_file_name("config.berth-backup");
        if !backup.exists() {
            std::fs::write(&backup, &existing)?;
        }
    }
    let mut contents = format!("{INCLUDE}\n\n").into_bytes();
    contents.extend_from_slice(&existing);
    write_private(&user_config, &contents)?;
    Ok(true)
}

/// Replace a file atomically, readable only by the user.
fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("berth-tmp");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(contents.as_ref())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
