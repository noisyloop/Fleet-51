//! Fleet-51 for Windows: the app window. It starts the background service (`fleet51.exe up`,
//! installed next to this program), then shows the chat in a native window using WebView2,
//! Windows' built-in browser engine. A tray icon reopens the window or stops everything.
//! On other systems this program does nothing (macOS has its own Swift app).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "fleet51-app is the Windows app window; on this system run `fleet51 up` and `fleet51 url`."
    );
}

#[cfg(windows)]
fn main() {
    if let Err(e) = app::run() {
        app::message_box(&format!("Fleet-51 could not start:\n\n{e:#}"));
    }
}

#[cfg(windows)]
mod app {
    use std::collections::HashMap;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use anyhow::{bail, Context, Result};
    use tao::event::{Event, StartCause, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
    use tao::window::{Icon, Window, WindowBuilder, WindowId};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};
    use wry::{NewWindowResponse, WebContext, WebView, WebViewBuilder};

    const NO_WINDOW: u32 = 0x0800_0000;

    enum UserEvent {
        /// Open another chat window (the chat's "Open in new window").
        Open(String),
        Menu(MenuEvent),
        Tray(TrayIconEvent),
    }

    fn service() -> Result<PathBuf> {
        let exe = std::env::current_exe()?;
        let svc = exe.with_file_name("fleet51.exe");
        if !svc.is_file() {
            bail!("{} is missing; reinstall Fleet-51", svc.display());
        }
        Ok(svc)
    }

    fn quiet(svc: &PathBuf, args: &[&str]) -> Command {
        let mut c = Command::new(svc);
        c.args(args).stdin(Stdio::null()).creation_flags(NO_WINDOW);
        c
    }

    /// Start (or reuse) the background service and get the chat's address, token included.
    fn start_service() -> Result<String> {
        let svc = service()?;
        let up = quiet(&svc, &["up"]).output().context("starting Fleet-51")?;
        if !up.status.success() {
            bail!("{}", String::from_utf8_lossy(&up.stderr).trim());
        }
        let url = quiet(&svc, &["url"]).output()?;
        let url = String::from_utf8_lossy(&url.stdout).trim().to_string();
        if !url.starts_with("http://127.0.0.1:") {
            bail!("unexpected address from the background service: {url}");
        }
        Ok(url)
    }

    fn origin(url: &str) -> String {
        url.split('/').take(3).collect::<Vec<_>>().join("/")
    }

    /// Open a web link in the user's browser. Only https links, and never through a shell.
    fn open_in_browser(url: &str) {
        if url.starts_with("https://") {
            let _ = Command::new("explorer.exe").arg(url).spawn();
        }
    }

    fn icon() -> Option<Icon> {
        let png = include_bytes!("../../packaging/windows/icon-64.png");
        let mut reader = png::Decoder::new(std::io::Cursor::new(&png[..]))
            .read_info()
            .ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        buf.truncate(info.buffer_size());
        Icon::from_rgba(buf, info.width, info.height).ok()
    }

    fn tray_icon() -> Option<tray_icon::Icon> {
        let png = include_bytes!("../../packaging/windows/icon-64.png");
        let mut reader = png::Decoder::new(std::io::Cursor::new(&png[..]))
            .read_info()
            .ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        buf.truncate(info.buffer_size());
        tray_icon::Icon::from_rgba(buf, info.width, info.height).ok()
    }

    struct App {
        home: String,
        context: WebContext,
        windows: HashMap<WindowId, (Window, WebView)>,
        proxy: EventLoopProxy<UserEvent>,
    }

