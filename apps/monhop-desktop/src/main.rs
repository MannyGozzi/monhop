#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod appearance;
mod arrangement_library;
mod autostart;
mod computers;
mod dimming;
#[cfg(target_os = "macos")]
mod display_labels;
mod launch;
mod lifecycle;
mod links;
#[cfg(target_os = "macos")]
mod location;
#[cfg(windows)]
mod log_reveal;
mod logging;
mod pairing;
mod public_code_copy;
mod settings;
mod sharing;
mod sharing_preferences;
mod snapshot;
#[cfg(windows)]
mod timer_resolution;
mod tray;
mod trial;
#[cfg(target_os = "macos")]
mod trial_dispatch;
mod ui_smoke;
mod updates;

use lifecycle::AppController;
use pairing::{PairingController, PairingView};
use settings::PermissionPane;
use sharing::{LayoutRequest, SharingView};
use snapshot::SetupSnapshot;
use std::sync::Arc;
use tauri::Manager;
use tauri::{WebviewUrl, WebviewWindowBuilder, webview::NewWindowResponse};

/// Held by every test that claims native input ownership or asserts on state that reads it.
#[cfg(test)]
static NATIVE_LIFECYCLE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(any(windows, test))]
const WEBVIEW2_DEFAULT_BROWSER_ARGS: &str =
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

#[cfg(windows)]
struct MainThreadSta {
    _not_send_or_sync: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(windows)]
impl MainThreadSta {
    fn initialize() -> windows::core::Result<Self> {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

        // SAFETY: Main retains this thread-affine token until Tauri shuts down.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self {
            _not_send_or_sync: std::marker::PhantomData,
        })
    }
}

#[cfg(windows)]
impl Drop for MainThreadSta {
    fn drop(&mut self) {
        use windows::Win32::System::Com::CoUninitialize;

        // SAFETY: This token represents one successful STA initialization on this thread.
        unsafe { CoUninitialize() };
    }
}

/// Runs `action` on the blocking pool; `failure_message` is used only if that task never joins.
async fn spawn_blocking_command<T: Send + 'static>(
    action: impl FnOnce() -> T + Send + 'static,
    failure_message: &'static str,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(action)
        .await
        .map_err(|_| failure_message.to_owned())
}

async fn pairing_action(
    app: tauri::AppHandle,
    action: impl FnOnce(&PairingController) -> Result<PairingView, String> + Send + 'static,
) -> Result<PairingView, String> {
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.pairing_action(action),
        "The pairing action did not finish. Reload pairing before continuing.",
    )
    .await?
}

#[tauri::command]
async fn pairing_open(app: tauri::AppHandle, interface_id: String) -> Result<PairingView, String> {
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.pairing_read_action(move |pairing| pairing.open(interface_id, false)),
        "The pairing action did not finish. Reload pairing before continuing.",
    )
    .await?
}
#[tauri::command]
async fn pairing_create_identity(
    app: tauri::AppHandle,
    interface_id: String,
) -> Result<PairingView, String> {
    pairing_action(app, move |controller| controller.open(interface_id, true)).await
}
#[tauri::command]
async fn pairing_inspect(app: tauri::AppHandle, code: String) -> Result<PairingView, String> {
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.pairing_inspect(code),
        "The pairing action did not finish. Reload pairing before continuing.",
    )
    .await?
}
#[tauri::command]
async fn pairing_confirm(app: tauri::AppHandle, candidate_id: u64) -> Result<PairingView, String> {
    pairing_action(app, move |controller| controller.confirm(candidate_id)).await
}
#[tauri::command]
async fn pairing_request_network_access(
    app: tauri::AppHandle,
    candidate_id: u64,
) -> Result<PairingView, String> {
    pairing_action(app, move |controller| {
        controller.request_network_access(candidate_id)
    })
    .await
}
#[tauri::command]
fn pairing_status(controller: tauri::State<'_, Arc<PairingController>>) -> PairingView {
    controller.status()
}

