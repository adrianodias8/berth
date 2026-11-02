//! The accessibility tree (docs/gui-protocol.md's Node) and finding things
//! in it: an element by label, and the element under a point.
//!
//! Toolkits nest labels: a GTK button "Save" holds a label "Save", a window
//! titled "Calculator" holds everything else. So a lookup that matched a
//! node and its descendant means one element (the outer one, the one that
//! acts), and windows and panels lose to anything more specific.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Node {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub frame: [i64; 4],
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub children: Vec<Node>,
}

/// Roles that hold other elements rather than being one to click.
const CONTAINERS: &[&str] = &["desktop frame", "application", "frame", "window", "dialog", "filler", "panel", "scroll pane", "viewport"];

impl Node {
    fn visible(&self) -> bool {
        self.frame[2] > 0 && self.frame[3] > 0
    }

    pub fn center(&self) -> (i64, i64) {
        (self.frame[0] + self.frame[2] / 2, self.frame[1] + self.frame[3] / 2)
    }

    /// For the host's log: the element without its subtree.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({"role": self.role, "label": self.label, "frame": self.frame})
    }
}

/// Every node with its path (child indices from the root).
fn flatten<'a>(node: &'a Node, path: &mut Vec<usize>, out: &mut Vec<(Vec<usize>, &'a Node)>) {
    out.push((path.clone(), node));
    for (i, child) in node.children.iter().enumerate() {
        path.push(i);
        flatten(child, path, out);
        path.pop();
    }
}

/// The one element labelled `label`: case-insensitive, an exact match before
/// a substring, on-screen elements only.
pub fn find<'a>(root: &'a Node, label: &str) -> Result<&'a Node, String> {
    let want = label.trim().to_lowercase();
    if want.is_empty() {
        return Err("empty label".into());
    }
    let mut all = Vec::new();
    flatten(root, &mut Vec::new(), &mut all);
    let visible: Vec<&(Vec<usize>, &Node)> = all.iter().filter(|(_, n)| n.visible() && !n.label.is_empty()).collect();
    let mut matches: Vec<&(Vec<usize>, &Node)> = visible.iter().copied().filter(|(_, n)| n.label.to_lowercase() == want).collect();
    if matches.is_empty() {
        matches = visible.iter().copied().filter(|(_, n)| n.label.to_lowercase().contains(&want)).collect();
    }
    if matches.iter().any(|(_, n)| !CONTAINERS.contains(&n.role.as_str())) {
        matches.retain(|(_, n)| !CONTAINERS.contains(&n.role.as_str()));
    }
    // A match inside another match is the same element.
    let outer: Vec<&(Vec<usize>, &Node)> =
        matches.iter().copied().filter(|(p, _)| !matches.iter().any(|(q, _)| q.len() < p.len() && p.starts_with(q))).collect();
    match outer.as_slice() {
        [(_, one)] => Ok(one),
        [] => Err(format!("no element labelled {label:?} on screen")),
        many => {
            let list: Vec<String> = many.iter().take(10).map(|(_, n)| format!("{} {:?} at {:?}", n.role, n.label, n.frame)).collect();
            Err(format!("label {label:?} is ambiguous: {}", list.join("; ")))
        }
    }
}

/// The innermost on-screen element at (x, y), for logging what a
/// coordinate click hit. The desktop root itself doesn't count.
pub fn hit(root: &Node, x: i64, y: i64) -> Option<&Node> {
    let inside = |n: &Node| n.visible() && x >= n.frame[0] && y >= n.frame[1] && x < n.frame[0] + n.frame[2] && y < n.frame[1] + n.frame[3];
    // Children may lie outside their parent's frame (popovers), so search
    // the whole tree and keep the deepest hit; later siblings are on top.
    fn walk<'a>(n: &'a Node, depth: usize, inside: &dyn Fn(&Node) -> bool, best: &mut Option<(usize, &'a Node)>) {
        if depth > 0 && inside(n) && best.is_none_or(|(d, _)| depth >= d) {
            *best = Some((depth, n));
        }
        for c in &n.children {
            walk(c, depth + 1, inside, best);
        }
    }
    let mut best = None;
    walk(root, 0, &inside, &mut best);
    best.map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, label: &str, frame: [i64; 4], children: Vec<Node>) -> Node {
        Node { role: role.into(), label: label.into(), frame, enabled: true, children, ..Default::default() }
    }

    fn sample() -> Node {
        node(
            "desktop frame",
            "",
            [0, 0, 1280, 800],
            vec![node(
                "application",
                "gnome-calculator",
                [0, 0, 0, 0],
                vec![node(
                    "frame",
                    "Calculator",
                    [0, 0, 400, 600],
                    vec![
                        node("push button", "Save", [10, 10, 80, 30], vec![node("label", "Save", [20, 15, 60, 20], vec![])]),
                        node("push button", "Save As…", [100, 10, 80, 30], vec![]),
                        node("push button", "Calculate", [10, 50, 80, 30], vec![]),
                        node("push button", "Hidden", [0, 0, 0, 0], vec![]),
                        node("push button", "Seven", [10, 90, 40, 40], vec![]),
                        node("push button", "Seventy", [60, 90, 40, 40], vec![]),
                    ],
                )],
            )],
        )
    }

    #[test]
    fn exact_beats_substring_and_nesting_collapses() {
        let root = sample();
        let save = find(&root, "save").unwrap();
        assert_eq!((save.role.as_str(), save.frame), ("push button", [10, 10, 80, 30]));
        assert_eq!(find(&root, "SAVE AS").unwrap().label, "Save As…");
        assert_eq!(find(&root, "seven").unwrap().label, "Seven");
    }

    #[test]
    fn containers_lose_and_hidden_are_skipped() {
        let root = sample();
        // "Calc" is in the window's title and a button's label.
        assert_eq!(find(&root, "calc").unwrap().label, "Calculate");
        assert_eq!(find(&root, "calculator").unwrap().role, "frame");
        assert!(find(&root, "hidden").unwrap_err().contains("no element"));
    }

    #[test]
    fn ambiguity_lists_candidates() {
        let root = sample();
        let err = find(&root, "sev").unwrap_err();
        assert!(err.contains("ambiguous") && err.contains("Seven") && err.contains("Seventy"), "{err}");
    }

    #[test]
    fn hits_the_innermost() {
        let root = sample();
        assert_eq!(hit(&root, 25, 20).unwrap().role, "label");
        assert_eq!(hit(&root, 12, 12).unwrap().role, "push button");
        assert_eq!(hit(&root, 300, 500).unwrap().role, "frame");
        assert!(hit(&root, 900, 700).is_none());
    }

    #[test]
    fn parses_partial_nodes() {
        let n: Node = serde_json::from_str(r#"{"role":"push button","label":"OK","frame":[1,2,3,4]}"#).unwrap();
        assert_eq!(n.frame, [1, 2, 3, 4]);
        assert!(n.children.is_empty());
    }
}
