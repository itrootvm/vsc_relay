import SwiftUI
import AppKit
import Foundation
import Security
import ServiceManagement

enum Keychain {
    static let service = "dev.vscrelay.app"

    static func set(_ account: String, _ value: String) {
        let base: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(base as CFDictionary)
        var add = base
        add[kSecValueData as String] = value.data(using: .utf8) ?? Data()
        add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlocked
        SecItemAdd(add as CFDictionary, nil)
    }

    static func get(_ account: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var out: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &out) == errSecSuccess,
              let data = out as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }
}

struct AgentSendResult: Decodable {
    let protocolName: String
    let status: String
    let alias: String
    let sessionId: String
    let rewrote: Bool
    let inputBytes: Int
    let deliveredBytes: Int
    let deliveredText: String?

    enum CodingKeys: String, CodingKey {
        case status, alias, rewrote
        case protocolName = "protocol"
        case sessionId = "session_id"
        case inputBytes = "input_bytes"
        case deliveredBytes = "delivered_bytes"
        case deliveredText = "delivered_text"
    }
}

struct PromptHistoryEntry: Codable, Identifiable {
    let id: String
    let sessionId: String
    let alias: String
    let prompt: String
    let submittedAt: TimeInterval
    var status: String
    var inputBytes: Int
    var deliveredBytes: Int?
    var deliveredText: String?
    var observedWorking: Bool
    var completedAt: TimeInterval?
}

final class RelayController: ObservableObject {
    static let shared = RelayController()

    @Published var token: String = ""
    @Published var secret: String = ""
    @Published var isRunning: Bool = false
    private var adopted: pid_t = 0
    @Published var pid: Int32 = 0
    @Published var shimInstalled: Bool = false
    @Published var logLines: [String] = []
    @Published var note: String = ""
    @Published var showSettings: Bool = false
    @Published var showHelp: Bool = false
    @Published var appUpdate: String = ""
    @Published var updateAsset: String = ""
    @Published var updating: Bool = false
    let releasesURL = "https://github.com/itrootvm/vsc_relay/releases/latest"
    @Published var turnsToday: Int = 0
    @Published var sessionsToday: Int = 0
    @Published var vscodeOK: Bool = true
    @Published var extensionOK: Bool = true
    @Published var launchAtLogin: Bool = false
    @Published var extVersion: String = "-"
    @Published var sessions: [SessionCard] = []
    @Published var promptHistory: [PromptHistoryEntry] = []
    @Published var providers: [ProviderRow] = []
    @Published var providerHealth: [String: HealthInfo] = [:]
    @Published var checkingHealth = false
    @Published var smartEnabled = false
    @Published var steerEnabled = false
    @Published var gateEnabled = false
    @Published var semanticBackend = "off"
    @Published var allowUncalibratedSteer = false
    @Published var budgetUsd: Double?
    @Published var reviewEnabled = false
    @Published var reviewSteer = false
    @Published var reviewDepth = "normal"
    @Published var reviewEverySecs = 900
    @Published var reviewers: [String] = []

    private var process: Process?
    private var refreshingSessions = false
    private let maxLines = 800
    private let relayDir: URL
    private let logURL: URL
    private let legacyConfigURL: URL
    private let statsURL: URL
    private let promptHistoryURL: URL
    private var logOffset: UInt64 = 0

