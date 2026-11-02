// The accessibility tree as the protocol sends it (`Node`), and label lookup
// over it. The executable builds Nodes from AXUIElements; matching works on
// the plain tree so it can be tested without a GUI.
import Foundation

public struct Frame: Equatable {
    public var x, y, w, h: Double
    public init(_ x: Double, _ y: Double, _ w: Double, _ h: Double) {
        self.x = x; self.y = y; self.w = w; self.h = h
    }
    public var isEmpty: Bool { !(w > 0 && h > 0) }
    public var center: (Double, Double) { (x + w / 2, y + h / 2) }
    public var json: JSON { .frame(x, y, w, h) }
}

public struct Node: Equatable {
    public var role: String
    public var label: String?
    public var value: String?
    public var frame: Frame?
    public var enabled: Bool
    public var focused: Bool
    public var children: [Node]
    /// Where this node sits in the tree (child indexes from the root), so the
    /// executable can find the AX element a match came from.
    public var path: [Int]

    public init(role: String, label: String? = nil, value: String? = nil, frame: Frame? = nil,
                enabled: Bool = true, focused: Bool = false, children: [Node] = [], path: [Int] = []) {
        self.role = role
        self.label = label
        self.value = value
        self.frame = frame
        self.enabled = enabled
        self.focused = focused
        self.children = children
        self.path = path
    }

    public var json: JSON {
        .object([
            "role": .string(role),
            "label": .from(label),
            "value": .from(value),
            "frame": frame?.json ?? .null,
            "enabled": .bool(enabled),
            "focused": .bool(focused),
            "children": .array(children.map(\.json)),
        ])
    }

    /// What the host logs as `hit`.
    public var hitJSON: JSON {
        .object(["role": .string(role), "label": .from(label), "frame": frame?.json ?? .null])
    }

    /// Every node, parents before children.
    public func flattened() -> [Node] {
        var out: [Node] = []
        var stack: [Node] = [self]
        while let n = stack.popLast() {
            out.append(n)
            stack.append(contentsOf: n.children.reversed())
        }
        return out
    }

    /// The tree without the subtrees rooted at `roles` (paths unchanged).
    /// Label lookup searches windows before the menu bar this way, so "Save"
    /// finds the dialog's button, not File > Save.
    public func pruned(roles: Set<String>) -> Node {
        var n = self
        n.children = children.filter { !roles.contains($0.role) }.map { $0.pruned(roles: roles) }
        return n
    }

    /// The label a node gets from its AX attributes: the title, else the
    /// description; static text carries its words in the value.
    public static func label(title: String?, description: String?, value: String?, role: String) -> String? {
        for s in [title, description] {
            if let s, !s.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return s }
        }
        if role == "AXStaticText", let v = value, !v.isEmpty { return v }
        return nil
    }
}

public enum LabelMatch {
    /// Finds the one node whose label is `label`, case-insensitively: exact
    /// matches first, then substring matches. AX trees often repeat the same
    /// element (a button and its cell with one frame), so matches with the
    /// same role and frame count once. More than one distinct match is an
    /// error naming them, so an agent can pick a more specific label.
    public static func find(_ label: String, in root: Node) throws -> Node {
        let want = label.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        guard !want.isEmpty else { throw GuiError("empty label") }
        let all = root.flattened().filter { $0.label != nil }
        let exact = narrow(all.filter { $0.label!.lowercased() == want })
        if exact.count == 1 { return exact[0] }
        if exact.count > 1 { throw ambiguous(label, exact) }
        let partial = narrow(all.filter { $0.label!.lowercased().contains(want) })
        if partial.count == 1 { return partial[0] }
        if partial.count > 1 { throw ambiguous(label, partial) }
        throw GuiError("no element labelled '\(label)'")
    }

    /// Roles that hold other elements rather than being one to click
    /// (docs/gui-protocol.md: a container loses to any other match).
    static let containers: Set<String> = [
        "AXApplication", "AXWindow", "AXSheet", "AXDrawer", "AXGroup",
        "AXScrollArea", "AXSplitGroup", "AXLayoutArea",
    ]

    /// The protocol's rules, as the Linux guest applies them: containers lose
    /// to anything else, a match inside another match is that same element
    /// (the outer one is kept), and duplicates with one role and frame count once.
    static func narrow(_ nodes: [Node]) -> [Node] {
        var m = nodes
        if m.contains(where: { !containers.contains($0.role) }) {
            m.removeAll { containers.contains($0.role) }
        }
        let outer = m.filter { n in
            !m.contains { o in o.path.count < n.path.count && Array(n.path.prefix(o.path.count)) == o.path }
        }
        return dedupe(outer)
    }

    /// Whether any node carries the label (exact or substring); `wait` needs
    /// presence, not uniqueness.
    public static func exists(_ label: String, in root: Node) -> Bool {
        let want = label.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        guard !want.isEmpty else { return false }
        return root.flattened().contains { $0.label?.lowercased().contains(want) ?? false }
    }

    static func dedupe(_ nodes: [Node]) -> [Node] {
        var out: [Node] = []
        for n in nodes where !out.contains(where: { $0.role == n.role && $0.frame == n.frame && $0.frame != nil }) {
            out.append(n)
        }
        return out
    }

    static func ambiguous(_ label: String, _ nodes: [Node]) -> GuiError {
        let shown = nodes.prefix(10).map { n -> String in
            let f = n.frame.map { "[\(Int($0.x)),\(Int($0.y)),\(Int($0.w)),\(Int($0.h))]" } ?? "[no frame]"
            return "\(n.role) '\(n.label ?? "")' at \(f)"
        }
        let more = nodes.count > 10 ? ", and \(nodes.count - 10) more" : ""
        return GuiError("label '\(label)' is ambiguous: \(shown.joined(separator: "; "))\(more)")
    }
}
