// berth-gui-mac: berth's GUI agent for macOS guests. It runs as a LaunchAgent
// in the logged-in user's session (dev.berth.gui-mac.plist), listens on vsock
// port 2401 and answers docs/gui-protocol.md: screenshots, clicks, typing and
// the accessibility tree, for coding agents testing apps in the VM.
//
// It relies on two TCC grants made once in the base image (install.sh says
// how): Accessibility (AX tree, CGEvent input) and Screen Recording
// (screenshots, idle detection). Without them those verbs fail with a message
// naming the missing grant; hello, windows, open and push still work.
//
//   berth-gui-mac                     listen on vsock 2401
//   berth-gui-mac --port 2402         another vsock port
//   berth-gui-mac --unix <path>       a unix socket instead (development)
//   berth-gui-mac --check             print the grants as JSON and exit (run
//                                     from a shell, TCC answers for the
//                                     terminal app; `hello` asks the agent)
import AppKit
import ApplicationServices
import Foundation
import GuiCore

func usage() -> Never {
    FileHandle.standardError.write(Data("""
    usage: berth-gui-mac [--port N | --unix PATH] [--check] [--version]

    """.utf8))
    exit(2)
}

var listenOn = Listen.vsock(port: 2401)
var args = CommandLine.arguments.dropFirst()
while let arg = args.popFirst() {
    switch arg {
    case "--unix":
        guard let p = args.popFirst() else { usage() }
        listenOn = .unix(path: p)
    case "--port":
        guard let s = args.popFirst(), let p = UInt32(s) else { usage() }
        listenOn = .vsock(port: p)
    case "--check":
        let line = JSON.object([
            "accessibility": .bool(AXIsProcessTrusted()),
            "screen_recording": .bool(CGPreflightScreenCaptureAccess()),
        ]).encodedLine()
        FileHandle.standardOutput.write(line + Data("\n".utf8))
        exit(0)
    case "--version":
        print(agentVersion)
        exit(0)
    default:
        usage()
    }
}

// A host that hangs up mid-reply must not kill the agent.
signal(SIGPIPE, SIG_IGN)

// Touching the system-wide element sets the AX messaging timeout for every
// element this process asks about.
_ = AX.systemWide

let listener: Int32
do {
    listener = try listenSocket(listenOn)
} catch {
    log("berth-gui-mac: \(error)")
    exit(1)
}
switch listenOn {
case .vsock(let port): log("berth-gui-mac \(agentVersion) listening on vsock port \(port)")
case .unix(let path): log("berth-gui-mac \(agentVersion) listening on \(path)")
}
log("accessibility: \(AXIsProcessTrusted()), screen recording: \(CGPreflightScreenCaptureAccess())")

// Under launchd (vsock) the agent is its own "responsible process", so asking
// here puts berth-gui-mac itself in System Settings' lists, which is what the
// one-time grant in the base image needs. Asked from a shell, the grant would
// go to the terminal app instead. In --unix development runs, don't prompt.
if case .vsock = listenOn {
    if !AXIsProcessTrusted() {
        let opts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
        _ = AXIsProcessTrustedWithOptions(opts)
    }
    if !CGPreflightScreenCaptureAccess() {
        _ = CGRequestScreenCaptureAccess()
    }
}

let handler = Handler()
Thread.detachNewThread {
    serve(listener, handler: handler)
}

// The main run loop keeps NSWorkspace's app lists and NSRunningApplication's
// state current; connections run on their own threads. A run loop with no
// sources returns at once, so an idle timer keeps it alive.
let keepAlive = Timer(timeInterval: 3600, repeats: true) { _ in }
RunLoop.main.add(keepAlive, forMode: .default)
while true {
    RunLoop.main.run(mode: .default, before: .distantFuture)
}
