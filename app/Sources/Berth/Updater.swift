import AppKit
import CryptoKit
import Foundation
import Security

/// Updates from the GitHub repository's latest release (drafts and
/// prereleases never count). The zip that scripts/release.sh builds is
/// downloaded in the background and only installed when the user says so,
/// since installing restarts the VM.
///
/// What gets installed must be signed with a Developer ID by the same team
/// as the running app, notarized, and newer than it: the download's origin
/// isn't trusted, the signature is. Builds that aren't Developer ID signed
/// (ad-hoc, dev builds) don't update.
@MainActor
final class Updater: ObservableObject {
    nonisolated static let repo = "adrianodias8/berth"
    nonisolated static let bundleID = "dev.berth.app"
    private static let interval: TimeInterval = 24 * 3600

    struct Release: Equatable {
        let version: String
        let notes: URL
        /// The verified Berth.app, staged in Caches.
        let app: URL
    }

    enum Phase: Equatable {
        case idle, checking, upToDate, installing
        case downloading(String)
        case ready(Release)
        case failed(String)
    }

    @Published var phase: Phase = .idle

    let current = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0"
    /// The running app's Developer ID team; nil when it has none.
    let team: String? = Updater.developerIDTeam(of: Bundle.main.bundleURL)

    /// Why this copy can't update itself, if it can't.
    var unavailableReason: String? {
        if team == nil { return "Only signed releases update themselves; this build isn't one." }
        if Bundle.main.bundlePath.contains("/AppTranslocation/") {
            return "Move Berth to the Applications folder to get updates."
        }
        return nil
    }

    var lastCheck: Date? { UserDefaults.standard.object(forKey: "lastUpdateCheck") as? Date }

    private var autoUpdate: Bool { UserDefaults.standard.object(forKey: "autoUpdate") as? Bool ?? true }

