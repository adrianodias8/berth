//! `berth config`: read and change ~/.berth/config.json (docs/configuration.md)
//! with each key checked. Keys it doesn't know are kept as they are.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::paths::Paths;

/// Name, what it takes, and its default (as `berth config list` shows it).
const KEYS: [(&str, &str, &str); 16] = [
    ("cpus", "a number of CPUs", "every core"),
    ("memory_mib", "MiB, at least 1024", "half the Mac's memory, 2-16 GiB"),
    ("file_sharing", "berth or apple", "berth (macOS 27)"),
    ("ipv6", "true or false", "false"),
    ("stop", "shutdown or suspend", "shutdown"),
    ("finder", "true or false", "true"),
    ("proxy", "auto, none or a URL (http://host:port)", "auto"),
    ("ports", "all or localhost (published ports only on the Mac's loopback)", "all"),
    ("kernel", "berth or debian (the VM's own kernel)", "berth"),
    ("image_store", "classic or containerd (multi-platform images, slower builds)", "classic"),
    ("mac", "true or false (experimental: macOS VMs, berth mac)", "false"),
    ("gui", "true or false (experimental: Linux GUI machines and the VM's display)", "false"),
    ("gpu_memory_mib", "MiB, 16-16384 (the display's shared memory)", "256"),
    ("gpu_display", "WIDTHxHEIGHT, 640x480 to 7680x4320", "1920x1080"),
    ("gui_idle_minutes", "minutes, 0 for never (berth gui sessions)", "30"),
    ("gui_titles", "true or false (show Claude Code chat titles in the activity log)", "false"),
];

fn parse(key: &str, value: &str) -> Result<Value> {
    let boolean = || match value {
        "true" | "on" | "yes" => Ok(Value::Bool(true)),
        "false" | "off" | "no" => Ok(Value::Bool(false)),
        _ => bail!("{key} is true or false"),
    };
    Ok(match key {
        "cpus" => match value.parse::<u64>() {
            Ok(n) if n >= 1 => Value::from(n),
            _ => bail!("cpus is a number, at least 1"),
        },
        "memory_mib" => match value.parse::<u64>() {
            Ok(n) if (1024..=1 << 22).contains(&n) => Value::from(n),
            _ => bail!("memory_mib is MiB, at least 1024 (e.g. 8192 for 8 GiB)"),
        },
        "file_sharing" if matches!(value, "berth" | "apple") => Value::from(value),
        "stop" if matches!(value, "shutdown" | "suspend") => Value::from(value),
        "ports" if matches!(value, "all" | "localhost") => Value::from(value),
        "kernel" if matches!(value, "berth" | "debian") => Value::from(value),
        "image_store" if matches!(value, "classic" | "containerd") => Value::from(value),
        "proxy" if matches!(value, "auto" | "none") || value.starts_with("http://") || value.starts_with("https://") => {
            Value::from(value)
        }
        "gpu_memory_mib" => match value.parse::<u64>() {
            Ok(n) if (16..=16384).contains(&n) => Value::from(n),
            _ => bail!("gpu_memory_mib is MiB, 16 to 16384"),
        },
        "gpu_display" => match value.split_once(['x', 'X']).map(|(w, h)| (w.parse::<u32>(), h.parse::<u32>())) {
            Some((Ok(w), Ok(h))) if (640..=7680).contains(&w) && (480..=4320).contains(&h) => Value::from(format!("{w}x{h}")),
            _ => bail!("gpu_display is WIDTHxHEIGHT, 640x480 to 7680x4320"),
        },
        "gui_idle_minutes" => match value.parse::<u64>() {
            Ok(n) => Value::from(n),
            _ => bail!("gui_idle_minutes is a number of minutes (0: never)"),
        },
        "ipv6" | "finder" | "mac" | "gui" | "gui_titles" => boolean()?,
        _ => match KEYS.iter().find(|(k, _, _)| *k == key) {
            Some((_, takes, _)) => bail!("{key} takes {takes}"),
            None => bail!("unknown key {key}; keys: {}", KEYS.map(|k| k.0).join(", ")),
        },
    })
}

fn load(paths: &Paths) -> Result<Map<String, Value>> {
    match std::fs::read(paths.config()) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("{} isn't valid JSON", paths.config().display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e.into()),
    }
}

fn save(paths: &Paths, config: &Map<String, Value>) -> Result<()> {
    std::fs::create_dir_all(&paths.root)?;
    let tmp = paths.config().with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(config)? + "\n")?;
    std::fs::rename(&tmp, paths.config())?;
    Ok(())
}

