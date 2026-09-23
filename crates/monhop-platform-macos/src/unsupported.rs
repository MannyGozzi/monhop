use std::sync::atomic::AtomicBool;
use std::time::Duration;

use monhop_core::{MouseButton, Point};

use crate::{MacDisplay, MacError, MacVirtualKey, PassiveDiagnosticCounts, PermissionState};

#[derive(Clone, Copy, Debug)]
pub enum PostingDestination {
    Unsupported,
}

pub const fn system_destination() -> PostingDestination {
    PostingDestination::Unsupported
}

pub fn current_process_destination() -> Result<PostingDestination, MacError> {
    Err(MacError::UnsupportedPlatform)
}

pub fn enumerate_active_displays() -> Result<Vec<MacDisplay>, MacError> {
    Err(MacError::UnsupportedPlatform)
}

pub fn preflight_permissions() -> Result<PermissionState, MacError> {
    Err(MacError::UnsupportedPlatform)
}

pub fn request_permissions() -> Result<PermissionState, MacError> {
    Err(MacError::UnsupportedPlatform)
}

pub fn run_passive_diagnostic(
    _: Duration,
    _: i64,
    _: &AtomicBool,
) -> Result<PassiveDiagnosticCounts, MacError> {
    Err(MacError::UnsupportedPlatform)
}

pub fn ensure_injection_permission() -> Result<(), MacError> {
    Err(MacError::UnsupportedPlatform)
}

pub fn post_key(
    _: PostingDestination,
    _: MacVirtualKey,
    _: bool,
    _: i64,
    _: u64,
) -> Result<(), MacError> {
    Err(MacError::UnsupportedPlatform)
}

pub fn post_button(
    _: PostingDestination,
    _: MouseButton,
    _: bool,
    _: u8,
    _: i64,
    _: u64,
    _: Point,
) -> Result<(), MacError> {
    Err(MacError::UnsupportedPlatform)
}

pub fn post_scroll(
    _: PostingDestination,
    _: i32,
    _: i32,
    _: i64,
    _: u64,
    _: Point,
) -> Result<(), MacError> {
    Err(MacError::UnsupportedPlatform)
}

pub fn post_motion(
    _: PostingDestination,
    _: Point,
    _: i64,
    _: u64,
    _: Option<MouseButton>,
) -> Result<(), MacError> {
    Err(MacError::UnsupportedPlatform)
}
