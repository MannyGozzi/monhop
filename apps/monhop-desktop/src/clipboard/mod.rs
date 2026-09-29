//! Optional clipboard sharing with connected computers: text and images, off by default.

// The hub wiring step removes this once the app calls into the module.
#![cfg_attr(not(test), allow(dead_code))]

mod content;
mod hub;
mod image;
mod native;
mod settings;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