    init() {
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(10))
            while !Task.isCancelled {
                if let self, self.autoUpdate, self.unavailableReason == nil,
                   Date().timeIntervalSince(self.lastCheck ?? .distantPast) > Self.interval {
                    await self.check()
                }
                try? await Task.sleep(for: .seconds(3600))
            }
        }
    }

    // MARK: Checking and downloading

    private struct GitHubRelease: Decodable {
        let tagName: String
        let htmlUrl: URL
        let assets: [Asset]
        struct Asset: Decodable {
            let name: String
            let browserDownloadUrl: URL
        }
    }

    /// Look for a newer release and, if there is one, download and verify it.
    func check() async {
        guard unavailableReason == nil else { return }
        switch phase {
        case .checking, .downloading, .installing: return
        case .ready: return
        default: break
        }
        phase = .checking
        do {
            var request = URLRequest(url: URL(string: "https://api.github.com/repos/\(Self.repo)/releases/latest")!)
            request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
            request.setValue("Berth/\(current)", forHTTPHeaderField: "User-Agent")
            let (data, response) = try await URLSession.shared.data(for: request)
            UserDefaults.standard.set(Date(), forKey: "lastUpdateCheck")
            if (response as? HTTPURLResponse)?.statusCode == 404 {
                phase = .upToDate // no release published yet
                return
            }
            let decoder = JSONDecoder()
            decoder.keyDecodingStrategy = .convertFromSnakeCase
            let release = try decoder.decode(GitHubRelease.self, from: data)
            let version = release.tagName.hasPrefix("v") ? String(release.tagName.dropFirst()) : release.tagName
            guard Self.isNewer(version, than: current) else {
                phase = .upToDate
                return
            }
            guard let zip = release.assets.first(where: { $0.name == "Berth-\(version).zip" }),
                  let sum = release.assets.first(where: { $0.name == "Berth-\(version).zip.sha256" }) else {
                throw UpdateError("release \(release.tagName) has no Berth-\(version).zip and .sha256")
            }
            phase = .downloading(version)
            let app = try await download(zip.browserDownloadUrl, sum: sum.browserDownloadUrl, version: version)
            phase = .ready(Release(version: version, notes: release.htmlUrl, app: app))
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    private func download(_ zipURL: URL, sum sumURL: URL, version: String) async throws -> URL {
        let dir = try FileManager.default.url(for: .cachesDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
            .appendingPathComponent("\(Self.bundleID)/Updates")
        try? FileManager.default.removeItem(at: dir)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)

        let (sumData, _) = try await URLSession.shared.data(from: sumURL)
        let expected = String(decoding: sumData, as: UTF8.self).split(separator: " ").first.map(String.init) ?? ""
        let (downloaded, _) = try await URLSession.shared.download(from: zipURL)
        let zip = dir.appendingPathComponent("Berth-\(version).zip")
        try FileManager.default.moveItem(at: downloaded, to: zip)

        let team = self.team
        return try await offPool {
            guard try Self.sha256(of: zip) == expected.lowercased() else {
                throw UpdateError("the download doesn't match its checksum")
            }
            let unpacked = dir.appendingPathComponent("unpacked")
            let ditto = Engine.run(["/usr/bin/ditto", "-x", "-k", zip.path, unpacked.path])
            guard ditto.code == 0 else { throw UpdateError("unzipping: \(ditto.output)") }
            try? FileManager.default.removeItem(at: zip)
            let app = unpacked.appendingPathComponent("Berth.app")
            try Self.verify(app, version: version, team: team)
            return app
        }
    }

    // MARK: Verifying

    nonisolated static func verify(_ app: URL, version: String, team: String?) throws {
        guard let team else { throw UpdateError("this build can't verify updates") }
        let info = Bundle(url: app)?.infoDictionary ?? [:]
        guard info["CFBundleIdentifier"] as? String == bundleID,
              info["CFBundleShortVersionString"] as? String == version else {
            throw UpdateError("the download isn't Berth \(version)")
        }
        // Developer ID Application (that certificate extension), our team.
        let requirement = "anchor apple generic and identifier \"\(bundleID)\" and certificate leaf[field.1.2.840.113635.100.6.1.13] and certificate leaf[subject.OU] = \"\(team)\""
        guard validate(app, requirement: requirement) else {
            throw UpdateError("the download isn't signed by Berth's developer")
        }
        // Notarized: what Gatekeeper would check had a browser downloaded it.
        let spctl = Engine.run(["/usr/sbin/spctl", "--assess", "--type", "execute", app.path])
        guard spctl.code == 0 else { throw UpdateError("the download isn't notarized: \(spctl.output)") }
    }

    /// The Developer ID team that signed `app`, if one did and the signature holds.
    nonisolated static func developerIDTeam(of app: URL) -> String? {
        guard validate(app, requirement: "anchor apple generic and certificate leaf[field.1.2.840.113635.100.6.1.13]") else { return nil }
        var code: SecStaticCode?
        guard SecStaticCodeCreateWithPath(app as CFURL, [], &code) == errSecSuccess, let code else { return nil }
        var info: CFDictionary?
        guard SecCodeCopySigningInformation(code, SecCSFlags(rawValue: kSecCSSigningInformation), &info) == errSecSuccess,
              let dict = info as? [String: Any] else { return nil }
        return dict[kSecCodeInfoTeamIdentifier as String] as? String
    }

    private nonisolated static func validate(_ app: URL, requirement: String) -> Bool {
        var code: SecStaticCode?
        var req: SecRequirement?
        guard SecStaticCodeCreateWithPath(app as CFURL, [], &code) == errSecSuccess, let code,
              SecRequirementCreateWithString(requirement as CFString, [], &req) == errSecSuccess else { return false }
        let flags = SecCSFlags(rawValue: kSecCSCheckAllArchitectures | kSecCSStrictValidate | kSecCSCheckNestedCode)
        return SecStaticCodeCheckValidity(code, flags, req) == errSecSuccess
    }

    private nonisolated static func sha256(of file: URL) throws -> String {
        let handle = try FileHandle(forReadingFrom: file)
        defer { try? handle.close() }
        var hash = SHA256()
        while let chunk = try handle.read(upToCount: 1 << 20), !chunk.isEmpty { hash.update(data: chunk) }
        return hash.finalize().map { String(format: "%02x", $0) }.joined()
    }

    /// "0.10.0" > "0.9.1"; a "-rc1" suffix is ignored.
    nonisolated static func isNewer(_ a: String, than b: String) -> Bool {
        func parts(_ s: String) -> [Int] {
            (s.split(separator: "-").first ?? "").split(separator: ".").map { Int($0) ?? 0 }
        }
        let x = parts(a), y = parts(b)
        for i in 0..<max(x.count, y.count) {
            let p = i < x.count ? x[i] : 0, q = i < y.count ? y[i] : 0
            if p != q { return p > q }
        }
        return false
    }

    // MARK: Installing

    /// Ask, then shut berth down, put the new app in place of this one and
    /// relaunch it; it starts berth again if it was running.
    func confirmInstall(_ release: Release, engine: Engine) {
        let alert = NSAlert()
        alert.messageText = "Install Berth \(release.version)?"
        alert.informativeText = engine.isRunning
            ? "Berth restarts: the VM shuts down and starts again, so running containers stop. Those with a restart policy start again."
            : "Berth relaunches."
        alert.addButton(withTitle: "Install and Restart")
        alert.addButton(withTitle: "Later")
        NSApp.activate(ignoringOtherApps: true)
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        install(release, restartBerth: engine.isRunning || engine.state == .starting)
    }

    private func install(_ release: Release, restartBerth: Bool) {
        phase = .installing
        let target = Bundle.main.bundleURL
        Task {
            let result: Result<Void, Error> = await offPool {
                Result {
                    if restartBerth {
                        _ = Engine.run([BerthPaths.cli, "stop", "--shutdown"])
                    }
                    // The staged copy is in Caches, on the same volume as
                    // /Applications (the Data volume), so this is a rename.
                    _ = try FileManager.default.replaceItemAt(target, withItemAt: release.app)
                }
            }
            switch result {
            case .success:
                Self.relaunch(target, startBerth: restartBerth)
            case let .failure(error):
                phase = .failed("Installing: \(error.localizedDescription)")
                if restartBerth { _ = await offPool { Engine.run([BerthPaths.cli, "start"]) } }
            }
        }
    }

    /// Open the new app once this one has quit.
    private static func relaunch(_ app: URL, startBerth: Bool) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", "while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; exec /usr/bin/open \"$2\" --args -startAfterUpdate \"$3\"",
                             "sh", "\(ProcessInfo.processInfo.processIdentifier)", app.path, startBerth ? "YES" : "NO"]
        try? process.run()
        NSApp.terminate(nil)
    }

    /// The "Check for Updates…" menu item: like a scheduled check, but it
    /// says how it went.
    func checkInteractively(engine: Engine) {
        Task {
            if let reason = unavailableReason {
                show("Updates aren't available", reason)
                return
            }
            if case .failed = phase { phase = .idle }
            await check()
            switch phase {
            case .upToDate: show("You're up to date", "Berth \(current) is the latest version.")
            case let .failed(message): show("Couldn't check for updates", message)
            case let .ready(release): confirmInstall(release, engine: engine)
            default: break
            }
        }
    }

    private func show(_ title: String, _ text: String) {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = text
        NSApp.activate(ignoringOtherApps: true)
        alert.runModal()
    }
}

struct UpdateError: LocalizedError {
    let message: String
    init(_ message: String) { self.message = message }
    var errorDescription: String? { message }
}
