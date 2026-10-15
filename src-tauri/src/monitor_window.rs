//! The receiver as a real desktop window: a native SysX window (tao) that
//! hosts the monitor page in WebView2 (wry). Closing it quits SysX; if the
//! backend goes away the window closes too.
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::thread;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::{Icon, Theme, WindowBuilder};
use wry::{WebContext, WebViewBuilder};

/// Same near-black blue as the monitor page, so no white flash on open.
const BACKGROUND: (u8, u8, u8, u8) = (9, 12, 16, 255);

/// Shows the monitor until the window is closed or `backend_gone` fires.
/// Returns the receiver back unused when no window/WebView2 can be created,
/// so the caller can fall back to a browser window.
pub fn run(
    config_port: u16,
    data_dir: &Path,
    backend_gone: Receiver<()>,
) -> Result<(), Receiver<()>> {
    let mut event_loop = EventLoopBuilder::<()>::with_user_event().build();
    let Ok(window) = WindowBuilder::new()
        .with_title("SysX")
        .with_inner_size(LogicalSize::new(550.0, 620.0))
        .with_min_inner_size(LogicalSize::new(360.0, 420.0))
        .with_window_icon(app_icon())
        // Dark title bar to match the near-black receiver page.
        .with_theme(Some(Theme::Dark))
        .with_visible(false)
        .build(&event_loop)
    else {
        return Err(backend_gone);
    };
    // WebView2 profile under app data (not next to the exe, which may be
    // read-only or moved between machines).
    let mut context = WebContext::new(Some(data_dir.join("webview")));
    let Ok(webview) = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(format!("http://127.0.0.1:{config_port}/monitor.html"))
        .with_background_color(BACKGROUND)
        // An app window, not a browser: no Back/Reload/Inspect menu.
        .with_initialization_script(
            "document.addEventListener('contextmenu', (e) => e.preventDefault());",
        )
        .build(&window)
    else {
        return Err(backend_gone);
    };
    window.set_visible(true);

    let proxy = event_loop.create_proxy();
    thread::spawn(move || {
        if backend_gone.recv().is_ok() {
            let _ = proxy.send_event(());
        }
    });
    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _keep_alive = &webview;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
            | Event::UserEvent(()) => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
    Ok(())
}

fn app_icon() -> Option<Icon> {
    let image = image::load_from_memory(include_bytes!("../icons/icon.png"))
        .ok()?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).ok()
}
