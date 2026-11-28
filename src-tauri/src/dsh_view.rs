//! DSH content is a native child WebView, not a cross-site HTML iframe.
//!
//! The toolbar keeps its local Tauri origin and IPC permissions. The remote DSH
//! document receives no remote capability and performs its own first-party
//! authentication, HTTP requests and WebSocket connections.

use tauri::{
    webview::WebviewBuilder, AppHandle, LogicalPosition, LogicalSize, Manager, WebviewUrl,
};

use crate::{service::ServiceManager, AppState};

const LABEL: &str = "dsh-content";
// Matches #topbar in ui/assets/app.css; unlike proportional auto-resizing,
// this fixed logical inset stays correct when the window or display changes.
const TOOLBAR_HEIGHT: f64 = 40.0;

#[derive(Default)]
pub(crate) struct State {
    last_launch_url: Option<String>,
}

fn is_dsh_url(url: &tauri::Url) -> bool {
    url.scheme() == "http"
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        && url.port_or_known_default() == Some(crate::service::DSH_PORT)
        && url.username().is_empty()
        && url.password().is_none()
}

fn content_size(width: f64, height: f64) -> LogicalSize<f64> {
    LogicalSize::new(width.max(1.0), (height - TOOLBAR_HEIGHT).max(1.0))
}

#[cfg(any(target_os = "linux", test))]
fn resize_direction(
    (x, y): (f64, f64),
    (width, height): (f64, f64),
    inset: f64,
) -> Option<(i8, i8)> {
    if x < 0.0 || y < 0.0 || x >= width || y >= height {
        return None;
    }
    let horizontal = if x < inset {
        -1
    } else if x >= width - inset {
        1
    } else {
        0
    };
    let vertical = if y < inset {
        -1
    } else if y >= height - inset {
        1
    } else {
        0
    };
    ((horizontal, vertical) != (0, 0)).then_some((horizontal, vertical))
}

#[cfg(target_os = "linux")]
fn gtk_resize_target(
    widget: &impl gtk::prelude::IsA<gtk::Widget>,
    root: (f64, f64),
) -> Option<(gtk::gdk::Window, gtk::gdk::WindowEdge)> {
    use gtk::{gdk::WindowEdge, prelude::*};

    let toplevel = widget.toplevel()?.downcast::<gtk::Window>().ok()?;
    if toplevel.is_decorated() || !toplevel.is_resizable() || toplevel.is_maximized() {
        return None;
    }
    let window = toplevel.window()?;
    if window.state().contains(gtk::gdk::WindowState::FULLSCREEN) {
        return None;
    }
    let (window_x, window_y) = window.position();
    // Match Tauri's GTK edge inset. Root coordinates keep the child's 40px
    // toolbar offset from being mistaken for the top edge of the window.
    let direction = resize_direction(
        (root.0 - f64::from(window_x), root.1 - f64::from(window_y)),
        (f64::from(window.width()), f64::from(window.height())),
        f64::from(window.scale_factor() * 5),
    )?;
    let edge = match direction {
        (-1, -1) => WindowEdge::NorthWest,
        (0, -1) => WindowEdge::North,
        (1, -1) => WindowEdge::NorthEast,
        (-1, 0) => WindowEdge::West,
        (1, 0) => WindowEdge::East,
        (-1, 1) => WindowEdge::SouthWest,
        (0, 1) => WindowEdge::South,
        (1, 1) => WindowEdge::SouthEast,
        _ => return None,
    };
    Some((window, edge))
}

