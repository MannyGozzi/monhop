//! The notch island: MonHop's connection status around the MacBook camera, and clipboard copies
//! when the user asks for them. macOS only, shown only on a display with a camera notch, and on
//! until the user turns it off; while off, nothing of it runs. Without a notch the commands report
//! it as unsupported.

#[cfg(target_os = "macos")]
pub mod check;
#[cfg(target_os = "macos")]
mod driver;
#[cfg(target_os = "macos")]
mod macos;

// Pure and tested everywhere; off macOS only their tests use them.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod feed;
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod geometry;
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod model;
// Every target: the live pointer's marks read their style here too.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod tokens;

use serde::Serialize;
#[cfg(not(target_os = "macos"))]
use tauri::AppHandle;

/// What the Settings card shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IslandView {
    /// A display with a camera notch is connected; the card shows only then.
    supported: bool,
    enabled: bool,
    /// Clipboard copies show beside the notch; off until the user turns it on.
    clipboard_notices: bool,
    error: Option<String>,
}

/// Wakes the island to read the app's state again; a no-op unless the island is on.
pub fn notify() {
    #[cfg(target_os = "macos")]
    driver::wake();
}

#[cfg(target_os = "macos")]
pub use enabled::Island;

#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn island_status(app: tauri::AppHandle) -> Result<IslandView, String> {
    enabled::status(app).await
}

/// Shows or removes the island at once, even when saving fails.
#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn island_set_enabled(
    app: tauri::AppHandle,
    enabled: bool,
) -> Result<IslandView, String> {
    enabled::set_enabled(app, enabled).await
}

/// Starts or stops clipboard notices at once, even when saving fails.
#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn island_set_clipboard_notices(
    app: tauri::AppHandle,
    enabled: bool,
) -> Result<IslandView, String> {
    enabled::set_clipboard_notices(app, enabled).await
}

#[cfg(target_os = "macos")]
mod enabled {
    use std::sync::{Arc, OnceLock};

    use serde::Deserialize;
    use tauri::{AppHandle, Manager};

    use super::IslandView;
    use crate::switch_preference::{SwitchNames, SwitchPreference, SwitchView};

    const NAMES: SwitchNames = SwitchNames {
        file: "island.json",
        setting: "notch island",
        logged: "notch island",
    };

    const CLIPBOARD_NAMES: SwitchNames = SwitchNames {
        file: "island-clipboard.json",
        setting: "clipboard notices",
        logged: "notch island clipboard notices",
    };

    /// The handle the switch's apply function starts the island with.
    static APP: OnceLock<AppHandle> = OnceLock::new();

    pub struct Island {
        shown: SwitchPreference,
        clipboard: SwitchPreference,
    }

    impl Island {
        /// Loads both switches, the island on and its clipboard notices off until the user
        /// changes them, and starts the island if it is on. Without `show` (the window check) the
        /// switches are kept but the island never shows anything.
        pub fn start(app: &AppHandle, show: bool) -> Arc<Self> {
            let _ = APP.set(app.clone());
            // Loaded first, so a feed the island's switch starts reads it from its first snapshot.
            let clipboard = SwitchPreference::start(
                app,
                CLIPBOARD_NAMES,
                false,
                super::driver::set_clipboard_notices,
            );
            let apply: fn(bool) = if show { apply } else { |_| {} };
            Arc::new(Self {
                shown: SwitchPreference::start(app, NAMES, true, apply),
                clipboard,
            })
        }
    }

    fn apply(enabled: bool) {
        let Some(app) = APP.get() else {
            return;
        };
        if enabled {
            if let Err(error) = super::tokens::tokens() {
                log::warn!("island: not started, {error}");
                return;
            }
            super::driver::start(app);
        } else {
            super::driver::stop(app);
        }
    }

    #[derive(Deserialize)]
    struct Switch {
        enabled: bool,
        error: Option<String>,
    }

    fn switch(view: SwitchView, names: &SwitchNames) -> Result<Switch, String> {
        serde_json::to_value(&view)
            .and_then(serde_json::from_value)
            .map_err(|_| names.not_saved())
    }

    async fn view_of(app: &AppHandle, island: &Island) -> Result<IslandView, String> {
        let shown = switch(island.shown.view(), &NAMES)?;
        let clipboard = switch(island.clipboard.view(), &CLIPBOARD_NAMES)?;
        let supported = crate::run_on_main_thread_for_result(
            app,
            super::macos::has_notch,
            "The notch island could not read the displays. Try again.",
            "The notch island could not read the displays. Try again.",
        )
        .await?;
        let error = shown.error.or(clipboard.error).or_else(|| {
            (shown.enabled && super::tokens::tokens().is_err())
                .then(|| "The notch island could not start.".to_owned())
        });
        Ok(IslandView {
            supported,
            enabled: shown.enabled,
            clipboard_notices: clipboard.enabled,
            error,
        })
    }

    pub async fn status(app: AppHandle) -> Result<IslandView, String> {
        let island = app.state::<Arc<Island>>().inner().clone();
        view_of(&app, &island).await
    }

    pub async fn set_enabled(app: AppHandle, enabled: bool) -> Result<IslandView, String> {
        let island = app.state::<Arc<Island>>().inner().clone();
        let changing = Arc::clone(&island);
        tauri::async_runtime::spawn_blocking(move || changing.shown.set_enabled(enabled))
            .await
            .map_err(|_| NAMES.not_saved())?;
        view_of(&app, &island).await
    }

    pub async fn set_clipboard_notices(
        app: AppHandle,
        enabled: bool,
    ) -> Result<IslandView, String> {
        let island = app.state::<Arc<Island>>().inner().clone();
        let changing = Arc::clone(&island);
        tauri::async_runtime::spawn_blocking(move || changing.clipboard.set_enabled(enabled))
            .await
            .map_err(|_| CLIPBOARD_NAMES.not_saved())?;
        view_of(&app, &island).await
    }
}

#[cfg(not(target_os = "macos"))]
const UNSUPPORTED: IslandView = IslandView {
    supported: false,
    enabled: false,
    clipboard_notices: false,
    error: None,
};

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn island_status(app: AppHandle) -> Result<IslandView, String> {
    let _ = app;
    Ok(UNSUPPORTED)
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn island_set_enabled(app: AppHandle, enabled: bool) -> Result<IslandView, String> {
    let _ = (app, enabled);
    Ok(UNSUPPORTED)
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn island_set_clipboard_notices(
    app: AppHandle,
    enabled: bool,
) -> Result<IslandView, String> {
    let _ = (app, enabled);
    Ok(UNSUPPORTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_names_its_fields_for_the_window() {
        let view = IslandView {
            supported: true,
            enabled: false,
            clipboard_notices: true,
            error: None,
        };
        assert_eq!(
            serde_json::to_value(view).unwrap(),
            serde_json::json!({
                "supported": true,
                "enabled": false,
                "clipboardNotices": true,
                "error": null
            })
        );
    }
}