    private init() {
        let home = FileManager.default.homeDirectoryForCurrentUser
        relayDir = home.appendingPathComponent(".vsc-relay")
        logURL = relayDir.appendingPathComponent("agent.log")
        legacyConfigURL = relayDir.appendingPathComponent("config.env")
        statsURL = relayDir.appendingPathComponent("stats.json")
        promptHistoryURL = relayDir.appendingPathComponent("ui-prompt-history.json")
        try? FileManager.default.createDirectory(at: relayDir, withIntermediateDirectories: true)
        try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: relayDir.path)
        loadExistingLogTail()
        logOffset = ((try? FileManager.default.attributesOfItem(atPath: logURL.path))?[.size]
            as? NSNumber)?.uint64Value ?? 0
        loadPromptHistory()
        launchAtLogin = (SMAppService.mainApp.status == .enabled)
        refreshEnv()
        refreshStats()
        refreshAutomationStatus()
        checkAppUpdate()
    }

    func refreshEnv() {
        runAgentSub(["env-check"]) { [weak self] out in
            guard let self = self else { return }
            for line in out.split(separator: "\n") {
                let kv = line.split(separator: "=", maxSplits: 1).map(String.init)
                guard kv.count == 2 else { continue }
                let val = kv[1].trimmingCharacters(in: .whitespaces)
                let on = val == "true"
                switch kv[0] {
                case "vscode": self.vscodeOK = on
                case "extension": self.extensionOK = on
                case "shim_installed": self.shimInstalled = on
                case "version":
                    if self.extVersion != "-" && self.extVersion != val && !val.isEmpty && val != "-" {
                        self.appendLog("[env] Claude Code changed \(self.extVersion) -> \(val)")
                    }
                    self.extVersion = val
                default: break
                }
            }
        }
    }

    func setLaunchAtLogin(_ on: Bool) {
        do {
            if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
            launchAtLogin = on
        } catch {
            note = "Login item error: \(error.localizedDescription)"
        }
    }

    func refreshStats() {
        guard let data = try? Data(contentsOf: statsURL),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
        let s = (obj["sessions"] as? NSNumber)?.intValue ?? 0
        let t = (obj["turns"] as? NSNumber)?.intValue ?? 0
        if s != sessionsToday { sessionsToday = s }
        if t != turnsToday { turnsToday = t }
    }

    var hasSecrets: Bool { !token.isEmpty && !secret.isEmpty }

    func loadSecrets(_ done: @escaping (Bool) -> Void) {
        let legacyURL = legacyConfigURL
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            var loadedToken = Keychain.get("telegram_token") ?? ""
            var loadedSecret = Keychain.get("pair_secret") ?? ""
            var migratedLegacy = false
            if (loadedToken.isEmpty || loadedSecret.isEmpty),
               let text = try? String(contentsOf: legacyURL, encoding: .utf8) {
                for line in text.split(separator: "\n") {
                    let parts = line.split(separator: "=", maxSplits: 1).map(String.init)
                    guard parts.count == 2 else { continue }
                    let key = parts[0].trimmingCharacters(in: .whitespaces)
                    let val = parts[1].trimmingCharacters(in: .whitespaces)
                    if key == "TELEGRAM_BOT_TOKEN", loadedToken.isEmpty { loadedToken = val }
                    if key == "RELAY_PAIR_SECRET", loadedSecret.isEmpty { loadedSecret = val }
                }
                migratedLegacy = !loadedToken.isEmpty && !loadedSecret.isEmpty
            }
            if migratedLegacy {
                Keychain.set("telegram_token", loadedToken)
                Keychain.set("pair_secret", loadedSecret)
                try? FileManager.default.removeItem(at: legacyURL)
            }
            DispatchQueue.main.async {
                guard let self else { return }
                self.token = loadedToken
                self.secret = loadedSecret
                done(!loadedToken.isEmpty)
            }
        }
    }

    func saveSecrets() {
        Keychain.set("telegram_token", token)
        Keychain.set("pair_secret", secret)
    }

    func generateSecret() -> String {
        let bytes = (0..<24).map { _ in UInt8.random(in: 0...255) }
        return bytes.map { String(format: "%02x", $0) }.joined()
    }

    private func resourceBinary(_ name: String) -> URL {
        if let res = Bundle.main.resourceURL {
            let p = res.appendingPathComponent(name)
            if FileManager.default.fileExists(atPath: p.path) { return p }
        }
        let exeDir = URL(fileURLWithPath: CommandLine.arguments[0]).deletingLastPathComponent()
        return exeDir.appendingPathComponent(name)
    }

    private func commandForPid(_ p: Int32) -> String {
        let ps = Process()
        ps.executableURL = URL(fileURLWithPath: "/bin/ps")
        ps.arguments = ["-o", "command=", "-p", "\(p)"]
        let pipe = Pipe()
        ps.standardOutput = pipe
        try? ps.run()
        ps.waitUntilExit()
        let data = (try? pipe.fileHandleForReading.readToEnd()) ?? Data()
        return String(data: data, encoding: .utf8) ?? ""
    }

    private func killStrayDaemons() {
        let pgrep = Process()
        pgrep.executableURL = URL(fileURLWithPath: "/usr/bin/pgrep")
        pgrep.arguments = ["-x", "vsc-relay-agent"]
        let pipe = Pipe()
        pgrep.standardOutput = pipe
        try? pgrep.run()
        pgrep.waitUntilExit()
        let data = (try? pipe.fileHandleForReading.readToEnd()) ?? Data()
        let mypid = ProcessInfo.processInfo.processIdentifier
        guard let out = String(data: data, encoding: .utf8) else { return }
        for line in out.split(separator: "\n") {
            guard let p = Int32(line.trimmingCharacters(in: .whitespaces)), p != mypid else { continue }
            let cmd = commandForPid(p)
            if cmd.contains(" hook ") { continue }
            if cmd.contains("vsc-relay-agent") { kill(p, SIGTERM) }
        }
    }

    func adoptRunningDaemon() {
        if isRunning { return }
        let pgrep = Process()
        pgrep.executableURL = URL(fileURLWithPath: "/usr/bin/pgrep")
        pgrep.arguments = ["-x", "vsc-relay-agent"]
        let pipe = Pipe()
        pgrep.standardOutput = pipe
        try? pgrep.run()
        pgrep.waitUntilExit()
        let out = String(data: (try? pipe.fileHandleForReading.readToEnd()) ?? Data(), encoding: .utf8) ?? ""
        let mypid = ProcessInfo.processInfo.processIdentifier
        for line in out.split(separator: "\n") {
            guard let found = Int32(line.trimmingCharacters(in: .whitespaces)), found != mypid else { continue }
            let cmd = commandForPid(found)
            if cmd.contains(" hook ") { continue }
            adopted = found
            isRunning = true
            pid = found
            note = "Running. This one was started outside the app, so its log is in \(logURL.path)."
            return
        }
        if adopted != 0 {
            adopted = 0
            isRunning = false
            pid = 0
        }
        if note.hasPrefix("Running") {
            note = "Stopped."
        }
    }

    private var launchdJobPlist: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents/dev.vscrelay.agent.plist")
    }

    private var launchdOwnsDaemon: Bool {
        FileManager.default.fileExists(atPath: launchdJobPlist.path)
    }

    private func launchctl(_ arguments: [String]) {
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        task.arguments = arguments
        try? task.run()
        task.waitUntilExit()
    }

    func start() {
        if isRunning { return }
        adoptRunningDaemon()
        if isRunning { return }
        if launchdOwnsDaemon {
            launchctl(["bootstrap", "gui/\(getuid())", launchdJobPlist.path])
            launchctl(["kickstart", "gui/\(getuid())/dev.vscrelay.agent"])
            note = "Starting under launchd..."
            return
        }
        killStrayDaemons()
        let proc = Process()
        proc.executableURL = resourceBinary("vsc-relay-agent")
        var env = ProcessInfo.processInfo.environment
        if token.isEmpty {
            env.removeValue(forKey: "TELEGRAM_BOT_TOKEN")
        } else {
            env["TELEGRAM_BOT_TOKEN"] = token
        }
        if secret.isEmpty {
            env.removeValue(forKey: "RELAY_PAIR_SECRET")
        } else {
            env["RELAY_PAIR_SECRET"] = secret
        }
        let relayTrace = "relay::gate=info,relay::trace=info,relay::hook=info"
        if FileManager.default.fileExists(atPath: relayDir.appendingPathComponent("debug").path) {
            env["RUST_LOG"] = "debug,hyper=warn,reqwest=warn,rustls=warn,h2=warn,tungstenite=warn,\(relayTrace)"
        } else if let inherited = env["RUST_LOG"], !inherited.trimmingCharacters(in: .whitespaces).isEmpty {
            env["RUST_LOG"] = "\(inherited),\(relayTrace)"
        } else {
            env["RUST_LOG"] = "info,\(relayTrace)"
        }
        proc.environment = env
        let pipe = Pipe()
        proc.standardOutput = pipe
        proc.standardError = pipe
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty, let s = String(data: data, encoding: .utf8) else { return }
            DispatchQueue.main.async { self?.appendLog(s) }
        }
        proc.terminationHandler = { [weak self] ended in
            DispatchQueue.main.async {
                guard self?.process === ended else { return }
                self?.isRunning = false
                self?.pid = 0
                self?.process = nil
                self?.appendLog("[relay stopped]\n")
            }
        }
        do {
            try proc.run()
            process = proc
            isRunning = true
            pid = proc.processIdentifier
            note = hasSecrets
                ? "Running. In Telegram, send /auth <your key> to the bot."
                : "Running locally. Telegram is disabled until token and pairing key are configured."
        } catch {
            note = "Failed to start: \(error.localizedDescription)"
        }
    }

    func stop() {
        stop(includingAdopted: true)
    }

    func stop(includingAdopted: Bool) {
        if let p = process, p.isRunning { p.terminate() }
        if includingAdopted, adopted != 0 {
            if launchdOwnsDaemon {
                launchctl(["bootout", "gui/\(getuid())/dev.vscrelay.agent"])
            }
            kill(adopted, SIGTERM)
        }
        adopted = 0
        process = nil
        isRunning = false
        pid = 0
    }

    func restart() {
        stop()
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [weak self] in
            self?.start()
        }
    }

    private func runAgentSub(_ args: [String], input: String? = nil, done: @escaping (String) -> Void) {
        let proc = Process()
        proc.executableURL = resourceBinary("vsc-relay-agent")
        proc.arguments = args
        let pipe = Pipe()
        let inputPipe = input == nil ? nil : Pipe()
        proc.standardOutput = pipe
        proc.standardError = pipe
        proc.standardInput = inputPipe
        var env = ProcessInfo.processInfo.environment
        env["TELEGRAM_BOT_TOKEN"] = token
        env["RELAY_PAIR_SECRET"] = secret
        proc.environment = env
        proc.terminationHandler = { _ in
            let data = (try? pipe.fileHandleForReading.readToEnd()) ?? Data()
            let out = String(data: data, encoding: .utf8) ?? ""
            DispatchQueue.main.async { done(out) }
        }
        do {
            try proc.run()
            if let input, let inputPipe {
                inputPipe.fileHandleForWriting.write(Data(input.utf8))
                try? inputPipe.fileHandleForWriting.close()
            }
        } catch { done("error: \(error.localizedDescription)") }
    }

    func installShim() {
        runAgentSub(["shim-install"]) { [weak self] out in
            self?.appendLog(out)
            self?.refreshEnv()
        }
    }

    func uninstallShim() {
        runAgentSub(["shim-uninstall"]) { [weak self] out in
            self?.appendLog(out); self?.refreshEnv()
        }
    }

    func loadAutomationDefault(_ done: @escaping (String) -> Void) {
        runAgentSub(["automation", "list"]) { out in
            var mode = "manual"
            for line in out.split(separator: "\n") {
                if line.hasPrefix("default:") {
                    mode = line.dropFirst("default:".count).trimmingCharacters(in: .whitespaces)
                }
            }
            done(mode)
        }
    }

    func setAutomationDefault(_ mode: String) {
        runAgentSub(["automation", "set-default", mode]) { [weak self] out in
            self?.appendLog(out)
        }
    }

    func loadSmart(_ done: @escaping (Bool, Bool, Bool, Bool) -> Void) {
        runAgentSub(["automation", "get"]) { out in
            var enabled = false
            var steer = false
            var gate = false
            var feedback = true
            if let data = out.data(using: .utf8),
               let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
               let smart = obj["smart"] as? [String: Any] {
                enabled = (smart["enabled"] as? Bool) ?? false
                steer = (smart["steer"] as? Bool) ?? false
                gate = (smart["gate"] as? Bool) ?? false
                feedback = (smart["feedback_protocol"] as? Bool) ?? true
            }
            self.smartEnabled = enabled
            self.steerEnabled = steer
            self.gateEnabled = gate
            done(enabled, steer, gate, feedback)
        }
    }

    func refreshAutomationStatus() {
        runAgentSub(["automation", "get"]) { [weak self] out in
            guard let self,
                  let data = out.data(using: .utf8),
                  let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
                  let smart = obj["smart"] as? [String: Any] else { return }
            self.smartEnabled = (smart["enabled"] as? Bool) ?? false
            self.steerEnabled = (smart["steer"] as? Bool) ?? false
            self.gateEnabled = (smart["gate"] as? Bool) ?? false
            if let semantic = smart["semantic"] as? [String: Any] {
                self.semanticBackend = (semantic["backend"] as? String) ?? "off"
                self.allowUncalibratedSteer =
                    (semantic["allow_uncalibrated_steer"] as? Bool) ?? false
            }
            let providers = (obj["robot"] as? [String: Any])?["providers"] as? [String: Any]
            self.budgetUsd = (providers?["budget_usd"] as? NSNumber)?.doubleValue
        }
    }

    func setSmart(_ on: Bool) {
        runAgentSub(["automation", "smart", on ? "on" : "off"]) { [weak self] out in
            self?.appendLog(out)
            self?.refreshAutomationStatus()
        }
    }

    func setSteer(_ on: Bool) {
        runAgentSub(["automation", "smart", "steer", on ? "on" : "off"]) { [weak self] out in
            self?.appendLog(out)
            self?.refreshAutomationStatus()
        }
    }

    func setGate(_ on: Bool) {
        runAgentSub(["automation", "smart", "gate", on ? "on" : "off"]) { [weak self] out in
            self?.appendLog(out)
            self?.refreshAutomationStatus()
        }
    }

    func setFeedback(_ on: Bool) {
        runAgentSub(["automation", "smart", "feedback", on ? "on" : "off"]) { [weak self] out in
            self?.appendLog(out)
        }
    }

    func refreshReviewStatus() {
        runAgentSub(["automation", "get"]) { [weak self] out in
            guard let self,
                  let data = out.data(using: .utf8),
                  let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let review = obj["review"] as? [String: Any] else { return }
            self.reviewEnabled = (review["enabled"] as? Bool) ?? false
            self.reviewSteer = (review["steer"] as? Bool) ?? false
            self.reviewDepth = (review["depth"] as? String) ?? "normal"
            self.reviewEverySecs = (review["every_secs"] as? Int) ?? 900
            self.reviewers = (review["reviewers"] as? [String]) ?? []
        }
    }

    func setReview(_ on: Bool) {
        runAgentSub(["automation", "review", on ? "on" : "off"]) { [weak self] out in
            self?.appendLog(out)
            self?.refreshReviewStatus()
        }
    }

    func setReviewSteer(_ on: Bool) {
        runAgentSub(["automation", "review", "steer", on ? "on" : "off"]) { [weak self] out in
            self?.appendLog(out)
            self?.refreshReviewStatus()
        }
    }

    func setReviewDepth(_ depth: String) {
        runAgentSub(["automation", "review", "depth", depth]) { [weak self] out in
            self?.appendLog(out)
            self?.refreshReviewStatus()
        }
    }

    func setReviewEvery(_ secs: Int) {
        runAgentSub(["automation", "review", "every", String(secs)]) { [weak self] out in
            self?.appendLog(out)
            self?.refreshReviewStatus()
        }
    }

    func runReview(_ sessionId: String, reviewer: String?, depth: String, done: @escaping (String) -> Void) {
        var args = ["automation", "review", "run", sessionId, "--depth", depth]
        if let reviewer { args += ["--reviewer", reviewer] }
        runAgentSub(args) { [weak self] out in
            let text = out.trimmingCharacters(in: .whitespacesAndNewlines)
            self?.appendLog(text)
            done(text.isEmpty ? "no reviewer answered" : text)
        }
    }

    func toggleReviewer(_ id: String) {
        var picked = reviewers
        if let at = picked.firstIndex(of: id) {
            picked.remove(at: at)
        } else {
            picked.append(id)
        }
        guard !picked.isEmpty else {
            appendLog("cross review needs at least one reviewer")
            return
        }
        runAgentSub(["automation", "review", "reviewers", picked.joined(separator: ",")]) { [weak self] out in
            self?.appendLog(out)
            self?.refreshReviewStatus()
        }
    }

    func setBudget(_ usd: Double?) {
        runAgentSub(["automation", "smart", "budget", usd.map { String($0) } ?? "off"]) { [weak self] out in
            self?.appendLog(out.trimmingCharacters(in: .whitespacesAndNewlines))
            self?.refreshAutomationStatus()
        }
    }

    func loadUsage(_ sid: String, _ done: @escaping (String) -> Void) {
        runAgentSub(["automation", "usage", sid]) { out in
            done(out.trimmingCharacters(in: .whitespacesAndNewlines))
        }
    }

    func loadHandoffDestinations(
        _ sid: String, _ done: @escaping ([HandoffDestination]) -> Void
    ) {
        runAgentSub(["handoff", "destinations", sid, "--json"]) { out in
            guard let line = out.split(separator: "\n").last(where: { $0.hasPrefix("[") }),
                let data = line.data(using: .utf8),
                let rows = try? JSONDecoder().decode([HandoffDestination].self, from: data)
            else {
                done([])
                return
            }
            done(rows)
        }
    }

    func runHandoff(
        _ sid: String, to destination: String, _ done: @escaping (HandoffOutcome?) -> Void
    ) {
        runAgentSub(["handoff", sid, "--to", destination, "--json"]) { [weak self] out in
            guard let line = out.split(separator: "\n").last(where: { $0.hasPrefix("{") }),
                let data = line.data(using: .utf8),
                let outcome = try? JSONDecoder().decode(HandoffOutcome.self, from: data)
            else {
                self?.appendLog(out.trimmingCharacters(in: .whitespacesAndNewlines))
                done(nil)
                return
            }
            self?.appendLog("handoff wrote \(outcome.path)")
            done(outcome)
        }
    }

    func loadHandoffReceipt(_ workspace: String, _ done: @escaping (HandoffReceipt?) -> Void) {
        runAgentSub(["handoff", "receipt", workspace, "--json"]) { out in
            guard let line = out.split(separator: "\n").last(where: { $0.hasPrefix("{") }),
                let data = line.data(using: .utf8),
                let receipt = try? JSONDecoder().decode(HandoffReceipt.self, from: data)
            else {
                done(nil)
                return
            }
            done(receipt)
        }
    }

    func loadPins(_ sid: String, _ done: @escaping (String) -> Void) {
        runAgentSub(["automation", "compass-pins", sid]) { out in
            done(out.trimmingCharacters(in: .whitespacesAndNewlines))
        }
    }

    func clearPins(_ sid: String, _ done: @escaping (String) -> Void) {
        runAgentSub(["automation", "compass-unpin", sid]) { [weak self] out in
            self?.appendLog(out.trimmingCharacters(in: .whitespacesAndNewlines))
            done(out.trimmingCharacters(in: .whitespacesAndNewlines))
        }
    }

    func addPin(_ sid: String, target: String, obligation: String, epoch: String, _ done: @escaping (String) -> Void) {
        runAgentSub(["automation", "compass-pin", sid, target, obligation, epoch]) { [weak self] out in
            self?.appendLog(out.trimmingCharacters(in: .whitespacesAndNewlines))
            done(out.trimmingCharacters(in: .whitespacesAndNewlines))
        }
    }

    func loadSemantic(_ done: @escaping (String, String, String, String, Bool) -> Void) {
        runAgentSub(["automation", "get"]) { out in
            var provider = "local"
            var model = ""
            var endpoint = ""
            var localDir = ""
            var trust = false
            if let data = out.data(using: .utf8),
               let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
               let smart = obj["smart"] as? [String: Any],
               let semantic = smart["semantic"] as? [String: Any] {
                let backend = semantic["backend"] as? String ?? "local"
                model = semantic["model"] as? String ?? ""
                endpoint = semantic["endpoint"] as? String ?? ""
                localDir = semantic["local_dir"] as? String ?? ""
                trust = semantic["allow_uncalibrated_steer"] as? Bool ?? false
                if backend == "open_ai_compatible" || backend == "openai_compatible" {
                    if endpoint.contains("openrouter.ai") { provider = "openrouter" }
                    else if endpoint.contains("api.nvidia.com") { provider = "nvidia" }
                    else { provider = "openai-compatible" }
                } else if backend == "agent_cli" {
                    provider = model
                } else { provider = backend }
            }
            self.semanticBackend = provider
            self.allowUncalibratedSteer = trust
            done(provider, model, endpoint, localDir, trust)
        }
    }

    func semanticAction(_ args: [String]) {
        runAgentSub(["automation", "smart"] + args) { [weak self] out in
            self?.appendLog(out.trimmingCharacters(in: .whitespacesAndNewlines))
        }
    }

    func setSemanticProvider(_ provider: String) { semanticAction(["provider", provider]) }
    func setSemanticModel(_ model: String) { semanticAction(["model", model]) }
    func setSemanticEndpoint(_ endpoint: String) { semanticAction(["endpoint", endpoint]) }
    func setSemanticLocalDir(_ path: String) { semanticAction(["local-dir", path]) }
    func trainSemantic(dataset: String, output: String) {
        semanticAction(["train-local", dataset, output])
    }
    func setSemanticTrust(_ on: Bool) { semanticAction(["trust", on ? "on" : "off"]) }
    func setSemanticKey(_ key: String) {
        runAgentSub(["automation", "smart", "key", "-"], input: key) { [weak self] out in
            self?.appendLog(out.trimmingCharacters(in: .whitespacesAndNewlines))
        }
    }
    func installSemanticModel() { semanticAction(["install-local"]) }
    func checkSemanticBackend() { semanticAction(["check"]) }

    private func loadPromptHistory() {
        guard let data = try? Data(contentsOf: promptHistoryURL),
              let entries = try? JSONDecoder().decode([PromptHistoryEntry].self, from: data)
        else { return }
        promptHistory = entries.sorted { $0.submittedAt > $1.submittedAt }
    }

    private func savePromptHistory() {
        let retained = promptHistory.sorted { $0.submittedAt > $1.submittedAt }
        promptHistory = retained
        guard let data = try? JSONEncoder().encode(retained) else { return }
        try? data.write(to: promptHistoryURL, options: .atomic)
        try? FileManager.default.setAttributes(
            [.posixPermissions: 0o600],
            ofItemAtPath: promptHistoryURL.path
        )
    }

    func history(for sessionId: String) -> [PromptHistoryEntry] {
        promptHistory
            .filter { $0.sessionId == sessionId }
            .sorted { $0.submittedAt > $1.submittedAt }
    }

    private func beginPrompt(alias: String, sessionId: String, text: String) -> String {
        let id = UUID().uuidString
        promptHistory.insert(
            PromptHistoryEntry(
                id: id,
                sessionId: sessionId,
                alias: alias,
                prompt: text,
                submittedAt: Date().timeIntervalSince1970,
                status: "sending",
                inputBytes: text.utf8.count,
                deliveredBytes: nil,
                deliveredText: nil,
                observedWorking: false,
                completedAt: nil
            ),
            at: 0
        )
        savePromptHistory()
        return id
    }

    private func finishPrompt(id: String, result: AgentSendResult?, error: String?) {
        guard let index = promptHistory.firstIndex(where: { $0.id == id }) else { return }
        if let result, result.protocolName == "vsc-relay.send-result.v1" {
            promptHistory[index].status = result.status
            promptHistory[index].inputBytes = result.inputBytes
            promptHistory[index].deliveredBytes = result.deliveredBytes
            promptHistory[index].deliveredText = result.deliveredText
            note = "Accepted · \(result.alias) · \(result.inputBytes) B → \(result.deliveredBytes) B"
            appendLog("[gui_send] accepted session=\(String(result.sessionId.prefix(8))) bytes=\(result.inputBytes)->\(result.deliveredBytes) improved=\(result.rewrote)")
        } else {
            promptHistory[index].status = "failed"
            promptHistory[index].completedAt = Date().timeIntervalSince1970
            let detail = error?.trimmingCharacters(in: .whitespacesAndNewlines) ?? "send failed"
            note = String(detail.prefix(240))
            appendLog("[gui_send] failed session=\(String(promptHistory[index].sessionId.prefix(8)))")
        }
        savePromptHistory()
    }

    private func reconcilePromptHistory(_ cards: [SessionCard]) {
        let states = Dictionary(cards.map { ($0.sessionId, $0.state) }, uniquingKeysWith: { _, latest in latest })
        var changed = false
        for index in promptHistory.indices {
            guard let state = states[promptHistory[index].sessionId],
                  !["failed", "robot_verified_final", "robot_claimed_complete", "robot_safe_partial", "robot_final"].contains(promptHistory[index].status)
            else { continue }
            if ["working", "subagent", "subagent_running"].contains(state) {
                if promptHistory[index].status == "sending" || promptHistory[index].status == "accepted" {
                    promptHistory[index].status = "working"
                    promptHistory[index].observedWorking = true
                    changed = true
                }
            } else if state == "idle" && promptHistory[index].observedWorking && promptHistory[index].status == "working" {
                promptHistory[index].status = "done"
                promptHistory[index].completedAt = Date().timeIntervalSince1970
                changed = true
            }
        }
        if changed { savePromptHistory() }
    }

    func refreshSessions() {
        guard !refreshingSessions else { return }
        refreshingSessions = true
        runAgentSub(["sessions"]) { [weak self] out in
            guard let self = self else { return }
            self.refreshingSessions = false
            guard let data = out.data(using: .utf8),
                  let cards = try? JSONDecoder().decode([SessionCard].self, from: data)
            else { return }
            if self.sessions != cards { self.sessions = cards }
            self.reconcilePromptHistory(cards)
        }
    }

    func sendPrompt(_ alias: String, _ sid: String, _ text: String, media: [String] = []) {
        let historyText = media.isEmpty ? text : "\(text) [\(media.count) attachment(s)]"
        let historyId = beginPrompt(alias: alias, sessionId: sid, text: historyText)
        note = "Sending to \(alias)…"
        var args = ["send", alias, sid]
        for path in media {
            args.append("--media")
            args.append(path)
        }
        if !text.isEmpty {
            args.append(text)
        }
        runAgentSub(args) { [weak self] out in
            guard let self = self else { return }
            let result = out
                .split(separator: "\n")
                .reversed()
                .compactMap { line -> AgentSendResult? in
                    guard let data = String(line).data(using: .utf8) else { return nil }
                    return try? JSONDecoder().decode(AgentSendResult.self, from: data)
                }
                .first
            self.finishPrompt(id: historyId, result: result, error: result == nil ? out : nil)
            self.refreshSessions()
        }
    }

    func setSessionMode(_ sid: String, _ mode: String) {
        runAgentSub(["automation", "set-session", sid, mode]) { [weak self] out in
            self?.appendLog(out.trimmingCharacters(in: .whitespacesAndNewlines))
            self?.refreshSessions()
        }
    }

    static func parseProviders(_ discOut: String, _ getOut: String) -> [ProviderRow] {
        guard let dData = discOut.data(using: .utf8),
              let arr = (try? JSONSerialization.jsonObject(with: dData)) as? [[String: Any]]
        else { return [] }
        let cfg = (getOut.data(using: .utf8).flatMap { try? JSONSerialization.jsonObject(with: $0) })
            as? [String: Any]
        let providers = ((cfg?["robot"] as? [String: Any])?["providers"]) as? [String: Any]
        let enabled = (providers?["enabled"] as? [String]) ?? []
        let per = (providers?["per_provider"] as? [String: Any]) ?? [:]
        return arr.compactMap { o in
            guard let id = o["id"] as? String else { return nil }
            let model = ((per[id] as? [String: Any])?["model"] as? String) ?? ""
            return ProviderRow(
                id: id,
                reason: o["reason"] as? String ?? "",
                available: o["available"] as? Bool ?? false,
                enabled: enabled.contains(id),
                model: model
            )
        }
    }

    func refreshProviders() {
        runAgentSub(["automation", "discover", "--json"]) { [weak self] discOut in
            guard let self = self else { return }
            self.runAgentSub(["automation", "get"]) { getOut in
                self.providers = RelayController.parseProviders(discOut, getOut)
            }
        }
    }

    func checkHealth(_ only: String? = nil) {
        checkingHealth = true
        var args = ["automation", "health", "--json"]
        if let only { args.append(only) }
        appendLog("[provider] checking \(only ?? "all backends")")
        runAgentSub(args) { [weak self] out in
            guard let self = self else { return }
            self.checkingHealth = false
            guard let data = out.data(using: .utf8),
                  let arr = (try? JSONSerialization.jsonObject(with: data)) as? [[String: Any]]
            else {
                self.appendLog("[provider] health probe returned unparsable output: \(out.trimmingCharacters(in: .whitespacesAndNewlines))")
                return
            }
            var map: [String: HealthInfo] = [:]
            for o in arr {
                if let id = o["id"] as? String {
                    map[id] = HealthInfo(
                        status: o["status"] as? String ?? "",
                        detail: o["detail"] as? String ?? ""
                    )
                }
            }
            if only == nil {
                self.providerHealth = map
            } else {
                for (id, health) in map { self.providerHealth[id] = health }
            }
            for (id, health) in map {
                self.appendLog("[provider] \(id) health=\(health.status) detail=\(health.detail)")
            }
        }
    }

    func providerAction(_ args: [String]) {
        runAgentSub(args) { [weak self] out in
            self?.appendLog(out.trimmingCharacters(in: .whitespacesAndNewlines))
            self?.refreshProviders()
        }
    }

    func setProviderEnabled(_ id: String, _ on: Bool) {
        providerAction(["automation", "provider", id, on ? "on" : "off"])
    }

    func setProviderKey(_ id: String, _ key: String) {
        providerAction(["automation", "provider-key", id, key])
    }

    func loginBackend(_ id: String) {
        let interactive = ["claude-cli", "codex-cli", "cursor-cli"]
        guard interactive.contains(id) else {
            let msg = "[provider] \(id) uses its existing CLI/app session; running a real health probe instead of a nonexistent login command."
            note = msg
            appendLog(msg)
            checkHealth(id)
            return
        }
        let bin = resourceBinary("vsc-relay-agent").path
        let script =
            "tell application \"Terminal\"\ndo script \"'\(bin)' automation login \(id)\"\nactivate\nend tell"
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        p.arguments = ["-e", script]
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        p.terminationHandler = { [weak self] process in
            let data = (try? pipe.fileHandleForReading.readToEnd()) ?? Data()
            let detail = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            DispatchQueue.main.async {
                let suffix = detail.isEmpty ? "" : ": \(detail)"
                self?.appendLog("[provider] login launcher \(id) exited \(process.terminationStatus)\(suffix)")
            }
        }
        do {
            try p.run()
            note = "Opened Terminal for \(id) login."
            appendLog("[provider] opened interactive login for \(id)")
        } catch {
            note = "Could not open \(id) login: \(error.localizedDescription)"
            appendLog("[provider] login launch failed for \(id): \(error.localizedDescription)")
        }
    }

    func openAccessibility() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility") {
            NSWorkspace.shared.open(url)
        }
    }

    func openReleases() {
        if let url = URL(string: releasesURL) { NSWorkspace.shared.open(url) }
    }

    private func runShell(_ tool: String, _ args: [String]) {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: tool)
        p.arguments = args
        try? p.run()
        p.waitUntilExit()
    }

    private func finishUpdate(_ fail: String) {
        DispatchQueue.main.async {
            self.updating = false
            self.note = "Update failed: \(fail). You can still download it from the release page."
        }
    }

    func performSelfUpdate() {
        guard !updating else { return }
        guard !updateAsset.isEmpty, let url = URL(string: updateAsset) else {
            openReleases()
            return
        }
        updating = true
        note = "Downloading update..."
        var req = URLRequest(url: url)
        req.setValue("VSCRelay", forHTTPHeaderField: "User-Agent")
        URLSession.shared.downloadTask(with: req) { [weak self] tmp, _, err in
            guard let self = self else { return }
            guard let tmp = tmp, err == nil else {
                self.finishUpdate("download failed")
                return
            }
            self.installFrom(dmg: tmp)
        }.resume()
    }

    private func installFrom(dmg: URL) {
        let fm = FileManager.default
        let work = fm.temporaryDirectory.appendingPathComponent("vscrelay-update")
        try? fm.removeItem(at: work)
        try? fm.createDirectory(at: work, withIntermediateDirectories: true)
        let dmgPath = work.appendingPathComponent("VSCRelay.dmg")
        do { try fm.moveItem(at: dmg, to: dmgPath) } catch { finishUpdate("temp move"); return }

        let mount = work.appendingPathComponent("mnt")
        runShell("/usr/bin/hdiutil", ["attach", dmgPath.path, "-nobrowse", "-mountpoint", mount.path])
        let apps = (try? fm.contentsOfDirectory(at: mount, includingPropertiesForKeys: nil))?
            .filter { $0.pathExtension == "app" } ?? []
        guard let src = apps.first else {
            runShell("/usr/bin/hdiutil", ["detach", mount.path, "-force"])
            finishUpdate("no app in dmg")
            return
        }
        let newApp = work.appendingPathComponent("VSCRelay.app")
        try? fm.removeItem(at: newApp)
        do { try fm.copyItem(at: src, to: newApp) } catch {
            runShell("/usr/bin/hdiutil", ["detach", mount.path, "-force"])
            finishUpdate("copy failed")
            return
        }
        runShell("/usr/bin/hdiutil", ["detach", mount.path, "-force"])
        runShell("/usr/bin/xattr", ["-dr", "com.apple.quarantine", newApp.path])

        let dest = Bundle.main.bundlePath
        let pid = ProcessInfo.processInfo.processIdentifier
        let scriptPath = work.appendingPathComponent("swap.sh")
        let script = """
        #!/bin/bash
        while kill -0 \(pid) 2>/dev/null; do sleep 0.4; done
        /bin/rm -rf "\(dest)"
        /bin/cp -R "\(newApp.path)" "\(dest)"
        /usr/bin/xattr -dr com.apple.quarantine "\(dest)" 2>/dev/null
        /usr/bin/open "\(dest)"
        """
        do { try script.write(to: scriptPath, atomically: true, encoding: .utf8) } catch {
            finishUpdate("script write")
            return
        }
        runShell("/bin/chmod", ["+x", scriptPath.path])

        let launcher = Process()
        launcher.executableURL = URL(fileURLWithPath: "/bin/sh")
        launcher.arguments = ["-c", "nohup /bin/bash '\(scriptPath.path)' >/dev/null 2>&1 &"]
        try? launcher.run()
        launcher.waitUntilExit()

        DispatchQueue.main.async {
            self.note = "Installing update and restarting..."
            self.stop()
            NSApp.terminate(nil)
        }
    }

    private func semverGreater(_ a: String, _ b: String) -> Bool {
        func parse(_ s: String) -> [Int] {
            s.split(separator: ".").map { Int($0.prefix(while: { $0.isNumber })) ?? 0 }
        }
        let pa = parse(a), pb = parse(b)
        for i in 0..<max(pa.count, pb.count) {
            let x = i < pa.count ? pa[i] : 0
            let y = i < pb.count ? pb[i] : 0
            if x != y { return x > y }
        }
        return false
    }

    var appVersion: String {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0.1.4"
    }

    func checkAppUpdate(manual: Bool = false) {
        guard let url = URL(string: "https://api.github.com/repos/itrootvm/vsc_relay/releases/latest") else { return }
        if manual { DispatchQueue.main.async { self.note = "Checking for updates..." } }
        var req = URLRequest(url: url)
        req.setValue("VSCRelay", forHTTPHeaderField: "User-Agent")
        req.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        URLSession.shared.dataTask(with: req) { [weak self] data, _, _ in
            guard let self = self else { return }
            let current = self.appVersion
            guard let data = data,
                  let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let tag = obj["tag_name"] as? String else {
                if manual { DispatchQueue.main.async { self.note = "Update check failed. Try again later." } }
                return
            }
            let latest = tag.hasPrefix("v") ? String(tag.dropFirst()) : tag
            var dmgURL = ""
            if let assets = obj["assets"] as? [[String: Any]],
               let dmg = assets.first(where: { ($0["name"] as? String)?.hasSuffix(".dmg") == true }),
               let dl = dmg["browser_download_url"] as? String {
                dmgURL = dl
            }
            DispatchQueue.main.async {
                if self.semverGreater(latest, current) {
                    self.appUpdate = tag
                    self.updateAsset = dmgURL
                    if manual { self.note = "" }
                } else {
                    self.appUpdate = ""
                    self.updateAsset = ""
                    if manual { self.note = "You are on the latest version (v\(current))." }
                }
            }
        }.resume()
    }

    func clearLog() { logLines.removeAll() }

    func recentLog(_ n: Int) -> [String] {
        Array(logLines.suffix(n))
    }

    private func appendLog(_ s: String) {
        for p in s.split(separator: "\n", omittingEmptySubsequences: false).map(String.init) where !p.isEmpty {
            logLines.append(p)
            observeLifecycleLog(p)
        }
        if logLines.count > maxLines {
            logLines.removeFirst(logLines.count - maxLines)
        }
    }

    private func logField(_ name: String, in line: String) -> String? {
        guard let start = line.range(of: "\(name)=")?.upperBound else { return nil }
        let suffix = line[start...]
        let token = suffix.prefix { !$0.isWhitespace }
        return String(token).trimmingCharacters(in: CharacterSet(charactersIn: "\""))
    }

    private func observeLifecycleLog(_ line: String) {
        guard line.contains("pipeline=\"event\"") || line.contains("pipeline=event") else { return }
        guard let alias = logField("alias", in: line) else { return }
        let action = logField("action", in: line)
        let kind = logField("kind", in: line)
        if let action,
           ["robot_verified_final", "robot_claimed_complete", "robot_safe_partial", "robot_final"].contains(action) {
            guard let index = promptHistory.firstIndex(where: {
                $0.alias == alias && $0.status != "failed" && $0.status != "robot_verified_final"
            }) else { return }
            promptHistory[index].status = action
            promptHistory[index].completedAt = Date().timeIntervalSince1970
            savePromptHistory()
        } else if kind == "turn_complete",
                  let index = promptHistory.firstIndex(where: {
                      $0.alias == alias
                          && !["failed", "robot_verified_final", "robot_claimed_complete", "robot_safe_partial", "robot_final"].contains($0.status)
                  }),
                  promptHistory[index].observedWorking {
            promptHistory[index].status = "done"
            promptHistory[index].completedAt = Date().timeIntervalSince1970
            savePromptHistory()
        } else if kind == "media_received" {
            let count = logField("count", in: line) ?? "?"
            note = "📎 media received → \(alias) (\(count))"
        }
    }

    private func loadExistingLogTail() {
        guard let text = try? String(contentsOf: logURL, encoding: .utf8) else { return }
        try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: logURL.path)
        logLines = Array(text.split(separator: "\n").suffix(maxLines)).map(String.init)
    }

    func followLog() {
        let attrs = try? FileManager.default.attributesOfItem(atPath: logURL.path)
        let size = (attrs?[.size] as? NSNumber)?.uint64Value ?? 0
        if size < logOffset { logOffset = 0 }
        guard size > logOffset else { return }
        guard let handle = try? FileHandle(forReadingFrom: logURL) else { return }
        defer { try? handle.close() }
        try? handle.seek(toOffset: logOffset)
        let data = (try? handle.readToEnd()) ?? Data()
        logOffset = size
        guard let text = String(data: data, encoding: .utf8), !text.isEmpty else { return }
        DispatchQueue.main.async { self.appendLog(text) }
    }

}

