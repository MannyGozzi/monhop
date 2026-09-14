use crate::cli::{Command, HELP};

#[cfg(target_os = "macos")]
use monhop_platform_macos::network;
#[cfg(windows)]
use monhop_platform_windows::network;
#[cfg(windows)]
use network::enumerate_adapters as read_diagnostic_adapters;
#[cfg(target_os = "macos")]
use network::enumerate_adapters_with_attachment as read_diagnostic_adapters;

pub fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Status => {
            println!(
                "MonHop 0.1.0 | sharing DISABLED\nNo peer configured. No listeners, capture, suppression, or injection active.\nRun monhop --help for explicit native diagnostics."
            );
            Ok(())
        }
        Command::Help => {
            println!("{HELP}");
            Ok(())
        }
        Command::Displays => displays(),
        Command::Permissions => permissions(),
        Command::RequestPermissions => request_permissions(),
        Command::Interfaces => interfaces(),
        Command::ShowIdentity => crate::identity_storage::show(),
        Command::CreateIdentity => crate::identity_storage::create(),
        Command::Capture(duration) => capture(duration),
        Command::CheckPeer {
            interface,
            local,
            peer,
        } => check_peer(interface, local, peer),
    }
}

fn display_summary(display: &monhop_core::Display) {
    println!(
        "{} | id={} | native={}x{} | logical={}x{} at ({},{}) | scale={} | refresh={} | primary={}",
        display.name,
        display.id.0,
        display.native_size.width,
        display.native_size.height,
        display.logical_size.width,
        display.logical_size.height,
        display.origin.x,
        display.origin.y,
        display.scale_factor,
        display
            .refresh_rate_hz
            .map_or_else(|| "unknown".into(), |hz| hz.to_string()),
        display.primary
    );
}

#[cfg(windows)]
fn displays() -> Result<(), String> {
    let displays = monhop_platform_windows::enumerate_displays(monhop_core::DeviceId::default())
        .map_err(|error| error.to_string())?;
    for display in displays {
        display_summary(&display);
    }
    Ok(())
}
#[cfg(target_os = "macos")]
fn displays() -> Result<(), String> {
    for display in
        monhop_platform_macos::enumerate_active_displays().map_err(|error| error.to_string())?
    {
        let name = format!("Display {}", display.id.0);
        display_summary(&display.into_core_display(monhop_core::DeviceId::default(), name));
    }
    Ok(())
}

#[cfg(windows)]
fn permissions() -> Result<(), String> {
    println!(
        "Windows diagnostics run without elevation. SendInput remains subject to UIPI.\nSecure desktop and elevated targets are unsupported. No permissions changed."
    );
    Ok(())
}

#[cfg(windows)]
fn request_permissions() -> Result<(), String> {
    Err("This permission request is macOS-only. MonHop does not request Windows elevation".into())
}

#[cfg(target_os = "macos")]
fn request_permissions() -> Result<(), String> {
    let state = monhop_platform_macos::request_permissions().map_err(|error| error.to_string())?;
    println!(
        "Accessibility={} | Input Monitoring={}. Complete any macOS prompts, then rerun permissions.",
        state.accessibility, state.listen_events
    );
    Ok(())
}
#[cfg(target_os = "macos")]
fn permissions() -> Result<(), String> {
    let state =
        monhop_platform_macos::preflight_permissions().map_err(|error| error.to_string())?;
    println!(
        "Accessibility={} | Input Monitoring={}. No permission prompt requested.",
        state.accessibility, state.listen_events
    );
    Ok(())
}

#[cfg(windows)]
fn capture(duration: std::time::Duration) -> Result<(), String> {
    println!(
        "Passive event counts for {} seconds. Input remains local. No keys or text recorded.",
        duration.as_secs()
    );
    let counts =
        monhop_platform_windows::capture_counts(duration).map_err(|error| error.to_string())?;
    println!("{counts:?}");
    Ok(())
}
#[cfg(target_os = "macos")]
fn capture(duration: std::time::Duration) -> Result<(), String> {
    println!(
        "Passive event counts for {} seconds. Input remains local. No keys or text recorded.",
        duration.as_secs()
    );
    let counts = monhop_platform_macos::run_passive_diagnostic(duration)
        .map_err(|error| error.to_string())?;
    println!("{counts:?}");
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
fn interfaces() -> Result<(), String> {
    for adapter in read_diagnostic_adapters().map_err(|error| error.to_string())? {
        println!(
            "{} | index={} | {}/{} | physical={} | up={} | kind={} | attachment-known={}",
            adapter.name,
            adapter.index,
            adapter.address,
            adapter.prefix_len,
            adapter.physical,
            adapter.up,
            if adapter.ethernet {
                "Ethernet"
            } else if adapter.wifi {
                "Wi-Fi"
            } else {
                "unsupported"
            },
            adapter.attachment.is_some()
        );
    }
    Ok(())
}
#[cfg(not(any(windows, target_os = "macos")))]
fn interfaces() -> Result<(), String> {
    Err("Native network-interface enforcement is not implemented on this OS yet".into())
}

#[cfg(any(windows, target_os = "macos"))]
fn check_peer(
    interface: u32,
    local: std::net::Ipv4Addr,
    peer: std::net::Ipv4Addr,
) -> Result<(), String> {
    use monhop_transport::policy::*;
    let adapter = read_diagnostic_adapters()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|adapter| adapter.index == interface && adapter.address == local)
        .ok_or("The exact selected interface/address is not present")?;
    let snapshot = InterfaceSnapshot {
        stable_id: adapter.stable_id,
        name: adapter.name,
        index: interface,
        address: local,
        prefix_len: adapter.prefix_len,
        kind: if adapter.ethernet {
            InterfaceKind::Ethernet
        } else if adapter.wifi {
            InterfaceKind::WiFi
        } else {
            InterfaceKind::Other
        },
        is_hardware: adapter.physical,
        is_up: adapter.up,
        network_signature: adapter.attachment.unwrap_or_default(),
    };
    validate_peer(&snapshot, peer).map_err(|error| error.to_string())?;
    let route = network::best_route(local, peer).map_err(|error| error.to_string())?;
    NetworkLock::new(
        snapshot,
        peer,
        RouteSnapshot {
            interface_index: route.interface_index,
            source: route.source,
            next_hop: route.next_hop,
        },
        false,
    )
    .map_err(|error| error.to_string())?;
    println!(
        "Route approved for {local} -> {peer} on interface {interface}, without a gateway.\nRead-only check: no connection opened. Sharing remains disabled."
    );
    Ok(())
}
#[cfg(not(any(windows, target_os = "macos")))]
fn check_peer(_: u32, _: std::net::Ipv4Addr, _: std::net::Ipv4Addr) -> Result<(), String> {
    Err("Native route enforcement is not implemented on this OS yet".into())
}
