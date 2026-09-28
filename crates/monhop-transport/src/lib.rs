//! Fail-closed, interface-pinned transport. Networking is opt-in.
pub mod crypto;
#[cfg(any(windows, target_os = "macos", test))]
pub mod display_arrangement;
#[cfg(any(windows, target_os = "macos"))]
pub mod guarded_endpoint;
pub mod identity_store;
pub mod policy;

pub mod native_storage;
pub mod pairing;
pub mod session;
pub mod session_actor;
pub mod session_clipboard;
pub(crate) mod session_clock;
pub mod session_group;
pub mod session_handshake;
pub mod session_health;
#[cfg(any(windows, target_os = "macos"))]
pub mod session_hub;
pub mod session_hub_actor;
#[cfg(any(windows, target_os = "macos"))]
pub mod session_link;
#[cfg(any(windows, target_os = "macos"))]
pub mod session_native;
pub mod session_receiver;
pub mod session_receiver_set;
#[cfg(any(windows, target_os = "macos"))]
mod session_runtime;
#[cfg(any(windows, target_os = "macos"))]
pub mod session_setup;
pub mod session_source;
#[cfg(any(windows, target_os = "macos"))]
mod session_source_runtime;
pub(crate) mod session_startup;
pub mod session_threads;
pub mod session_wire;

#[cfg(test)]
pub(crate) static NATIVE_OWNERSHIP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