struct HandoffDestination: Decodable, Identifiable, Equatable {
    let id: String
    let label: String
    let workspace: String
    let kind: String
    let linked: Bool
}

struct HandoffOutcome: Decodable {
    let path: String
    let bytes: Int
    let proven: Int
    let remaining: Int
    let compactAuthor: String?
    let prompt: String
    let delivery: String?

    enum CodingKeys: String, CodingKey {
        case path, bytes, proven, remaining, prompt, delivery
        case compactAuthor = "compact_author"
    }
}

struct HandoffReceipt: Decodable {
    let receipt: String?
    let missingAnchors: [String]?

    enum CodingKeys: String, CodingKey {
        case receipt
        case missingAnchors = "missing_anchors"
    }
}

struct SessionCard: Decodable, Identifiable, Equatable {
    let alias: String
    let agent: String
    let sessionId: String
    let title: String?
    let state: String
    let mode: String
    let tapped: Bool
    let rewrite: Bool
    let workspace: String?
    var id: String { sessionId }
    enum CodingKeys: String, CodingKey {
        case alias, agent, title, state, mode, tapped, rewrite, workspace
        case sessionId = "session_id"
    }
}

enum SessionFilter: String, CaseIterable, Identifiable {
    case all, active, attention, robot
    var id: String { rawValue }
    var title: String {
        switch self {
        case .all: return "All"
        case .active: return "Active"
        case .attention: return "Attention"
        case .robot: return "Robot"
        }
    }
}

