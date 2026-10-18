// Fleet-51.app: a native window around the Fleet-51 web UI. It starts the bundled daemon if it
// is not running, then shows the UI in a WKWebView with a normal Mac menu bar and Dock icon.
// Closing the window leaves the daemon (and any running agents) going; Quit in the app menu
// offers to stop it.

import Cocoa
import WebKit

final class AppDelegate: NSObject, NSApplicationDelegate, WKNavigationDelegate, WKUIDelegate {
    var window: NSWindow!
    var web: WKWebView!
    var home: URL?
    let bin = Bundle.main.resourceURL!.appendingPathComponent("fleet51").path

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildMenu()
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .default()
        web = WKWebView(frame: .zero, configuration: config)
        web.navigationDelegate = self
        web.uiDelegate = self
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1280, height: 820),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered, defer: false)
        window.title = "Fleet-51"
        window.minSize = NSSize(width: 420, height: 480)
        window.contentView = web
        window.center()
        window.setFrameAutosaveName("Fleet-51 main window")
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        showMessage("Starting Fleet-51…", detail: "")
        connect()
    }

    /// Start the daemon if needed, then load the web UI. Runs the CLI off the main thread.
    func connect() {
        DispatchQueue.global(qos: .userInitiated).async {
            let up = self.run(["up"])
            let url = self.run(["url"]).output.trimmingCharacters(in: .whitespacesAndNewlines)
            DispatchQueue.main.async {
                if up.status == 0, let u = URL(string: url), u.scheme == "http" {
                    self.home = u
                    self.web.load(URLRequest(url: u))
                } else {
                    self.showMessage(
                        "Fleet-51 could not start",
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
        // Apps started from Finder get a minimal PATH; add where agents and Ollama usually live.
        var env = ProcessInfo.processInfo.environment
        let extra = ["/opt/homebrew/bin", "/usr/local/bin", NSHomeDirectory() + "/.local/bin", NSHomeDirectory() + "/.cargo/bin"]
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

    func showMessage(_ title: String, detail: String) {
        let esc = { (s: String) in
            s.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
        }
        web.loadHTMLString("""
            <html><body style="font:15px -apple-system,sans-serif;display:grid;place-items:center;height:90vh;margin:0;color:#888">
            <div style="text-align:center;max-width:560px"><h2 style="color:#555">\(esc(title))</h2>
            <pre style="white-space:pre-wrap;text-align:left">\(esc(detail))</pre></div></body></html>
            """, baseURL: nil)
    }

    // Links outside Fleet-51 open in the default browser, not inside the app window.
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                 decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        if let u = action.request.url, let host = u.host, host != home?.host, u.scheme?.hasPrefix("http") == true {
            NSWorkspace.shared.open(u)
            decisionHandler(.cancel)
        } else if let u = action.request.url, u.scheme == "vscode" || u.scheme == "cursor" {
            NSWorkspace.shared.open(u)
            decisionHandler(.cancel)
        } else {
            decisionHandler(.allow)
        }
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if let u = action.request.url { NSWorkspace.shared.open(u) }
        return nil
    }

    // The web UI uses confirm() and alert(); WKWebView shows nothing unless the app does.
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

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag { window.makeKeyAndOrderFront(nil) }
        return true
    }

    @objc func reload() {
        if home == nil { connect() } else { web.reload() }
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
            item("Quit and Stop Agents", #selector(quitAndStop), "q", [.command, .option]),
            item("Quit Fleet-51 (agents keep running)", #selector(NSApplication.terminate(_:)), "q"),
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
            item("Show Daemon Log", #selector(showLog), "l", [.command, .shift]),
            .separator(),
            item("Enter Full Screen", #selector(NSWindow.toggleFullScreen(_:)), "f", [.command, .control]),
        ])
        add("Window", [
            item("Minimize", #selector(NSWindow.performMiniaturize(_:)), "m"),
            item("Close", #selector(NSWindow.performClose(_:)), "w"),
        ])
        NSApp.mainMenu = main
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
