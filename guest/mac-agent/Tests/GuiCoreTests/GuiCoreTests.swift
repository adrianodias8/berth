// Unit tests for the pure parts of berth-gui-mac: the wire format, key
// combos, label matching on a synthetic AX tree, app lookup, push paths,
// sessions and idle detection. Nothing here needs a GUI session or grants.
import Foundation
import XCTest
@testable import GuiCore

final class ProtocolTests: XCTestCase {
    func testParsesRequest() throws {
        let r = try Request.parse(Data(#"{"v":1,"session":"s-3f9a","verb":"click","args":{"label":"Save"}}"#.utf8))
        XCTAssertEqual(r.verb, "click")
        XCTAssertEqual(r.session, "s-3f9a")
        XCTAssertEqual(r.args["label"]?.string, "Save")
        XCTAssertNil(r.payloadLength)
    }

    func testMissingVersionAndArgsDefault() throws {
        let r = try Request.parse(Data(#"{"verb":"hello"}"#.utf8))
        XCTAssertEqual(r.version, 1)
        XCTAssertEqual(r.args, [:])
        let n = try Request.parse(Data(#"{"v":1,"verb":"hello","args":null}"#.utf8))
        XCTAssertEqual(n.args, [:])
    }

    func testRejectsUnknownVersion() {
        XCTAssertThrowsError(try Request.parse(Data(#"{"v":2,"verb":"hello"}"#.utf8))) {
            XCTAssertEqual($0 as? GuiError, GuiError("unsupported version"))
        }
    }

    func testRejectsBadRequests() {
        for bad in ["nope", "[1]", #"{"v":1}"#, #"{"verb":"x","args":[1]}"#, #"{"verb":"x","payload_len":-1}"#] {
            XCTAssertThrowsError(try Request.parse(Data(bad.utf8)), bad)
        }
    }

    func testPayloadLength() throws {
        let r = try Request.parse(Data(#"{"v":1,"verb":"push","args":{"name":"A.app"},"payload_len":42}"#.utf8))
        XCTAssertEqual(r.payloadLength, 42)
    }

    func testReplyIsOneLineThenPayload() throws {
        let png = Data([0x89, 0x50, 0x4E, 0x47])
        let hit: JSON = ["role": "AXButton", "label": "Save", "frame": .frame(10, 20, 80, 24)]
        let out = Reply.success(["width": 2, "height": 1], payload: png, hit: hit).encoded()
        let nl = try XCTUnwrap(out.firstIndex(of: 0x0A))
        XCTAssertEqual(out.filter { $0 == 0x0A }.count, 1)
        let head = try JSON.parse(out[..<nl])
        XCTAssertEqual(head["ok"], true)
        XCTAssertEqual(head["error"], .null)
        XCTAssertEqual(head["payload_len"], 4)
        XCTAssertEqual(head["result"]?["width"], 2)
        XCTAssertEqual(head["hit"]?["label"], "Save")
        XCTAssertEqual(Data(out[(nl + 1)...]), png)
    }

    func testFailureReply() throws {
        let out = Reply.failure("line one\nline two").encoded()
        XCTAssertEqual(out.last, 0x0A)
        XCTAssertEqual(out.filter { $0 == 0x0A }.count, 1, "newlines in messages are escaped")
        let head = try JSON.parse(out.dropLast())
        XCTAssertEqual(head["ok"], false)
        XCTAssertEqual(head["error"], "line one\nline two")
        XCTAssertEqual(head["payload_len"], 0)
        XCTAssertNil(head["hit"])
    }

    func testNumbersStayIntegers() {
        let line = String(decoding: JSON.object(["pid": 123, "frame": .frame(0, 25, 1440.5, 875)]).encodedLine(), as: UTF8.self)
        XCTAssertEqual(line, #"{"frame":[0,25,1440.5,875],"pid":123}"#)
        XCTAssertEqual(String(decoding: JSON.double(.nan).encodedLine(), as: UTF8.self), "null")
        XCTAssertEqual(String(decoding: JSON.string("/Users/berth").encodedLine(), as: UTF8.self), #""/Users/berth""#)
    }

    func testJSONRoundTrip() throws {
        let v: JSON = ["a": [1, 2.5, "x", true, nil], "b": ["c": "d"]]
        XCTAssertEqual(try JSON.parse(v.encodedLine()), v)
    }

    /// A read function that hands out the given chunks, then EOF.
    func reader(_ chunks: [String]) -> LineReader {
        var rest = chunks.map { Data($0.utf8) }
        return LineReader(read: { rest.isEmpty ? Data() : rest.removeFirst() })
    }

    func testLineReaderSplitsAcrossChunks() throws {
        var r = reader([#"{"verb":"pu"#, #"sh"}"#, "\nTARDA", "TA"])
        XCTAssertEqual(String(decoding: try r.readLine(), as: UTF8.self), #"{"verb":"push"}"#)
        var payload = Data()
        try r.readPayload(length: nil) { payload.append($0) }
        XCTAssertEqual(String(decoding: payload, as: UTF8.self), "TARDATA")
    }

    func testLineReaderExactPayload() throws {
        var r = reader(["{}\nabc", "defgh"])
        _ = try r.readLine()
        var payload = Data()
        try r.readPayload(length: 5) { payload.append($0) }
        XCTAssertEqual(String(decoding: payload, as: UTF8.self), "abcde")
    }

    func testLineReaderShortPayloadFails() throws {
        var r = reader(["{}\nab"])
        _ = try r.readLine()
        XCTAssertThrowsError(try r.readPayload(length: 5) { _ in })
    }

    func testLineWithoutNewlineEndsAtEOF() throws {
        var r = reader([#"{"verb":"hello"}"#])
        XCTAssertEqual(try Request.parse(try r.readLine()).verb, "hello")
    }

    func testLineTooLong() {
        var r = LineReader(read: { Data(repeating: 0x41, count: 4096) })
        XCTAssertThrowsError(try r.readLine(limit: 10_000))
    }
}

final class KeyComboTests: XCTestCase {
    func testPlainAndModified() throws {
        XCTAssertEqual(try KeyCombo.parse("a"), KeyCombo(keyCode: 0, modifiers: []))
        XCTAssertEqual(try KeyCombo.parse("cmd+a"), KeyCombo(keyCode: 0, modifiers: .cmd))
        XCTAssertEqual(try KeyCombo.parse("Cmd+Shift+S"), KeyCombo(keyCode: 1, modifiers: [.cmd, .shift]))
        XCTAssertEqual(try KeyCombo.parse("ctrl+alt+delete"), KeyCombo(keyCode: 51, modifiers: [.ctrl, .alt]))
        XCTAssertEqual(try KeyCombo.parse("option + left"), KeyCombo(keyCode: 123, modifiers: .alt))
        XCTAssertEqual(try KeyCombo.parse("super+space"), KeyCombo(keyCode: 49, modifiers: .cmd))
    }

    func testNamedKeys() throws {
        XCTAssertEqual(try KeyCombo.parse("return").keyCode, 36)
        XCTAssertEqual(try KeyCombo.parse("enter").keyCode, 36)
        XCTAssertEqual(try KeyCombo.parse("esc").keyCode, 53)
        XCTAssertEqual(try KeyCombo.parse("tab").keyCode, 48)
        XCTAssertEqual(try KeyCombo.parse("f5").keyCode, 96)
        XCTAssertEqual(try KeyCombo.parse("f12").keyCode, 111)
        XCTAssertEqual(try KeyCombo.parse("pagedown").keyCode, 121)
        XCTAssertEqual(try KeyCombo.parse("cmd+,").keyCode, 43)
        XCTAssertEqual(try KeyCombo.parse("cmd+comma").keyCode, 43)
        XCTAssertEqual(try KeyCombo.parse("cmd+/").keyCode, 44)
        XCTAssertEqual(try KeyCombo.parse("shift"), KeyCombo(keyCode: 56, modifiers: []))
    }

    func testPlus() throws {
        XCTAssertEqual(try KeyCombo.parse("cmd++"), KeyCombo(keyCode: 24, modifiers: [.cmd, .shift]))
        XCTAssertEqual(try KeyCombo.parse("cmd+plus"), KeyCombo(keyCode: 24, modifiers: [.cmd, .shift]))
        XCTAssertEqual(try KeyCombo.parse("cmd+="), KeyCombo(keyCode: 24, modifiers: .cmd))
        XCTAssertEqual(try KeyCombo.parse("cmd+-"), KeyCombo(keyCode: 27, modifiers: .cmd))
    }

    func testErrors() {
        for bad in ["", "cmd+", "hyper+a", "cmd+nosuchkey", "a+b"] {
            XCTAssertThrowsError(try KeyCombo.parse(bad), bad)
        }
    }
}

final class LabelMatchTests: XCTestCase {
    /// A window with a toolbar, a form and a menu bar, the way AX shows a
    /// small document app.
    let tree = Node(role: "AXApplication", label: "Notes", children: [
        Node(role: "AXWindow", label: "Untitled", frame: Frame(0, 25, 800, 600), children: [
            Node(role: "AXButton", label: "Save", frame: Frame(700, 560, 80, 24), path: [0, 0]),
            Node(role: "AXButton", label: "Save As…", frame: Frame(600, 560, 90, 24), path: [0, 1]),
            Node(role: "AXButton", label: "Cancel", frame: Frame(500, 560, 80, 24), path: [0, 2]),
            Node(role: "AXGroup", children: [
                Node(role: "AXStaticText", label: "Saved 2 minutes ago", frame: Frame(10, 40, 200, 16), path: [0, 3, 0]),
                Node(role: "AXCheckBox", label: "Dark", frame: Frame(10, 70, 60, 16), path: [0, 3, 1]),
                Node(role: "AXCheckBox", label: "Dark mode", frame: Frame(10, 90, 60, 16), path: [0, 3, 2]),
                Node(role: "AXTextField", label: "Name", frame: Frame(10, 110, 200, 22), path: [0, 3, 3]),
                Node(role: "AXTextField", label: "Name", frame: Frame(10, 140, 200, 22), path: [0, 3, 4]),
                // AX often lists the same control twice (cell and button).
                Node(role: "AXButton", label: "Share", frame: Frame(300, 30, 40, 20), path: [0, 3, 5]),
                Node(role: "AXButton", label: "Share", frame: Frame(300, 30, 40, 20), path: [0, 3, 6]),
            ], path: [0, 3]),
        ], path: [0]),
        Node(role: "AXMenuBar", children: [
            Node(role: "AXMenuBarItem", label: "File", frame: Frame(40, 0, 30, 24), children: [
                Node(role: "AXMenuItem", label: "Close", path: [1, 0, 0]),
                Node(role: "AXMenuItem", label: "Save", path: [1, 0, 1]),
            ], path: [1, 0]),
        ], path: [1]),
    ])

    func testExactBeatsSubstring() throws {
        // "Save" is also inside "Save As…" and "Saved 2 minutes ago".
        let n = try LabelMatch.find("save", in: tree.pruned(roles: ["AXMenuBar"]))
        XCTAssertEqual(n.path, [0, 0])
        XCTAssertEqual(n.role, "AXButton")
    }

    func testExactDuplicatesAreAmbiguous() {
        // Without pruning, File > Save matches exactly too.
        XCTAssertThrowsError(try LabelMatch.find("Save", in: tree)) { e in
            let msg = (e as? GuiError)?.message ?? ""
            XCTAssertTrue(msg.contains("ambiguous"), msg)
            XCTAssertTrue(msg.contains("AXMenuItem"), msg)
            XCTAssertTrue(msg.contains("[700,560,80,24]"), msg)
        }
        XCTAssertThrowsError(try LabelMatch.find("name", in: tree))
    }

    func testSameElementTwiceIsNotAmbiguous() throws {
        XCTAssertEqual(try LabelMatch.find("Share", in: tree).path, [0, 3, 5])
    }

    func testSubstringWhenNoExact() throws {
        XCTAssertEqual(try LabelMatch.find("cance", in: tree).path, [0, 2])
        XCTAssertEqual(try LabelMatch.find("minutes", in: tree).role, "AXStaticText")
        // "dark" exactly matches one checkbox even though both contain it.
        XCTAssertEqual(try LabelMatch.find("DARK", in: tree).path, [0, 3, 1])
        XCTAssertThrowsError(try LabelMatch.find("ar", in: tree)) // Dark, Dark mode, AXMenuBar…
    }

    func testMenuOnlyLabelFoundInFullTree() throws {
        XCTAssertThrowsError(try LabelMatch.find("Close", in: tree.pruned(roles: ["AXMenuBar"])))
        XCTAssertEqual(try LabelMatch.find("Close", in: tree).path, [1, 0, 0])
    }

    func testMissingAndEmpty() {
        XCTAssertThrowsError(try LabelMatch.find("Delete everything", in: tree)) {
            XCTAssertTrue(($0 as? GuiError)?.message.hasPrefix("no element") ?? false)
        }
        XCTAssertThrowsError(try LabelMatch.find("  ", in: tree))
    }

    func testExists() {
        XCTAssertTrue(LabelMatch.exists("saved", in: tree))
        XCTAssertTrue(LabelMatch.exists("Save", in: tree))
        XCTAssertFalse(LabelMatch.exists("Done", in: tree))
    }

    func testLabelFromAttributes() {
        XCTAssertEqual(Node.label(title: "OK", description: "confirm", value: nil, role: "AXButton"), "OK")
        XCTAssertEqual(Node.label(title: "", description: "Close", value: nil, role: "AXButton"), "Close")
        XCTAssertEqual(Node.label(title: nil, description: nil, value: "Hello", role: "AXStaticText"), "Hello")
        XCTAssertNil(Node.label(title: nil, description: " ", value: "typed text", role: "AXTextField"))
    }

    func testNodeJSONShape() throws {
        let j = Node(role: "AXButton", label: "Save", frame: Frame(1, 2, 3, 4), enabled: false).json
        XCTAssertEqual(j["role"], "AXButton")
        XCTAssertEqual(j["label"], "Save")
        XCTAssertEqual(j["value"], .null)
        XCTAssertEqual(j["frame"], [1, 2, 3, 4])
        XCTAssertEqual(j["enabled"], false)
        XCTAssertEqual(j["focused"], false)
        XCTAssertEqual(j["children"], [])
        XCTAssertNil(j["path"], "paths are internal")
    }
}

final class AppMatchTests: XCTestCase {
    let apps = [
        AppInfo(pid: 50, name: "Finder", bundleID: "com.apple.finder", path: "/System/Library/CoreServices/Finder.app"),
        AppInfo(pid: 900, name: "TextEdit", bundleID: "com.apple.TextEdit", path: "/System/Applications/TextEdit.app"),
        AppInfo(pid: 901, name: "My App", bundleID: "dev.example.myapp", path: "/Users/berth/Pushed/MyApp.app"),
        AppInfo(pid: 1200, name: "My App", bundleID: "dev.example.myapp", path: "/Users/berth/Pushed/MyApp.app", active: true),
    ]

    func testByNameBundleAndPath() throws {
        XCTAssertEqual(try AppMatch.find("finder", in: apps).pid, 50)
        XCTAssertEqual(try AppMatch.find("com.apple.TextEdit", in: apps).pid, 900)
        XCTAssertEqual(try AppMatch.find("TextEdit.app", in: apps).pid, 900)
        XCTAssertEqual(try AppMatch.find("/System/Applications/TextEdit.app", in: apps).pid, 900)
        XCTAssertEqual(try AppMatch.find("MyApp", in: apps).pid, 1200, "bundle file name, active copy first")
        XCTAssertEqual(try AppMatch.find("900", in: apps).pid, 900)
    }

    func testMissing() {
        XCTAssertThrowsError(try AppMatch.find("Safari", in: apps))
        XCTAssertThrowsError(try AppMatch.find("", in: apps))
    }
}

final class PushPathTests: XCTestCase {
    func testNames() throws {
        XCTAssertEqual(try PushPath.validateName("MyApp.app"), "MyApp.app")
        for bad in ["", "..", ".", "a/b", "../x", "/etc"] {
            XCTAssertThrowsError(try PushPath.validateName(bad), bad)
        }
    }

    func testDestinations() {
        let home = "/Users/berth"
        XCTAssertEqual(PushPath.destination(nil, home: home), "/Users/berth/Pushed")
        XCTAssertEqual(PushPath.destination("", home: home), "/Users/berth/Pushed")
        XCTAssertEqual(PushPath.destination("~", home: home), "/Users/berth")
        XCTAssertEqual(PushPath.destination("~/Desktop", home: home), "/Users/berth/Desktop")
        XCTAssertEqual(PushPath.destination("/tmp/x/../y", home: home), "/tmp/y")
        XCTAssertEqual(PushPath.destination("Apps", home: home), "/Users/berth/Apps")
    }
}

final class SessionTests: XCTestCase {
    func testLifecycle() throws {
        let book = SessionBook(now: { Date(timeIntervalSince1970: 1_793_620_800) })
        let id = book.new(agent: "claude-code")
        XCTAssertTrue(id.hasPrefix("s-"))
        XCTAssertEqual(id.count, 10)
        book.noteApp("MyApp", session: id)
        book.noteApp("MyApp", session: id)
        book.noteApp("Other", session: "s-unknown")
        let list = book.listJSON()["sessions"]?.array ?? []
        XCTAssertEqual(list.count, 1)
        XCTAssertEqual(list[0]["session"]?.string, id)
        XCTAssertEqual(list[0]["agent"], "claude-code")
        XCTAssertEqual(list[0]["apps"], ["MyApp"])
        XCTAssertEqual(list[0]["since"], "2026-11-02T12:00:00Z")
        book.end(id)
        book.end(id)
        XCTAssertEqual(book.list().count, 0)
    }
}

final class IdleTests: XCTestCase {
    func frame(_ fill: UInt8, changed: Int = 0) -> [UInt8] {
        var f = [UInt8](repeating: fill, count: 100 * 4)
        for p in 0..<changed { f[p * 4] = fill &+ 100 }
        return f
    }

    func testChangedFraction() {
        XCTAssertEqual(ImageDiff.changedFraction(frame(10), frame(10)), 0)
        XCTAssertEqual(ImageDiff.changedFraction(frame(10), frame(14)), 0, "within tolerance")
        XCTAssertEqual(ImageDiff.changedFraction(frame(10), frame(10, changed: 25)), 0.25)
        XCTAssertEqual(ImageDiff.changedFraction(frame(10), [1, 2, 3, 4]), 1)
    }

    func testDetectorNeedsConsecutiveStillFrames() {
        var d = IdleDetector(threshold: 0.02, stillNeeded: 2)
        XCTAssertFalse(d.add(frame(0)))
        XCTAssertFalse(d.add(frame(50)))             // changed
        XCTAssertFalse(d.add(frame(50)))             // still 1
        XCTAssertFalse(d.add(frame(50, changed: 30))) // changed again, resets
        XCTAssertFalse(d.add(frame(50, changed: 31))) // 1 % changed: still 1
        XCTAssertTrue(d.add(frame(50, changed: 31)))  // still 2
    }
}
