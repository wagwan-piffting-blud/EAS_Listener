//! Desktop tray shim: the "double click it and it keeps watching" front end.
//!
//! The listener is a background service, so it gets a tray icon rather than a window. The
//! dashboard opens in the user's own browser, which is already the real interface -- embedding a
//! WebView would add a large dependency to show the same page in a worse window.
//!
//! Compiled only with the `tray` feature, so container and headless builds never see it.

use anyhow::{Context, Result};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::StartCause;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};

const ICON_PNG: &[u8] = include_bytes!("../web_server/assets/favicon-96x96.png");

fn load_icon() -> Result<Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(ICON_PNG));
    let mut reader = decoder
        .read_info()
        .context("Failed to read tray icon PNG")?;
    let size = reader
        .output_buffer_size()
        .context("Tray icon PNG reports no decodable size")?;
    let mut buffer = vec![0; size];
    let info = reader
        .next_frame(&mut buffer)
        .context("Failed to decode tray icon PNG")?;
    buffer.truncate(info.buffer_size());

    // tray-icon wants straight RGBA; widen anything narrower rather than requiring a specific
    // source format.
    let rgba = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::Grayscale => buffer.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buffer
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[v, a]| [v, v, v, a])
            .collect(),
        other => anyhow::bail!("Unsupported tray icon colour type: {other:?}"),
    };

    Icon::from_rgba(rgba, info.width, info.height).context("Failed to build the tray icon")
}

enum TrayEvent {
    Menu(MenuEvent),
    /// The listener's task ended: `Some` with why when it failed.
    ListenerStopped(Option<String>),
}

/// Handed to the listener's task so that its end also ends the tray. Otherwise the icon would
/// outlive a listener that stopped, and a service manager watching the process would never see
/// it fail and restart it.
pub struct ListenerStopped(EventLoopProxy<TrayEvent>);

impl ListenerStopped {
    pub fn notify(&self, error: Option<String>) {
        let _ = self.0.send_event(TrayEvent::ListenerStopped(error));
    }
}

struct TrayApp {
    dashboard_url: String,
    log_dir: std::path::PathBuf,
    menu: Option<Menu>,
    open_id: MenuId,
    logs_id: MenuId,
    quit_id: MenuId,
    /// Created once the event loop is running, which macOS requires; held from then on, since
    /// dropping it removes the icon.
    tray: Option<TrayIcon>,
    failure: Option<anyhow::Error>,
}

impl TrayApp {
    fn create_tray(&mut self) -> Result<()> {
        let menu = self.menu.take().context("The tray menu was already used")?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(format!("EAS Listener - {}", self.dashboard_url))
            .with_icon(load_icon()?)
            .build()
            .context("Failed to create the tray icon")?;
        self.tray = Some(tray);
        Ok(())
    }
}

impl ApplicationHandler<TrayEvent> for TrayApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if cause == StartCause::Init && self.tray.is_none() {
            if let Err(err) = self.create_tray() {
                self.failure = Some(err);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: TrayEvent) {
        match event {
            TrayEvent::Menu(event) if event.id == self.open_id => {
                if let Err(err) = open::that_detached(&self.dashboard_url) {
                    tracing::warn!("Could not open {}: {}", self.dashboard_url, err);
                }
            }
            TrayEvent::Menu(event) if event.id == self.logs_id => {
                if let Err(err) = open::that_detached(&self.log_dir) {
                    tracing::warn!("Could not open {}: {}", self.log_dir.display(), err);
                }
            }
            TrayEvent::Menu(event) if event.id == self.quit_id => {
                tracing::info!("Quit selected from the tray; shutting down.");
                event_loop.exit();
            }
            TrayEvent::Menu(_) => {}
            TrayEvent::ListenerStopped(error) => {
                self.failure = error.map(|message| anyhow::anyhow!(message));
                event_loop.exit();
            }
        }
    }
}

/// Runs the tray on the calling thread until the user quits or the listener stops. Must be the
/// main thread: that is a hard requirement of the platform event loops underneath winit.
/// `start` is called once the loop exists, to start the listener with a way to end it.
pub fn run(
    dashboard_url: String,
    log_dir: std::path::PathBuf,
    start: impl FnOnce(ListenerStopped),
) -> Result<()> {
    let menu = Menu::new();
    let open_item = MenuItem::new("Open Dashboard", true, None);
    let logs_item = MenuItem::new("Open Log Folder", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    menu.append(&open_item)?;
    menu.append(&logs_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    let mut builder = EventLoop::<TrayEvent>::with_user_event();
    // A menu bar item, not an app: no Dock icon and no application menu.
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder
        .build()
        .context("Failed to create the tray event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);

    // Menu clicks arrive on the platform's own callback; forwarding them wakes the loop, which
    // is waiting for events and would otherwise not look until something else woke it.
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(TrayEvent::Menu(event));
    }));
    start(ListenerStopped(event_loop.create_proxy()));

    let mut app = TrayApp {
        dashboard_url,
        log_dir,
        menu: Some(menu),
        open_id: open_item.id().clone(),
        logs_id: logs_item.id().clone(),
        quit_id: quit_item.id().clone(),
        tray: None,
        failure: None,
    };

    event_loop
        .run_app(&mut app)
        .context("The tray event loop failed")?;
    match app.failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}