fn attach_resize_handler(view: &tauri::Webview) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    view.with_webview(|platform| {
        use gtk::{gdk, glib::Propagation, prelude::*};

        // Tauri only installs this behavior on its original WindowContent
        // WebView. Our sibling content view must handle its own window edges.
        let child = platform.inner();
        child.add_events(
            gdk::EventMask::BUTTON1_MOTION_MASK
                | gdk::EventMask::BUTTON_PRESS_MASK
                | gdk::EventMask::TOUCH_MASK,
        );
        child.connect_button_press_event(|child, event| {
            if event.button() == 1 {
                let (x, y) = event.root();
                if let Some((window, edge)) = gtk_resize_target(child, (x, y)) {
                    window.begin_resize_drag(edge, 1, x as i32, y as i32, event.time());
                }
            }
            Propagation::Proceed
        });
        child.connect_touch_event(|child, event| {
            if event.event_type() != gdk::EventType::TouchBegin {
                return Propagation::Proceed;
            }
            let (Some((x, y)), Some(device)) = (event.root_coords(), event.device()) else {
                return Propagation::Proceed;
            };
            if let Some((window, edge)) = gtk_resize_target(child, (x, y)) {
                window.begin_resize_drag_for_device(
                    edge,
                    &device,
                    0,
                    x as i32,
                    y as i32,
                    event.time(),
                );
            }
            Propagation::Proceed
        });
    })
    .map_err(|e| e.to_string())?;
    #[cfg(not(target_os = "linux"))]
    let _ = view;
    Ok(())
}

