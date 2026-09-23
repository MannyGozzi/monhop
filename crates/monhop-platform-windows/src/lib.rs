//! Native Windows input and display adapter. Startup must not install hooks or inject input.

pub use monhop_core::{capture, capture_control, capture_physical};
#[path = "capture/decode.rs"]
pub mod capture_decode;
#[cfg(windows)]
pub mod desktop_state;
pub mod displays;
pub mod input;
pub mod keymap;
#[cfg(windows)]
#[path = "capture/native.rs"]
pub mod native_capture;

#[cfg(windows)]
pub mod identity;
#[cfg(any(windows, test))]
pub mod identity_storage;
#[cfg(windows)]
pub mod network;
#[cfg(any(windows, test))]
pub mod network_watch;
#[cfg(any(windows, test))]
mod power_watch;
pub mod threads;
#[cfg(any(windows, test))]
pub mod udp_receive;

pub use displays::*;
pub use input::*;
pub use keymap::*;
