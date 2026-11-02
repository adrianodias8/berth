//! `berth machine create --gui[=omarchy]`: what a machine needs for GUI
//! sessions, from its own distro's packages, and the GPU's device nodes.
//!
//! Sessions (mod.rs) need a headless wlroots compositor (sway; cage for
//! single-app sessions), grim for screenshots, wtype for the keyboard, a
//! session D-Bus and at-spi2-core for the accessibility tree, read with
//! python3-gi. Xwayland runs X11 apps (Electron's default). Mesa is named
//! explicitly, since `--no-install-recommends` would leave its drivers out,
//! with seatd for desktop sessions (a compositor driving the VM's display,
//! card0, as its DRM master) and a few GL clients to try the GPU with.
//!
//! `--gui=omarchy` (Arch only) adds Hyprland from Arch's repositories and
//! Omarchy's configuration from its git tag `OMARCHY_VERSION`, best effort:
//! Omarchy is a whole distribution installed from its own ISO, so its full
//! app list and its own package repository are left out, and a failure there
//! doesn't fail the machine. Its log is /var/log/berth-omarchy.log inside.

use std::io;

/// The Omarchy release berth sets up (a tag of github.com/basecamp/omarchy).
pub const OMARCHY_VERSION: &str = "v4.0.6";
/// The commit that tag names, checked after the clone: a moved tag fails
/// the Omarchy step instead of installing something unreviewed.
pub const OMARCHY_COMMIT: &str = "237f1b4071a0bff2296c6d740ba2b9fd695ebfa0";
const OMARCHY_REPO: &str = "https://github.com/basecamp/omarchy.git";

/// The render node a GPU-backed VM has; machines get it bound in when it
/// exists in the VM.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";
/// The VM's display, the virtio-gpu's KMS device. Every GUI machine gets it
/// when the VM has it, so a desktop session works off Omarchy too.
pub const CARD_NODE: &str = "/dev/dri/card0";
/// Where seatd listens in a machine (its default, and libseat's).
pub const SEATD_SOCK: &str = "/run/seatd.sock";

/// Whether the VM has its display device.
pub fn card_node_present() -> bool {
    std::path::Path::new(CARD_NODE).exists()
}

/// Whether the VM's GPU does 3D (virgl). A 2D-only virtio-gpu can still
/// have a render node, and wlroots' gles2 renderer fails on it, so the
/// compositor uses pixman unless the kernel says 3D is there
/// (DRM_IOCTL_VIRTGPU_GETPARAM, VIRTGPU_PARAM_3D_FEATURES).
pub fn gpu_has_3d() -> bool {
    use std::os::fd::AsRawFd;
    let Ok(node) = std::fs::OpenOptions::new().read(true).write(true).open(RENDER_NODE) else { return false };
    #[repr(C)]
    struct GetParam {
        param: u64,
        value: u64,
    }
    let mut value: libc::c_int = 0;
    let mut req = GetParam { param: 1, value: &mut value as *mut libc::c_int as u64 };
    // _IOWR('d', 0x40 + 0x03, struct drm_virtgpu_getparam)
    const GETPARAM: libc::c_ulong = 0xC010_6443;
    let rc = unsafe { libc::ioctl(node.as_raw_fd(), GETPARAM as _, &mut req) };
    rc == 0 && value != 0
}

/// The machine's package manager, from its os-release ID and ID_LIKE.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Family {
    Debian,
    Fedora,
    Arch,
}

pub fn family(os_release: &str) -> Option<Family> {
    let field = |key: &str| {
        os_release
            .lines()
            .find_map(|l| l.strip_prefix(key).map(|v| v.trim_matches('"').to_lowercase()))
            .unwrap_or_default()
    };
    let ids = format!("{} {}", field("ID="), field("ID_LIKE="));
    let has = |id: &str| ids.split_whitespace().any(|w| w == id);
    if has("debian") || has("ubuntu") {
        Some(Family::Debian)
    } else if has("fedora") {
        Some(Family::Fedora)
    } else if has("arch") || has("archarm") {
        Some(Family::Arch)
    } else {
        None
    }
}