#[tauri::command]
async fn sharing_edit_begin(
    app: tauri::AppHandle,
    interface_id: String,
    fingerprint: String,
) -> Result<SharingView, String> {
    #[cfg(target_os = "macos")]
    on_main_thread(&app, display_labels::refresh).await?;
    #[cfg(windows)]
    monhop_platform_windows::refresh_display_names();
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.edit_begin(interface_id, &fingerprint),
        "Arranging did not start. Try again.",
    )
    .await?
}

#[tauri::command]
async fn sharing_edit_end(app: tauri::AppHandle) -> Result<SharingView, String> {
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.edit_end(),
        "Arranging did not end cleanly. Try again.",
    )
    .await?
}

#[tauri::command]
async fn sharing_set_active(
    app: tauri::AppHandle,
    fingerprint: Option<String>,
) -> Result<SharingView, String> {
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.set_active(fingerprint.as_deref()),
        "The choice of computer did not apply. Try again.",
    )
    .await?
}

#[tauri::command]
async fn sharing_apply_setup(
    app: tauri::AppHandle,
    revision: String,
    source: String,
    layout: LayoutRequest,
) -> Result<SharingView, String> {
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.apply_setup(revision, source, layout),
        "Applying the layout did not finish. Nothing changed.",
    )
    .await?
}

#[tauri::command]
fn sharing_touch(controller: tauri::State<'_, Arc<AppController>>) {
    controller.sharing.touch();
}

#[tauri::command]
fn sharing_select_source(
    controller: tauri::State<'_, Arc<AppController>>,
    revision: String,
    source: String,
) -> Result<SharingView, String> {
    controller.select_source(&revision, &source)
}

#[tauri::command]
async fn sharing_trial_open(
    app: tauri::AppHandle,
    revision: String,
    layout: LayoutRequest,
    peer_name: String,
) -> Result<(), String> {
    let main_app = app.clone();
    run_on_main_thread_for_result(
        &app,
        move || trial::open(&main_app, revision, layout, peer_name),
        "The test window could not open.",
        "The test window did not finish opening.",
    )
    .await?
}
#[tauri::command]
async fn trial_start(window: tauri::WebviewWindow) -> Result<trial::TrialView, String> {
    trial::start(window).await
}
#[tauri::command]
fn trial_status(window: tauri::WebviewWindow) -> Result<trial::TrialView, String> {
    trial::require_window(&window)?;
    let controller = window.state::<Arc<AppController>>();
    Ok(controller.trial.view(&controller.sharing))
}
#[tauri::command]
fn trial_stop(window: tauri::WebviewWindow) -> Result<trial::TrialView, String> {
    trial::stop(&window)
}
#[tauri::command]
fn trial_close(window: tauri::WebviewWindow) -> Result<(), String> {
    trial::require_window(&window)?;
    trial::close(window.app_handle());
    Ok(())
}
#[tauri::command]
fn trial_last_result(app: tauri::AppHandle) -> Option<trial::LastTrialView> {
    app.state::<Arc<AppController>>().trial.last_result()
}
#[tauri::command]
async fn trial_copy_last_result(app: tauri::AppHandle) -> Result<(), String> {
    let report = app
        .state::<Arc<AppController>>()
        .trial
        .last_result()
        .ok_or("No test result to copy yet.")?
        .report();
    run_on_main_thread_for_result(
        &app,
        move || public_code_copy::write(report),
        "Could not copy the result. Try again.",
        "Copy did not finish. Try again.",
    )
    .await?
}

/// The smoke check never writes here; a failure to open the log leaves the app fully usable.
fn start_logging(app: &tauri::AppHandle) {
    let Ok(dir) = app.path().app_local_data_dir() else {
        return;
    };
    match logging::init(&dir.join("logs")) {
        Ok(path) => log::info!(
            "MonHop {} ({}) on {}, protocol {}, log at {}",
            env!("CARGO_PKG_VERSION"),
            env!("MONHOP_BUILD_COMMIT"),
            window_platform(),
            monhop_protocol::PROTOCOL_VERSION,
            path.display()
        ),
        Err(error) => eprintln!("MonHop could not open its log: {error}"),
    }
}

