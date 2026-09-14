//! Explicit, write-only copying of the already-opened public connection code.

use crate::pairing::PairingController;

pub fn copy(
    controller: &PairingController,
    write: impl FnOnce(String) -> Result<(), String>,
) -> Result<(), String> {
    write(controller.code_for_copy()?)
}

pub fn write(code: String) -> Result<(), String> {
    #[cfg(any(target_os = "macos", windows))]
    {
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(code))
            .map_err(|_| "Could not copy the code. The clipboard may be busy. Try again.".into())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = code;
        Err("Copy is available in the macOS and Windows apps.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unopened_pairing_cannot_touch_the_clipboard() {
        let controller = PairingController::default();
        let result = copy(&controller, |_| panic!("clipboard must remain untouched"));
        assert!(result.is_err());
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    #[ignore = "Explicit native check: replaces the system clipboard with a synthetic public pairing code"]
    fn native_copy_writes_a_synthetic_public_offer() {
        let identity = monhop_transport::crypto::DeviceIdentity::generate().unwrap();
        let offer = monhop_transport::pairing::PairingOffer::new(
            "192.168.50.10:24872".parse().unwrap(),
            identity.certificate_der(),
        )
        .unwrap();
        assert!(write(offer.to_code()).is_ok());
    }
}