/// Packages for GUI sessions. `a|b` picks the first the distro has (names
/// moved between releases); a name the distro has none of is skipped with a
/// warning, so a wrong one costs only that package. The ones marked
/// unverified haven't been installed on every release and arch yet.
pub fn packages(family: Family) -> &'static [&'static str] {
    match family {
        Family::Debian => &[
            "sway",
            "cage",
            "grim",
            "wtype",
            "dbus-daemon|dbus",
            "dbus-bin|dbus",
            "at-spi2-core",
            "libatk-adaptor",
            "python3-gi",
            "gir1.2-atspi-2.0",
            "xwayland",
            "fonts-dejavu-core",
            "foot",
            "libegl-mesa0",
            "libgbm1",
            "libgles2",
            // Debian 13 moved the Gallium drivers to mesa-libgallium and
            // kept libgl1-mesa-dri for the DRI loader (unverified on noble).
            "libgl1-mesa-dri|mesa-libgallium",
            "seatd",
            "mesa-utils",
            "wayland-utils",
            "weston",
            "glmark2-es2-wayland", // unverified on arm64
            "glmark2-es2-drm",     // unverified on arm64
        ],
        Family::Fedora => &[
            "sway",
            "cage",
            "grim",
            "wtype",
            "dbus-daemon",
            "at-spi2-core",
            "at-spi2-atk",
            "python3-gobject",
            "xorg-x11-server-Xwayland",
            "dejavu-sans-fonts",
            "foot",
            "mesa-dri-drivers",
            "mesa-libEGL",
            "mesa-libgbm",
            "seatd",
            "mesa-demos",
            "weston",
            "glmark2", // unverified on aarch64
        ],
        Family::Arch => &[
            "sway",
            "cage",
            "grim",
            "wtype",
            "dbus",
            "at-spi2-core",
            "python-gobject",
            "xorg-xwayland",
            "ttf-dejavu",
            "foot",
            "mesa",
            "mesa-utils",
            "wayland-utils",
            "seatd",
            "weston",
            "glmark2", // unverified on Arch Linux ARM
        ],
    }
}

/// Omarchy's desktop core that Arch's own repositories carry (the rest of
/// its list is apps, or comes from Omarchy's repository).
const OMARCHY_CORE: &[&str] = &[
    "hyprland",
    "seatd",
    "hyprpicker",
    "hyprsunset",
    "uwsm",
    "quickshell",
    "xdg-desktop-portal-hyprland",
    "xdg-desktop-portal-gtk",
    "wl-clipboard",
    "slurp",
    "gnome-themes-extra",
    "yaru-icon-theme",
    "noto-fonts",
    "noto-fonts-emoji",
    "ttf-jetbrains-mono-nerd",
    "git",
];

/// The root script that installs `packages` with the family's manager,
/// skipping names it doesn't have (and saying so).
pub fn install_script(family: Family, packages: &[&str]) -> String {
    let (refresh, has, install) = match family {
        Family::Debian => (
            "export DEBIAN_FRONTEND=noninteractive; apt-get update -qq",
            "apt-cache show \"$p\" >/dev/null 2>&1",
            "apt-get install -y -qq --no-install-recommends",
        ),
        Family::Fedora => ("dnf makecache -q >/dev/null", "dnf info -q \"$p\" >/dev/null 2>&1", "dnf install -y -q"),
        Family::Arch => ("pacman -Sy --noconfirm >/dev/null", "pacman -Si \"$p\" >/dev/null 2>&1", "pacman -S --noconfirm --needed"),
    };
    // Quoted: "a|b" would be a pipe.
    let wanted = packages.iter().map(|p| format!("'{p}'")).collect::<Vec<_>>().join(" ");
    format!(
        "set -e; export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin; {refresh}; list=; \
         for choice in {wanted}; do found=; \
           for p in $(echo \"$choice\" | tr '|' ' '); do if {has}; then found=$p; break; fi; done; \
           if [ -n \"$found\" ]; then case \" $list \" in *\" $found \"*) ;; *) list=\"$list $found\";; esac; \
           else echo \"berth: no package $choice here, skipped\" >&2; fi; \
         done; \
         {install} $list >/dev/null"
    )
}