/// Shows the active log in Notepad on Windows or selects it in Finder on macOS.
#[tauri::command]
async fn reveal_logs() -> Result<(), String> {
    let path = logging::path().ok_or("Logging is not active in this run.")?;
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::NSWorkspace;
        use objc2_foundation::NSString;
        let file = NSString::from_str(&path.to_string_lossy());
        let folder = NSString::from_str(
            &path
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        let shown = NSWorkspace::sharedWorkspace()
            .selectFile_inFileViewerRootedAtPath(Some(&file), &folder);
        if shown {
            Ok(())
        } else {
            Err(format!("Open {} in Finder.", path.display()))
        }
    }
    #[cfg(windows)]
    {
        log_reveal::reveal(path).await
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Err(format!("Open {} with your file browser.", path.display()))
    }
}

fn sharing_setup_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .app_local_data_dir()
        .map(|path| path.join("sharing-setup.json"))
        .map_err(|_| "The setup folder could not be located.".to_owned())
}

fn arrangements_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    sharing_setup_path(app).map(|path| path.with_file_name("arrangements.json"))
}

#[tauri::command]
async fn sharing_arrangements(
    app: tauri::AppHandle,
) -> Result<Vec<arrangement_library::ArrangementView>, String> {
    let path = arrangements_path(&app)?;
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.arrangements(&path),
        "The saved arrangements could not be read.",
    )
    .await?
}

#[tauri::command]
async fn sharing_arrangement_save(
    app: tauri::AppHandle,
    revision: String,
    name: String,
    layout: LayoutRequest,
) -> Result<Vec<arrangement_library::ArrangementView>, String> {
    let path = arrangements_path(&app)?;
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.save_arrangement(&path, &revision, &name, layout),
        "The arrangement could not be saved.",
    )
    .await?
}

#[tauri::command]
async fn sharing_arrangement_delete(
    app: tauri::AppHandle,
    name: String,
) -> Result<Vec<arrangement_library::ArrangementView>, String> {
    let path = arrangements_path(&app)?;
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.delete_arrangement(&path, &name),
        "The arrangement could not be deleted.",
    )
    .await?
}

#[tauri::command]
async fn sharing_copy_last_drop(app: tauri::AppHandle) -> Result<(), String> {
    let text = app
        .state::<Arc<AppController>>()
        .sharing
        .last_drop_text()
        .ok_or("No drop to copy yet.")?;
    run_on_main_thread_for_result(
        &app,
        move || public_code_copy::write(text),
        "Could not copy the drop. Try again.",
        "Copy did not finish. Try again.",
    )
    .await?
}

#[tauri::command]
async fn sharing_save_setup(
    app: tauri::AppHandle,
    revision: String,
    layout: LayoutRequest,
) -> Result<sharing_preferences::SavedSetupView, String> {
    let path = sharing_setup_path(&app)?;
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || controller.save_setup(&path, &revision, layout),
        "The setup could not be saved.",
    )
    .await?
}

async fn computers_action(
    app: tauri::AppHandle,
    action: impl FnOnce(&AppController) -> Result<computers::ComputersView, String> + Send + 'static,
) -> Result<computers::ComputersView, String> {
    let controller = app.state::<Arc<AppController>>().inner().clone();
    spawn_blocking_command(
        move || action(&controller),
        "Your saved computers could not be loaded.",
    )
    .await?
}

#[tauri::command]
async fn computers_load(app: tauri::AppHandle) -> Result<computers::ComputersView, String> {
    computers_action(app, AppController::computers_load).await
}

#[tauri::command]
async fn computers_rename(
    app: tauri::AppHandle,
    fingerprint: String,
    name: String,
) -> Result<computers::ComputersView, String> {
    computers_action(app, move |controller| {
        controller.computers_rename(&fingerprint, &name)
    })
    .await
}

#[tauri::command]
async fn pairing_forget(
    app: tauri::AppHandle,
    fingerprint: String,
) -> Result<computers::ComputersView, String> {
    computers_action(app, move |controller| {
        controller.forget_computer(&fingerprint)
    })
    .await
}

