fn main() {
    // The log banner names the exact source; "unknown" when git is unavailable at build time.
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|hash| hash.trim().to_owned())
        .filter(|hash| !hash.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=MONHOP_BUILD_COMMIT={commit}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "setup_snapshot",
            "request_permissions",
            "open_permission_settings",
            "request_wifi_permission",
            "pairing_open",
            "pairing_create_identity",
            "pairing_inspect",
            "pairing_confirm",
            "pairing_request_network_access",
            "pairing_status",
            "pairing_cancel",
            "pairing_forget",
            "pairing_copy_code",
            "sharing_edit_begin",
            "sharing_edit_end",
            "sharing_set_active",
            "sharing_apply_setup",
            "sharing_touch",
            "sharing_select_source",
            "sharing_status",
            "sharing_save_setup",
            "sharing_arrangements",
            "sharing_arrangement_save",
            "sharing_arrangement_delete",
            "sharing_dismiss_display_notice",
            "sharing_copy_last_drop",
            "dimming_status",
            "dimming_set_enabled",
            "dimming_set_level",
            "dimming_preview_level",
            "dimming_toggle",
            "appearance_status",
            "appearance_set_theme",
            "sharing_trial_open",
            "trial_start",
            "trial_status",
            "trial_stop",
            "trial_close",
            "window_hide",
            "computers_load",
            "computers_rename",
            "reveal_logs",
            "updates_status",
            "updates_set_automatic",
            "updates_check",
            "updates_install",
            "app_open_link",
            "autostart_status",
            "autostart_set",
            "autostart_open_settings",
        ]),
    ))
    .expect("could not build the local setup app");
}
