//! Native menu-bar controls. Refreshes only project already-held controller state.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tauri::{
    AppHandle, Manager,
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};

use crate::lifecycle::AppController;

const TRAY_ID: &str = "monhop-tray";
const STATUS_ID: &str = "tray-status";
const SHOW_ID: &str = "tray-show";
const HIDE_ID: &str = "tray-hide";
const PAUSE_ID: &str = "tray-pause";
const QUIT_ID: &str = "tray-quit";
const MAIN_WINDOW: &str = "main";
const ICON_SIDE: usize = 32;
/// Drawn under the white glyph on Windows so it stays visible on a pale taskbar.
const WINDOWS_BACKPLATE: [u8; 4] = [24, 32, 30, 235];
const GLYPH_WHITE: [u8; 3] = [236, 236, 236];
/// sRGB of the dark `--destructive` token in ui/styles.css.
const GLYPH_ERROR: [u8; 3] = [255, 99, 103];
/// Idle keeps the white glyph and drops it to 40% opacity.
const IDLE_OPACITY: u8 = 102;
const TRAY_MASK: &[u8; ICON_SIDE * ICON_SIDE] = include_bytes!("../icons/tray-mask.bin");
/// Well under the 1 s sharing supervision tick, so a change still reaches the tray promptly.
const REFRESH_INTERVAL: Duration = Duration::from_millis(200);

struct TrayState {
    _icon: tauri::tray::TrayIcon,
    status_item: MenuItem<tauri::Wry>,
    presentation: Mutex<TrayPresentation>,
    last_refresh: Mutex<Option<Instant>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IconTone {
    Active,
    Idle,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrayPresentation {
    tone: IconTone,
    label: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StatusInput {
    sharing_active: bool,
    busy: bool,
    sharing_phase: &'static str,
    /// Named so the menu can say which computer is on the other end of the link.
    peer_platform: Option<&'static str>,
}

impl TrayPresentation {
    fn tooltip(&self) -> String {
        format!("MonHop — {}", self.label)
    }

    fn menu_label(&self) -> String {
        format!("Status: {}", self.label)
    }
}

fn peer_name(platform: Option<&str>) -> &'static str {
    match platform {
        Some("macos") => "Mac",
        _ => "Windows PC",
    }
}

fn presentation_for(input: StatusInput) -> TrayPresentation {
    if input.sharing_phase == "error" {
        return TrayPresentation {
            tone: IconTone::Error,
            label: "Needs attention".to_owned(),
        };
    }
    if input.sharing_active {
        return TrayPresentation {
            tone: IconTone::Active,
            label: "Sharing active".to_owned(),
        };
    }
    let label = match input.sharing_phase {
        "connecting" => "Connecting".to_owned(),
        "connected" => format!("Connected to {}", peer_name(input.peer_platform)),
        "reconnecting" => "Reconnecting".to_owned(),
        "stopping" => "Stopping".to_owned(),
        "starting" => "Sharing starting".to_owned(),
        _ if input.busy => "Working".to_owned(),
        _ => "Not connected".to_owned(),
    };
    TrayPresentation {
        tone: IconTone::Idle,
        label,
    }
}

fn presentation_from_controller(controller: &AppController) -> TrayPresentation {
    let sharing = controller.sharing.status();
    presentation_for(StatusInput {
        sharing_active: sharing.sharing_active,
        busy: sharing.busy,
        sharing_phase: sharing.phase,
        peer_platform: sharing.peer_platform,
    })
}

/// Creates the retained native tray icon after the main window exists.
pub fn install(app: &tauri::App) -> tauri::Result<()> {
    if app.try_state::<TrayState>().is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "MonHop menu-bar controls were already installed",
        )
        .into());
    }

    let controller = app.state::<Arc<AppController>>();
    let presentation = presentation_from_controller(&controller);
    let status_item = MenuItem::with_id(
        app,
        STATUS_ID,
        presentation.menu_label(),
        false,
        None::<&str>,
    )?;
    let show_item = MenuItem::with_id(app, SHOW_ID, "Show MonHop", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, HIDE_ID, "Hide MonHop", true, None::<&str>)?;
    let pause_item = MenuItem::with_id(app, PAUSE_ID, "Pause sharing", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, QUIT_ID, "Quit", true, None::<&str>)?;
    let first_separator = PredefinedMenuItem::separator(app)?;
    let second_separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[
            &status_item,
            &first_separator,
            &show_item,
            &hide_item,
            &pause_item,
            &second_separator,
            &quit_item,
        ],
    )?;
    let icon = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .icon(icon_for(presentation.tone))
        .icon_as_template(is_template_icon(presentation.tone))
        .tooltip(presentation.tooltip())
        .show_menu_on_left_click(true)
        .on_menu_event(|handle, event| {
            let _ = handle_menu_action(handle, event.id().as_ref());
        })
        .build(app)?;
    let state = TrayState {
        _icon: icon,
        status_item,
        presentation: Mutex::new(presentation),
        last_refresh: Mutex::new(None),
    };
    if app.manage(state) {
        Ok(())
    } else {
        drop(app.remove_tray_by_id(TRAY_ID));
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "MonHop menu-bar controls could not be retained",
        )
        .into())
    }
}