#[tauri::command]
fn window_hide(app: tauri::AppHandle) -> Result<(), String> {
    tray::hide_main_window(&app)
}

#[tauri::command]
fn sharing_status(controller: tauri::State<'_, Arc<AppController>>) -> SharingView {
    controller.sharing.status()
}

#[tauri::command]
fn sharing_dismiss_display_notice(controller: tauri::State<'_, Arc<AppController>>) -> SharingView {
    controller.sharing.dismiss_display_notice()
}

#[tauri::command]
fn pairing_cancel(controller: tauri::State<'_, Arc<PairingController>>) -> PairingView {
    controller.cancel()
}

#[tauri::command]
async fn pairing_copy_code(app: tauri::AppHandle) -> Result<(), String> {
    let controller = app.state::<Arc<PairingController>>().inner().clone();
    run_on_main_thread_for_result(
        &app,
        move || {
            // Clipboard access is lazy and serialized on the UI thread, including on Windows.
            public_code_copy::copy(&controller, public_code_copy::write)
        },
        "Could not copy the code. Try again.",
        "Copy did not finish. Try again.",
    )
    .await?
}

#[tauri::command]
async fn setup_snapshot(app: tauri::AppHandle) -> Result<SetupSnapshot, String> {
    #[cfg(target_os = "macos")]
    let authorization = on_main_thread(&app, |marker| {
        display_labels::refresh(marker)?;
        location::read(marker)
    })
    .await?;
    #[cfg(not(target_os = "macos"))]
    let _ = app;
    #[cfg(windows)]
    monhop_platform_windows::refresh_display_names();
    let snapshot = tauri::async_runtime::spawn_blocking(snapshot::read)
        .await
        .map_err(|_| "The read-only setup check could not finish. Try Check again.".to_owned())?;
    #[cfg(target_os = "macos")]
    let snapshot = snapshot.with_wifi_authorization(authorization.as_str());
    Ok(snapshot)
}

/// Runs `action` on the main thread and returns what it sends; the schedule message covers a
/// failure to hop threads, the finish message a main thread that never replies.
async fn run_on_main_thread_for_result<T: Send + 'static>(
    app: &tauri::AppHandle,
    action: impl FnOnce() -> T + Send + 'static,
    schedule_message: &'static str,
    finish_message: &'static str,
) -> Result<T, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = sender.send(action());
    })
    .map_err(|_| schedule_message.to_owned())?;
    receiver.await.map_err(|_| finish_message.to_owned())
}

#[cfg(target_os = "macos")]
async fn on_main_thread<T: Send + 'static>(
    app: &tauri::AppHandle,
    action: impl FnOnce(objc2::MainThreadMarker) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (sender, mut receiver) = tauri::async_runtime::channel(1);
    app.run_on_main_thread(move || {
        let result = objc2::MainThreadMarker::new()
            .ok_or_else(|| "The macOS setup action needs the app's main thread.".to_owned())
            .and_then(action);
        let _ = sender.try_send(result);
    })
    .map_err(|_| "The macOS setup action could not be scheduled. Try again.".to_owned())?;
    receiver
        .recv()
        .await
        .ok_or_else(|| "The macOS setup action ended before reporting a result.".to_owned())?
}

#[tauri::command]
async fn request_wifi_permission(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        on_main_thread(&app, location::request).await.map(|_| ())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("Wi-Fi privacy authorization is a macOS-only setup action.".into())
    }
}

#[tauri::command]
async fn request_permissions(app: tauri::AppHandle) -> Result<SetupSnapshot, String> {
    #[cfg(target_os = "macos")]
    {
        on_main_thread(&app, |_| {
            monhop_platform_macos::request_permissions().map_err(|error| error.to_string())
        })
        .await?;
        setup_snapshot(app).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("No Windows elevation is required or requested by MonHop.".into())
    }
}

#[tauri::command]
fn open_permission_settings(pane: PermissionPane) -> Result<(), String> {
    settings::open(pane)
}

