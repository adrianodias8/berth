//! Windows of a sway session, from `swaymsg -t get_tree` (sway's IPC, JSON).
//!
//! A view's `rect` is its container on the screen (borders and title bar
//! included); `window_rect` is where the client's surface sits inside it.
//! Native Wayland windows name themselves with `app_id`, Xwayland ones with
//! `window_properties.class`.

use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Window {
    /// sway's container id, for `[con_id=…]` criteria (a number: nothing to
    /// quote).
    #[serde(skip)]
    pub id: i64,
    pub app: String,
    pub title: String,
    /// The client's surface on the screen.
    pub frame: [i64; 4],
    pub focused: bool,
    pub pid: i64,
}

fn rect(v: &Value) -> [i64; 4] {
    let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    [n("x"), n("y"), n("width"), n("height")]
}

/// Every view in the tree, in sway's order (tiled before floating).
pub fn windows(tree: &Value) -> Vec<Window> {
    let mut out = Vec::new();
    walk(tree, &mut out);
    out
}

fn walk(node: &Value, out: &mut Vec<Window>) {
    let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
    let pid = node.get("pid").and_then(Value::as_i64);
    if matches!(kind, "con" | "floating_con")
        && let Some(pid) = pid
    {
        let app = node
            .get("app_id")
            .and_then(Value::as_str)
            .or_else(|| node.pointer("/window_properties/class").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let outer = rect(&node["rect"]);
        let inner = rect(&node["window_rect"]);
        let frame = if inner[2] > 0 { [outer[0] + inner[0], outer[1] + inner[1], inner[2], inner[3]] } else { outer };
        out.push(Window {
            id: node.get("id").and_then(Value::as_i64).unwrap_or(0),
            app,
            title: node.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            frame,
            focused: node.get("focused").and_then(Value::as_bool).unwrap_or(false),
            pid,
        });
    }
    for key in ["nodes", "floating_nodes"] {
        for child in node.get(key).and_then(Value::as_array).into_iter().flatten() {
            walk(child, out);
        }
    }
}

/// The window an agent means by `app`: its app id, title or pid, exactly
/// (ignoring case) first, then as a substring. Ambiguity is an error.
pub fn find<'a>(windows: &'a [Window], app: &str) -> Result<&'a Window, String> {
    let want = app.to_lowercase();
    let exact: Vec<&Window> = windows
        .iter()
        .filter(|w| w.app.to_lowercase() == want || w.title.to_lowercase() == want || w.pid.to_string() == want)
        .collect();
    let chosen = if exact.is_empty() {
        windows.iter().filter(|w| w.app.to_lowercase().contains(&want) || w.title.to_lowercase().contains(&want)).collect()
    } else {
        exact
    };
    match chosen.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no window matches {app:?}")),
        many => {
            // Several windows of one app: its focused one, else the first.
            if many.iter().all(|w| w.app == many[0].app) {
                return Ok(many.iter().find(|w| w.focused).unwrap_or(&many[0]));
            }
            let names: Vec<String> = many.iter().map(|w| format!("{} ({})", w.title, w.app)).collect();
            Err(format!("{app:?} matches several windows: {}", names.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from a real `swaymsg -t get_tree` of a headless sway with a
    // GTK app tiled and an Xwayland app floating.
    const TREE: &str = r#"{"id":1,"type":"root","rect":{"x":0,"y":0,"width":1280,"height":800},"nodes":[
      {"id":2,"type":"output","name":"HEADLESS-1","nodes":[
        {"id":3,"type":"workspace","name":"1","nodes":[
          {"id":4,"type":"con","name":"Calculator","app_id":"org.gnome.Calculator","pid":120,"focused":true,
           "rect":{"x":0,"y":0,"width":1280,"height":800},"window_rect":{"x":1,"y":1,"width":1278,"height":798}}],
         "floating_nodes":[
          {"id":5,"type":"floating_con","name":"xterm","app_id":null,"window_properties":{"class":"XTerm"},"pid":130,
           "focused":false,"rect":{"x":100,"y":80,"width":500,"height":300},"window_rect":{"x":2,"y":24,"width":496,"height":274}}]}]}]}"#;

    #[test]
    fn parses_views() {
        let tree: Value = serde_json::from_str(TREE).unwrap();
        let w = windows(&tree);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0], Window { id: 4, app: "org.gnome.Calculator".into(), title: "Calculator".into(), frame: [1, 1, 1278, 798], focused: true, pid: 120 });
        assert_eq!(w[1].app, "XTerm");
        assert_eq!(w[1].frame, [102, 104, 496, 274]);
    }

    #[test]
    fn finds_windows() {
        let tree: Value = serde_json::from_str(TREE).unwrap();
        let w = windows(&tree);
        assert_eq!(find(&w, "calculator").unwrap().id, 4);
        assert_eq!(find(&w, "xterm").unwrap().id, 5);
        assert_eq!(find(&w, "130").unwrap().id, 5);
        assert_eq!(find(&w, "gnome").unwrap().id, 4);
        assert!(find(&w, "firefox").is_err());
        assert!(find(&w, "e").unwrap_err().contains("several"));
    }
}
