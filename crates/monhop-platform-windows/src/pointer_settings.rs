//! Reads the user's Windows pointer settings for [`Ballistics`]. Read-only: nothing here changes a
//! setting, and each read is a plain system call that never runs inside an input callback.

use std::ptr;

use monhop_core::pointer_ballistics::{Ballistics, CURVE_BYTES};
use windows_sys::Win32::{
    Foundation::{ERROR_SUCCESS, POINT},
    Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint},
    System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RegGetValueW},
    UI::{
        Shell::{Common::DEVICE_SCALE_FACTOR, GetScaleFactorForMonitor},
        WindowsAndMessaging::{SPI_GETMOUSE, SPI_GETMOUSESPEED, SystemParametersInfoW},
    },
};

/// The pointer speed, Enhance pointer precision and SmoothMouse curves as Windows applies them
/// now; a setting it cannot read takes Windows' default.
pub fn read() -> Ballistics {
    let mut mouse = [0_i32; 3];
    // SAFETY: SPI_GETMOUSE writes the threshold pair and acceleration flag into three i32s.
    let read = unsafe { SystemParametersInfoW(SPI_GETMOUSE, 0, mouse.as_mut_ptr().cast(), 0) };
    // Unread, it keeps Windows' default: on.
    let enhance = read == 0 || mouse[2] != 0;
    let mut speed = 0_u32;
    // SAFETY: SPI_GETMOUSESPEED writes one u32; a failed read leaves 0, which is no slider notch.
    unsafe { SystemParametersInfoW(SPI_GETMOUSESPEED, 0, (&raw mut speed).cast(), 0) };
    let x = curve(windows_sys::core::w!("SmoothMouseXCurve"));
    let y = curve(windows_sys::core::w!("SmoothMouseYCurve"));
    Ballistics::new(
        enhance,
        speed,
        x.as_ref().map(|bytes| &bytes[..]),
        y.as_ref().map(|bytes| &bytes[..]),
        // The primary display sits at the desktop's origin.
        pixels_per_dip_at(0, 0),
    )
}

/// One curve's registry bytes, if it holds exactly a curve's worth.
fn curve(name: *const u16) -> Option<[u8; CURVE_BYTES]> {
    let mut bytes = [0_u8; CURVE_BYTES];
    let mut size = CURVE_BYTES as u32;
    // SAFETY: both names are nul-terminated literals; bytes has `size` writable bytes. A larger
    // value fails with ERROR_MORE_DATA instead of writing past them.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows_sys::core::w!("Control Panel\\Mouse"),
            name,
            RRF_RT_REG_BINARY,
            ptr::null_mut(),
            bytes.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS && size == CURVE_BYTES as u32).then_some(bytes)
}

/// The scale of the display nearest the physical desktop point `x`, `y`; 1 when Windows cannot
/// say. The primary display's sets the DIP an unaccelerated pixel count is measured in.
pub fn pixels_per_dip_at(x: i32, y: i32) -> f64 {
    // SAFETY: a by-value point and flag; the monitor handle needs no release.
    let monitor = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) };
    let mut percent: DEVICE_SCALE_FACTOR = 0;
    // SAFETY: `monitor` came from MonitorFromPoint and percent is writable.
    if monitor.is_null() || unsafe { GetScaleFactorForMonitor(monitor, &mut percent) } != 0 {
        return 1.0;
    }
    f64::from(percent) / 100.0
}
