use std::path::PathBuf;

/// Filesystem layout under `~/.berth`.
pub struct Paths {
    pub home: PathBuf,
    pub root: PathBuf,
}

impl Paths {
    pub fn new() -> Self {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME is not set"));
        let root = home.join(".berth");
        Self { home, root }
    }

    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }
    pub fn data(&self) -> PathBuf {
        self.root.join("data")
    }
    pub fn run(&self) -> PathBuf {
        self.root.join("run")
    }
    pub fn log(&self) -> PathBuf {
        self.root.join("log")
    }
    pub fn ssh(&self) -> PathBuf {
        self.root.join("ssh")
    }
    /// Files the guest reads over the shared home directory.
    pub fn guest(&self) -> PathBuf {
        self.root.join("guest")
    }
    pub fn agent(&self) -> PathBuf {
        self.guest().join("berth-agent")
    }

    /// User settings (`cpus`, `memory_mib`), read at VM start.
    pub fn config(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// The VM disk: `disk.img` (ASIF on macOS 26+, see provision.rs), or a
    /// `disk.raw` from before that.
    pub fn disk(&self) -> PathBuf {
        let (img, raw) = (self.data().join("disk.img"), self.data().join("disk.raw"));
        if raw.exists() && !img.exists() { raw } else { img }
    }
    pub fn seed_iso(&self) -> PathBuf {
        self.data().join("seed.iso")
    }
    pub fn efi_vars(&self) -> PathBuf {
        self.data().join("efi-vars.fd")
    }
    pub fn machine_id(&self) -> PathBuf {
        self.data().join("machine-id")
    }
    pub fn mac_address(&self) -> PathBuf {
        self.data().join("mac-address")
    }

    pub fn control_socket(&self) -> PathBuf {
        self.run().join("berth.sock")
    }
    pub fn docker_socket(&self) -> PathBuf {
        self.run().join("docker.sock")
    }
    pub fn podman_socket(&self) -> PathBuf {
        self.run().join("podman.sock")
    }
    pub fn ssh_socket(&self) -> PathBuf {
        self.run().join("ssh.sock")
    }
    /// `berth machine proxy` → a machine's sshd (macexec.rs).
    pub fn machine_ssh_socket(&self) -> PathBuf {
        self.run().join("machine-ssh.sock")
    }
    pub fn pid_file(&self) -> PathBuf {
        self.run().join("berthd.pid")
    }

    pub fn console_log(&self) -> PathBuf {
        self.log().join("console.log")
    }
    pub fn daemon_log(&self) -> PathBuf {
        self.log().join("daemon.log")
    }

    pub fn ssh_key(&self) -> PathBuf {
        self.ssh().join("id_ed25519")
    }

    pub fn create_dirs(&self) -> std::io::Result<()> {
        for dir in [self.cache(), self.data(), self.run(), self.log(), self.ssh(), self.guest()] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}