enum SessionSort: String, CaseIterable, Identifiable {
    case priority, workspace, state
    var id: String { rawValue }
    var title: String {
        switch self {
        case .priority: return "Priority"
        case .workspace: return "Workspace"
        case .state: return "State"
        }
    }
}

enum ConsolePage: String, CaseIterable, Identifiable {
    case sessions, diagnostics
    var id: String { rawValue }
    var title: String { self == .sessions ? "Sessions" : "Diagnostics" }
}

func sessionStateColor(_ state: String) -> Color {
    switch state {
    case "working", "subagent", "subagent_running": return .green
    case "error": return .red
    case "pending_question", "pending_permission": return .orange
    case "idle": return .blue
    default: return .secondary
    }
}

func sessionStateRank(_ state: String) -> Int {
    switch state {
    case "error": return 0
    case "pending_question", "pending_permission": return 1
    case "working", "subagent", "subagent_running": return 2
    case "idle": return 3
    default: return 4
    }
}

func sessionIsActive(_ state: String) -> Bool {
    ["working", "subagent", "subagent_running", "pending_question", "pending_permission"].contains(state)
}

func promptStatusLabel(_ status: String) -> String {
    switch status {
    case "accepted": return "DELIVERED"
    case "done": return "CLAUDE DONE"
    case "robot_verified_final": return "VERIFIED FINAL"
    case "robot_claimed_complete": return "CLAIMED"
    case "robot_safe_partial": return "SAFE PARTIAL"
    case "robot_final": return "LEGACY · UNVERIFIED"
    default: return status.replacingOccurrences(of: "_", with: " ").uppercased()
    }
}

func promptStatusExplanation(_ status: String) -> String {
    switch status {
    case "sending": return "handing the prompt to the session"
    case "accepted": return "prompt accepted; completion has not arrived yet"
    case "working": return "the coding session is processing this prompt"
    case "done": return "the coding agent finished its turn"
    case "robot_verified_final": return "Contract Ledger verified every active obligation with fresh evidence"
    case "robot_claimed_complete": return "a helper accepted the agent's claim; Ledger verification is absent"
    case "robot_safe_partial": return "Robot stopped safely with unresolved or unprovable obligations"
    case "robot_final": return "legacy helper final; this was not evidence-backed"
    case "failed": return "the prompt was not delivered"
    default: return status
    }
}

func promptStatusColor(_ status: String) -> Color {
    switch status {
    case "done", "robot_verified_final": return .green
    case "robot_claimed_complete", "robot_final": return .purple
    case "robot_safe_partial": return .orange
    case "failed": return .red
    case "working": return .blue
    case "accepted": return .purple
    default: return .secondary
    }
}

func promptNeedsAttention(_ status: String) -> Bool {
    ["failed", "robot_claimed_complete", "robot_safe_partial", "robot_final"].contains(status)
}

struct HealthInfo {
    let status: String
    let detail: String
}

struct ProviderRow: Identifiable {
    let id: String
    let reason: String
    let available: Bool
    let enabled: Bool
    let model: String
}

func healthColor(_ status: String) -> Color {
    switch status {
    case "ok": return .green
    case "needs-login": return .orange
    case "unavailable", "no-model": return .secondary
    default: return .red
    }
}

struct ProviderRowView: View {
    let row: ProviderRow
    let health: HealthInfo?
    @ObservedObject var ctl: RelayController
    @State private var key = ""

    private var isCli: Bool { row.id.hasSuffix("-cli") || row.id == "antigravity" }
    private var supportsInteractiveLogin: Bool {
        ["claude-cli", "codex-cli", "cursor-cli"].contains(row.id)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Toggle("", isOn: Binding(
                    get: { row.enabled },
                    set: { ctl.setProviderEnabled(row.id, $0) }
                )).labelsHidden()
                Text(row.id).bold()
                if let h = health {
                    Text(h.status).font(.caption2).foregroundStyle(healthColor(h.status))
                } else {
                    Circle().fill(row.available ? Color.green : Color.gray).frame(width: 8, height: 8)
                }
                if !row.model.isEmpty {
                    Text("model: \(row.model)").font(.caption2).foregroundStyle(.secondary)
                }
                Spacer()
            }
            if let h = health, h.status != "ok", !h.detail.isEmpty {
                Text(h.detail).font(.caption2).foregroundStyle(.secondary).lineLimit(2)
            } else if !row.available {
                Text(row.reason).font(.caption2).foregroundStyle(.secondary).lineLimit(2)
            }
            HStack {
                if row.id != "ollama" {
                    SecureField("API key", text: $key).textFieldStyle(.roundedBorder).frame(width: 180)
                    Button("Set key") {
                        let k = key.trimmingCharacters(in: .whitespacesAndNewlines)
                        if !k.isEmpty {
                            ctl.setProviderKey(row.id, k)
                            key = ""
                        }
                    }
                }
                if supportsInteractiveLogin {
                    Button("Login") { ctl.loginBackend(row.id) }
                } else if isCli {
                    Button("Test session") { ctl.loginBackend(row.id) }
                    Text("uses existing CLI/app session").font(.caption2).foregroundStyle(.secondary)
                }
                Spacer()
            }
        }
        .padding(8)
        .background(Color(nsColor: .controlBackgroundColor))
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }
}

struct PromptHistoryRow: View {
    let entry: PromptHistoryEntry

    private var statusLabel: String {
        promptStatusLabel(entry.status)
    }

    private var statusExplanation: String {
        promptStatusExplanation(entry.status)
    }

    private var statusColor: Color {
        promptStatusColor(entry.status)
    }

    private var byteSummary: String {
        guard let delivered = entry.deliveredBytes else { return "\(entry.inputBytes) B" }
        return "\(entry.inputBytes) B → \(delivered) B"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 7) {
                Text(statusLabel)
                    .font(.caption2).bold().foregroundStyle(statusColor)
                Text(Date(timeIntervalSince1970: entry.submittedAt), style: .time)
                    .font(.caption2).foregroundStyle(.secondary)
                Text(byteSummary).font(.caption2).foregroundStyle(.secondary)
                Spacer()
            }
            Text(statusExplanation).font(.caption2).foregroundStyle(.secondary)
            Text(entry.prompt)
                .font(.callout)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
            if let delivered = entry.deliveredText, delivered != entry.prompt {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Delivered after improvement").font(.caption2).bold().foregroundStyle(.purple)
                    Text(delivered).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                }
                .padding(.top, 2)
            }
        }
        .padding(8)
        .background(statusColor.opacity(0.07))
        .clipShape(RoundedRectangle(cornerRadius: 6))
    }
}

struct SessionListRow: View {
    let card: SessionCard
    let latest: PromptHistoryEntry?
    @ObservedObject var ctl: RelayController