/// One setting as stored, for the modules that read their own keys
/// (macOS VMs, berth gui, the GPU). Missing or unreadable is None.
pub fn value(paths: &Paths, key: &str) -> Option<Value> {
    load(paths).ok()?.remove(key)
}

/// A true/false setting; only a JSON `true` is on (the app reads it the
/// same way), so a missing, unreadable or mistyped one is off.
pub fn flag(paths: &Paths, key: &str) -> bool {
    value(paths, key).and_then(|v| v.as_bool()).unwrap_or(false)
}

pub fn list(paths: &Paths) -> Result<()> {
    let config = load(paths)?;
    for (key, takes, default) in KEYS {
        match config.get(key) {
            Some(v) => println!("{key:<16} {v}"),
            None => println!("{key:<16} (default: {default}; takes {takes})"),
        }
    }
    for (key, v) in config.iter().filter(|(k, _)| !KEYS.iter().any(|(known, _, _)| known == k)) {
        println!("{key:<16} {v} (not a berth setting)");
    }
    Ok(())
}

pub fn get(paths: &Paths, key: &str) -> Result<()> {
    match load(paths)?.get(key) {
        Some(Value::String(s)) => println!("{s}"),
        Some(v) => println!("{v}"),
        None => match KEYS.iter().find(|(k, _, _)| *k == key) {
            Some((_, _, default)) => println!("(default: {default})"),
            None => bail!("unknown key {key}"),
        },
    }
    Ok(())
}

pub fn set(paths: &Paths, key: &str, value: &str) -> Result<()> {
    let parsed = parse(key, value)?;
    let mut config = load(paths)?;
    config.insert(key.to_string(), parsed);
    save(paths, &config)?;
    println!("{key} = {value}; {}", applies(key));
    Ok(())
}

pub fn unset(paths: &Paths, key: &str) -> Result<()> {
    let mut config = load(paths)?;
    if config.remove(key).is_some() {
        save(paths, &config)?;
        println!("{key} is back to its default; {}", applies(key));
    }
    Ok(())
}

/// ports.rs reads `ports` each time it opens a forward, so it needs no restart.
fn applies(key: &str) -> &'static str {
    match key {
        "ports" => "applies to new forwards right away",
        "mac" | "gui_idle_minutes" | "gui_titles" => "applies right away",
        // Sessions and `machine create --gui` read config.json live; the
        // GPU device is attached at start.
        "gui" => "Linux GUI sessions follow right away; the GPU device on the next start (berth restart)",
        _ => "applies on the next start (berth restart)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_checked() {
        assert_eq!(parse("cpus", "4").unwrap(), Value::from(4));
        assert!(parse("cpus", "0").is_err());
        assert!(parse("memory_mib", "512").is_err());
        assert_eq!(parse("ipv6", "on").unwrap(), Value::Bool(true));
        assert!(parse("stop", "hibernate").is_err());
        assert!(parse("proxy", "socks5://x").is_err());
        assert_eq!(parse("proxy", "http://127.0.0.1:3128").unwrap(), Value::from("http://127.0.0.1:3128"));
        assert!(parse("colour", "blue").unwrap_err().to_string().contains("unknown key"));
        assert_eq!(parse("mac", "true").unwrap(), Value::Bool(true));
        assert_eq!(parse("gui", "off").unwrap(), Value::Bool(false));
        assert_eq!(parse("gpu_memory_mib", "512").unwrap(), Value::from(512));
        assert!(parse("gpu_memory_mib", "8").is_err());
        assert_eq!(parse("gpu_display", "2560x1440").unwrap(), Value::from("2560x1440"));
        assert!(parse("gpu_display", "100x100").is_err());
        assert_eq!(parse("gui_idle_minutes", "0").unwrap(), Value::from(0));
        assert!(parse("gui_titles", "maybe").is_err());
    }

    #[test]
    fn flags_are_only_a_json_true() {
        let root = std::env::temp_dir().join(format!("berth-config-flags-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let paths = Paths { home: root.clone(), root: root.clone() };
        assert!(!flag(&paths, "mac"));
        std::fs::write(paths.config(), r#"{"mac": true, "gui": "true", "gui_idle_minutes": 5}"#).unwrap();
        assert!(flag(&paths, "mac"));
        assert!(!flag(&paths, "gui"));
        assert_eq!(value(&paths, "gui_idle_minutes"), Some(Value::from(5)));
        std::fs::remove_dir_all(root).unwrap();
    }
}