fn bundled_origin(url: &tauri::Url) -> bool {
    #[cfg(target_os = "macos")]
    {
        url.scheme() == "tauri" && url.host_str() == Some("localhost")
    }
    #[cfg(windows)]
    {
        url.scheme() == "http" && url.host_str() == Some("tauri.localhost")
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = url;
        false
    }
}

fn local_navigation(url: &tauri::Url) -> bool {
    bundled_origin(url)
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        // Tauri simplifies index.html to tauri://localhost, whose non-HTTP URL path is empty.
        && matches!(url.path(), "" | "/" | "/index.html")
}

#[cfg(windows)]
fn constrained_webview2_environment(
    data_directory: &std::path::Path,
) -> Result<
    webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment,
    Box<dyn std::error::Error>,
> {
    use std::sync::mpsc;

    use webview2_com::{
        CoreWebView2EnvironmentOptions, CreateCoreWebView2EnvironmentCompletedHandler,
        Microsoft::Web::WebView2::Win32::{
            CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2EnvironmentOptions,
        },
    };
    use windows::{
        Win32::Foundation::{E_POINTER, E_UNEXPECTED},
        core::{Error, HSTRING, PCWSTR},
    };

    let data_directory = HSTRING::from(data_directory);
    let options = CoreWebView2EnvironmentOptions::default();
    // SAFETY: This setup has exclusive access to the local options before WebView2 receives them.
    unsafe {
        options.set_additional_browser_arguments(WEBVIEW2_DEFAULT_BROWSER_ARGS.into());
        options.set_allow_single_sign_on_using_os_primary_account(false);
        options.set_enable_tracking_prevention(true);
        options.set_are_browser_extensions_enabled(false);
        // This suppresses automatic crash upload; it does not network-isolate WebView2.
        options.set_is_custom_crash_reporting_enabled(true);
    }

    let (sender, receiver) = mpsc::channel();
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |error_code, environment| {
            let result = (|| {
                error_code?;
                environment.ok_or_else(|| Error::from(E_POINTER))
            })();
            sender.send(result).map_err(|_| Error::from(E_UNEXPECTED))
        },
    ));

    // SAFETY: The options and callback use documented WebView2 COM types for this synchronous call.
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            &data_directory,
            &ICoreWebView2EnvironmentOptions::from(options),
            &handler,
        )?;
    }
    Ok(webview2_com::wait_with_pump(receiver)??)
}