pub fn is_installed(app: &AppHandle) -> bool {
    app.try_state::<TrayState>().is_some()
}

/// Re-projects local controller state without setup, network, or native-input work. Called on
/// every `MainEventsCleared`, so a call inside the refresh interval is a no-op.
pub fn refresh(app: &AppHandle) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    {
        let mut last_refresh = lock(&state.last_refresh);
        let now = Instant::now();
        if last_refresh.is_some_and(|previous| now.duration_since(previous) < REFRESH_INTERVAL) {
            return;
        }
        *last_refresh = Some(now);
    }
    let controller = app.state::<Arc<AppController>>();
    let next = presentation_from_controller(&controller);
    let mut current = lock(&state.presentation);
    if *current == next {
        return;
    }
    if current.tone != next.tone
        && state
            ._icon
            .set_icon_with_as_template(Some(icon_for(next.tone)), is_template_icon(next.tone))
            .is_err()
    {
        return;
    }
    if current.label != next.label
        && (state.status_item.set_text(next.menu_label()).is_err()
            || state._icon.set_tooltip(Some(next.tooltip())).is_err())
    {
        return;
    }
    *current = next;
}

pub fn show_main_window(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window(MAIN_WINDOW)
        .ok_or_else(|| "The MonHop window is unavailable.".to_owned())?;
    window.unminimize().map_err(|error| error.to_string())?;
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())
}

pub fn hide_main_window(app: &AppHandle) -> Result<(), String> {
    if !is_installed(app) {
        return Err("The menu-bar control is unavailable, so MonHop remains visible.".into());
    }
    app.get_webview_window(MAIN_WINDOW)
        .ok_or_else(|| "The MonHop window is unavailable.".to_owned())?
        .hide()
        .map_err(|error| error.to_string())
}

/// Dispatches only the fixed native-menu actions. No menu item can begin sharing.
pub fn handle_menu_action(app: &AppHandle, id: &str) -> bool {
    match id {
        SHOW_ID => {
            let _ = show_main_window(app);
        }
        HIDE_ID => {
            let _ = hide_main_window(app);
        }
        PAUSE_ID => {
            let controller = app.state::<Arc<AppController>>().inner().clone();
            controller.pairing.cancel();
            // Pausing is the same choice as on Home: no computer is active until the user picks one.
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(message) = controller.set_active(None, None) {
                    log::warn!("tray: pause did not apply: {message}");
                }
            });
        }
        QUIT_ID => crate::request_app_shutdown(app),
        _ => return false,
    }
    refresh(app);
    true
}

fn icon_for(tone: IconTone) -> Image<'static> {
    Image::new_owned(icon_rgba(tone), ICON_SIDE as u32, ICON_SIDE as u32)
}

