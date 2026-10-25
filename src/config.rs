//! `berth config`: read and change ~/.berth/config.json (docs/configuration.md)
//! with each key checked. Keys it doesn't know are kept as they are.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::paths::Paths;

/// Name, what it takes, and its default (as `berth config list` shows it).
const KEYS: [(&str, &str, &str); 10] = [
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
        "ipv6" | "finder" => boolean()?,
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

pub fn list(paths: &Paths) -> Result<()> {
    let config = load(paths)?;
    for (key, takes, default) in KEYS {
        match config.get(key) {
            Some(v) => println!("{key:<13} {v}"),
            None => println!("{key:<13} (default: {default}; takes {takes})"),
        }
    }
    for (key, v) in config.iter().filter(|(k, _)| !KEYS.iter().any(|(known, _, _)| known == k)) {
        println!("{key:<13} {v} (not a berth setting)");
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
    println!("{key} = {value}; applies on the next start (berth restart)");
    Ok(())
}

pub fn unset(paths: &Paths, key: &str) -> Result<()> {
    let mut config = load(paths)?;
    if config.remove(key).is_some() {
        save(paths, &config)?;
        println!("{key} is back to its default; applies on the next start (berth restart)");
    }
    Ok(())
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
    }
}