fn main() {
    let launch = launch::Launch::parse(std::env::args_os().skip(1));
    let check_ui = launch == launch::Launch::CheckUi;
    let login = launch == launch::Launch::Login;
    #[cfg(windows)]
    let com_apartment = match MainThreadSta::initialize() {
        Ok(apartment) => apartment,
        Err(error) => {
            eprintln!("MonHop could not initialize its Windows UI thread: {error}");
            std::process::exit(1);
        }
    };

    let controller = Arc::new(AppController::default());
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .device_event_filter(tauri::DeviceEventFilter::Always)
        .manage(controller.pairing.clone())
        .manage(controller)
        .invoke_handler(tauri::generate_handler![
            setup_snapshot,
            request_permissions,
            open_permission_settings,
            request_wifi_permission,
            pairing_open,
            pairing_create_identity,
            pairing_inspect,
            pairing_confirm,
            pairing_request_network_access,
            pairing_status,
            pairing_cancel,
            pairing_forget,
            pairing_copy_code,
            sharing_edit_begin,
            sharing_edit_end,
            sharing_set_active,
            sharing_apply_setup,
            sharing_touch,
            sharing_select_source,
            sharing_status,
            sharing_save_setup,
            sharing_arrangements,
            sharing_arrangement_save,
            sharing_arrangement_delete,
            sharing_dismiss_display_notice,
            sharing_copy_last_drop,
            dimming::dimming_status,
            dimming::dimming_set_enabled,
            dimming::dimming_set_level,
            dimming::dimming_preview_level,
            dimming::dimming_toggle,
            appearance::appearance_status,
            appearance::appearance_set_theme,
            sharing_trial_open,
            trial_start,
            trial_status,
            trial_stop,
            trial_close,
            trial_last_result,
            trial_copy_last_result,
            window_hide,
            computers_load,
            computers_rename,
            reveal_logs,
            updates::updates_status,
            updates::updates_set_automatic,
            updates::updates_check,
            updates::updates_install,
            links::app_open_link,
            autostart::autostart_status,
            autostart::autostart_set,
            autostart::autostart_open_settings
        ])
        .setup(move |app| {
            if check_ui {
                ui_smoke::arm_timeout(app.handle().clone());
            } else {
                start_logging(app.handle());
            }
            if login {
                log::info!("launched at login");
            }
            #[cfg(windows)]
            app.manage(timer_resolution::TimerResolution::raise());
            app.state::<Arc<AppController>>()
                .use_setup_path(sharing_setup_path(app.handle())?);
            autostart::init(app.handle());
            app.manage(dimming::Dimming::start(app.handle(), !check_ui));
            app.manage(updates::Updates::start(app.handle(), !check_ui));
            let appearance = appearance::Appearance::start(app.handle());
            app.manage(appearance.clone());
            if !check_ui {
                // The active computer stays connected by itself, even with the window hidden.
                let supervisor = app.state::<Arc<AppController>>().inner().clone();
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                    loop {
                        tick.tick().await;
                        let controller = supervisor.clone();
                        let _ = tauri::async_runtime::spawn_blocking(move || {
                            controller.supervise_sharing();
                        })
                        .await;
                        // A newly raised display banner brings MonHop forward once.
                        if supervisor.sharing.take_window_forward() {
                            let window = handle.clone();
                            let _ = handle.run_on_main_thread(move || {
                                let _ = tray::show_main_window(&window);
                            });
                        }
                    }
                });
            }
            #[cfg(windows)]
            let webview_data_directory = app.path().app_local_data_dir()?;
            #[cfg(windows)]
            // WebView2 writes only under Tauri's per-app local-data directory.
            std::fs::create_dir_all(&webview_data_directory)?;

            let window =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .title("MonHop")
                    .visible(!login)
                    .initialization_script(format!(
                        "window.__MONHOP_PLATFORM__ = {:?}; window.__MONHOP_UI_CHECK__ = {};",
                        window_platform(),
                        check_ui
                    ))
                    .inner_size(980.0, 720.0)
                    .min_inner_size(760.0, 560.0)
                    .incognito(true)
                    .devtools(false)
                    .disable_drag_drop_handler()
                    .browser_extensions_enabled(false)
                    .general_autofill_enabled(false)
                    .on_navigation(local_navigation)
                    .on_page_load(move |window, payload| {
                        if check_ui && payload.event() == tauri::webview::PageLoadEvent::Finished {
                            ui_smoke::check(window);
                        }
                    })
                    .on_new_window(|_, _| NewWindowResponse::Deny)
                    .on_download(|_, _| false)
                    .theme(appearance.theme().native())
                    .transparent(true);
            #[cfg(target_os = "macos")]
            let window = window
                .title_bar_style(tauri::TitleBarStyle::Overlay)
                .hidden_title(true)
                .traffic_light_position(tauri::LogicalPosition::new(14.0, 27.0))
                .effects(tauri::utils::config::WindowEffectsConfig {
                    effects: vec![tauri::window::Effect::HudWindow],
                    state: Some(tauri::window::EffectState::Active),
                    radius: None,
                    color: None,
                });
            #[cfg(windows)]
            let window = window
                .decorations(false)
                .shadow(true)
                .with_environment(constrained_webview2_environment(&webview_data_directory)?)
                .effects(tauri::utils::config::WindowEffectsConfig {
                    effects: vec![tauri::window::Effect::Acrylic],
                    state: None,
                    radius: None,
                    color: None,
                });
            window.build()?;
            if let Err(error) = tray::install(app) {
                if check_ui {
                    return Err(error.into());
                }
                eprintln!("MonHop tray controls are unavailable. The window remains open.");
                if login {
                    // No tray to show it from later, so a hidden login launch shows it now.
                    let _ = tray::show_main_window(app.handle());
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!());
    let exit_code = match result {
        Ok(app) if check_ui => {
            // The runtime discards requested exit codes; preserve the diagnostic's own verdict.
            app.run_return(on_app_event);
            ui_smoke::exit_code()
        }
        Ok(app) => {
            app.run(on_app_event);
            0
        }
        Err(_) => {
            eprintln!("MonHop could not open its setup window. No sharing was enabled.");
            1
        }
    };
    #[cfg(windows)]
    drop(com_apartment);
    std::process::exit(exit_code);
}

fn on_app_event(handle: &tauri::AppHandle, event: tauri::RunEvent) {
    match event {
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::CloseRequested { api, .. },
            ..
        } if label == trial::WINDOW_LABEL => {
            api.prevent_close();
            trial::close(handle);
        }
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::Focused(false) | tauri::WindowEvent::Destroyed,
            ..
        } if label == trial::WINDOW_LABEL => {
            trial::focus_lost_or_destroyed(handle);
        }
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::Moved(position),
            ..
        } if label == trial::WINDOW_LABEL => {
            trial::moved(handle, position);
        }
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::Resized(size),
            ..
        } if label == trial::WINDOW_LABEL => {
            trial::resized(handle, size);
        }
        tauri::RunEvent::WindowEvent {
            event: tauri::WindowEvent::CloseRequested { api, .. },
            ..
        } => {
            api.prevent_close();
            if tray::is_installed(handle) {
                let _ = tray::hide_main_window(handle);
            } else {
                request_app_shutdown(handle);
            }
        }
        tauri::RunEvent::ExitRequested { api, .. } => {
            if handle.state::<Arc<AppController>>().shutdown_ready() {
                // The way out is where a verified build lands; it never waits on the network.
                updates::install_on_quit(handle);
            } else {
                api.prevent_exit();
                request_app_shutdown(handle);
            }
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => {
            let _ = tray::show_main_window(handle);
        }
        tauri::RunEvent::MainEventsCleared => tray::refresh(handle),
        tauri::RunEvent::Exit => {
            // Cmd+Q, an AppleScript quit and logout reach only this event on macOS, so the
            // bounded drain and the verified install happen here; after a tray Quit both are no-ops.
            handle.state::<Arc<AppController>>().drain_for_exit();
            updates::install_on_quit(handle);
        }
        _ => {}
    }
}

