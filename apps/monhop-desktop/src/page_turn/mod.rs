//! The chevron that blooms beside this computer's pointer when a swipe from another computer turns
//! the page, so the user sees the swipe land.

use tauri::AppHandle;

#[cfg(target_os = "macos")]
mod macos;

/// Hands the transport a sink that passes each page turn to the renderer and never waits.
pub fn register(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let registered = {
        let app = app.clone();
        monhop_transport::session_native::set_page_turn_mark(move |turn| {
            let _ = app.run_on_main_thread(move || {
                if let Some(mtm) = objc2::MainThreadMarker::new() {
                    macos::show(mtm, turn);
                }
            });
        })
    };
    #[cfg(windows)]
    let registered = {
        let _ = app;
        monhop_transport::session_native::set_page_turn_mark(
            monhop_platform_windows::page_turn::show,
        )
    };
    #[cfg(any(target_os = "macos", windows))]
    if !registered {
        log::warn!("page turn: mark already registered");
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = app;
}
