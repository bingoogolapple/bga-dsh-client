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

pub(crate) fn resize(window: &tauri::Window) -> tauri::Result<()> {
    // GTK's vertical Box allocates the remaining space to the child WebView.
    // Wry's set_bounds cannot position children packed into that container.
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
                let result = shell
                    .parent()
                    .and_then(|parent| parent.downcast::<gtk::Box>().ok())
                    .ok_or_else(|| "Main WebView is not in its GTK container".to_string())
                    .map(|container| {
                        // GtkBox combines its packing flag with the widget's
                        // computed expansion. WebKit requests vexpand itself,
                        // so packing=false alone still consumes the full window
                        // and leaves the DSH child one pixel below the bottom.
                        shell.set_hexpand(true);
                        shell.set_vexpand(!content_visible);
                        container.set_child_packing(
                            &shell,
                            !content_visible,
                            true,
                            0,
                            gtk::PackType::Start,
                        );
                        shell.set_size_request(
                            -1,
                            if content_visible {
                                TOOLBAR_HEIGHT as i32
                            } else {
                                -1
                            },
                        );
                        shell.queue_resize();
                    });
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
        window
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
            .map_err(|e| e.to_string())?
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
}