fn request_app_shutdown(handle: &tauri::AppHandle) {
    let controller = handle.state::<Arc<AppController>>().inner().clone();
    if !controller.request_shutdown() {
        return;
    }
    let handle = handle.clone();
    tauri::async_runtime::spawn(async move {
        // Keep the native event loop alive until prompt continuations and input releases finish.
        while !controller.shutdown_ready() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        handle.exit(0);
    });
}

fn window_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_accepts_only_the_current_platform_bundled_entrypoint() {
        #[cfg(target_os = "macos")]
        for url in [
            "tauri://localhost",
            "tauri://localhost/",
            "tauri://localhost/index.html#permissions",
        ] {
            assert!(local_navigation(&url.parse().unwrap()), "rejected {url}");
        }
        #[cfg(windows)]
        for url in [
            "http://tauri.localhost/",
            "http://tauri.localhost/index.html#permissions",
        ] {
            assert!(local_navigation(&url.parse().unwrap()), "rejected {url}");
        }
    }

    #[test]
    fn navigation_rejects_network_and_cross_platform_origins() {
        for url in [
            "https://example.com/",
            "https://tauri.localhost/",
            "https://tauri.localhost.example.com/",
            "http://127.0.0.1/",
            "file:///etc/passwd",
            "tauri://localhost/other.html",
            "tauri://localhost/?url=https://example.com",
            "http://tauri.localhost:8080/",
            "https://user@tauri.localhost/",
        ] {
            assert!(!local_navigation(&url.parse().unwrap()), "accepted {url}");
        }

        #[cfg(target_os = "macos")]
        for url in [
            "http://tauri.localhost/index.html",
            "tauri://localhost.example.com",
            "tauri://127.0.0.1",
            "tauri://localhost?",
            "tauri://localhost/%2F",
            "tauri://user@localhost/",
            "tauri://localhost:8080/",
            "tauri://localhost/?url=https://example.com",
        ] {
            assert!(!local_navigation(&url.parse().unwrap()), "accepted {url}");
        }
        #[cfg(windows)]
        for url in [
            "tauri://localhost/index.html",
            "http://user@tauri.localhost/",
            "http://tauri.localhost:8080/",
            "http://tauri.localhost/?url=https://example.com",
        ] {
            assert!(!local_navigation(&url.parse().unwrap()), "accepted {url}");
        }
    }

    #[test]
    fn config_keeps_the_webview_off_external_origins() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(
            config["app"]["security"]["csp"],
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src ipc: http://ipc.localhost; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'"
        );
        assert_eq!(config["app"]["windows"], serde_json::json!([]));
        assert_eq!(config["app"]["macOSPrivateApi"], true);
    }

    #[test]
    fn only_the_local_main_window_has_setup_pairing_and_metadata_commands() {
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/setup.json")).unwrap();
        assert_eq!(capability["windows"], serde_json::json!(["main"]));
        assert_eq!(capability["local"], true);
        assert!(capability.get("remote").is_none());
        assert_eq!(
            capability["permissions"],
            serde_json::json!([
                "allow-setup-snapshot",
                "allow-request-permissions",
                "allow-open-permission-settings",
                "allow-request-wifi-permission",
                "allow-pairing-open",
                "allow-pairing-create-identity",
                "allow-pairing-inspect",
                "allow-pairing-confirm",
                "allow-pairing-request-network-access",
                "allow-pairing-status",
                "allow-pairing-cancel",
                "allow-pairing-forget",
                "allow-pairing-copy-code",
                "allow-sharing-edit-begin",
                "allow-sharing-edit-end",
                "allow-sharing-set-active",
                "allow-sharing-apply-setup",
                "allow-sharing-touch",
                "allow-sharing-select-source",
                "allow-sharing-status",
                "allow-sharing-save-setup",
                "allow-sharing-arrangements",
                "allow-sharing-arrangement-save",
                "allow-sharing-arrangement-delete",
                "allow-sharing-dismiss-display-notice",
                "allow-sharing-copy-last-drop",
                "allow-dimming-status",
                "allow-dimming-set-enabled",
                "allow-dimming-set-level",
                "allow-dimming-preview-level",
                "allow-dimming-toggle",
                "allow-appearance-status",
                "allow-appearance-set-theme",
                "core:window:allow-start-dragging",
                "core:window:allow-internal-toggle-maximize",
                "core:window:allow-minimize",
                "core:window:allow-toggle-maximize",
                "core:window:allow-close",
                "core:window:allow-is-maximized",
                "core:event:allow-listen",
                "core:event:allow-unlisten",
                "allow-sharing-trial-open",
                "allow-window-hide",
                "allow-computers-load",
                "allow-computers-rename",
                "allow-reveal-logs",
                "allow-updates-status",
                "allow-updates-set-automatic",
                "allow-updates-check",
                "allow-updates-install",
                "allow-app-open-link",
                "allow-autostart-status",
                "allow-autostart-set",
                "allow-autostart-open-settings"
            ])
        );
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(
            config["app"]["security"]["capabilities"],
            serde_json::json!(["setup", "trial"])
        );
        assert_eq!(
            config["bundle"]["windows"]["webviewInstallMode"]["type"],
            "skip"
        );
    }

    #[test]
    fn test_window_cannot_call_setup_or_general_input_commands() {
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/trial.json")).unwrap();
        assert_eq!(
            capability["windows"],
            serde_json::json!(["controlled-trial"])
        );
        assert_eq!(capability["local"], true);
        assert!(capability.get("remote").is_none());
        assert_eq!(
            capability["permissions"],
            serde_json::json!([
                "allow-trial-start",
                "allow-trial-status",
                "allow-trial-stop",
                "allow-trial-close"
            ])
        );
    }

    #[test]
    fn webview2_defaults_retain_wry_browser_hardening() {
        assert_eq!(
            WEBVIEW2_DEFAULT_BROWSER_ARGS,
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection"
        );
    }
}
