// Reading the accessibility tree (needs the Accessibility grant). Every AX
// call is a round trip to the target app, which can hang; the messaging
// timeout caps each one so a stuck app costs seconds, not the connection.
// Trees are built with AXUIElementCopyMultipleAttributeValues (one round trip
// per element instead of seven) and capped in size, because a web view or a
// big table can have tens of thousands of elements.
import ApplicationServices
import AppKit
import GuiCore

/// Per-call AX timeout, in seconds.
let axTimeout: Float = 2

/// Most elements one tree walk visits.
let maxTreeNodes = 4000

enum AX {
    static let systemWide: AXUIElement = {
        let e = AXUIElementCreateSystemWide()
        // On the system-wide element this sets the default for every element.
        AXUIElementSetMessagingTimeout(e, axTimeout)
        return e
    }()

    static var trusted: Bool { AXIsProcessTrusted() }

    static func requireTrust() throws {
        if !trusted {
            throw GuiError("Accessibility is not granted to berth-gui-mac (System Settings > Privacy & Security > Accessibility)")
        }
    }

    static func app(_ pid: pid_t) -> AXUIElement {
        let e = AXUIElementCreateApplication(pid)
        AXUIElementSetMessagingTimeout(e, axTimeout)
        return e
    }

    static func attribute(_ e: AXUIElement, _ name: String) -> CFTypeRef? {
        var v: CFTypeRef?
        guard AXUIElementCopyAttributeValue(e, name as CFString, &v) == .success else { return nil }
        return v
    }

    static func string(_ e: AXUIElement, _ name: String) -> String? {
        stringValue(attribute(e, name))
    }

    static func elements(_ e: AXUIElement, _ name: String) -> [AXUIElement] {
        (attribute(e, name) as? [AXUIElement]) ?? []
    }

    static func element(_ e: AXUIElement, _ name: String) -> AXUIElement? {
        guard let v = attribute(e, name), CFGetTypeID(v) == AXUIElementGetTypeID() else { return nil }
        return (v as! AXUIElement)
    }

    /// AX values as text: strings as they are, numbers and booleans printed;
    /// anything else (ranges, elements) isn't a useful `value`.
    static func stringValue(_ v: CFTypeRef?) -> String? {
        guard let v else { return nil }
        if let s = v as? String { return s }
        if CFGetTypeID(v) == CFBooleanGetTypeID() { return (v as! Bool) ? "true" : "false" }
        if let n = v as? NSNumber { return n.stringValue }
        if let u = v as? URL { return u.absoluteString }
        return nil
    }

    static func point(_ v: CFTypeRef?) -> CGPoint? {
        guard let v, CFGetTypeID(v) == AXValueGetTypeID() else { return nil }
        var p = CGPoint.zero
        return AXValueGetValue(v as! AXValue, .cgPoint, &p) ? p : nil
    }

    static func size(_ v: CFTypeRef?) -> CGSize? {
        guard let v, CFGetTypeID(v) == AXValueGetTypeID() else { return nil }
        var s = CGSize.zero
        return AXValueGetValue(v as! AXValue, .cgSize, &s) ? s : nil
    }

    static func frame(_ e: AXUIElement) -> Frame? {
        guard let p = point(attribute(e, kAXPositionAttribute)),
              let s = size(attribute(e, kAXSizeAttribute)) else { return nil }
        return Frame(p.x, p.y, s.width, s.height)
    }

    static let nodeAttributes = [
        kAXRoleAttribute, kAXTitleAttribute, kAXDescriptionAttribute, kAXValueAttribute,
        kAXPositionAttribute, kAXSizeAttribute, kAXEnabledAttribute, kAXFocusedAttribute,
        kAXChildrenAttribute,
    ] as CFArray

    /// One element as a Node without children, plus its child elements.
    static func describe(_ e: AXUIElement) -> (Node, [AXUIElement]) {
        var values: CFArray?
        var vals: [CFTypeRef?] = Array(repeating: nil, count: 9)
        if AXUIElementCopyMultipleAttributeValues(e, nodeAttributes, AXCopyMultipleAttributeOptions(rawValue: 0), &values) == .success,
           let arr = values as? [AnyObject], arr.count == 9 {
            // Missing attributes come back as AXValue errors or CFNull.
            vals = arr.map { v -> CFTypeRef? in
                if v is NSNull { return nil }
                if CFGetTypeID(v) == AXValueGetTypeID(), AXValueGetType(v as! AXValue) == .axError { return nil }
                return v
            }
        }
        let role = stringValue(vals[0]) ?? "AXUnknown"
        let title = stringValue(vals[1])
        let desc = stringValue(vals[2])
        let value = stringValue(vals[3])
        var frame: Frame?
        if let p = point(vals[4]), let s = size(vals[5]) { frame = Frame(p.x, p.y, s.width, s.height) }
        let enabled = (vals[6].flatMap { $0 as? Bool }) ?? true
        let focused = (vals[7].flatMap { $0 as? Bool }) ?? false
        let children = (vals[8] as? [AXUIElement]) ?? []
        let node = Node(role: role, label: Node.label(title: title, description: desc, value: value, role: role),
                        value: value.map { String($0.prefix(500)) }, frame: frame,
                        enabled: enabled, focused: focused)
        return (node, children)
    }

    /// A walked tree: the Node plus the element behind every path in it.
    struct Tree {
        var root: Node
        var elements: [[Int]: AXUIElement]
        var truncated: Bool
    }

    static func tree(_ root: AXUIElement, depth: Int) -> Tree {
        var count = 0
        var elements: [[Int]: AXUIElement] = [:]
        var truncated = false
        func walk(_ e: AXUIElement, path: [Int], level: Int) -> Node {
            count += 1
            elements[path] = e
            var (node, kids) = describe(e)
            node.path = path
            if level >= depth {
                if !kids.isEmpty { truncated = true }
                return node
            }
            for (i, k) in kids.enumerated() {
                if count >= maxTreeNodes { truncated = true; break }
                node.children.append(walk(k, path: path + [i], level: level + 1))
            }
            return node
        }
        let node = walk(root, path: [], level: 0)
        return Tree(root: node, elements: elements, truncated: truncated)
    }

    /// The element under a screen point (top-left origin, points), as `hit`.
    static func hit(at x: Double, _ y: Double) -> JSON? {
        guard trusted else { return nil }
        var e: AXUIElement?
        guard AXUIElementCopyElementAtPosition(systemWide, Float(x), Float(y), &e) == .success, let e else { return nil }
        return describe(e).0.hitJSON
    }

    static func press(_ e: AXUIElement) -> Bool {
        AXUIElementPerformAction(e, kAXPressAction as CFString) == .success
    }
}