/// macOS tints template glyphs to the menu bar and keeps their alpha, so idle dims natively.
fn is_template_icon(tone: IconTone) -> bool {
    tone != IconTone::Error
}

#[cfg(test)]
fn glyph_rgba(tone: IconTone) -> Vec<u8> {
    rasterize_icon(tone, false)
}

fn icon_rgba(tone: IconTone) -> Vec<u8> {
    rasterize_icon(tone, needs_windows_backplate(tone))
}

fn needs_windows_backplate(tone: IconTone) -> bool {
    cfg!(windows) && tone != IconTone::Error
}

fn rasterize_icon(tone: IconTone, backplate: bool) -> Vec<u8> {
    let mut rgba = vec![0; ICON_SIDE * ICON_SIDE * 4];
    let (color, opacity) = match tone {
        IconTone::Active => (GLYPH_WHITE, u8::MAX),
        IconTone::Idle => (GLYPH_WHITE, IDLE_OPACITY),
        IconTone::Error => (GLYPH_ERROR, u8::MAX),
    };
    for y in 0..ICON_SIDE {
        for x in 0..ICON_SIDE {
            let offset = (y * ICON_SIDE + x) * 4;
            let background = if backplate && is_backplate_pixel(x, y) {
                WINDOWS_BACKPLATE
            } else {
                [0; 4]
            };
            let alpha = scale_alpha(mask_alpha(x, y), opacity);
            rgba[offset..offset + 4].copy_from_slice(&source_over(
                [color[0], color[1], color[2], alpha],
                background,
            ));
        }
    }
    rgba
}

fn mask_alpha(x: usize, y: usize) -> u8 {
    TRAY_MASK[y * ICON_SIDE + x]
}

fn scale_alpha(alpha: u8, opacity: u8) -> u8 {
    ((u32::from(alpha) * u32::from(opacity) + 127) / 255) as u8
}

fn source_over(foreground: [u8; 4], background: [u8; 4]) -> [u8; 4] {
    let foreground_alpha = u32::from(foreground[3]);
    let background_alpha = u32::from(background[3]);
    let output_alpha = foreground_alpha + (background_alpha * (255 - foreground_alpha) + 127) / 255;
    if output_alpha == 0 {
        return [0; 4];
    }
    let rgb = [0, 1, 2].map(|channel| {
        let numerator = u32::from(foreground[channel]) * foreground_alpha * 255
            + u32::from(background[channel]) * background_alpha * (255 - foreground_alpha);
        ((numerator + output_alpha * 127) / (output_alpha * 255)) as u8
    });
    [rgb[0], rgb[1], rgb[2], output_alpha as u8]
}

