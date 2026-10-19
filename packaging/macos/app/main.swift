// Fleet-51.app: native windows around the Fleet-51 chat UI. It starts the bundled background
// service (replacing an older version if one is running), clears any cached copy of an older
// interface, and shows the chat. "Open in new window" in a bot's menu (or File > New Window)
// opens another window, so several bots can be chatted with side by side.

import Cocoa
import WebKit

final class ChatWindow: NSObject, WKNavigationDelegate, WKUIDelegate, NSWindowDelegate {
    let window: NSWindow
    let web: WKWebView
    unowned let app: AppDelegate

    init(app: AppDelegate, url: URL?, cascadeFrom: NSWindow?) {
        self.app = app
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .default()
        web = WKWebView(frame: .zero, configuration: config)
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1180, height: 780),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered, defer: false)
        super.init()
        web.navigationDelegate = self
        web.uiDelegate = self
        window.title = "Fleet-51"
        window.minSize = NSSize(width: 420, height: 480)
        window.contentView = web
        window.delegate = self
        window.isReleasedWhenClosed = false
        if let prev = cascadeFrom {
            window.setFrameTopLeftPoint(NSPoint(x: prev.frame.minX + 28, y: prev.frame.maxY - 28))
        } else {
            window.center()
            window.setFrameAutosaveName("Fleet-51 main window")
        }
        window.makeKeyAndOrderFront(nil)
        if let u = url { load(u) } else { showMessage("Starting Fleet-51…", detail: "") }
    }

    func load(_ u: URL) {
        web.load(URLRequest(url: u, cachePolicy: .reloadIgnoringLocalCacheData))
    }

    func showMessage(_ title: String, detail: String) {
        let esc = { (s: String) in s.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;") }
        web.loadHTMLString("""
            <html><body style="font:15px -apple-system,sans-serif;display:grid;place-items:center;height:90vh;margin:0;background:#070b09;color:#8fa596">
            <div style="text-align:center;max-width:560px"><h2 style="color:#e6f2e9">\(esc(title))</h2>
            <pre style="white-space:pre-wrap;text-align:left">\(esc(detail))</pre></div></body></html>
            """, baseURL: nil)
    }

    func windowWillClose(_ notification: Notification) {
        app.windows.removeAll { $0 === self }
    }

    // Links outside Fleet-51 open in the default browser; Fleet-51 links open a new app window.
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                 decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        if let u = action.request.url, let host = u.host, host != app.home?.host, u.scheme?.hasPrefix("http") == true {
            NSWorkspace.shared.open(u)
            decisionHandler(.cancel)
        } else {
            decisionHandler(.allow)
        }
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if let u = action.request.url {
            if u.host == app.home?.host {
                app.openWindow(url: u)
            } else {
                NSWorkspace.shared.open(u)
            }
        }
        return nil
    }

    // The chat uses confirm() and alert(); WKWebView shows nothing unless the app does.
    func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping () -> Void) {
        let a = NSAlert()
        a.messageText = message
        a.beginSheetModal(for: window) { _ in completionHandler() }
    }

    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void) {
        let a = NSAlert()
        a.messageText = message
        a.addButton(withTitle: "OK")
        a.addButton(withTitle: "Cancel")
        a.beginSheetModal(for: window) { r in completionHandler(r == .alertFirstButtonReturn) }
    }

    // The + button opens a file picker; WKWebView shows none unless the app does.
    func webView(_ webView: WKWebView, runOpenPanelWith parameters: WKOpenPanelParameters,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping ([URL]?) -> Void) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = parameters.allowsMultipleSelection
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        panel.prompt = "Add"
        panel.message = "Choose files to add to this bot's folder (they're copied; your originals stay as they are)"
        panel.beginSheetModal(for: window) { r in completionHandler(r == .OK ? panel.urls : nil) }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    var windows: [ChatWindow] = []
    var home: URL?
    let bin = Bundle.main.resourceURL!.appendingPathComponent("fleet51").path

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildMenu()
        windows.append(ChatWindow(app: self, url: nil, cascadeFrom: nil))
        NSApp.activate(ignoringOtherApps: true)
        // Drop cached pages from older versions, keeping saved settings (localStorage).
        let stale: Set<String> = [WKWebsiteDataTypeDiskCache, WKWebsiteDataTypeMemoryCache,
                                  WKWebsiteDataTypeFetchCache, WKWebsiteDataTypeServiceWorkerRegistrations]
        WKWebsiteDataStore.default().removeData(ofTypes: stale, modifiedSince: .distantPast) {
            self.connect()
        }
    }

    /// Start (or replace) the background service, then load the chat. Runs the CLI off the main thread.
    func connect() {
        DispatchQueue.global(qos: .userInitiated).async {
            let up = self.run(["up"])
            let url = self.run(["url"]).output.trimmingCharacters(in: .whitespacesAndNewlines)
            DispatchQueue.main.async {
                if up.status == 0, let u = URL(string: url), u.scheme == "http" {
                    self.home = u
                    for w in self.windows { w.load(u) }
                } else {
                    self.windows.first?.showMessage("Fleet-51 could not start",
                                                    detail: "See ~/.fleet51/daemon.log for details.\n\n" + up.output)
                }
            }
        }
    }

    @discardableResult
    func run(_ args: [String]) -> (status: Int32, output: String) {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: bin)
        p.arguments = args
        var env = ProcessInfo.processInfo.environment
        let extra = ["/opt/homebrew/bin", "/usr/local/bin"]
        env["PATH"] = (extra + [env["PATH"] ?? "/usr/bin:/bin"]).joined(separator: ":")
        p.environment = env
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        do { try p.run() } catch { return (1, "\(error)") }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return (p.terminationStatus, String(data: data, encoding: .utf8) ?? "")
    }

    func openWindow(url: URL?) {
        let target = url ?? home
        windows.append(ChatWindow(app: self, url: target, cascadeFrom: NSApp.keyWindow ?? windows.last?.window))
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag {
            if let w = windows.first { w.window.makeKeyAndOrderFront(nil) } else { openWindow(url: nil) }
        }
        return true
    }

    @objc func newWindow() { openWindow(url: nil) }

    @objc func reload() {
        if home == nil { connect() } else if let w = windows.first(where: { $0.window.isKeyWindow }) ?? windows.first { w.web.reload() }
    }

    @objc func openInBrowser() {
        if let u = home { NSWorkspace.shared.open(u) }
    }

    @objc func showLog() {
        NSWorkspace.shared.open(URL(fileURLWithPath: NSHomeDirectory() + "/.fleet51/daemon.log"))
    }

    @objc func quitAndStop() {
        run(["down"])
        NSApp.terminate(nil)
    }

    func buildMenu() {
        let main = NSMenu()
        func add(_ title: String, _ items: [NSMenuItem]) {
            let holder = NSMenuItem()
            let menu = NSMenu(title: title)
            for i in items { menu.addItem(i) }
            holder.submenu = menu
            main.addItem(holder)
        }
        func item(_ title: String, _ action: Selector?, _ key: String, _ mods: NSEvent.ModifierFlags = .command) -> NSMenuItem {
            let i = NSMenuItem(title: title, action: action, keyEquivalent: key)
            i.keyEquivalentModifierMask = mods
            return i
        }
        add("Fleet-51", [
            item("About Fleet-51", #selector(NSApplication.orderFrontStandardAboutPanel(_:)), ""),
            .separator(),
            item("Hide Fleet-51", #selector(NSApplication.hide(_:)), "h"),
            item("Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option]),
            .separator(),
            item("Quit and Stop Bots", #selector(quitAndStop), "q", [.command, .option]),
            item("Quit Fleet-51 (bots keep running)", #selector(NSApplication.terminate(_:)), "q"),
        ])
        add("File", [
            item("New Window", #selector(newWindow), "n", [.command, .shift]),
            item("Close Window", #selector(NSWindow.performClose(_:)), "w"),
        ])
        add("Edit", [
            item("Undo", Selector(("undo:")), "z"),
            item("Redo", Selector(("redo:")), "z", [.command, .shift]),
            .separator(),
            item("Cut", #selector(NSText.cut(_:)), "x"),
            item("Copy", #selector(NSText.copy(_:)), "c"),
            item("Paste", #selector(NSText.paste(_:)), "v"),
            item("Select All", #selector(NSText.selectAll(_:)), "a"),
        ])
        add("View", [
            item("Reload", #selector(reload), "r"),
            item("Open in Browser", #selector(openInBrowser), "b", [.command, .shift]),
            item("Show Log", #selector(showLog), "l", [.command, .shift]),
            .separator(),
            item("Enter Full Screen", #selector(NSWindow.toggleFullScreen(_:)), "f", [.command, .control]),
        ])
        add("Window", [
            item("Minimize", #selector(NSWindow.performMiniaturize(_:)), "m"),
        ])
        NSApp.mainMenu = main
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