/// Omarchy's configuration for `user`, from its pinned tag and commit, without
/// overwriting the user's own files. Best effort, logged inside.
pub fn omarchy_script(user: &str) -> String {
    format!(
        "export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin; \
         {{ {} && h=$(getent passwd {user} | cut -d: -f6) && \
           rm -rf \"$h/.local/share/omarchy\" && \
           su {user} -c 'git -c advice.detachedHead=false clone -q --depth 1 --branch {OMARCHY_VERSION} {OMARCHY_REPO} ~/.local/share/omarchy && \
             {{ [ \"$(git -C ~/.local/share/omarchy rev-parse HEAD)\" = {OMARCHY_COMMIT} ] || \
               {{ echo \"omarchy {OMARCHY_VERSION} is not commit {OMARCHY_COMMIT}\"; rm -rf ~/.local/share/omarchy; false; }}; }} && \
             mkdir -p ~/.config && cp -rn ~/.local/share/omarchy/config/. ~/.config/' && \
           echo \"omarchy {OMARCHY_VERSION} configured for {user}\"; }} >>/var/log/berth-omarchy.log 2>&1 || \
         echo 'berth: Omarchy setup failed; see /var/log/berth-omarchy.log in the machine' >&2; true",
        install_script(Family::Arch, OMARCHY_CORE).replacen("set -e; ", "", 1)
    )
}

/// The VM's GPU nodes that exist.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct GpuNodes {
    pub render: bool,
    pub card: bool,
}

impl GpuNodes {
    pub fn present() -> GpuNodes {
        GpuNodes { render: std::path::Path::new(RENDER_NODE).exists(), card: card_node_present() }
    }

    fn paths(&self) -> Vec<&'static str> {
        [(self.render, RENDER_NODE), (self.card, CARD_NODE)].into_iter().filter(|(on, _)| *on).map(|(_, p)| p).collect()
    }
}

/// Whether an .nspawn line is one of `gpu_binds`' lines, for a node that
/// exists or not.
pub fn is_gpu_bind(line: &str) -> bool {
    line.strip_prefix("Bind=").is_some_and(|p| p == RENDER_NODE || p == CARD_NODE)
}

/// The .nspawn lines and the unit drop-in that give a machine the GPU's
/// nodes: nspawn binds them in, and systemd's device policy for the
/// machine's unit must allow them (DeviceAllow= is a unit setting, not an
/// .nspawn one). None when the VM has neither.
pub fn gpu_binds(nodes: &GpuNodes) -> Option<(String, String)> {
    let paths = nodes.paths();
    (!paths.is_empty()).then(|| {
        (
            paths.iter().map(|p| format!("Bind={p}\n")).collect(),
            format!(
                "# berth: the VM's GPU for GUI sessions (agent/src/gui/install.rs).\n[Service]\n{}",
                paths.iter().map(|p| format!("DeviceAllow={p} rw\n")).collect::<String>()
            ),
        )
    })
}

/// Where that drop-in goes for machine `name`.
pub fn gpu_dropin_path(name: &str) -> String {
    format!("/etc/systemd/system/systemd-nspawn@{name}.service.d/berth-gpu.conf")
}

/// The compositor of a desktop session: sway, or Hyprland (Omarchy's).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Flavour {
    Sway,
    Hyprland,
}

impl Flavour {
    pub fn parse(s: &str) -> Option<Flavour> {
        match s {
            "sway" => Some(Flavour::Sway),
            "hyprland" => Some(Flavour::Hyprland),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Flavour::Sway => "sway",
            Flavour::Hyprland => "hyprland",
        }
    }
}

