//! Key combos (`cmd+shift+a`, `ctrl+Return`, `f5`) as wtype arguments.
//!
//! The protocol's modifiers are the Mac's: `cmd` is the Super (logo) key on
//! Linux. Keys are X keysym names, which is what wtype's `-k` takes; common
//! spellings and punctuation are mapped to them.

/// wtype's arguments for `combo`: press the modifiers, tap the key, release
/// the modifiers in reverse.
pub fn wtype_args(combo: &str) -> Result<Vec<String>, String> {
    let combo = combo.trim();
    if combo.is_empty() {
        return Err("empty key combo".into());
    }
    // A trailing "+" is the plus key itself ("ctrl++").
    let (mods_part, key) = if let Some(rest) = combo.strip_suffix("++") {
        (rest, "+")
    } else if combo == "+" {
        ("", "+")
    } else {
        match combo.rsplit_once('+') {
            Some((m, k)) => (m, k),
            None => ("", combo),
        }
    };
    let mut mods = Vec::new();
    for m in mods_part.split('+').filter(|m| !m.is_empty()) {
        let name = match m.to_lowercase().as_str() {
            "cmd" | "command" | "super" | "meta" | "win" | "logo" => "logo",
            "ctrl" | "control" => "ctrl",
            "alt" | "option" | "opt" => "alt",
            "shift" => "shift",
            other => return Err(format!("unknown modifier {other:?} (cmd, ctrl, alt, shift)")),
        };
        if !mods.contains(&name) {
            mods.push(name);
        }
    }
    let keysym = keysym(key).ok_or_else(|| format!("unknown key {key:?}"))?;
    let mut args = Vec::new();
    for m in &mods {
        args.extend(["-M".to_string(), m.to_string()]);
    }
    args.extend(["-k".to_string(), keysym]);
    for m in mods.iter().rev() {
        args.extend(["-m".to_string(), m.to_string()]);
    }
    Ok(args)
}

fn keysym(key: &str) -> Option<String> {
    let lower = key.to_lowercase();
    let named = match lower.as_str() {
        "enter" | "return" => "Return",
        "esc" | "escape" => "Escape",
        "tab" => "Tab",
        "space" => "space",
        "backspace" => "BackSpace",
        "delete" | "del" | "forwarddelete" => "Delete",
        "insert" | "ins" => "Insert",
        "up" | "uparrow" => "Up",
        "down" | "downarrow" => "Down",
        "left" | "leftarrow" => "Left",
        "right" | "rightarrow" => "Right",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" => "Page_Up",
        "pagedown" | "pgdn" => "Page_Down",
        "menu" => "Menu",
        "print" | "printscreen" => "Print",
        "-" | "minus" => "minus",
        "=" | "equal" | "equals" => "equal",
        "+" | "plus" => "plus",
        "," | "comma" => "comma",
        "." | "period" => "period",
        "/" | "slash" => "slash",
        "\\" | "backslash" => "backslash",
        ";" | "semicolon" => "semicolon",
        "'" | "apostrophe" | "quote" => "apostrophe",
        "`" | "grave" => "grave",
        "[" | "bracketleft" => "bracketleft",
        "]" | "bracketright" => "bracketright",
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.to_string());
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
        && (1..=24).contains(&n)
    {
        return Some(format!("F{n}"));
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        // Letters stay lowercase: the shift modifier makes them capitals.
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_ascii_lowercase().to_string()),
        // Anything else that looks like a keysym name (XF86AudioMute, KP_Enter).
        _ if key.len() > 1 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => Some(key.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(combo: &str) -> String {
        wtype_args(combo).unwrap().join(" ")
    }

    #[test]
    fn combos() {
        assert_eq!(args("a"), "-k a");
        assert_eq!(args("cmd+a"), "-M logo -k a -m logo");
        assert_eq!(args("ctrl+shift+T"), "-M ctrl -M shift -k t -m shift -m ctrl");
        assert_eq!(args("Enter"), "-k Return");
        assert_eq!(args("alt+F4"), "-M alt -k F4 -m alt");
        assert_eq!(args("ctrl++"), "-M ctrl -k plus -m ctrl");
        assert_eq!(args("ctrl+-"), "-M ctrl -k minus -m ctrl");
        assert_eq!(args("+"), "-k plus");
        assert_eq!(args("Super+pageDown"), "-M logo -k Page_Down -m logo");
        assert_eq!(args("XF86AudioMute"), "-k XF86AudioMute");
    }

    #[test]
    fn bad_combos() {
        assert!(wtype_args("").is_err());
        assert!(wtype_args("hyper+a").is_err());
        assert!(wtype_args("ctrl+é").is_err());
        assert!(wtype_args("ctrl+$(rm)").is_err());
    }
}
