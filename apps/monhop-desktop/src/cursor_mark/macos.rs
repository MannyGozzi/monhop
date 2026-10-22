//! The Mac renderer: the copy and cursor hiding, and the arrival ring. Nothing yet; until it lands
//! the switch only records the user's choice.

use monhop_core::pointer_mark::MarkStyle;
use tauri::AppHandle;

pub(super) fn start(app: &AppHandle, style: MarkStyle) {
    let _ = (app, style);
}

pub(super) fn reveal_for_exit() {}