/// A user script, run in the machine, saying what each desktop flavour
/// lacks: a `desktop <flavour> ok` or `desktop <flavour> missing <what>…`
/// line for each, then `uwsm` when Omarchy's session manager can start it.
///
/// Hyprland needs its binary, hyprctl and a ~/.config/hypr (Omarchy's, or
/// the user's own); sway needs sway and swaymsg; both need seatd. uwsm
/// runs the compositor as a unit of the user's systemd manager, so it
/// counts only when Omarchy is installed and that manager is running
/// (a lingering user, say); otherwise Hyprland starts directly.
pub fn desktop_probe() -> String {
    let check = |flavour: &str, tools: &str, dir: Option<&str>| {
        format!(
            "m=; for t in {tools} seatd; do command -v $t >/dev/null 2>&1 || m=\"$m $t\"; done; {}\
             if [ -n \"$m\" ]; then echo \"desktop {flavour} missing$m\"; else echo 'desktop {flavour} ok'; fi; ",
            dir.map(|d| format!("[ -d \"$HOME/{d}\" ] || m=\"$m ~/{d}\"; ")).unwrap_or_default()
        )
    };
    format!(
        "{}{}if [ -d \"$HOME/.local/share/omarchy\" ] && command -v uwsm >/dev/null 2>&1 && [ -S \"/run/user/$(id -u)/systemd/private\" ]; then echo uwsm; fi; true",
        check("hyprland", "Hyprland hyprctl", Some(".config/hypr")),
        check("sway", "sway swaymsg", None)
    )
}

/// The desktop a session gets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Desktop {
    pub flavour: Flavour,
    /// Start Hyprland through uwsm (Omarchy's way).
    pub uwsm: bool,
}

/// Pick the desktop from `desktop_probe`'s output: the flavour asked for if
/// it's ready, or with none asked, Hyprland when it's ready, else sway.
pub fn desktop_ready(requested: Option<Flavour>, probe: &str) -> Result<Desktop, String> {
    let missing = |flavour: Flavour| -> Option<String> {
        let line = probe.lines().find_map(|l| l.strip_prefix(&format!("desktop {} ", flavour.as_str())))?;
        if line.trim() == "ok" { None } else { Some(line.trim().strip_prefix("missing").unwrap_or(line).trim().to_string()) }
    };
    let status = |flavour: Flavour| -> Result<Flavour, String> {
        match missing(flavour) {
            None if probe.lines().any(|l| l.starts_with(&format!("desktop {} ", flavour.as_str()))) => Ok(flavour),
            None => Err(format!("{}: not checked", flavour.as_str())),
            Some(what) => Err(format!("{} needs {what}", flavour.as_str())),
        }
    };
    let flavour = match requested {
        Some(f) => status(f).map_err(|e| format!("no {} desktop in this machine: {e}", f.as_str()))?,
        None => status(Flavour::Hyprland).or_else(|hypr| {
            status(Flavour::Sway).map_err(|sway| format!("no desktop in this machine: {hypr}; {sway}"))
        })?,
    };
    let uwsm = flavour == Flavour::Hyprland && probe.lines().any(|l| l.trim() == "uwsm");
    Ok(Desktop { flavour, uwsm })
}

/// A root script for a desktop session: the `seat` group with `user` in it
/// (and in `video`, the group Debian's own seatd service uses), then
/// `running` on stdout when a seatd already serves the machine. A stale
/// socket is removed.
pub fn seat_script(user: &str) -> String {
    format!(
        "export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin; \
         getent group seat >/dev/null || groupadd -f seat 2>/dev/null || addgroup -S seat 2>/dev/null || true; \
         for g in seat video; do getent group $g >/dev/null && {{ usermod -aG $g {user} 2>/dev/null || addgroup {user} $g 2>/dev/null || true; }}; done; \
         for p in /proc/[0-9]*; do if [ \"$(cat $p/comm 2>/dev/null)\" = seatd ] && [ -S {SEATD_SOCK} ]; then echo running; exit 0; fi; done; \
         rm -f {SEATD_SOCK}; true"
    )
}

