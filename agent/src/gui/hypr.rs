//! Windows of a Hyprland desktop session, from `hyprctl clients -j`
//! (Hyprland's IPC, JSON), in sway.rs's shape so the verbs share one path.
//!
//! A client's `address` is its pointer in Hyprland, printed in hex; it
//! stands in for sway's container id and goes back as `address:0x…` in
//! dispatchers. `at` and `size` are the window on the layout, borders
//! excluded. The focused window is the one with `focusHistoryID` 0. Windows
//! that aren't mapped, are hidden (a group's other tabs) or sit on a special
//! workspace (the scratchpad, negative ids) aren't on the screen and are
//! left out.

use serde_json::Value;

use super::sway::Window;

/// The windows on the screen, in Hyprland's order.
pub fn windows(clients: &Value) -> Vec<Window> {
    clients
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c.get("mapped").and_then(Value::as_bool) != Some(false))
        .filter(|c| c.get("hidden").and_then(Value::as_bool) != Some(true))
        .filter(|c| c.pointer("/workspace/id").and_then(Value::as_i64).is_none_or(|id| id >= 0))
        .filter_map(|c| {
            let address = c.get("address").and_then(Value::as_str)?;
            let id = i64::from_str_radix(address.strip_prefix("0x")?, 16).ok()?;
            let pair = |k: &str| {
                let a = c.get(k).and_then(Value::as_array);
                let n = |i: usize| a.and_then(|a| a.get(i)).and_then(Value::as_i64).unwrap_or(0);
                [n(0), n(1)]
            };
            let ([x, y], [w, h]) = (pair("at"), pair("size"));
            let class = c.get("class").and_then(Value::as_str).filter(|s| !s.is_empty());
            Some(Window {
                id,
                app: class.or_else(|| c.get("initialClass").and_then(Value::as_str)).unwrap_or("").to_string(),
                title: c.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                frame: [x, y, w, h],
                focused: c.get("focusHistoryID").and_then(Value::as_i64) == Some(0),
                pid: c.get("pid").and_then(Value::as_i64).unwrap_or(0),
            })
        })
        .collect()
}

/// The dispatcher argument naming window `id`.
fn address(id: i64) -> String {
    format!("address:0x{id:x}")
}

/// The command that focuses window `id` (a number: nothing to quote).
pub fn focus_cmd(id: i64) -> String {
    format!("hyprctl dispatch focuswindow {}", address(id))
}

/// The command that closes window `id`, as its close button would.
pub fn close_cmd(id: i64) -> String {
    format!("hyprctl dispatch closewindow {}", address(id))
}

/// hyprctl answers a dispatch with "ok", or the error, and exits 0 either
/// way.
pub fn dispatch_result(out: &str) -> Result<(), String> {
    match out.trim() {
        "ok" => Ok(()),
        other => Err(format!("hyprctl: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // In the shape of Hyprland 0.5x's `hyprctl clients -j` (written from its
    // format, not captured from a session): a tiled foot (focused), a
    // floating Xwayland app, a window hidden in a group, and one in the
    // scratchpad.
    const CLIENTS: &str = r#"[
      {"address":"0x5612a3b4c5d0","mapped":true,"hidden":false,"at":[10,48],"size":[940,1022],
       "workspace":{"id":1,"name":"1"},"floating":false,"pseudo":false,"monitor":0,"class":"foot","title":"~",
       "initialClass":"foot","initialTitle":"foot","pid":812,"xwayland":false,"pinned":false,"fullscreen":0,
       "fullscreenClient":0,"grouped":[],"tags":[],"swallowing":"0x0","focusHistoryID":0,"inhibitingIdle":false},
      {"address":"0x5612a3b4d100","mapped":true,"hidden":false,"at":[600,300],"size":[480,320],
       "workspace":{"id":1,"name":"1"},"floating":true,"monitor":0,"class":"","title":"xclock",
       "initialClass":"XClock","initialTitle":"xclock","pid":901,"xwayland":true,"focusHistoryID":1},
      {"address":"0x5612a3b4e200","mapped":true,"hidden":true,"at":[10,48],"size":[940,1022],
       "workspace":{"id":1,"name":"1"},"floating":false,"monitor":0,"class":"org.gnome.Calculator","title":"Calculator",
       "initialClass":"org.gnome.Calculator","initialTitle":"Calculator","pid":930,"xwayland":false,"focusHistoryID":2},
      {"address":"0x5612a3b4f300","mapped":true,"hidden":false,"at":[200,100],"size":[800,600],
       "workspace":{"id":-98,"name":"special:magic"},"floating":true,"monitor":0,"class":"Alacritty","title":"scratch",
       "initialClass":"Alacritty","initialTitle":"scratch","pid":940,"xwayland":false,"focusHistoryID":3}
    ]"#;

    #[test]
    fn parses_clients() {
        let clients: Value = serde_json::from_str(CLIENTS).unwrap();
        let w = windows(&clients);
        assert_eq!(w.len(), 2, "{w:?}");
        assert_eq!(w[0], Window { id: 0x5612a3b4c5d0, app: "foot".into(), title: "~".into(), frame: [10, 48, 940, 1022], focused: true, pid: 812 });
        assert_eq!((w[1].app.as_str(), w[1].focused, w[1].frame), ("XClock", false, [600, 300, 480, 320]));
        assert!(windows(&serde_json::json!({"error": "no"})).is_empty());
    }

    #[test]
    fn commands() {
        assert_eq!(focus_cmd(0x5612a3b4c5d0), "hyprctl dispatch focuswindow address:0x5612a3b4c5d0");
        assert_eq!(close_cmd(0x5612a3b4c5d0), "hyprctl dispatch closewindow address:0x5612a3b4c5d0");
        assert_eq!(dispatch_result("ok\n"), Ok(()));
        assert!(dispatch_result("No such window found").is_err());
    }
}