fn is_backplate_pixel(x: usize, y: usize) -> bool {
    const LEFT: usize = 2;
    const TOP: usize = 2;
    const RIGHT: usize = ICON_SIDE - 3;
    const BOTTOM: usize = ICON_SIDE - 3;
    const CORNER_CENTER: usize = 6;
    const CORNER_RADIUS: usize = 4;

    if !(LEFT..=RIGHT).contains(&x) || !(TOP..=BOTTOM).contains(&y) {
        return false;
    }
    let corner_x = if x < CORNER_CENTER {
        CORNER_CENTER - x
    } else {
        x.saturating_sub(RIGHT - CORNER_RADIUS)
    };
    let corner_y = if y < CORNER_CENTER {
        CORNER_CENTER - y
    } else {
        y.saturating_sub(BOTTOM - CORNER_RADIUS)
    };
    corner_x * corner_x + corner_y * corner_y <= CORNER_RADIUS * CORNER_RADIUS
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(sharing_active: bool, busy: bool, sharing_phase: &'static str) -> TrayPresentation {
        presentation_for(StatusInput {
            sharing_active,
            busy,
            sharing_phase,
            peer_platform: None,
        })
    }

    #[test]
    fn only_sharing_is_bright_and_only_errors_are_colored() {
        assert_eq!(status(false, false, "off").label, "Not connected");
        assert_eq!(status(false, false, "off").tone, IconTone::Idle);
        let connecting = status(false, true, "connecting");
        assert_eq!(connecting.label, "Connecting");
        assert_eq!(connecting.tone, IconTone::Idle);
        assert_eq!(status(false, true, "reconnecting").tone, IconTone::Idle);
        assert_eq!(status(false, true, "stopping").tone, IconTone::Idle);
        let connected = presentation_for(StatusInput {
            sharing_active: false,
            busy: false,
            sharing_phase: "connected",
            peer_platform: Some("macos"),
        });
        assert_eq!(connected.label, "Connected to Mac");
        assert_eq!(connected.tone, IconTone::Idle);
        let windows_peer = presentation_for(StatusInput {
            sharing_active: false,
            busy: false,
            sharing_phase: "connected",
            peer_platform: Some("windows"),
        });
        assert_eq!(windows_peer.label, "Connected to Windows PC");
        let sharing = status(true, true, "sharing");
        assert_eq!(sharing.label, "Sharing active");
        assert_eq!(sharing.tone, IconTone::Active);
        assert_eq!(status(false, false, "error").tone, IconTone::Error);
    }

    #[test]
    fn bright_requires_native_sharing_and_an_error_outranks_it() {
        assert_ne!(status(false, true, "sharing").tone, IconTone::Active);
        let sharing = status(true, true, "sharing");
        assert_eq!(sharing.tone, IconTone::Active);
        assert_eq!(sharing.tooltip(), "MonHop — Sharing active");
        let failed = status(true, false, "error");
        assert_eq!(failed.tone, IconTone::Error);
        assert_eq!(failed.label, "Needs attention");
    }

    #[test]
    fn busy_and_error_states_have_honest_textual_status() {
        assert_eq!(
            status(false, true, "starting").menu_label(),
            "Status: Sharing starting"
        );
        assert_eq!(
            status(false, false, "error").menu_label(),
            "Status: Needs attention"
        );
    }

    #[test]
    fn mark_uses_the_embedded_antialiased_mask_for_every_status_tone() {
        let active = glyph_rgba(IconTone::Active);
        let idle = glyph_rgba(IconTone::Idle);
        let error = glyph_rgba(IconTone::Error);
        assert_eq!(active.len(), ICON_SIDE * ICON_SIDE * 4);
        let mut empty = 0;
        let mut antialiased = 0;
        let mut opaque = 0;

        for (index, alpha) in TRAY_MASK.iter().copied().enumerate() {
            let offset = index * 4;
            assert_eq!(active[offset + 3], alpha);
            assert_eq!(error[offset + 3], alpha);
            assert_eq!(idle[offset + 3], scale_alpha(alpha, IDLE_OPACITY));
            match alpha {
                0 => {
                    empty += 1;
                    assert_eq!(&active[offset..offset + 4], &[0, 0, 0, 0]);
                    assert_eq!(&idle[offset..offset + 4], &[0, 0, 0, 0]);
                    assert_eq!(&error[offset..offset + 4], &[0, 0, 0, 0]);
                }
                255 => {
                    opaque += 1;
                    assert_eq!(idle[offset + 3], IDLE_OPACITY);
                }
                _ => antialiased += 1,
            }
            if alpha != 0 {
                assert_eq!(&active[offset..offset + 3], &GLYPH_WHITE);
                assert_eq!(&idle[offset..offset + 3], &GLYPH_WHITE);
                assert_eq!(&error[offset..offset + 3], &GLYPH_ERROR);
            }
        }
        assert!(empty > 0);
        assert!(antialiased > 0);
        assert!(opaque > 0);
    }

    #[test]
    fn windows_backplate_keeps_the_glyph_visible_on_pale_and_dark_taskbars() {
        let macos_active = rasterize_icon(IconTone::Active, false);
        let windows_active = rasterize_icon(IconTone::Active, true);
        let windows_idle = rasterize_icon(IconTone::Idle, true);
        let (empty_x, empty_y) = mask_pixel(|x, y, alpha| alpha == 0 && is_backplate_pixel(x, y));
        let (opaque_x, opaque_y) =
            mask_pixel(|x, y, alpha| alpha == 255 && is_backplate_pixel(x, y));
        let (edge_x, edge_y) =
            mask_pixel(|x, y, alpha| (1..255).contains(&alpha) && is_backplate_pixel(x, y));
        let (exterior_x, exterior_y) =
            mask_pixel(|x, y, alpha| alpha == 0 && !is_backplate_pixel(x, y));
        let backplate = pixel(&windows_active, empty_x, empty_y);
        let glyph = pixel(&windows_active, opaque_x, opaque_y);
        let idle_glyph = pixel(&windows_idle, opaque_x, opaque_y);
        let edge = pixel(&windows_active, edge_x, edge_y);
        let edge_alpha = mask_alpha(edge_x, edge_y);
        let [red, green, blue] = GLYPH_WHITE;

        assert_eq!(pixel(&macos_active, empty_x, empty_y), [0, 0, 0, 0]);
        assert_eq!(pixel(&windows_active, exterior_x, exterior_y), [0, 0, 0, 0]);
        assert_eq!(backplate, WINDOWS_BACKPLATE);
        assert_eq!(glyph, [red, green, blue, 255]);
        assert_eq!(
            edge,
            source_over([red, green, blue, edge_alpha], WINDOWS_BACKPLATE)
        );
        assert!(edge[3] > WINDOWS_BACKPLATE[3]);
        assert!(edge[3] < 255);
        assert!(luminance_after_composite(backplate, [240, 240, 240]) < 0.25);
        assert!(luminance_after_composite(glyph, [24, 24, 24]) > 0.85);
        assert!(
            luminance_after_composite(glyph, [240, 240, 240])
                - luminance_after_composite(backplate, [240, 240, 240])
                > 0.5
        );
        let idle = luminance_after_composite(idle_glyph, [240, 240, 240]);
        assert!(idle > luminance_after_composite(backplate, [240, 240, 240]) + 0.1);
        assert!(idle < luminance_after_composite(glyph, [240, 240, 240]) - 0.3);
    }

    #[test]
    fn only_white_glyphs_on_windows_get_the_backplate() {
        for tone in [IconTone::Active, IconTone::Idle] {
            assert_eq!(icon_rgba(tone), rasterize_icon(tone, cfg!(windows)));
        }
        assert!(!needs_windows_backplate(IconTone::Error));
        assert_eq!(icon_rgba(IconTone::Error), glyph_rgba(IconTone::Error));
    }

    #[test]
    fn only_the_error_icon_opts_out_of_the_native_macos_template_mode() {
        assert!(is_template_icon(IconTone::Active));
        assert!(is_template_icon(IconTone::Idle));
        assert!(!is_template_icon(IconTone::Error));
    }

    fn pixel(rgba: &[u8], x: usize, y: usize) -> [u8; 4] {
        let offset = (y * ICON_SIDE + x) * 4;
        rgba[offset..offset + 4].try_into().unwrap()
    }

    fn mask_pixel(predicate: impl Fn(usize, usize, u8) -> bool) -> (usize, usize) {
        (0..ICON_SIDE)
            .flat_map(|y| (0..ICON_SIDE).map(move |x| (x, y)))
            .find(|&(x, y)| predicate(x, y, mask_alpha(x, y)))
            .expect("the embedded tray mask must contain the requested pixel")
    }

    fn luminance_after_composite(pixel: [u8; 4], background: [u8; 3]) -> f64 {
        let alpha = f64::from(pixel[3]) / 255.0;
        let rgb = [0, 1, 2].map(|index| {
            (f64::from(pixel[index]) * alpha + f64::from(background[index]) * (1.0 - alpha)) / 255.0
        });
        0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
    }
}
