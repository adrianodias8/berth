//! `berth machine create --gui [omarchy]`: what a machine needs for GUI
//! sessions, from its own distro's packages, and the GPU's render node.
//!
//! Sessions (mod.rs) need a headless wlroots compositor (sway; cage for
//! single-app sessions), grim for screenshots, wtype for the keyboard, a
//! session D-Bus and at-spi2-core for the accessibility tree, read with
//! python3-gi. Xwayland runs X11 apps (Electron's default).
//!
//! `--gui omarchy` (Arch only) adds Hyprland from Arch's repositories and
//! Omarchy's configuration from its git tag `OMARCHY_VERSION`, best effort:
//! Omarchy is a whole distribution installed from its own ISO, so its full
//! app list and its own package repository are left out, and a failure there
//! doesn't fail the machine. Its log is /var/log/berth-omarchy.log inside.

use std::io;

/// The Omarchy release berth sets up (a tag of github.com/basecamp/omarchy).
pub const OMARCHY_VERSION: &str = "v4.0.6";
const OMARCHY_REPO: &str = "https://github.com/basecamp/omarchy.git";

/// The render node a GPU-backed VM has; machines get it bound in when it
/// exists in the VM.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";

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
/// moved between releases).
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
        ],
    }
}

/// Omarchy's desktop core that Arch's own repositories carry (the rest of
/// its list is apps, or comes from Omarchy's repository).
const OMARCHY_CORE: &[&str] = &[
    "hyprland",
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

/// Omarchy's configuration for `user`, from its pinned tag, without
/// overwriting the user's own files. Best effort, logged inside.
pub fn omarchy_script(user: &str) -> String {
    format!(
        "export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin; \
         {{ {} && h=$(getent passwd {user} | cut -d: -f6) && \
           rm -rf \"$h/.local/share/omarchy\" && \
           su {user} -c 'git -c advice.detachedHead=false clone -q --depth 1 --branch {OMARCHY_VERSION} {OMARCHY_REPO} ~/.local/share/omarchy && \
             mkdir -p ~/.config && cp -rn ~/.local/share/omarchy/config/. ~/.config/' && \
           echo \"omarchy {OMARCHY_VERSION} configured for {user}\"; }} >>/var/log/berth-omarchy.log 2>&1 || \
         echo 'berth: Omarchy setup failed; see /var/log/berth-omarchy.log in the machine' >&2; true",
        install_script(Family::Arch, OMARCHY_CORE).replacen("set -e; ", "", 1)
    )
}

/// The .nspawn lines and the unit drop-in that give a machine the render
/// node: nspawn binds it in, and systemd's device policy for the machine's
/// unit must allow it (DeviceAllow= is a unit setting, not an .nspawn one).
pub fn gpu_binds(present: bool) -> Option<(String, String)> {
    present.then(|| {
        (
            format!("Bind={RENDER_NODE}\n"),
            format!("# berth: the VM's GPU for GUI sessions (agent/src/gui/install.rs).\n[Service]\nDeviceAllow={RENDER_NODE} rw\n"),
        )
    })
}

/// Where that drop-in goes for machine `name`.
pub fn gpu_dropin_path(name: &str) -> String {
    format!("/etc/systemd/system/systemd-nspawn@{name}.service.d/berth-gpu.conf")
}

/// Check the machine's os-release (read by the caller inside its root) and
/// return the root scripts that set it up for `kind` ("1" or "omarchy").
pub fn scripts(os_release: &str, kind: &str, user: &str) -> io::Result<Vec<String>> {
    let family = family(os_release).ok_or_else(|| io::Error::other("--gui supports Debian, Ubuntu, Fedora and Arch Linux machines"))?;
    let mut out = vec![install_script(family, packages(family))];
    match kind {
        "" | "1" | "true" => {}
        "omarchy" if family == Family::Arch => out.push(omarchy_script(user)),
        "omarchy" => return Err(io::Error::other("--gui omarchy needs an Arch Linux machine (--distro archlinux)")),
        other => return Err(io::Error::other(format!("--gui {other}: plain --gui, or --gui omarchy"))),
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
    fn gpu_lines_only_with_a_device() {
        assert!(gpu_binds(false).is_none());
        let (nspawn, dropin) = gpu_binds(true).unwrap();
        assert_eq!(nspawn, "Bind=/dev/dri/renderD128\n");
        assert!(dropin.contains("DeviceAllow=/dev/dri/renderD128 rw"));
    }
}