    private var modeColor: Color {
        switch card.mode {
        case "auto": return .green
        case "robot": return .purple
        default: return .secondary
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Circle()
                .fill(sessionStateColor(card.state))
                .frame(width: 9, height: 9)
                .padding(.top, 5)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 5) {
                    Text((card.title?.isEmpty == false) ? card.title! : card.sessionId)
                        .font(.subheadline).bold().lineLimit(1)
                    Spacer(minLength: 4)
                    Text(card.mode.uppercased())
                        .font(.caption2).bold().foregroundStyle(modeColor)
                    Menu {
                        Section("Session mode") {
                            ForEach(["manual", "auto", "robot"], id: \.self) { mode in
                                Button(card.mode == mode ? "✓ \(mode.capitalized)" : mode.capitalized) {
                                    if card.mode != mode { ctl.setSessionMode(card.sessionId, mode) }
                                }
                            }
                        }
                    } label: {
                        Image(systemName: "ellipsis")
                            .font(.caption).bold()
                            .foregroundStyle(Color.primary)
                            .frame(width: 20, height: 18)
                            .background(Color.primary.opacity(0.07))
                            .clipShape(RoundedRectangle(cornerRadius: 4))
                    }
                    .menuStyle(.borderlessButton)
                    .fixedSize()
                    .help("Session actions")
                }
                HStack(spacing: 5) {
                    Text(card.agent).font(.caption2).foregroundStyle(.secondary)
                    Text(card.state.replacingOccurrences(of: "_", with: " "))
                        .font(.caption2).foregroundStyle(sessionStateColor(card.state))
                    if card.rewrite {
                        Image(systemName: "wand.and.stars").font(.caption2).foregroundStyle(.purple)
                    }
                    if !card.tapped {
                        Image(systemName: "link.slash").font(.caption2).foregroundStyle(.secondary)
                    }
                    Spacer()
                }
                if let latest {
                    HStack(spacing: 5) {
                        Text(promptStatusLabel(latest.status))
                            .font(.caption2).bold().foregroundStyle(promptStatusColor(latest.status))
                        Text(latest.prompt).font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
            }
        }
        .padding(.vertical, 4)
        .contentShape(Rectangle())
    }
}

func pickMediaFiles() -> [String] {
    let panel = NSOpenPanel()
    panel.allowsMultipleSelection = true
    panel.canChooseDirectories = false
    panel.canChooseFiles = true
    panel.resolvesAliases = true
    if panel.runModal() == .OK {
        return panel.urls.map { $0.path }
    }
    return []
}

struct SessionDetailView: View {
    let card: SessionCard
    @ObservedObject var ctl: RelayController
    @State private var compose = ""
    @State private var attachments: [String] = []
    @State private var usageText = ""
    @State private var pinsText = ""
    @State private var pinTarget = ""
    @State private var pinObligation = ""
    @State private var pinEpoch = ""
    @State private var handoffDestinations: [HandoffDestination] = []
    @State private var handoffTarget = ""
    @State private var handoffOutcome: HandoffOutcome?
    @State private var handoffReceipt: HandoffReceipt?
    @State private var handoffBusy = false
    @State private var handoffLoaded = false
    @State private var reviewBusy = false
    @State private var reviewOutput = ""
    @State private var reviewDepthChoice = "normal"

    private var canSend: Bool {
        card.tapped
            && (!compose.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                || !attachments.isEmpty)
    }

    private var promptHistory: [PromptHistoryEntry] {
        ctl.history(for: card.sessionId)
    }

    private var handoffPlaceholder: String {
        if handoffBusy && handoffDestinations.isEmpty { return "Looking for agents…" }
        if handoffLoaded && handoffDestinations.isEmpty { return "No other agent available" }
        if handoffDestinations.isEmpty { return "Not loaded yet" }
        return "Pick an agent"
    }

    private var handoffChats: [HandoffDestination] {
        handoffDestinations.filter { $0.kind == "chat" }
    }

    private var handoffClis: [HandoffDestination] {
        handoffDestinations.filter { $0.kind == "cli" }
    }

    private var handoffApps: [HandoffDestination] {
        handoffDestinations.filter { $0.kind == "app" }
    }

    private var handoffWorkspace: String {
        handoffDestinations.first(where: { $0.id == handoffTarget })?.workspace ?? ""
    }

    private func loadHandoffTargets() {
        handoffBusy = true
        ctl.loadHandoffDestinations(card.sessionId) { rows in
            handoffDestinations = rows
            handoffLoaded = true
            handoffBusy = false
            if !rows.contains(where: { $0.id == handoffTarget }) { handoffTarget = "" }
        }
    }

    private func resetHandoff() {
        handoffDestinations = []
        handoffTarget = ""
        handoffOutcome = nil
        handoffReceipt = nil
        handoffLoaded = false
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                HStack(alignment: .top, spacing: 10) {
                    Circle().fill(sessionStateColor(card.state)).frame(width: 11, height: 11).padding(.top, 5)
                    VStack(alignment: .leading, spacing: 3) {
                        Text((card.title?.isEmpty == false) ? card.title! : "Untitled session")
                            .font(.title3).bold().lineLimit(2)
                        HStack(spacing: 6) {
                            Text(card.alias).font(.subheadline)
                            Text("· \(card.agent)").font(.caption).foregroundStyle(.secondary)
                            Text(card.state.replacingOccurrences(of: "_", with: " "))
                                .font(.caption).foregroundStyle(sessionStateColor(card.state))
                            if card.rewrite {
                                Label("rewrite", systemImage: "wand.and.stars")
                                    .font(.caption2).foregroundStyle(.purple)
                            }
                        }
                    }
                    Spacer()
                    Button { ctl.refreshSessions() } label: { Image(systemName: "arrow.clockwise") }
                        .buttonStyle(.borderless)
                }
                Text(card.sessionId)
                    .font(.system(size: 10, design: .monospaced))
                    .foregroundStyle(.tertiary)
                    .textSelection(.enabled)
                Picker("Mode", selection: Binding(
                    get: { card.mode },
                    set: { if $0 != card.mode { ctl.setSessionMode(card.sessionId, $0) } }
                )) {
                    Text("Manual").tag("manual")
                    Text("Auto").tag("auto")
                    Text("Robot").tag("robot")
                }
                .pickerStyle(.segmented)
                .frame(maxWidth: 360)
            }
            .padding(14)

