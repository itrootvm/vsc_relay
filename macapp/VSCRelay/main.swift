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

final class RelayController: ObservableObject {
    static let shared = RelayController()

    @Published var token: String = ""
    @Published var secret: String = ""
    @Published var isRunning: Bool = false
    @Published var pid: Int32 = 0
    @Published var shimInstalled: Bool = false
    @Published var logLines: [String] = []
    @Published var note: String = ""
    @Published var showSettings: Bool = false
    @Published var showHelp: Bool = false
    @Published var appUpdate: String = ""
    let releasesURL = "https://github.com/itrootvm/vsc_relay/releases/latest"
    @Published var turnsToday: Int = 0
    @Published var sessionsToday: Int = 0
    @Published var vscodeOK: Bool = true
    @Published var extensionOK: Bool = true
    @Published var launchAtLogin: Bool = false
    @Published var extVersion: String = "-"

    private var process: Process?
    private let maxLines = 800
    private let relayDir: URL
    private let logURL: URL
    private let legacyConfigURL: URL
    private let statsURL: URL
    private let logCap: UInt64 = 2 * 1024 * 1024

    private init() {
        let home = FileManager.default.homeDirectoryForCurrentUser
        relayDir = home.appendingPathComponent(".vsc-relay")
        logURL = relayDir.appendingPathComponent("agent.log")
        legacyConfigURL = relayDir.appendingPathComponent("config.env")
        statsURL = relayDir.appendingPathComponent("stats.json")
        try? FileManager.default.createDirectory(at: relayDir, withIntermediateDirectories: true)
        try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: relayDir.path)
        loadSecrets()
        launchAtLogin = (SMAppService.mainApp.status == .enabled)
        refreshEnv()
        refreshStats()
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

    private func loadSecrets() {
        token = Keychain.get("telegram_token") ?? ""
        secret = Keychain.get("pair_secret") ?? ""
        if token.isEmpty, let text = try? String(contentsOf: legacyConfigURL, encoding: .utf8) {
            for line in text.split(separator: "\n") {
                let parts = line.split(separator: "=", maxSplits: 1).map(String.init)
                guard parts.count == 2 else { continue }
                let key = parts[0].trimmingCharacters(in: .whitespaces)
                let val = parts[1].trimmingCharacters(in: .whitespaces)
                if key == "TELEGRAM_BOT_TOKEN" { token = val }
                if key == "RELAY_PAIR_SECRET" { secret = val }
            }
            if !token.isEmpty {
                saveSecrets()
                try? FileManager.default.removeItem(at: legacyConfigURL)
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
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        return String(data: data, encoding: .utf8) ?? ""
    }

    private func killStrayDaemons() {
        let pgrep = Process()
        pgrep.executableURL = URL(fileURLWithPath: "/usr/bin/pgrep")
        pgrep.arguments = ["-f", "vsc-relay-agent"]
        let pipe = Pipe()
        pgrep.standardOutput = pipe
        try? pgrep.run()
        pgrep.waitUntilExit()
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        let mypid = ProcessInfo.processInfo.processIdentifier
        guard let out = String(data: data, encoding: .utf8) else { return }
        for line in out.split(separator: "\n") {
            guard let p = Int32(line.trimmingCharacters(in: .whitespaces)), p != mypid else { continue }
            let cmd = commandForPid(p)
            if cmd.contains(" hook ") { continue }
            if cmd.contains("vsc-relay-agent") { kill(p, SIGTERM) }
        }
    }

    func start() {
        if isRunning { return }
        guard !token.isEmpty else { note = "Add a bot token in Settings first."; showSettings = true; return }
        guard !secret.isEmpty else { note = "Set a pairing key in Settings first."; showSettings = true; return }
        saveSecrets()
        killStrayDaemons()
        let proc = Process()
        proc.executableURL = resourceBinary("vsc-relay-agent")
        var env = ProcessInfo.processInfo.environment
        env["TELEGRAM_BOT_TOKEN"] = token
        env["RELAY_PAIR_SECRET"] = secret
        proc.environment = env
        let pipe = Pipe()
        proc.standardOutput = pipe
        proc.standardError = pipe
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else { return }
            self?.writeLogFile(data)
            if let s = String(data: data, encoding: .utf8) {
                DispatchQueue.main.async { self?.appendLog(s) }
            }
        }
        proc.terminationHandler = { [weak self] _ in
            DispatchQueue.main.async {
                self?.isRunning = false
                self?.pid = 0
                self?.appendLog("[relay stopped]\n")
            }
        }
        do {
            try proc.run()
            process = proc
            isRunning = true
            pid = proc.processIdentifier
            note = "Running. In Telegram, send /auth <your key> to the bot."
        } catch {
            note = "Failed to start: \(error.localizedDescription)"
        }
    }

    func stop() {
        if let p = process, p.isRunning { p.terminate() }
        process = nil
        isRunning = false
        pid = 0
    }

    private func runAgentSub(_ args: [String], done: @escaping (String) -> Void) {
        let proc = Process()
        proc.executableURL = resourceBinary("vsc-relay-agent")
        proc.arguments = args
        let pipe = Pipe()
        proc.standardOutput = pipe
        proc.standardError = pipe
        proc.terminationHandler = { _ in
            let data = pipe.fileHandleForReading.readDataToEndOfFile()
            let out = String(data: data, encoding: .utf8) ?? ""
            DispatchQueue.main.async { done(out) }
        }
        do { try proc.run() } catch { done("error: \(error.localizedDescription)") }
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

    func openAccessibility() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility") {
            NSWorkspace.shared.open(url)
        }
    }