/// seatd for the machine, as root: not tied to a VT (the VM has none in the
/// machine), its socket the user's and the seat group's.
pub fn seatd_script(user: &str) -> String {
    format!(
        "export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin SEATD_VTBOUND=0; \
         if getent group seat >/dev/null; then exec seatd -u {user} -g seat; else exec seatd -u {user}; fi"
    )
}

/// Check the machine's os-release (read by the caller inside its root) and
/// return the root scripts that set it up for `kind` ("1" or "omarchy").
pub fn scripts(os_release: &str, kind: &str, user: &str) -> io::Result<Vec<String>> {
    let family = family(os_release).ok_or_else(|| io::Error::other("--gui supports Debian, Ubuntu, Fedora and Arch Linux machines"))?;
    let mut out = vec![install_script(family, packages(family))];
    match kind {
        "" | "1" | "true" => {}
        "omarchy" if family == Family::Arch => out.push(omarchy_script(user)),
        "omarchy" => return Err(io::Error::other("--gui=omarchy needs an Arch Linux machine (--distro archlinux)")),
        other => return Err(io::Error::other(format!("--gui={other}: plain --gui, or --gui=omarchy"))),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families() {
        assert_eq!(family("ID=debian\nVERSION_ID=\"13\""), Some(Family::Debian));
        assert_eq!(family("ID=ubuntu\nID_LIKE=debian"), Some(Family::Debian));
        assert_eq!(family("ID=linuxmint\nID_LIKE=\"ubuntu debian\""), Some(Family::Debian));
        assert_eq!(family("ID=fedora"), Some(Family::Fedora));
        assert_eq!(family("ID=archarm\nID_LIKE=arch"), Some(Family::Arch));
        assert_eq!(family("ID=arch"), Some(Family::Arch));
        assert_eq!(family("ID=alpine"), None);
    }

    #[test]
    fn scripts_per_kind() {
        assert_eq!(scripts("ID=debian", "1", "dev").unwrap().len(), 1);
        assert_eq!(scripts("ID=arch", "omarchy", "dev").unwrap().len(), 2);
        assert!(scripts("ID=debian", "omarchy", "dev").unwrap_err().to_string().contains("Arch"));
        assert!(scripts("ID=alpine", "1", "dev").is_err());
        assert!(scripts("ID=fedora", "kde", "dev").is_err());
        assert!(omarchy_script("dev").contains("--branch v4.0.6"));
        assert!(omarchy_script("dev").contains("'seatd'"));
    }

    #[test]
    fn packages_have_mesa_and_seatd() {
        for (family, mesa) in [(Family::Debian, "libegl-mesa0"), (Family::Fedora, "mesa-dri-drivers"), (Family::Arch, "mesa")] {
            let list = packages(family);
            assert!(list.contains(&mesa), "{family:?}");
            assert!(list.contains(&"seatd"), "{family:?}");
            assert!(list.contains(&"weston"), "{family:?}");
        }
        assert!(packages(Family::Debian).contains(&"libgl1-mesa-dri|mesa-libgallium"));
    }

    #[test]
    fn install_script_picks_alternatives() {
        // Stub the package manager: it "has" only sway and dbus.
        let script = install_script(Family::Debian, &["sway", "dbus-daemon|dbus", "nothing-here"])
            .replace("apt-get update -qq", "true")
            .replace("apt-cache show \"$p\" >/dev/null 2>&1", "case $p in sway|dbus) true;; *) false;; esac")
            .replace("apt-get install -y -qq --no-install-recommends", "echo installing")
            .replace(" $list >/dev/null", " $list");
        let out = std::process::Command::new("/bin/sh").args(["-c", &script]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "installing sway dbus");
        assert!(String::from_utf8_lossy(&out.stderr).contains("no package nothing-here"));
    }

    #[test]
    fn gpu_lines_per_node() {
        assert!(gpu_binds(&GpuNodes::default()).is_none());
        let (nspawn, dropin) = gpu_binds(&GpuNodes { render: true, card: false }).unwrap();
        assert_eq!(nspawn, "Bind=/dev/dri/renderD128\n");
        assert_eq!(dropin, "# berth: the VM's GPU for GUI sessions (agent/src/gui/install.rs).\n[Service]\nDeviceAllow=/dev/dri/renderD128 rw\n");
        let (nspawn, dropin) = gpu_binds(&GpuNodes { render: true, card: true }).unwrap();
        assert_eq!(nspawn, "Bind=/dev/dri/renderD128\nBind=/dev/dri/card0\n");
        assert_eq!(
            dropin,
            "# berth: the VM's GPU for GUI sessions (agent/src/gui/install.rs).\n[Service]\nDeviceAllow=/dev/dri/renderD128 rw\nDeviceAllow=/dev/dri/card0 rw\n"
        );
        assert!(is_gpu_bind("Bind=/dev/dri/card0") && is_gpu_bind("Bind=/dev/dri/renderD128"));
        assert!(!is_gpu_bind("Bind=/Users/me") && !is_gpu_bind("BindReadOnly=/dev/dri/card0"));
    }

    /// Run `desktop_probe` with a fake home and a PATH of fake tools.
    fn probe_with(tools: &[&str], dirs: &[&str]) -> String {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("berth-probe-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (bin, home) = (root.join("bin"), root.join("home"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        for t in tools {
            let path = bin.join(t);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        for d in dirs {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        // Only the fake tools: `command -v` and `[` are the shell's own.
        let out = std::process::Command::new("/bin/sh").args(["-c", &desktop_probe()]).env("HOME", &home).env("PATH", &bin).output().unwrap();
        let _ = std::fs::remove_dir_all(&root);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn desktop_probe_on_a_fake_home() {
        let none = probe_with(&[], &[]);
        assert!(none.contains("desktop hyprland missing Hyprland hyprctl seatd ~/.config/hypr"), "{none}");
        assert!(none.contains("desktop sway missing sway swaymsg seatd"), "{none}");
        assert!(desktop_ready(None, &none).unwrap_err().contains("hyprland needs Hyprland"));
        assert!(desktop_ready(Some(Flavour::Sway), &none).unwrap_err().contains("no sway desktop"));

        let sway = probe_with(&["sway", "swaymsg", "seatd"], &[]);
        assert_eq!(desktop_ready(None, &sway), Ok(Desktop { flavour: Flavour::Sway, uwsm: false }));
        assert!(desktop_ready(Some(Flavour::Hyprland), &sway).unwrap_err().contains("~/.config/hypr"));

        // Hyprland without its config isn't ready.
        let bare = probe_with(&["Hyprland", "hyprctl", "seatd", "sway", "swaymsg"], &[]);
        assert_eq!(desktop_ready(None, &bare).unwrap().flavour, Flavour::Sway);

        let both = probe_with(&["Hyprland", "hyprctl", "seatd", "sway", "swaymsg", "uwsm"], &[".config/hypr", ".local/share/omarchy"]);
        // No user systemd manager here, so no uwsm.
        assert_eq!(desktop_ready(None, &both), Ok(Desktop { flavour: Flavour::Hyprland, uwsm: false }));
        assert_eq!(desktop_ready(Some(Flavour::Sway), &both).unwrap().flavour, Flavour::Sway);
    }

    #[test]
    fn desktop_picks_uwsm_only_for_hyprland() {
        let probe = "desktop hyprland ok\ndesktop sway ok\nuwsm\n";
        assert_eq!(desktop_ready(None, probe), Ok(Desktop { flavour: Flavour::Hyprland, uwsm: true }));
        assert_eq!(desktop_ready(Some(Flavour::Sway), probe), Ok(Desktop { flavour: Flavour::Sway, uwsm: false }));
        assert!(desktop_ready(None, "").is_err(), "a probe that didn't run isn't ready");
    }

    #[test]
    fn seat_scripts() {
        assert!(seat_script("dev").contains("usermod -aG $g dev"));
        assert!(seat_script("dev").contains("echo running"));
        let seatd = seatd_script("dev");
        assert!(seatd.contains("SEATD_VTBOUND=0") && seatd.contains("exec seatd -u dev -g seat"), "{seatd}");
    }
}