            Divider()

            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    VStack(alignment: .leading, spacing: 6) {
                        HStack {
                            Text("Send to session").font(.headline)
                            Spacer()
                            if !card.tapped {
                                Label("No background channel", systemImage: "link.slash")
                                    .font(.caption).foregroundStyle(.orange)
                            }
                        }
                        TextEditor(text: $compose)
                            .frame(minHeight: 86, maxHeight: 150)
                            .font(.callout)
                            .disabled(!card.tapped)
                            .overlay(RoundedRectangle(cornerRadius: 6).stroke(Color.secondary.opacity(0.3)))
                        if !attachments.isEmpty {
                            VStack(alignment: .leading, spacing: 3) {
                                ForEach(Array(attachments.enumerated()), id: \.offset) { idx, path in
                                    HStack(spacing: 6) {
                                        Image(systemName: "paperclip").font(.caption2)
                                        Text((path as NSString).lastPathComponent)
                                            .font(.caption).lineLimit(1).truncationMode(.middle)
                                        Button {
                                            attachments.remove(at: idx)
                                        } label: {
                                            Image(systemName: "xmark.circle.fill").font(.caption2)
                                        }
                                        .buttonStyle(.plain)
                                    }
                                }
                            }
                        }
                        HStack {
                            Button("Send") {
                                let text = compose.trimmingCharacters(in: .whitespacesAndNewlines)
                                if !text.isEmpty || !attachments.isEmpty {
                                    ctl.sendPrompt(card.alias, card.sessionId, text, media: attachments)
                                    compose = ""
                                    attachments = []
                                }
                            }
                            .buttonStyle(.borderedProminent)
                            .disabled(!canSend)
                            Button {
                                attachments.append(contentsOf: pickMediaFiles())
                            } label: {
                                Label("Attach", systemImage: "paperclip")
                            }
                            .disabled(!card.tapped)
                            Menu {
                                Button("Default order") {
                                    reviewBusy = true
                                    reviewOutput = ""
                                    ctl.runReview(card.sessionId, reviewer: nil, depth: reviewDepthChoice) { text in
                                        reviewBusy = false
                                        reviewOutput = text
                                    }
                                }
                                ForEach(reviewerChoices(for: card.agent), id: \.self) { id in
                                    Button(id) {
                                        reviewBusy = true
                                        reviewOutput = ""
                                        ctl.runReview(card.sessionId, reviewer: id, depth: reviewDepthChoice) { text in
                                            reviewBusy = false
                                            reviewOutput = text
                                        }
                                    }
                                }
                                Divider()
                                Picker("Depth", selection: $reviewDepthChoice) {
                                    Text("shallow").tag("shallow")
                                    Text("normal").tag("normal")
                                    Text("deep").tag("deep")
                                }
                            } label: {
                                Label("Cross review", systemImage: "magnifyingglass")
                            }
                            .fixedSize()
                            .disabled(reviewBusy)
                            if reviewBusy { ProgressView().controlSize(.small) }
                            if card.rewrite {
                                Text("Prompt improvement is enabled").font(.caption).foregroundStyle(.purple)
                            }
                            Spacer()
                        }
                        if !reviewOutput.isEmpty {
                            ScrollView {
                                Text(reviewOutput)
                                    .font(.system(size: 11, design: .monospaced))
                                    .textSelection(.enabled)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .frame(maxHeight: 160)
                            .padding(6)
                            .background(Color.secondary.opacity(0.08))
                            .clipShape(RoundedRectangle(cornerRadius: 6))
                        }
                    }
                    .padding(12)
                    .background(Color(nsColor: .controlBackgroundColor))
                    .clipShape(RoundedRectangle(cornerRadius: 8))

                    VStack(alignment: .leading, spacing: 6) {
                        HStack {
                            Text("Hand off").font(.headline)
                            Spacer()
                            if handoffBusy { ProgressView().controlSize(.small) }
                            Button("Refresh") { loadHandoffTargets() }
                                .disabled(handoffBusy)
                        }
                        Text(
                            "Moves this session to another agent. HANDOFF.md is written next to the work: the contract in your own words, the plan in force, a compact this agent writes about its own work, what is proven, what is open, and the files it touched. A tapped Claude chat gets the prompt directly, an agent CLI is started in a terminal already holding it, and an editor is opened on this project for you to paste it in."
                        )
                        .font(.caption).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)

                        HStack {
                            Picker("To", selection: $handoffTarget) {
                                Text(handoffPlaceholder).tag("")
                                if !handoffChats.isEmpty {
                                    Section("Claude chats") {
                                        ForEach(handoffChats) { d in
                                            Text("\(d.linked ? "→" : "·")  \(d.label)").tag(d.id)
                                        }
                                    }
                                }
                                if !handoffClis.isEmpty {
                                    Section("Agent CLIs") {
                                        ForEach(handoffClis) { d in
                                            Text(d.label).tag(d.id)
                                        }
                                    }
                                }
                                if !handoffApps.isEmpty {
                                    Section("Editors") {
                                        ForEach(handoffApps) { d in
                                            Text(d.label).tag(d.id)
                                        }
                                    }
                                }
                            }
                            .labelsHidden()
                            .disabled(handoffDestinations.isEmpty)
                            Button("Hand off") {
                                handoffBusy = true
                                handoffReceipt = nil
                                handoffOutcome = nil
                                ctl.runHandoff(card.sessionId, to: handoffTarget) { outcome in
                                    handoffOutcome = outcome
                                    handoffBusy = false
                                }
                            }
                            .disabled(handoffBusy || handoffTarget.isEmpty)
                        }
                        if handoffOutcome != nil {
                            HStack {
                                Button("Copy prompt") {
                                    NSPasteboard.general.clearContents()
                                    NSPasteboard.general.setString(
                                        handoffOutcome?.prompt ?? "", forType: .string)
                                }
                                Button("Check receipt") {
                                    ctl.loadHandoffReceipt(handoffWorkspace) { handoffReceipt = $0 }
                                }
                            }
                        }

                        if let outcome = handoffOutcome {
                            Text(
                                "\(outcome.path)\n\(outcome.proven) proven · \(outcome.remaining) open · \(outcome.bytes) bytes · compact by \(outcome.compactAuthor ?? "nobody")\n\(outcome.delivery ?? "")"
                            )
                            .font(.system(size: 11, design: .monospaced))
                            .textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                        }

                        if let receipt = handoffReceipt {
                            if let body = receipt.receipt {
                                Text(body)
                                    .font(.system(size: 11, design: .monospaced))
                                    .textSelection(.enabled)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                if let missing = receipt.missingAnchors, !missing.isEmpty {
                                    Text("the receipt never mentions \(missing.joined(separator: ", "))")
                                        .font(.caption).foregroundStyle(.orange)
                                } else {
                                    Text("no contract anchor is missing from the receipt")
                                        .font(.caption).foregroundStyle(.green)
                                }
                            } else {
                                Text("No receipt yet - the receiving agent writes it once it has read the brief.")
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    }
                    .padding(12)
                    .background(Color(nsColor: .controlBackgroundColor))
                    .clipShape(RoundedRectangle(cornerRadius: 8))

                    VStack(alignment: .leading, spacing: 6) {
                        HStack {
                            Text("Usage").font(.headline)
                            Spacer()
                            Button("Refresh") {
                                ctl.loadUsage(card.sessionId) { usageText = $0 }
                            }
                        }
                        if usageText.isEmpty {
                            Text("No usage loaded yet.").font(.caption).foregroundStyle(.secondary)
                        } else {
                            Text(usageText)
                                .font(.system(size: 11, design: .monospaced))
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                    .padding(12)
                    .background(Color(nsColor: .controlBackgroundColor))
                    .clipShape(RoundedRectangle(cornerRadius: 8))

                    VStack(alignment: .leading, spacing: 6) {
                        HStack {
                            Text("Gate pins").font(.headline)
                            Spacer()
                            Button("List") {
                                ctl.loadPins(card.sessionId) { pinsText = $0 }
                            }
                            Button("Clear pins") {
                                ctl.clearPins(card.sessionId) { _ in
                                    ctl.loadPins(card.sessionId) { pinsText = $0 }
                                }
                            }
                        }
                        if pinsText.isEmpty {
                            Text("No pins loaded yet.").font(.caption).foregroundStyle(.secondary)
                        } else {
                            Text(pinsText)
                                .font(.system(size: 11, design: .monospaced))
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                        HStack {
                            TextField("target", text: $pinTarget).textFieldStyle(.roundedBorder)
                            TextField("obligation", text: $pinObligation).textFieldStyle(.roundedBorder)
                            TextField("epoch", text: $pinEpoch).textFieldStyle(.roundedBorder).frame(width: 80)
                            Button("Add") {
                                let t = pinTarget.trimmingCharacters(in: .whitespaces)
                                let o = pinObligation.trimmingCharacters(in: .whitespaces)
                                let e = pinEpoch.trimmingCharacters(in: .whitespaces)
                                if !t.isEmpty && !o.isEmpty && !e.isEmpty {
                                    ctl.addPin(card.sessionId, target: t, obligation: o, epoch: e) { _ in
                                        ctl.loadPins(card.sessionId) { pinsText = $0 }
                                    }
                                    pinTarget = ""; pinObligation = ""; pinEpoch = ""
                                }
                            }
                        }
                    }
                    .padding(12)
                    .background(Color(nsColor: .controlBackgroundColor))
                    .clipShape(RoundedRectangle(cornerRadius: 8))

                    HStack {
                        Text("Prompt history").font(.headline)
                        Text("\(promptHistory.count)").font(.caption).foregroundStyle(.secondary)
                        Spacer()
                        Text("CLAIMED ≠ VERIFIED").font(.caption2).foregroundStyle(.secondary)
                    }

                    if promptHistory.isEmpty {
                        ContentUnavailableView(
                            "No local prompt history",
                            systemImage: "clock.arrow.circlepath",
                            description: Text("Prompts sent through Relay will appear here.")
                        )
                        .frame(maxWidth: .infinity, minHeight: 180)
                    } else {
                        ForEach(promptHistory) { entry in
                            PromptHistoryRow(entry: entry)
                        }
                    }
                }
                .padding(14)
            }
        }
        .background(Color(nsColor: .windowBackgroundColor))
        .onAppear { loadHandoffTargets() }
        .onChange(of: card.sessionId) { _, _ in
            resetHandoff()
            loadHandoffTargets()
        }
    }
}

struct ContentView: View {
    @ObservedObject var ctl = RelayController.shared
    @State private var gateLogOnly = false
    @State private var logAlias = "all"
    @State private var logSearch = ""
    @State private var sessionSearch = ""
    @State private var sessionFilter = SessionFilter.all
    @State private var sessionSort = SessionSort.priority
    @State private var selectedSessionId: String?
    @State private var page = ConsolePage.sessions

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            header
            updateBanner
            warningBanner
            statsRow
            controls
            automationStatusStrip
            if !ctl.note.isEmpty {
                Text(ctl.note).font(.caption).foregroundStyle(.secondary).lineLimit(2)
            }
            switch page {
            case .sessions: sessionConsole
            case .diagnostics: logView
            }
        }
        .padding(14)
        .frame(minWidth: 1000, minHeight: 700)
        .sheet(isPresented: $ctl.showSettings) { SettingsView(ctl: ctl) }
        .sheet(isPresented: $ctl.showHelp) { HelpView(ctl: ctl) }
        .onAppear {
            normalizeSelection()
            ctl.adoptRunningDaemon()
            ctl.refreshAutomationStatus()
        }
        .onChange(of: ctl.sessions.map(\.sessionId)) { _, _ in normalizeSelection() }
        .onChange(of: sessionFilter) { _, _ in normalizeSelection() }
        .onChange(of: sessionSearch) { _, _ in normalizeSelection() }
    }

    private var latestBySession: [String: PromptHistoryEntry] {
        var result: [String: PromptHistoryEntry] = [:]
        for entry in ctl.promptHistory where result[entry.sessionId] == nil {
            result[entry.sessionId] = entry
        }
        return result
    }

    private func needsAttention(
        _ card: SessionCard,
        latest: [String: PromptHistoryEntry]
    ) -> Bool {
        ["error", "pending_question", "pending_permission"].contains(card.state)
            || (latest[card.sessionId].map { promptNeedsAttention($0.status) } ?? false)
    }

    private func priority(_ card: SessionCard, latest: [String: PromptHistoryEntry]) -> Int {
        (needsAttention(card, latest: latest) ? 0 : 100)
            + sessionStateRank(card.state) * 10
            + (card.mode == "robot" ? 0 : 2)
    }

    private var filteredSessions: [SessionCard] {
        let latest = latestBySession
        let query = sessionSearch.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        return ctl.sessions
            .filter { card in
                let matchesQuery = query.isEmpty || [card.alias, card.agent, card.sessionId, card.title ?? ""]
                    .contains { $0.lowercased().contains(query) }
                let matchesFilter: Bool
                switch sessionFilter {
                case .all: matchesFilter = true
                case .active: matchesFilter = sessionIsActive(card.state)
                case .attention: matchesFilter = needsAttention(card, latest: latest)
                case .robot: matchesFilter = card.mode == "robot"
                }
                return matchesQuery && matchesFilter
            }
            .sorted { left, right in
                switch sessionSort {
                case .priority:
                    return (priority(left, latest: latest), left.alias.lowercased(), left.title ?? left.sessionId)
                        < (priority(right, latest: latest), right.alias.lowercased(), right.title ?? right.sessionId)
                case .workspace:
                    return (left.alias.lowercased(), left.title ?? left.sessionId)
                        < (right.alias.lowercased(), right.title ?? right.sessionId)
                case .state:
                    return (sessionStateRank(left.state), left.alias.lowercased(), left.title ?? left.sessionId)
                        < (sessionStateRank(right.state), right.alias.lowercased(), right.title ?? right.sessionId)
                }
            }
    }

    private func groupedSessions(
        _ sessions: [SessionCard],
        latest: [String: PromptHistoryEntry]
    ) -> [(alias: String, sessions: [SessionCard])] {
        let groups = Dictionary(grouping: sessions, by: \.alias)
        return groups.map { (alias: $0.key, sessions: $0.value) }.sorted { left, right in
            if sessionSort == .priority {
                let lp = left.sessions.map { priority($0, latest: latest) }.min() ?? Int.max
                let rp = right.sessions.map { priority($0, latest: latest) }.min() ?? Int.max
                if lp != rp { return lp < rp }
            }
            return left.alias.localizedCaseInsensitiveCompare(right.alias) == .orderedAscending
        }
    }

    private var selectedSession: SessionCard? {
        guard let selectedSessionId else { return nil }
        return ctl.sessions.first { $0.sessionId == selectedSessionId }
    }

    private var attentionCount: Int {
        let latest = latestBySession
        return ctl.sessions.filter { needsAttention($0, latest: latest) }.count
    }
    private var activeCount: Int { ctl.sessions.filter { sessionIsActive($0.state) }.count }
    private var robotCount: Int { ctl.sessions.filter { $0.mode == "robot" }.count }

    private func normalizeSelection() {
        let visibleIds = Set(filteredSessions.map(\.sessionId))
        if selectedSessionId == nil || !visibleIds.contains(selectedSessionId ?? "") {
            selectedSessionId = filteredSessions.first?.sessionId
        }
    }

    private var warningMessage: String? {
        if !ctl.vscodeOK { return "VS Code was not found. Install it and open a project." }
        if !ctl.extensionOK { return "Claude Code extension not found in VS Code." }
        if !ctl.shimInstalled { return "Background control is off. The extension may have updated." }
        return nil
    }

    @ViewBuilder private var warningBanner: some View {
        if let msg = warningMessage {
            HStack(spacing: 8) {
                Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                Text(msg).font(.callout)
                Spacer()
                if ctl.extensionOK && !ctl.shimInstalled {
                    Button("Reinstall shim") { ctl.installShim() }
                }
            }
            .padding(10)
            .background(Color.orange.opacity(0.12))
            .clipShape(RoundedRectangle(cornerRadius: 8))
        }
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(systemName: "antenna.radiowaves.left.and.right")
                .font(.title2)
                .foregroundStyle(ctl.isRunning ? .green : .secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text("VS Code Agent Relay").font(.headline)
                Text(ctl.isRunning ? "Running (pid \(ctl.pid))" : "Stopped")
                    .font(.subheadline)
                    .foregroundStyle(ctl.isRunning ? .green : .secondary)
                if ctl.extVersion != "-" {
                    Text("Claude Code \(ctl.extVersion)").font(.caption).foregroundStyle(.secondary)
                }
            }
            Spacer()
            Picker("Page", selection: $page) {
                ForEach(ConsolePage.allCases) { page in Text(page.title).tag(page) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .frame(width: 230)
            Button { ctl.showHelp = true } label: {
                Label("Help", systemImage: "questionmark.circle")
            }
            Button { ctl.showSettings = true } label: {
                Label("Settings", systemImage: "gearshape")
            }
        }
    }

    @ViewBuilder private var updateBanner: some View {
        if !ctl.appUpdate.isEmpty {
            HStack(spacing: 8) {
                Image(systemName: "arrow.down.circle.fill").foregroundStyle(.blue)
                Text(ctl.updating
                    ? "Updating to \(ctl.appUpdate) and restarting..."
                    : "App update \(ctl.appUpdate) is available.")
                    .font(.callout)
                Spacer()
                if ctl.updating {
                    ProgressView().controlSize(.small)
                } else {
                    Button("Update now") { ctl.performSelfUpdate() }.buttonStyle(.borderedProminent)
                    Button("Page") { ctl.openReleases() }
                }
            }
            .padding(10)
            .background(Color.blue.opacity(0.12))
            .clipShape(RoundedRectangle(cornerRadius: 8))
        }
    }

    private var statsRow: some View {
        HStack(spacing: 8) {
            statCard(
                title: "Sessions",
                value: "\(ctl.sessions.count)",
                detail: "\(ctl.sessionsToday) opened today",
                icon: "rectangle.stack"
            )
            statCard(
                title: "Active",
                value: "\(activeCount)",
                detail: "\(ctl.turnsToday) turns today",
                icon: "bolt.fill"
            )
            statCard(
                title: "Attention",
                value: "\(attentionCount)",
                detail: "questions, errors, weak finals",
                icon: "exclamationmark.triangle"
            )
            statCard(
                title: "Robot",
                value: "\(robotCount)",
                detail: "shim \(ctl.shimInstalled ? "on" : "off")",
                icon: "gearshape.2"
            )
        }
    }

    private func statCard(title: String, value: String, detail: String, icon: String) -> some View {
        HStack(spacing: 8) {
            Image(systemName: icon).foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                HStack(alignment: .firstTextBaseline, spacing: 5) {
                    Text(value).font(.system(size: 18, weight: .semibold))
                    Text(title).font(.caption).foregroundStyle(.secondary)
                }
                Text(detail).font(.caption2).foregroundStyle(.tertiary).lineLimit(1)
            }
            Spacer()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 10).padding(.vertical, 8)
        .background(Color(nsColor: .controlBackgroundColor))
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }

    private var controls: some View {
        HStack(spacing: 10) {
            if ctl.isRunning {
                Button(role: .destructive) { ctl.stop() } label: { Label("Stop", systemImage: "stop.fill") }
            } else {
                Button { ctl.start() } label: { Label("Start", systemImage: "play.fill") }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.return)
            }
            Divider().frame(height: 18)
            if ctl.shimInstalled {
                Button { ctl.uninstallShim() } label: { Text("Remove shim") }
            } else {
                Button { ctl.installShim() } label: { Text("Install shim") }
            }
            Button { ctl.openAccessibility() } label: { Text("Accessibility") }
            Spacer()
            Button { ctl.refreshSessions() } label: { Label("Refresh", systemImage: "arrow.clockwise") }
        }
    }

    private var automationStatusStrip: some View {
        HStack(spacing: 8) {
            statusPill(
                "Compass \(ctl.smartEnabled ? "on" : "off")",
                systemImage: "safari",
                color: ctl.smartEnabled ? .blue : .secondary
            )
            statusPill(
                "Steer \(ctl.steerEnabled ? "on" : "off")",
                systemImage: "steeringwheel",
                color: ctl.steerEnabled ? .purple : .secondary
            )
            statusPill(
                "Gate \(ctl.gateEnabled ? "on" : "off")",
                systemImage: "shield",
                color: ctl.gateEnabled ? .green : .secondary
            )
            Text("semantic: \(ctl.semanticBackend)")
                .font(.caption2)
                .foregroundStyle(.secondary)
            if ctl.steerEnabled && ctl.semanticBackend == "local" && !ctl.allowUncalibratedSteer {
                Label("uncalibrated semantic actions blocked", systemImage: "lock.fill")
                    .font(.caption2)
                    .foregroundStyle(.orange)
            }
            Spacer()
            Button("Configure") {
                ctl.showSettings = true
            }
            .buttonStyle(.borderless)
            .font(.caption)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .background(Color(nsColor: .controlBackgroundColor).opacity(0.7))
        .clipShape(RoundedRectangle(cornerRadius: 7))
    }

    private func statusPill(_ text: String, systemImage: String, color: Color) -> some View {
        Label(text, systemImage: systemImage)
            .font(.caption2).bold()
            .foregroundStyle(color)
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(color.opacity(0.1))
            .clipShape(Capsule())
    }

    private var sessionConsole: some View {
        let latest = latestBySession
        let visible = filteredSessions
        let groups = groupedSessions(visible, latest: latest)
        return HSplitView {
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 6) {
                    Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                    TextField("Search title, workspace, id…", text: $sessionSearch)
                        .textFieldStyle(.plain)
                    if !sessionSearch.isEmpty {
                        Button { sessionSearch = "" } label: { Image(systemName: "xmark.circle.fill") }
                            .buttonStyle(.borderless).foregroundStyle(.secondary)
                    }
                }
                .padding(7)
                .background(Color(nsColor: .controlBackgroundColor))
                .clipShape(RoundedRectangle(cornerRadius: 7))

                Picker("Filter", selection: $sessionFilter) {
                    ForEach(SessionFilter.allCases) { filter in Text(filter.title).tag(filter) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()

                HStack {
                    Text("\(visible.count) of \(ctl.sessions.count)")
                        .font(.caption).foregroundStyle(.secondary)
                    Spacer()
                    Picker("Sort", selection: $sessionSort) {
                        ForEach(SessionSort.allCases) { sort in Text(sort.title).tag(sort) }
                    }
                    .labelsHidden().frame(width: 120)
                }

                if visible.isEmpty {
                    VStack(spacing: 10) {
                        Spacer(minLength: 24)
                        Image(systemName: "rectangle.stack.badge.minus")
                            .font(.system(size: 42))
                            .foregroundStyle(.tertiary)
                        Text("No matching sessions")
                            .font(.title3).bold()
                            .foregroundStyle(.secondary)
                        Text("Change search or filter.")
                            .font(.callout)
                            .foregroundStyle(.tertiary)
                        Spacer()
                    }
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
                } else {
                    List(selection: $selectedSessionId) {
                        ForEach(groups, id: \.alias) { group in
                            Section {
                                ForEach(group.sessions) { card in
                                    SessionListRow(card: card, latest: latest[card.sessionId], ctl: ctl)
                                        .tag(card.sessionId)
                                }
                            } header: {
                                HStack {
                                    Text(group.alias)
                                    Spacer()
                                    Text("\(group.sessions.count)").foregroundStyle(.tertiary)
                                }
                            }
                        }
                    }
                    .listStyle(.sidebar)
                }
            }
            .padding(10)
            .frame(
                minWidth: 290,
                idealWidth: 350,
                maxWidth: 460,
                maxHeight: .infinity,
                alignment: .topLeading
            )

            if let selectedSession {
                SessionDetailView(card: selectedSession, ctl: ctl)
                    .id(selectedSession.sessionId)
                    .frame(minWidth: 540, maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ContentUnavailableView(
                    "Select a session",
                    systemImage: "sidebar.left",
                    description: Text("Use search and filters to find a chat.")
                )
                .frame(minWidth: 540, maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .background(Color(nsColor: .controlBackgroundColor).opacity(0.35))
        .clipShape(RoundedRectangle(cornerRadius: 9))
        .overlay(RoundedRectangle(cornerRadius: 9).stroke(Color.secondary.opacity(0.18)))
    }

    private var logView: some View {
        let aliases = Array(Set(ctl.sessions.map(\.alias))).sorted()
        let query = logSearch.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        let rows = Array(ctl.logLines.enumerated()).filter {
            (!gateLogOnly || $0.element.contains("[gate]"))
                && (logAlias == "all" || $0.element.contains("alias=\(logAlias)"))
                && (query.isEmpty || $0.element.lowercased().contains(query))
        }
        return VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Runtime diagnostics").font(.headline)
                Text("\(rows.count) / \(ctl.logLines.count)").font(.caption).foregroundStyle(.secondary)
                TextField("Filter log…", text: $logSearch).textFieldStyle(.roundedBorder).frame(maxWidth: 260)
                Picker("Session", selection: $logAlias) {
                    Text("all sessions").tag("all")
                    ForEach(aliases, id: \.self) { alias in Text(alias).tag(alias) }
                }
                .frame(width: 190)
                Toggle("Gate trace only", isOn: $gateLogOnly).toggleStyle(.switch).controlSize(.small)
                Spacer()
                Button("Clear view") { ctl.clearLog() }
            }
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 1) {
                        ForEach(rows, id: \.offset) { idx, line in
                            Text(line)
                                .font(.system(size: 11, design: .monospaced))
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .id(idx)
                        }
                    }
                    .padding(8)
                }
                .background(Color(nsColor: .textBackgroundColor))
                .clipShape(RoundedRectangle(cornerRadius: 6))
                .overlay(RoundedRectangle(cornerRadius: 6).stroke(Color.secondary.opacity(0.25)))
                .onChange(of: ctl.logLines.count) { _, _ in
                    if let last = rows.last { proxy.scrollTo(last.offset, anchor: .bottom) }
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct SettingsView: View {
    @ObservedObject var ctl: RelayController
    @Environment(\.dismiss) private var dismiss
    @State private var revealToken = false
    @State private var revealSecret = false
    @State private var autoMode = "manual"
    @State private var smartOn = false
    @State private var steerOn = false
    @State private var gateOn = false
    @State private var feedbackOn = true
    @State private var budgetText = ""
    @State private var semanticProvider = "local"
    @State private var semanticModel = ""
    @State private var semanticEndpoint = ""
    @State private var semanticLocalDir = ""
    @State private var semanticTrainDataset = ""
    @State private var semanticTrainOutput = ""
    @State private var semanticTrust = false
    @State private var semanticKey = ""

    private var semanticUsesAgentCli: Bool {
        ["claude", "codex", "gemini", "cursor", "antigravity"].contains(semanticProvider)
    }

    var body: some View {
        ScrollView {
        VStack(alignment: .leading, spacing: 16) {
            Text("Settings").font(.title2).bold()

            VStack(alignment: .leading, spacing: 6) {
                Text("Telegram bot token").font(.subheadline)
                HStack {
                    if revealToken {
                        TextField("123456789:token-from-BotFather", text: $ctl.token).textFieldStyle(.roundedBorder)
                    } else {
                        SecureField("123456789:token-from-BotFather", text: $ctl.token).textFieldStyle(.roundedBorder)
                    }
                    Button(revealToken ? "Hide" : "Show") { revealToken.toggle() }
                }
                Text("From @BotFather. Stored in your macOS Keychain, never on disk in the clear.")
                    .font(.caption).foregroundStyle(.secondary)
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Pairing key (required for Telegram)").font(.subheadline)
                HStack {
                    if revealSecret {
                        TextField("long random secret", text: $ctl.secret).textFieldStyle(.roundedBorder)
                    } else {
                        SecureField("long random secret", text: $ctl.secret).textFieldStyle(.roundedBorder)
                    }
                    Button(revealSecret ? "Hide" : "Show") { revealSecret.toggle() }
                    Button("Generate") { ctl.secret = ctl.generateSecret(); revealSecret = true }
                }
                Text("You send this once to the bot as /auth <key> to authorize your chat. Keep it private.")
                    .font(.caption).foregroundStyle(.secondary)
            }

            Divider()

            Toggle("Launch at login", isOn: Binding(
                get: { ctl.launchAtLogin },
                set: { ctl.setLaunchAtLogin($0) }
            ))
            Text("The service reinstalls the shim by itself after Claude Code updates and tells you in Telegram.")
                .font(.caption).foregroundStyle(.secondary)

            Divider()

            VStack(alignment: .leading, spacing: 6) {
                Text("Automation").font(.subheadline)
                Picker("Default mode", selection: Binding(
                    get: { autoMode },
                    set: { autoMode = $0; ctl.setAutomationDefault($0) }
                )) {
                    Text("Manual").tag("manual")
                    Text("Auto").tag("auto")
                    Text("Robot").tag("robot")
                }
                .pickerStyle(.segmented)
                Text("Manual: you drive everything. Auto: rule-based - auto-approve safe tools, auto-retry transient errors; danger always asks. Robot: an AI supervisor drives the chat. Per-chat overrides live in the Telegram bot (/auto).")
                    .font(.caption).foregroundStyle(.secondary)
            }
            .onAppear { ctl.loadAutomationDefault { autoMode = $0 } }

            Divider()

            VStack(alignment: .leading, spacing: 6) {
                Text("Compass (smart layer)").font(.subheadline)
                Toggle("Enable compass", isOn: Binding(
                    get: { smartOn },
                    set: { smartOn = $0; if !$0 { steerOn = false; gateOn = false }; ctl.setSmart($0) }
                ))
                Toggle("Allow auto-steer (Robot only)", isOn: Binding(
                    get: { steerOn },
                    set: { steerOn = $0; ctl.setSteer($0) }
                ))
                .disabled(!smartOn)
                Toggle("Deterministic pre-mutation gate (off by default)", isOn: Binding(
                    get: { gateOn },
                    set: { gateOn = $0; ctl.setGate($0) }
                ))
                .disabled(!smartOn)
                Toggle("Session health telemetry (Claude + Codex)", isOn: Binding(
                    get: { feedbackOn },
                    set: { feedbackOn = $0; ctl.setFeedback($0) }
                ))
                .disabled(!smartOn)
                Text("Session telemetry supplies local typed working/blocked/completion signals without a second model call. Policy and proof guards stay deterministic.")
                    .font(.caption).foregroundStyle(.secondary)

                Divider()

                Text("Cross review (one model family grades another)").font(.subheadline)
                Toggle("Enable cross review", isOn: Binding(
                    get: { ctl.reviewEnabled },
                    set: { ctl.setReview($0) }
                ))
                Toggle("Let the review correct the chat (Robot only)", isOn: Binding(
                    get: { ctl.reviewSteer },
                    set: { ctl.setReviewSteer($0) }
                ))
                .disabled(!ctl.reviewEnabled)
                HStack {
                    Text("Depth").frame(width: 60, alignment: .leading)
                    Picker("", selection: Binding(
                        get: { ctl.reviewDepth },
                        set: { ctl.setReviewDepth($0) }
                    )) {
                        Text("shallow").tag("shallow")
                        Text("normal").tag("normal")
                        Text("deep").tag("deep")
                    }
                    .pickerStyle(.segmented).frame(width: 240)
                }
                HStack {
                    Text("Every").frame(width: 60, alignment: .leading)
                    Picker("", selection: Binding(
                        get: { ctl.reviewEverySecs },
                        set: { ctl.setReviewEvery($0) }
                    )) {
                        Text("5m").tag(300)
                        Text("15m").tag(900)
                        Text("30m").tag(1800)
                        Text("1h").tag(3600)
                    }
                    .pickerStyle(.segmented).frame(width: 240)
                }
                HStack(spacing: 8) {
                    Text("Reviewers").frame(width: 60, alignment: .leading)
                    ForEach(["codex-cli", "claude-cli", "antigravity", "cursor-cli"], id: \.self) { id in
                        Button(action: { ctl.toggleReviewer(id) }) {
                            Text(id.replacingOccurrences(of: "-cli", with: ""))
                                .font(.caption)
                                .padding(.horizontal, 8).padding(.vertical, 3)
                                .background(ctl.reviewers.contains(id) ? Color.accentColor.opacity(0.25) : Color.secondary.opacity(0.12))
                                .clipShape(Capsule())
                        }
                        .buttonStyle(.plain)
                    }
                }
                Text("A reviewer is never asked to grade its own family: Claude chats skip claude-cli, Codex chats skip codex-cli. In Auto the verdict is only recorded and sent to Telegram; only Robot with corrections on may inject it, through the same safety guard as auto-steer.")
                    .font(.caption).foregroundStyle(.secondary)
                    .onAppear { ctl.refreshReviewStatus() }
                Text("Robot budget cap (USD)").font(.subheadline)
                HStack {
                    TextField("USD cap (e.g. 5.0)", text: $budgetText)
                        .textFieldStyle(.roundedBorder).frame(width: 160)
                    Button("Set cap") {
                        if let usd = Double(budgetText.trimmingCharacters(in: .whitespaces)), usd > 0 {
                            ctl.setBudget(usd)
                        }
                    }
                    Button("Clear") {
                        budgetText = ""
                        ctl.setBudget(nil)
                    }
                    if let budget = ctl.budgetUsd {
                        Text("current: \(String(format: "%.2f", budget)) USD")
                            .font(.caption2).foregroundStyle(.secondary)
                    } else {
                        Text("no cap").font(.caption2).foregroundStyle(.secondary)
                    }
                }
                Text("Global spend cap for metered Robot providers. A provider halts once its recorded usage reaches this cap; local and CLI providers record no cost.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            .onAppear {
                ctl.loadSmart { on, steer, gate, feedback in smartOn = on; steerOn = steer; gateOn = gate; feedbackOn = feedback }
                ctl.refreshAutomationStatus()
                if let budget = ctl.budgetUsd { budgetText = String(format: "%.2f", budget) }
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Compass semantic backend").font(.subheadline)
                Picker("Backend", selection: Binding(
                    get: { semanticProvider },
                    set: { semanticProvider = $0; ctl.setSemanticProvider($0) }
                )) {
                    Text("Off").tag("off")
                    Text("Built-in local NLI").tag("local")
                    Text("Agent CLI — Claude (recommended)").tag("claude")
                    Text("Agent CLI — Codex").tag("codex")
                    Text("Agent CLI — Gemini").tag("gemini")
                    Text("Agent CLI — Cursor").tag("cursor")
                    Text("Agent CLI — Antigravity").tag("antigravity")
                    Text("Ollama").tag("ollama")
                    Text("OpenRouter").tag("openrouter")
                    Text("NVIDIA NIM").tag("nvidia")
                    Text("OpenAI-compatible / custom").tag("openai-compatible")
                }
                if semanticProvider == "local" {
                    HStack {
                        TextField("/absolute/path/to/custom ONNX bundle", text: $semanticLocalDir)
                            .textFieldStyle(.roundedBorder)
                        Button("Use bundle") {
                            if !semanticLocalDir.trimmingCharacters(in: .whitespaces).isEmpty {
                                ctl.setSemanticLocalDir(semanticLocalDir)
                            }
                        }
                        Button("Use built-in") {
                            semanticLocalDir = ""
                            ctl.setSemanticLocalDir("builtin")
                        }
                    }
                    HStack {
                        TextField("labeled dataset.jsonl", text: $semanticTrainDataset)
                            .textFieldStyle(.roundedBorder)
                        TextField("new bundle directory", text: $semanticTrainOutput)
                            .textFieldStyle(.roundedBorder)
                        Button("Train + select") {
                            if !semanticTrainDataset.trimmingCharacters(in: .whitespaces).isEmpty &&
                               !semanticTrainOutput.trimmingCharacters(in: .whitespaces).isEmpty {
                                ctl.trainSemantic(
                                    dataset: semanticTrainDataset,
                                    output: semanticTrainOutput
                                )
                            }
                        }
                    }
                } else if semanticUsesAgentCli {
                    Label(
                        "Uses the installed \(semanticProvider) CLI as a typed extractor. Transcript excerpts leave this machine through that provider; Contract Ledger remains the authority.",
                        systemImage: "network"
                    )
                    .font(.caption)
                    .foregroundStyle(.orange)
                } else if semanticProvider != "off" {
                    HStack {
                        TextField("provider model id", text: $semanticModel).textFieldStyle(.roundedBorder)
                        Button("Apply model") {
                            if !semanticModel.trimmingCharacters(in: .whitespaces).isEmpty {
                                ctl.setSemanticModel(semanticModel)
                            }
                        }
                    }
                }
                if semanticProvider == "openai-compatible" {
                    HStack {
                        TextField("https://host/v1", text: $semanticEndpoint).textFieldStyle(.roundedBorder)
                        Button("Apply endpoint") {
                            if !semanticEndpoint.trimmingCharacters(in: .whitespaces).isEmpty {
                                ctl.setSemanticEndpoint(semanticEndpoint)
                            }
                        }
                    }
                }
                if semanticProvider != "local" && semanticProvider != "off" && semanticProvider != "ollama" {
                    HStack {
                        SecureField("API key (stored in 0600 key store)", text: $semanticKey)
                            .textFieldStyle(.roundedBorder)
                        Button("Set key") {
                            if !semanticKey.isEmpty { ctl.setSemanticKey(semanticKey); semanticKey = "" }
                        }
                    }
                }
                Toggle("Permit uncalibrated backend facts to reach auto-steer", isOn: Binding(
                    get: { semanticTrust },
                    set: { semanticTrust = $0; ctl.setSemanticTrust($0) }
                ))
                HStack {
                    Button("Install built-in bootstrap (~124 MB)") { ctl.installSemanticModel() }
                    Button("Check backend") { ctl.checkSemanticBackend() }
                }
                Text("The built-in NLI model is a low-memory shadow bootstrap, not a calibrated production detector. You can use a trained custom ONNX bundle or any compatible provider. Uncalibrated steering is blocked by default.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            .onAppear {
                ctl.loadSemantic { provider, model, endpoint, localDir, trust in
                    semanticProvider = provider; semanticModel = model
                    semanticEndpoint = endpoint; semanticLocalDir = localDir
                    semanticTrust = trust
                }
            }

            Divider()

            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Text("Providers").font(.subheadline)
                    Spacer()
                    Button("Refresh") { ctl.refreshProviders() }
                    if ctl.checkingHealth {
                        ProgressView().controlSize(.small)
                    } else {
                        Button("Check health") { ctl.checkHealth() }
                    }
                }
                Text("Enable providers for Robot and set a key where required. Login is shown only for CLIs with a real login command; Test session probes CLIs such as Antigravity that reuse an existing app session.")
                    .font(.caption).foregroundStyle(.secondary)
                if ctl.providers.isEmpty {
                    Text("click Refresh to list providers").font(.caption).foregroundStyle(.secondary)
                } else {
                    ScrollView {
                        VStack(spacing: 6) {
                            ForEach(ctl.providers) { p in
                                ProviderRowView(row: p, health: ctl.providerHealth[p.id], ctl: ctl)
                            }
                        }
                    }
                    .frame(maxHeight: 200)
                }
            }
            .onAppear { ctl.refreshProviders() }

            Divider()

            HStack {
                Text("Version \(ctl.appVersion)").font(.callout).foregroundStyle(.secondary)
                Spacer()
                Button("Check for updates") { ctl.checkAppUpdate(manual: true); dismiss() }
            }

            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button("Save") { ctl.saveSecrets(); dismiss() }
                    .buttonStyle(.borderedProminent)
            }
        }
        }
        .padding(20)
        .frame(width: 560, height: 760)
    }
}

struct HelpView: View {
    @ObservedObject var ctl: RelayController
    @Environment(\.dismiss) private var dismiss

    private let lines: [(String, String)] = [
        ("Getting started", "Local session supervision starts without Telegram. To add Telegram, open Settings, paste the bot token from @BotFather, set a pairing key, then send /auth <key> and /menu."),
        ("Telegram commands", "/menu buttons, /windows, /status <ws>, /say <ws> <claude|codex> <text>, /stop, /cont, /mode, /auto, /slash, /focus, /danger, /auth, /help."),
        ("Automation modes", "Manual drives nothing without you. Auto is rule-based: auto-approves safe tools, auto-retries transient API errors; danger-listed commands always ask. Robot lets an AI supervisor drive a chat. Set the default here; pin individual chats from the Telegram bot with /auto."),
        ("Sessions", "The Sessions panel lists live chats with state, mode, and channel. Switch a session's mode inline, or Open a card to type a prompt and send it into the real chat in the background. In Robot mode (and Manual if you enable rewrite) the prompt is improved by a supervisor model before it lands."),
        ("Background control", "Install shim gives background send, question answers, and Allow/Deny on permission prompts. Only chats opened after the shim is installed use it; already-open chats keep their old binary."),
        ("Claude Code updates", "When the extension updates, the service reinstalls the shim by itself and tells you in Telegram. It also notifies when a newer Claude Code version is available."),
        ("Permissions", "Accessibility is only needed for on-screen window focus and typing. Background control does not need it."),
        ("Where things live", "Secrets are in the macOS Keychain. Logs and state are under ~/.vsc-relay. The log rotates automatically."),
        ("Safety", "Nothing works for a chat until it pairs with your key. The token is never sent to a chat. The service does not listen on the network."),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Help").font(.title2).bold()
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(lines, id: \.0) { item in
                        VStack(alignment: .leading, spacing: 3) {
                            Text(item.0).font(.headline)
                            Text(item.1).font(.callout).foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                }
            }
            HStack {
                Button("Open the project page") { ctl.openReleases() }
                Spacer()
                Button("Done") { dismiss() }.buttonStyle(.borderedProminent)
            }
        }
        .padding(20)
        .frame(width: 540, height: 460)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private var window: NSWindow!
    private var statusItem: NSStatusItem!
    private var statusLine: NSMenuItem!
    private var startItem: NSMenuItem!
    private var stopItem: NSMenuItem!
    private var logMenu: NSMenu!
    private var timer: Timer?
    private var envTimer: Timer?
    private var updateTimer: Timer?
    private var sessionTimer: Timer?
    private var lastActiveCheck = Date.distantPast

    func applicationDidFinishLaunching(_ notification: Notification) {
        let hosting = NSHostingController(rootView: ContentView())
        window = NSWindow(contentViewController: hosting)
        window.title = "VS Code Agent Relay"
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.isReleasedWhenClosed = false
        window.minSize = NSSize(width: 1000, height: 700)
        window.setContentSize(NSSize(width: 1180, height: 780))
        window.center()
        window.setFrameAutosaveName("VSCRelayMain")
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        buildStatusItem()
        timer = Timer.scheduledTimer(withTimeInterval: 1.5, repeats: true) { [weak self] _ in
            self?.refreshStatus()
            RelayController.shared.refreshStats()
            RelayController.shared.followLog()
        }
        envTimer = Timer.scheduledTimer(withTimeInterval: 20, repeats: true) { _ in
            RelayController.shared.refreshEnv()
        }
        updateTimer = Timer.scheduledTimer(withTimeInterval: 6 * 60 * 60, repeats: true) { _ in
            RelayController.shared.checkAppUpdate()
        }
        RelayController.shared.refreshSessions()
        sessionTimer = Timer.scheduledTimer(withTimeInterval: 8, repeats: true) { _ in
            if RelayController.shared.isRunning {
                RelayController.shared.refreshSessions()
            }
        }
        RelayController.shared.start()
        RelayController.shared.loadSecrets { available in
            if available {
                RelayController.shared.restart()
            }
        }
    }

    func applicationDidBecomeActive(_ notification: Notification) {
        let now = Date()
        if now.timeIntervalSince(lastActiveCheck) > 300 {
            lastActiveCheck = now
            RelayController.shared.checkAppUpdate()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        RelayController.shared.stop(includingAdopted: false)
    }

    private func buildStatusItem() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        statusItem.button?.image = NSImage(systemSymbolName: "antenna.radiowaves.left.and.right", accessibilityDescription: "Relay")

        let menu = NSMenu()
        menu.delegate = self
        statusLine = NSMenuItem(title: "Stopped", action: nil, keyEquivalent: "")
        statusLine.isEnabled = false
        menu.addItem(statusLine)
        menu.addItem(.separator())

        startItem = NSMenuItem(title: "Start", action: #selector(startAction), keyEquivalent: "")
        startItem.target = self
        menu.addItem(startItem)
        stopItem = NSMenuItem(title: "Stop", action: #selector(stopAction), keyEquivalent: "")
        stopItem.target = self
        menu.addItem(stopItem)

        let show = NSMenuItem(title: "Show Window", action: #selector(showWindow), keyEquivalent: "")
        show.target = self
        menu.addItem(show)

        let upd = NSMenuItem(title: "Check for updates", action: #selector(checkUpdatesAction), keyEquivalent: "")
        upd.target = self
        menu.addItem(upd)

        let logItem = NSMenuItem(title: "Recent log", action: nil, keyEquivalent: "")
        logMenu = NSMenu()
        logMenu.delegate = self
        logItem.submenu = logMenu
        menu.addItem(logItem)

        menu.addItem(.separator())
        let quit = NSMenuItem(title: "Quit", action: #selector(quitAction), keyEquivalent: "q")
        quit.target = self
        menu.addItem(quit)

        statusItem.menu = menu
    }

    private var adoptTick = 0

    private func refreshStatus() {
        if !RelayController.shared.isRunning {
            adoptTick += 1
            if adoptTick % 3 == 0 {
                RelayController.shared.adoptRunningDaemon()
            }
        } else {
            adoptTick = 0
        }
        let running = RelayController.shared.isRunning
        statusLine.title = running ? "Running (pid \(RelayController.shared.pid))" : "Stopped"
        startItem.isEnabled = !running
        stopItem.isEnabled = running
    }

    func menuNeedsUpdate(_ menu: NSMenu) {
        guard menu === logMenu else { return }
        menu.removeAllItems()
        let lines = RelayController.shared.recentLog(12)
        if lines.isEmpty {
            menu.addItem(NSMenuItem(title: "no output yet", action: nil, keyEquivalent: ""))
            return
        }
        for line in lines {
            let trimmed = String(line.prefix(80))
            let item = NSMenuItem(title: trimmed, action: nil, keyEquivalent: "")
            item.isEnabled = false
            menu.addItem(item)
        }
    }

    @objc private func startAction() { RelayController.shared.start() }
    @objc private func stopAction() { RelayController.shared.stop() }
    @objc private func checkUpdatesAction() {
        RelayController.shared.checkAppUpdate(manual: true)
        showWindow()
    }
    @objc private func showWindow() {
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }
    @objc private func quitAction() { NSApp.terminate(nil) }
}

@main
enum AppMain {
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.regular)
        app.run()
    }
}

func reviewerChoices(for agent: String) -> [String] {
    let own = agent == "codex" ? "codex-cli" : "claude-cli"
    return ["codex-cli", "claude-cli", "antigravity", "cursor-cli", "gemini-cli"].filter { $0 != own }
}