pub(crate) fn resize(window: &tauri::Window) -> tauri::Result<()> {
    // GTK's overlay and its live content Box track the window allocation;
    // Wry's set_bounds cannot position a WebView packed into that Box.
    #[cfg(target_os = "linux")]
    {
        let _ = window;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        if window.label() != "main" {
            return Ok(());
        }
        if let Some(view) = window.app_handle().get_webview(LABEL) {
            let size = window
                .inner_size()?
                .to_logical::<f64>(window.scale_factor()?);
            view.set_bounds(tauri::Rect {
                position: LogicalPosition::new(0.0, TOOLBAR_HEIGHT).into(),
                size: content_size(size.width, size.height).into(),
            })?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn arrange_gtk_shell(shell: &gtk::Widget, content_visible: bool) -> Result<(), String> {
    use gtk::{glib::translate::ToGlibPtr, prelude::*};

    let overlay = if let Some(overlay) = shell
        .parent()
        .and_then(|parent| parent.downcast::<gtk::Overlay>().ok())
    {
        overlay
    } else {
        let container = shell
            .parent()
            .and_then(|parent| parent.downcast::<gtk::Box>().ok())
            .ok_or_else(|| "Main WebView is not in its GTK container".to_string())?;
        let window = container
            .parent()
            .and_then(|parent| parent.downcast::<gtk::Window>().ok())
            .ok_or_else(|| "Main GTK container is not in its window".to_string())?;
        let overlay = gtk::Overlay::new();
        overlay.connect_local("get-child-position", false, |values| {
            let Ok(overlay) = values[0].get::<gtk::Overlay>() else {
                return Some(false.to_value());
            };
            let Some(container) = overlay
                .child()
                .and_then(|child| child.downcast::<gtk::Box>().ok())
            else {
                return Some(false.to_value());
            };
            // GTK exposes this out parameter as a caller-owned boxed rectangle
            // with G_SIGNAL_TYPE_STATIC_SCOPE; gtk-rs omits the typed binding.
            // SAFETY: borrow it only for this synchronous signal and never
            // retain it after GTK's caller-owned allocation leaves scope.
            let allocation = unsafe {
                gtk::glib::gobject_ffi::g_value_get_boxed(values[2].to_glib_none().0)
                    .cast::<gtk::gdk::ffi::GdkRectangle>()
                    .as_mut()
            };
            let Some(allocation) = allocation else {
                return Some(false.to_value());
            };
            allocation.x = 0;
            allocation.y = 0;
            allocation.width = overlay.allocated_width().max(1);
            allocation.height = if container.margin_top() > 0 {
                TOOLBAR_HEIGHT as i32
            } else {
                overlay.allocated_height()
            }
            .max(1);
            Some(true.to_value())
        });

        // Keep Tao's default Box in the live tree for subsequent add_child
        // calls. Moving only the shell into the overlay also preserves the
        // shell -> container -> GTK window chain expected by Tauri's existing
        // undecorated-window resize handlers.
        container.remove(shell);
        window.remove(&container);
        overlay.add(&container);
        overlay.add_overlay(shell);
        overlay.set_overlay_pass_through(shell, false);
        window.add(&overlay);
        container.set_hexpand(true);
        container.set_vexpand(true);
        shell.set_size_request(-1, -1);
        shell.set_hexpand(true);
        shell.set_vexpand(true);
        overlay.show();
        container.show();
        shell.show();

        if std::env::var_os("DSH_WEBVIEW_LAYOUT_DEBUG").is_some() {
            let shell = shell.downgrade();
            let container = container.downgrade();
            overlay.connect_size_allocate(move |_, _| {
                if let (Some(shell), Some(container)) = (shell.upgrade(), container.upgrade()) {
                    let children: Vec<_> = container
                        .children()
                        .iter()
                        .map(|child| (child.is_visible(), child.allocation()))
                        .collect();
                    eprintln!(
                        "DSH GTK allocation: shell={:?}, content_box={:?}, children={children:?}",
                        shell.allocation(),
                        container.allocation(),
                    );
                }
            });
        }
        overlay
    };
    let container = overlay
        .child()
        .and_then(|child| child.downcast::<gtk::Box>().ok())
        .ok_or_else(|| "DSH overlay is missing its GTK content container".to_string())?;
    // The shell is an overlay child, so its WebKit minimum size cannot consume
    // the DSH document's allocation. This margin also selects the shell height
    // synchronously, without depending on an asynchronously queued show/hide.
    container.set_margin_top(if content_visible {
        TOOLBAR_HEIGHT as i32
    } else {
        0
    });
    overlay.queue_resize();
    Ok(())
}

fn layout_shell(app: &AppHandle, content_visible: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let shell = app
            .get_webview("main")
            .ok_or_else(|| "Main shell is unavailable".to_string())?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        // Called only by asynchronous commands, never by a GTK event callback.
        // with_webview performs all GTK operations on the UI thread.
        shell
            .with_webview(move |platform| {
                use gtk::prelude::*;
                let shell = platform.inner();
                let result = arrange_gtk_shell(shell.upcast_ref(), content_visible);
                let _ = tx.send(result);
            })
            .map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (app, content_visible);
        Ok(())
    }
}

pub(crate) async fn load(app: &AppHandle, force: bool, revision: u64) -> Result<(), String> {
    let app_state = app.state::<AppState>();
    if app_state.sm.current_revision() != revision {
        return Ok(());
    }
    let url = crate::dsh_launch_url(app.clone()).await;
    if !ServiceManager::is_up() {
        return hide(app, revision);
    }
    let url = tauri::Url::parse(&url).map_err(|_| "Invalid DSH address".to_string())?;
    if !is_dsh_url(&url) {
        return Err("DSH address must use the local service".into());
    }

    let state = app.state::<AppState>();
    let mut state = crate::state::lock(&state.dsh_view);
    // Token lookup is asynchronous. A stopped/restarted service must not be
    // shown again by an older pending load, or hidden by an older command.
    if app_state.sm.current_revision() != revision {
        return Ok(());
    }
    let window = app
        .get_window("main")
        .ok_or_else(|| "Main window is unavailable".to_string())?;
    let view = if let Some(view) = app.get_webview(LABEL) {
        if force || state.last_launch_url.as_deref() != Some(url.as_str()) {
            view.navigate(url.clone()).map_err(|e| e.to_string())?;
        }
        view
    } else {
        let size = window
            .inner_size()
            .map_err(|e| e.to_string())?
            .to_logical::<f64>(window.scale_factor().map_err(|e| e.to_string())?);
        let view = window
            .add_child(
                WebviewBuilder::new(LABEL, WebviewUrl::External(url.clone()))
                    .on_navigation(|url| {
                        if is_dsh_url(url) || url.as_str() == "about:blank" {
                            true
                        } else {
                            // Keep external links out of the privileged local shell.
                            if matches!(url.scheme(), "http" | "https" | "mailto") {
                                crate::open_url(url.as_str());
                            }
                            false
                        }
                    })
                    .on_new_window(|url, _| {
                        if matches!(url.scheme(), "http" | "https" | "mailto") {
                            crate::open_url(url.as_str());
                        }
                        tauri::webview::NewWindowResponse::Deny
                    }),
                LogicalPosition::new(0.0, TOOLBAR_HEIGHT),
                content_size(size.width, size.height),
            )
            .map_err(|e| e.to_string())?;
        attach_resize_handler(&view)?;
        view
    };
    if app_state.sm.current_revision() != revision {
        view.hide().map_err(|e| e.to_string())?;
        return Ok(());
    }
    state.last_launch_url = Some(url.to_string());
    resize(&window).map_err(|e| e.to_string())?;
    layout_shell(app, true)?;
    view.show().map_err(|e| e.to_string())?;
    if force && window.is_focused().unwrap_or(false) {
        let _ = view.set_focus();
    }
    Ok(())
}

pub(crate) fn hide(app: &AppHandle, revision: u64) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut view_state = crate::state::lock(&state.dsh_view);
    if state.sm.current_revision() != revision {
        return Ok(());
    }
    if let Some(view) = app.get_webview(LABEL) {
        view.hide().map_err(|e| e.to_string())?;
        // Stop the old document/WebSocket, just as clearing the iframe did.
        if view_state.last_launch_url.is_some() {
            view.navigate("about:blank".parse().unwrap())
                .map_err(|e| e.to_string())?;
        }
    }
    view_state.last_launch_url = None;
    layout_shell(app, false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_stays_on_the_dsh_listener() {
        for allowed in ["http://localhost:3080/", "http://127.0.0.1:3080/api"] {
            assert!(is_dsh_url(&allowed.parse().unwrap()));
        }
        for denied in [
            "tauri://localhost/",
            "http://localhost:1430/",
            "https://localhost:3080/",
            "http://localhost.example:3080/",
            "http://user:password@localhost:3080/",
        ] {
            assert!(!is_dsh_url(&denied.parse().unwrap()));
        }
    }

    #[test]
    fn content_keeps_a_fixed_toolbar_inset() {
        assert_eq!(content_size(1440.0, 900.0), LogicalSize::new(1440.0, 860.0));
        assert_eq!(content_size(0.0, 20.0), LogicalSize::new(1.0, 1.0));
    }

    #[test]
    fn resize_edges_use_window_coordinates_and_preserve_content_clicks() {
        let size = (1600.0, 1000.0);
        assert_eq!(resize_direction((1.0, 400.0), size, 5.0), Some((-1, 0)));
        assert_eq!(resize_direction((1598.0, 400.0), size, 5.0), Some((1, 0)));
        assert_eq!(resize_direction((800.0, 998.0), size, 5.0), Some((0, 1)));
        assert_eq!(resize_direction((1.0, 998.0), size, 5.0), Some((-1, 1)));
        assert_eq!(resize_direction((1598.0, 998.0), size, 5.0), Some((1, 1)));
        assert_eq!(resize_direction((800.0, 40.0), size, 5.0), None);
        assert_eq!(resize_direction((800.0, 500.0), size, 5.0), None);
        assert_eq!(resize_direction((-1.0, 500.0), size, 5.0), None);
        // Match Tauri's scaled 5px inset while retaining the same client area.
        assert_eq!(resize_direction((1591.0, 500.0), size, 10.0), Some((1, 0)));
        assert_eq!(resize_direction((1589.0, 500.0), size, 10.0), None);
    }
}
