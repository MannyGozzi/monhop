//! Explicit, write-only copying of text the user chose to copy.

pub fn write(text: String) -> Result<(), String> {
    #[cfg(any(target_os = "macos", windows))]
    {
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(text))
            .map_err(|_| "Could not copy. The clipboard may be busy. Try again.".into())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = text;
        Err("Copy is available in the macOS and Windows apps.".into())
    }
}