    func openReleases() {
        if let url = URL(string: releasesURL) { NSWorkspace.shared.open(url) }
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

    func checkAppUpdate() {
        guard let url = URL(string: "https://api.github.com/repos/itrootvm/vsc_relay/releases/latest") else { return }
        var req = URLRequest(url: url)
        req.setValue("VSCRelay", forHTTPHeaderField: "User-Agent")
        req.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        URLSession.shared.dataTask(with: req) { [weak self] data, _, _ in
            guard let self = self, let data = data,
                  let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let tag = obj["tag_name"] as? String else { return }
            let latest = tag.hasPrefix("v") ? String(tag.dropFirst()) : tag
            let current = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0.1.0"
            if self.semverGreater(latest, current) {
                DispatchQueue.main.async { self.appUpdate = tag }
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
        }
        if logLines.count > maxLines {
            logLines.removeFirst(logLines.count - maxLines)
        }
    }

    private func rotateIfNeeded() {
        let attrs = try? FileManager.default.attributesOfItem(atPath: logURL.path)
        let size = (attrs?[.size] as? NSNumber)?.uint64Value ?? 0
        if size > logCap {
            let bak = relayDir.appendingPathComponent("agent.log.1")
            try? FileManager.default.removeItem(at: bak)
            try? FileManager.default.moveItem(at: logURL, to: bak)
        }
    }

    private func writeLogFile(_ data: Data) {
        rotateIfNeeded()
        if let fh = try? FileHandle(forWritingTo: logURL) {
            fh.seekToEndOfFile(); fh.write(data); try? fh.close()
        } else {
            try? data.write(to: logURL)
        }
    }
}

struct ContentView: View {
    @ObservedObject var ctl = RelayController.shared

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header
            updateBanner
            warningBanner
            statsRow
            controls
            if !ctl.note.isEmpty {
                Text(ctl.note).font(.callout).foregroundStyle(.secondary)
            }
            logView
        }
        .padding(18)
        .frame(minWidth: 660, minHeight: 500)
        .sheet(isPresented: $ctl.showSettings) { SettingsView(ctl: ctl) }
        .sheet(isPresented: $ctl.showHelp) { HelpView(ctl: ctl) }
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
                Text("App update \(ctl.appUpdate) is available.").font(.callout)
                Spacer()
                Button("Download") { ctl.openReleases() }
            }
            .padding(10)
            .background(Color.blue.opacity(0.12))
            .clipShape(RoundedRectangle(cornerRadius: 8))
        }
    }

    private var statsRow: some View {
        HStack(spacing: 10) {
            statCard(title: "Sessions today", value: "\(ctl.sessionsToday)")
            statCard(title: "Turns today", value: "\(ctl.turnsToday)")
            statCard(title: "Shim", value: ctl.shimInstalled ? "on" : "off")
        }
    }

    private func statCard(title: String, value: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(value).font(.system(size: 20, weight: .semibold))
            Text(title).font(.caption).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(10)
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
            Button { ctl.clearLog() } label: { Text("Clear log") }
        }
    }

    private var logView: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 1) {
                    ForEach(Array(ctl.logLines.enumerated()), id: \.offset) { idx, line in
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
            .onChange(of: ctl.logLines.count) { _, count in
                if count > 0 { proxy.scrollTo(count - 1, anchor: .bottom) }
            }
        }
    }
}

struct SettingsView: View {
    @ObservedObject var ctl: RelayController
    @Environment(\.dismiss) private var dismiss
    @State private var revealToken = false
    @State private var revealSecret = false

    var body: some View {
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
                Text("Pairing key (required)").font(.subheadline)
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

            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button("Save") { ctl.saveSecrets(); dismiss() }
                    .buttonStyle(.borderedProminent)
                    .disabled(ctl.token.isEmpty || ctl.secret.isEmpty)
            }
        }
        .padding(20)
        .frame(width: 520)
    }
}

struct HelpView: View {
    @ObservedObject var ctl: RelayController
    @Environment(\.dismiss) private var dismiss

    private let lines: [(String, String)] = [
        ("Getting started", "Open Settings, paste your Telegram bot token from @BotFather, set a pairing key, then click Start. In Telegram send /auth <key> to your bot, then /menu."),
        ("Telegram commands", "/menu buttons, /windows, /status <ws>, /say <ws> <claude|codex> <text>, /stop, /cont, /mode, /slash, /focus, /danger, /auth, /help."),
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

    func applicationDidFinishLaunching(_ notification: Notification) {
        let hosting = NSHostingController(rootView: ContentView())
        window = NSWindow(contentViewController: hosting)
        window.title = "VS Code Agent Relay"
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.isReleasedWhenClosed = false
        window.setContentSize(NSSize(width: 700, height: 540))
        window.center()
        window.setFrameAutosaveName("VSCRelayMain")
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        buildStatusItem()
        timer = Timer.scheduledTimer(withTimeInterval: 1.5, repeats: true) { [weak self] _ in
            self?.refreshStatus()
            RelayController.shared.refreshStats()
        }
        envTimer = Timer.scheduledTimer(withTimeInterval: 20, repeats: true) { _ in
            RelayController.shared.refreshEnv()
        }
        if RelayController.shared.hasSecrets {
            RelayController.shared.start()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        RelayController.shared.stop()
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

    private func refreshStatus() {
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
