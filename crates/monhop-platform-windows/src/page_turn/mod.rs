//! The chevron that blooms beside the pointer when a swipe from another computer turns the page.

mod raster;
#[cfg(windows)]
mod renderer;

#[cfg(windows)]
pub use renderer::show;