    impl App {
        fn open(&mut self, target: &EventLoopWindowTarget<UserEvent>, url: &str) -> Result<()> {
            let window = WindowBuilder::new()
                .with_title("Fleet-51")
                .with_inner_size(tao::dpi::LogicalSize::new(1100.0, 760.0))
                .with_min_inner_size(tao::dpi::LogicalSize::new(360.0, 480.0))
                .with_window_icon(icon())
                .build(target)?;
            let home = origin(&self.home);
            let nav_home = home.clone();
            let proxy = self.proxy.clone();
            let webview = WebViewBuilder::new_with_web_context(&mut self.context)
                .with_url(url)
                // Only Fleet-51's own pages load in the window; https links go to the browser,
                // anything else is refused.
                .with_navigation_handler(move |to: String| {
                    if origin(&to) == nav_home || to == "about:blank" {
                        true
                    } else {
                        open_in_browser(&to);
                        false
                    }
                })
                .with_new_window_req_handler(move |to: String, _features| {
                    if origin(&to) == home {
                        let _ = proxy.send_event(UserEvent::Open(to));
                    } else {
                        open_in_browser(&to);
                    }
                    NewWindowResponse::Deny
                })
                .build(&window)?;
            self.windows.insert(window.id(), (window, webview));
            Ok(())
        }
    }

    pub fn run() -> Result<()> {
        let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        let proxy = event_loop.create_proxy();
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Menu(e));
        }));
        let p = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Tray(e));
        }));

        let home = start_service()?;
        let data = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Fleet-51")
            .join("WebView2");
        let mut app = App {
            home: home.clone(),
            context: WebContext::new(Some(data)),
            windows: HashMap::new(),
            proxy,
        };

        let menu = Menu::new();
        let open = MenuItem::new("Open Fleet-51", true, None);
        let new_window = MenuItem::new("New Window", true, None);
        let quit = MenuItem::new("Quit (bots keep running)", true, None);
        let stop = MenuItem::new("Quit and Stop Bots", true, None);
        menu.append_items(&[
            &open,
            &new_window,
            &PredefinedMenuItem::separator(),
            &quit,
            &stop,
        ])?;
        let mut tray: Option<TrayIcon> = None;

        event_loop.run(move |event, target, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::NewEvents(StartCause::Init) => {
                    if let Err(e) = app.open(target, &app.home.clone()) {
                        message_box(&format!("Fleet-51 could not open its window:\n\n{e:#}"));
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    tray = TrayIconBuilder::new()
                        .with_tooltip("Fleet-51")
                        .with_menu(Box::new(menu.clone()))
                        .with_icon(tray_icon().expect("built-in icon"))
                        .build()
                        .ok();
                }
                Event::UserEvent(UserEvent::Open(url)) => {
                    let _ = app.open(target, &url);
                }
                Event::UserEvent(UserEvent::Tray(TrayIconEvent::DoubleClick { .. })) => {
                    if app.windows.is_empty() {
                        let _ = app.open(target, &app.home.clone());
                    } else if let Some((w, _)) = app.windows.values().next() {
                        w.set_focus();
                    }
                }
                Event::UserEvent(UserEvent::Menu(e)) => {
                    if e.id == open.id() {
                        if let Some((w, _)) = app.windows.values().next() {
                            w.set_focus();
                        } else {
                            let _ = app.open(target, &app.home.clone());
                        }
                    } else if e.id == new_window.id() {
                        let _ = app.open(target, &app.home.clone());
                    } else if e.id == quit.id() {
                        tray.take();
                        *control_flow = ControlFlow::Exit;
                    } else if e.id == stop.id() {
                        if let Ok(svc) = service() {
                            let _ = quiet(&svc, &["down"]).status();
                        }
                        tray.take();
                        *control_flow = ControlFlow::Exit;
                    }
                }
                // Closing a window keeps the bots (and automations) running; the tray icon
                // stays so the window can be reopened or everything stopped.
                Event::WindowEvent {
                    window_id,
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    app.windows.remove(&window_id);
                }
                _ => {}
            }
        });
    }

    pub fn message_box(text: &str) {
        // A plain dialog without extra dependencies: PowerShell's message box, text passed in
        // an environment variable (never pasted into the script).
        let _ = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Add-Type -AssemblyName PresentationFramework; [System.Windows.MessageBox]::Show($env:FLEET51_MESSAGE, 'Fleet-51') | Out-Null",
            ])
            .env("FLEET51_MESSAGE", text)
            .creation_flags(NO_WINDOW)
            .status();
    }
}
