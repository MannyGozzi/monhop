//! Windows monitor enumeration in physical virtual-desktop pixels.

use monhop_core::{DeviceId, Display, DisplayId};
#[cfg(any(windows, test))]
use std::collections::BTreeMap;
use std::fmt;

#[cfg(any(windows, test))]
const MAX_DISPLAY_NAME_BYTES: usize = 96;

#[cfg(any(windows, test))]
fn sanitized_display_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > MAX_DISPLAY_NAME_BYTES
        || name.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'..='\u{200f}'
                        | '\u{2028}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
    {
        None
    } else {
        Some(name.to_owned())
    }
}

/// A GDI device's resolved friendly name and physical monitor identity, cached together because
/// both come from the same per-target `QueryDisplayConfig` read.
#[cfg(any(windows, test))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DisplayNameEntry {
    friendly_name: Option<String>,
    monitor: Option<monhop_core::MonitorIdentity>,
}

/// Collapses per-target readings to one entry per GDI source, dropping either field to `None` when
/// clone/mirror targets sharing a source disagree on it (an ambiguous label or identity is unsafe to show).
#[cfg(any(windows, test))]
fn unique_display_entries(
    entries: impl IntoIterator<Item = (String, Option<String>, Option<monhop_core::MonitorIdentity>)>,
) -> BTreeMap<String, DisplayNameEntry> {
    #[derive(Clone)]
    struct Accumulated {
        friendly_name: Option<String>,
        friendly_name_ambiguous: bool,
        monitor: Option<monhop_core::MonitorIdentity>,
        monitor_ambiguous: bool,
    }

    let mut accumulated = BTreeMap::<String, Accumulated>::new();
    for (source, friendly_name, monitor) in entries {
        let friendly_name = friendly_name.as_deref().and_then(sanitized_display_name);
        match accumulated.entry(source) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Accumulated {
                    friendly_name,
                    friendly_name_ambiguous: false,
                    monitor,
                    monitor_ambiguous: false,
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                if existing.friendly_name != friendly_name {
                    existing.friendly_name_ambiguous = true;
                }
                if existing.monitor != monitor {
                    existing.monitor_ambiguous = true;
                }
            }
        }
    }

    accumulated
        .into_iter()
        .map(|(source, accumulated)| {
            let friendly_name = (!accumulated.friendly_name_ambiguous)
                .then_some(accumulated.friendly_name)
                .flatten();
            let monitor = (!accumulated.monitor_ambiguous)
                .then_some(accumulated.monitor)
                .flatten();
            (
                source,
                DisplayNameEntry {
                    friendly_name,
                    monitor,
                },
            )
        })
        .collect()
}

#[cfg(any(windows, test))]
#[derive(Clone, Debug, PartialEq)]
struct DisplayGeometry {
    display_id: DisplayId,
    native_size: monhop_core::NativeSize,
    logical_size: monhop_core::LogicalSize,
    origin: monhop_core::Point,
    scale_factor: f64,
    primary: bool,
}

#[cfg(any(windows, test))]
type DisplayGeometrySet = BTreeMap<String, DisplayGeometry>;

#[cfg(any(windows, test))]
#[derive(Default)]
struct DisplayNameCache {
    geometry: Option<DisplayGeometrySet>,
    entries: BTreeMap<String, DisplayNameEntry>,
    refresh_requested: bool,
    querying: bool,
}

#[cfg(any(windows, test))]
impl DisplayNameCache {
    fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    fn begin_query(&mut self, geometry: &DisplayGeometrySet) -> bool {
        if self.querying || (!self.refresh_requested && self.geometry.as_ref() == Some(geometry)) {
            return false;
        }
        self.querying = true;
        self.refresh_requested = false;
        true
    }

    fn finish_query(
        &mut self,
        geometry: DisplayGeometrySet,
        entries: BTreeMap<String, DisplayNameEntry>,
    ) {
        self.entries = entries
            .into_iter()
            .filter(|(device_name, _)| geometry.contains_key(device_name))
            .collect();
        self.geometry = Some(geometry);
        self.querying = false;
    }

    fn entries_for(&self, geometry: &DisplayGeometrySet) -> BTreeMap<String, DisplayNameEntry> {
        if !self.querying && self.geometry.as_ref() == Some(geometry) {
            self.entries.clone()
        } else {
            BTreeMap::new()
        }
    }
}

#[cfg(any(windows, test))]
fn display_geometry_set(displays: &[Display]) -> Option<DisplayGeometrySet> {
    if displays.len() > monhop_core::MAX_DISPLAYS {
        return None;
    }

    let mut geometry = BTreeMap::new();
    for display in displays {
        let value = DisplayGeometry {
            display_id: display.id,
            native_size: display.native_size,
            logical_size: display.logical_size,
            origin: display.origin,
            scale_factor: display.scale_factor,
            primary: display.primary,
        };
        if geometry.insert(display.name.clone(), value).is_some() {
            return None;
        }
    }
    Some(geometry)
}

/// Windows display origins and `logical_size` values use the per-monitor-DPI-aware virtual desktop's
/// physical pixel coordinate system. Origins are never divided by a display scale factor.
pub const WINDOWS_DISPLAY_COORDINATE_UNITS: &str =
    "per-monitor-DPI-aware physical virtual-desktop pixels";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisplayError {
    UnsupportedPlatform,
    WindowsApi { operation: &'static str, code: u32 },
    InvalidDeviceName,
    InvalidMonitorGeometry,
    InvalidDisplayMode,
    InvalidDpi,
    NoDisplays,
}

impl fmt::Display for DisplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("Windows display enumeration is unavailable")
            }
            Self::WindowsApi { operation, code } => {
                write!(
                    formatter,
                    "Windows display operation {operation} failed with code {code}"
                )
            }
            Self::InvalidDeviceName => {
                formatter.write_str("Windows returned an invalid display device name")
            }
            Self::InvalidMonitorGeometry => {
                formatter.write_str("Windows returned invalid monitor geometry")
            }
            Self::InvalidDisplayMode => {
                formatter.write_str("Windows returned an invalid display mode")
            }
            Self::InvalidDpi => {
                formatter.write_str("Windows returned invalid monitor DPI metadata")
            }
            Self::NoDisplays => formatter.write_str("Windows reported no active displays"),
        }
    }
}

impl std::error::Error for DisplayError {}

/// Enumerates active Windows monitors without changing thread or process DPI awareness.
///
/// Each result keeps virtual-desktop origins and geometry in physical pixels so monitors with
/// different DPI scales remain aligned. `scale_factor` is effective DPI divided by the 96-DPI base.
#[cfg(windows)]
pub fn enumerate_displays(device: DeviceId) -> Result<Vec<Display>, DisplayError> {
    windows::enumerate_displays(device)
}

#[cfg(not(windows))]
pub fn enumerate_displays(_device: DeviceId) -> Result<Vec<Display>, DisplayError> {
    Err(DisplayError::UnsupportedPlatform)
}

/// Requests a best-effort friendly-name refresh after the next display enumeration.
/// This only changes cosmetic labels and never changes Windows display configuration.
#[cfg(windows)]
pub fn refresh_display_names() {
    windows::request_friendly_name_refresh();
}

#[cfg(not(windows))]
pub fn refresh_display_names() {}

/// Stable, non-secret identifier derived from Windows' display device name.
pub fn display_id_from_device_name(device_name: &str) -> DisplayId {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in device_name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    DisplayId(hash)
}

#[cfg(any(windows, test))]
const EDID_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

#[cfg(any(windows, test))]
const EDID_MIN_BYTES: usize = 128;

/// Parses vendor/product/serial from raw EDID bytes exactly as macOS's CGDisplay* accessors derive
/// them, so a monitor cabled to both computers reports an identical identity on each side.
#[cfg(any(windows, test))]
fn edid_monitor_identity(edid: &[u8]) -> Option<monhop_core::MonitorIdentity> {
    if edid.len() < EDID_MIN_BYTES || edid[..8] != EDID_HEADER {
        return None;
    }
    let vendor = u16::from_be_bytes([edid[8], edid[9]]);
    let product = u16::from_le_bytes([edid[10], edid[11]]);
    let serial = u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]);
    monhop_core::MonitorIdentity::new(vendor, product, serial)
}

#[cfg(any(windows, test))]
const REGISTRY_SEGMENT_MAX_LEN: usize = 64;

#[cfg(any(windows, test))]
fn valid_registry_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= REGISTRY_SEGMENT_MAX_LEN
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'&' | b'_' | b'.' | b'-'))
}

/// Derives the per-monitor `Device Parameters` registry key from a target's device interface path
/// (e.g. `\\?\DISPLAY#DELA0E5#5&2c8b8a0&0&UID4352#{...}`), rejecting anything but a bounded,
/// strictly ASCII hwid/instance pair so a registry path is never built from unvalidated text.
#[cfg(any(windows, test))]
fn monitor_device_registry_path(device_path: &str) -> Option<String> {
    let stripped = device_path.strip_prefix(r"\\?\")?;
    let mut segments = stripped.split('#');
    let _class = segments.next()?;
    let hwid = segments.next()?;
    let instance = segments.next()?;
    if !valid_registry_segment(hwid) || !valid_registry_segment(instance) {
        return None;
    }
    Some(format!(
        r"SYSTEM\CurrentControlSet\Enum\DISPLAY\{hwid}\{instance}\Device Parameters"
    ))
}

#[cfg(windows)]
mod windows {
    use std::{
        collections::BTreeMap,
        mem::size_of,
        ptr,
        sync::{Mutex, OnceLock},
    };

    use monhop_core::{DeviceId, Display, LogicalSize, MAX_DISPLAYS, NativeSize, Point};
    use windows_sys::Win32::{
        Devices::Display::{
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
            DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
            DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
            QueryDisplayConfig,
        },
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, GetLastError, LPARAM},
        Graphics::Gdi::{
            ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsExW, GetMonitorInfoW,
            HDC, HMONITOR, MONITORINFOEXW,
        },
        System::Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_BINARY, RegCloseKey, RegOpenKeyExW,
            RegQueryValueExW,
        },
        UI::{
            Shell::{Common::DEVICE_SCALE_FACTOR, GetScaleFactorForMonitor},
            WindowsAndMessaging::MONITORINFOF_PRIMARY,
        },
    };

    use super::{
        DisplayError, DisplayNameCache, DisplayNameEntry, EDID_MIN_BYTES, display_geometry_set,
        display_id_from_device_name, edid_monitor_identity, monitor_device_registry_path,
        unique_display_entries,
    };

    const MAX_ACTIVE_DISPLAY_PATHS: u32 = (MAX_DISPLAYS as u32) * 4;
    const MAX_DISPLAY_CONFIG_MODES: u32 = MAX_ACTIVE_DISPLAY_PATHS * 3;
    const DISPLAY_CONFIG_READ_ATTEMPTS: usize = 3;

    static FRIENDLY_NAME_CACHE: OnceLock<Mutex<DisplayNameCache>> = OnceLock::new();

    fn friendly_name_cache() -> &'static Mutex<DisplayNameCache> {
        FRIENDLY_NAME_CACHE.get_or_init(|| Mutex::new(DisplayNameCache::default()))
    }

    pub(super) fn request_friendly_name_refresh() {
        if let Ok(mut cache) = friendly_name_cache().lock() {
            cache.request_refresh();
        }
    }

    pub(super) fn enumerate_displays(device: DeviceId) -> Result<Vec<Display>, DisplayError> {
        let mut collector = MonitorCollector {
            device,
            displays: Vec::new(),
            error: None,
        };

        // SAFETY: The callback and context pointer remain valid for this synchronous enumeration.
        let enumerated = unsafe {
            EnumDisplayMonitors(
                ptr::null_mut::<core::ffi::c_void>() as HDC,
                ptr::null(),
                Some(monitor_callback),
                (&mut collector as *mut MonitorCollector).cast::<core::ffi::c_void>() as LPARAM,
            )
        };
        if enumerated == 0 {
            return Err(collector
                .error
                .unwrap_or_else(|| last_error("EnumDisplayMonitors")));
        }
        if let Some(error) = collector.error {
            return Err(error);
        }
        if collector.displays.is_empty() {
            return Err(DisplayError::NoDisplays);
        }

        let entries = cached_display_entries(&collector.displays);
        Ok(collector
            .displays
            .into_iter()
            .map(|mut display| {
                let entry = entries.get(&display.name).cloned().unwrap_or_default();
                if let Some(friendly_name) = entry.friendly_name {
                    display.name = friendly_name;
                }
                display.with_monitor(entry.monitor)
            })
            .collect())
    }

    struct MonitorCollector {
        device: DeviceId,
        displays: Vec<Display>,
        error: Option<DisplayError>,
    }

    unsafe extern "system" fn monitor_callback(
        monitor: HMONITOR,
        _hdc: HDC,
        _clip: *mut windows_sys::Win32::Foundation::RECT,
        data: LPARAM,
    ) -> i32 {
        // SAFETY: EnumDisplayMonitors invokes this synchronously with the context pointer above.
        let collector = unsafe { &mut *(data as *mut MonitorCollector) };
        match monitor_to_display(monitor, collector.device) {
            Ok(display) => {
                collector.displays.push(display);
                1
            }
            Err(error) => {
                collector.error = Some(error);
                0
            }
        }
    }

    fn monitor_to_display(monitor: HMONITOR, device: DeviceId) -> Result<Display, DisplayError> {
        let mut monitor_info = MONITORINFOEXW::default();
        monitor_info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `monitor` is supplied by EnumDisplayMonitors and monitor_info has the required size.
        if unsafe { GetMonitorInfoW(monitor, &mut monitor_info.monitorInfo) } == 0 {
            return Err(last_error("GetMonitorInfoW"));
        }

        let device_name = wide_name(&monitor_info.szDevice)?;
        let mut mode = windows_sys::Win32::Graphics::Gdi::DEVMODEW {
            dmSize: size_of::<windows_sys::Win32::Graphics::Gdi::DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: device_name is a nul-terminated field returned by GetMonitorInfoW and mode is sized.
        if unsafe {
            EnumDisplaySettingsExW(
                monitor_info.szDevice.as_ptr(),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
                0,
            )
        } == 0
        {
            return Err(last_error("EnumDisplaySettingsExW"));
        }
        if mode.dmPelsWidth == 0 || mode.dmPelsHeight == 0 {
            return Err(DisplayError::InvalidDisplayMode);
        }

        let rect = monitor_info.monitorInfo.rcMonitor;
        let width = rect
            .right
            .checked_sub(rect.left)
            .filter(|width| *width > 0)
            .ok_or(DisplayError::InvalidMonitorGeometry)?;
        let height = rect
            .bottom
            .checked_sub(rect.top)
            .filter(|height| *height > 0)
            .ok_or(DisplayError::InvalidMonitorGeometry)?;

        let mut scale_percent: DEVICE_SCALE_FACTOR = 0;
        // SAFETY: `monitor` is valid for this callback and scale_percent is writable.
        let hresult = unsafe { GetScaleFactorForMonitor(monitor, &mut scale_percent) };
        if hresult != 0 {
            return Err(DisplayError::WindowsApi {
                operation: "GetScaleFactorForMonitor",
                code: hresult as u32,
            });
        }
        if scale_percent <= 0 {
            return Err(DisplayError::InvalidDpi);
        }

        let primary = monitor_info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
        let display_id = display_id_from_device_name(&device_name);
        Ok(Display::new(
            display_id,
            device,
            device_name,
            NativeSize::new(mode.dmPelsWidth, mode.dmPelsHeight),
            LogicalSize::new(f64::from(width), f64::from(height)),
            Point::new(f64::from(rect.left), f64::from(rect.top)),
            f64::from(scale_percent) / 100.0,
            (mode.dmDisplayFrequency > 1).then_some(f64::from(mode.dmDisplayFrequency)),
            primary,
        ))
    }

    fn cached_display_entries(displays: &[Display]) -> BTreeMap<String, DisplayNameEntry> {
        let Some(geometry) = display_geometry_set(displays) else {
            return BTreeMap::new();
        };

        let should_query = match friendly_name_cache().lock() {
            Ok(mut cache) => cache.begin_query(&geometry),
            Err(_) => return BTreeMap::new(),
        };
        if should_query {
            let entries = display_entries_by_gdi_device();
            let Ok(mut cache) = friendly_name_cache().lock() else {
                return BTreeMap::new();
            };
            cache.finish_query(geometry.clone(), entries);
        }

        friendly_name_cache()
            .lock()
            .ok()
            .map_or_else(BTreeMap::new, |cache| cache.entries_for(&geometry))
    }

    fn display_entries_by_gdi_device() -> BTreeMap<String, DisplayNameEntry> {
        let Some(paths) = active_display_paths() else {
            return BTreeMap::new();
        };
        unique_display_entries(paths.iter().filter_map(|path| {
            let source = source_gdi_device_name(path)?;
            let target = target_device_info(path)?;
            Some((source, target.friendly_name, target.monitor))
        }))
    }

    fn active_display_paths() -> Option<Vec<DISPLAYCONFIG_PATH_INFO>> {
        for _ in 0..DISPLAY_CONFIG_READ_ATTEMPTS {
            let mut path_count = 0_u32;
            let mut mode_count = 0_u32;
            // SAFETY: Both count pointers are writable for the duration of this synchronous query.
            if unsafe {
                GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
            } != 0
            {
                return None;
            }
            if path_count > MAX_ACTIVE_DISPLAY_PATHS || mode_count > MAX_DISPLAY_CONFIG_MODES {
                return None;
            }

            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            // SAFETY: The vectors have the counts supplied by GetDisplayConfigBufferSizes. The API
            // updates those counts before returning and does not retain either pointer.
            let status = unsafe {
                QueryDisplayConfig(
                    QDC_ONLY_ACTIVE_PATHS,
                    &mut path_count,
                    paths.as_mut_ptr(),
                    &mut mode_count,
                    modes.as_mut_ptr(),
                    ptr::null_mut(),
                )
            };
            if status == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            if status != 0 || path_count as usize > paths.len() || mode_count as usize > modes.len()
            {
                return None;
            }
            paths.truncate(path_count as usize);
            return Some(paths);
        }
        None
    }

    fn source_gdi_device_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: source is initialized with the documented request header and remains writable.
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
            return None;
        }
        wide_name(&source.viewGdiDeviceName).ok()
    }

    struct TargetDeviceInfo {
        friendly_name: Option<String>,
        monitor: Option<monhop_core::MonitorIdentity>,
    }

    /// One `DISPLAYCONFIG_TARGET_DEVICE_NAME` read yields both the cosmetic label and the device
    /// interface path used to look up the physical monitor's EDID.
    fn target_device_info(path: &DISPLAYCONFIG_PATH_INFO) -> Option<TargetDeviceInfo> {
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: target is initialized with the documented request header and remains writable.
        if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } != 0 {
            return None;
        }
        let friendly_name = wide_name(&target.monitorFriendlyDeviceName).ok();
        let monitor = wide_name(&target.monitorDevicePath)
            .ok()
            .and_then(|device_path| monitor_identity_for_device_path(&device_path));
        Some(TargetDeviceInfo {
            friendly_name,
            monitor,
        })
    }

    const EDID_MAX_BYTES: usize = 1024;

    /// Reads and parses the target's EDID from its `Device Parameters` registry key. Any failure
    /// (missing key, wrong type, out-of-range size, malformed header) yields `None`, never a panic.
    fn monitor_identity_for_device_path(device_path: &str) -> Option<monhop_core::MonitorIdentity> {
        let registry_path = monitor_device_registry_path(device_path)?;
        let wide_path: Vec<u16> = registry_path
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let mut key: HKEY = ptr::null_mut();
        // SAFETY: wide_path is nul-terminated and valid for the call; key is writable.
        let open_status = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                wide_path.as_ptr(),
                0,
                KEY_READ,
                &mut key,
            )
        };
        if open_status != ERROR_SUCCESS || key.is_null() {
            return None;
        }

        let value_name: Vec<u16> = "EDID\0".encode_utf16().collect();
        let mut value_type: u32 = 0;
        let mut data = vec![0_u8; EDID_MAX_BYTES];
        let mut data_len = data.len() as u32;
        // SAFETY: key was just opened above, value_name is nul-terminated, data has data_len capacity.
        let query_status = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                ptr::null(),
                &mut value_type,
                data.as_mut_ptr(),
                &mut data_len,
            )
        };
        // SAFETY: key was returned by the successful RegOpenKeyExW call above.
        unsafe { RegCloseKey(key) };

        if query_status != ERROR_SUCCESS || value_type != REG_BINARY {
            return None;
        }
        let len = data_len as usize;
        if !(EDID_MIN_BYTES..=EDID_MAX_BYTES).contains(&len) {
            return None;
        }
        edid_monitor_identity(&data[..len])
    }

    fn wide_name(name: &[u16]) -> Result<String, DisplayError> {
        let length = name
            .iter()
            .position(|character| *character == 0)
            .ok_or(DisplayError::InvalidDeviceName)?;
        String::from_utf16(&name[..length]).map_err(|_| DisplayError::InvalidDeviceName)
    }

    fn last_error(operation: &'static str) -> DisplayError {
        // SAFETY: GetLastError has no preconditions and only reads this thread's error state.
        let code = unsafe { GetLastError() };
        DisplayError::WindowsApi { operation, code }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use monhop_core::{DeviceId, Display, LogicalSize, MonitorIdentity, NativeSize, Point};

    use super::{
        DisplayNameCache, DisplayNameEntry, display_geometry_set, display_id_from_device_name,
        edid_monitor_identity, monitor_device_registry_path, sanitized_display_name,
        unique_display_entries,
    };

    fn display(name: &str, width: f64) -> Display {
        Display::new(
            display_id_from_device_name(name),
            DeviceId::default(),
            name.into(),
            NativeSize::new(3_840, 2_160),
            LogicalSize::new(width, 1_080.0),
            Point::new(0.0, 0.0),
            2.0,
            Some(60.0),
            true,
        )
    }

    fn named_entries(
        entries: impl IntoIterator<Item = (&'static str, &'static str)>,
    ) -> BTreeMap<String, DisplayNameEntry> {
        entries
            .into_iter()
            .map(|(device_name, friendly_name)| {
                (
                    device_name.into(),
                    DisplayNameEntry {
                        friendly_name: Some(friendly_name.into()),
                        monitor: None,
                    },
                )
            })
            .collect()
    }

    fn friendly_name_of(
        entries: &BTreeMap<String, DisplayNameEntry>,
        device_name: &str,
    ) -> Option<String> {
        entries
            .get(device_name)
            .and_then(|entry| entry.friendly_name.clone())
    }

    #[test]
    fn friendly_names_and_monitors_are_mapped_only_when_source_is_unambiguous() {
        let dell = MonitorIdentity::new(0x10ac, 0xd0e5, 0x3030_5455);
        let lg = MonitorIdentity::new(0x1e6d, 0x5b12, 0);
        let entries = unique_display_entries([
            (r"\\.\DISPLAY1".into(), Some("Dell U2723QE".into()), dell),
            (r"\\.\DISPLAY2".into(), Some("LG UltraFine".into()), lg),
            (r"\\.\DISPLAY1".into(), Some("Dell U2723QE".into()), dell),
            (
                r"\\.\DISPLAY3".into(),
                Some("First clone target".into()),
                dell,
            ),
            (
                r"\\.\DISPLAY3".into(),
                Some("Second clone target".into()),
                dell,
            ),
        ]);

        assert_eq!(
            entries.get(r"\\.\DISPLAY1"),
            Some(&DisplayNameEntry {
                friendly_name: Some("Dell U2723QE".into()),
                monitor: dell,
            })
        );
        assert_eq!(
            entries.get(r"\\.\DISPLAY2"),
            Some(&DisplayNameEntry {
                friendly_name: Some("LG UltraFine".into()),
                monitor: lg,
            })
        );
        assert_eq!(
            entries.get(r"\\.\DISPLAY3"),
            Some(&DisplayNameEntry {
                friendly_name: None,
                monitor: dell,
            })
        );
    }

    #[test]
    fn ambiguous_monitor_identity_is_dropped_independently_of_the_name() {
        let dell = MonitorIdentity::new(0x10ac, 0xd0e5, 0x3030_5455);
        let other = MonitorIdentity::new(0x10ac, 0xd0e5, 1);
        let entries = unique_display_entries([
            (r"\\.\DISPLAY1".into(), Some("Dell U2723QE".into()), dell),
            (r"\\.\DISPLAY1".into(), Some("Dell U2723QE".into()), other),
        ]);

        assert_eq!(
            entries.get(r"\\.\DISPLAY1"),
            Some(&DisplayNameEntry {
                friendly_name: Some("Dell U2723QE".into()),
                monitor: None,
            })
        );
    }

    #[test]
    fn display_name_sanitization_rejects_invalid_os_labels() {
        assert_eq!(
            sanitized_display_name("  Dell U2723QE  "),
            Some("Dell U2723QE".into())
        );
        assert_eq!(sanitized_display_name("\u{202e}spoof"), None);
        assert_eq!(sanitized_display_name("line\nbreak"), None);
        assert_eq!(sanitized_display_name(&"x".repeat(97)), None);
    }

    #[test]
    fn cache_queries_once_for_an_unchanged_geometry_set() {
        let display = display(r"\\.\DISPLAY1", 1_920.0);
        let geometry = display_geometry_set(std::slice::from_ref(&display)).unwrap();
        let mut cache = DisplayNameCache::default();
        let mut queries = 0;

        assert!(cache.begin_query(&geometry));
        queries += 1;
        cache.finish_query(
            geometry.clone(),
            named_entries([(r"\\.\DISPLAY1", "Dell U2723QE")]),
        );

        assert!(!cache.begin_query(&geometry));
        assert_eq!(queries, 1);
        assert_eq!(
            friendly_name_of(&cache.entries_for(&geometry), r"\\.\DISPLAY1"),
            Some("Dell U2723QE".into())
        );
    }

    #[test]
    fn cache_invalidates_labels_before_querying_changed_geometry() {
        let first = display(r"\\.\DISPLAY1", 1_920.0);
        let first_geometry = display_geometry_set(std::slice::from_ref(&first)).unwrap();
        let changed = display(r"\\.\DISPLAY1", 1_600.0);
        let changed_geometry = display_geometry_set(std::slice::from_ref(&changed)).unwrap();
        let mut cache = DisplayNameCache::default();

        assert!(cache.begin_query(&first_geometry));
        cache.finish_query(
            first_geometry,
            named_entries([(r"\\.\DISPLAY1", "Dell U2723QE")]),
        );

        assert!(cache.begin_query(&changed_geometry));
        assert!(cache.entries_for(&changed_geometry).is_empty());
        cache.finish_query(
            changed_geometry.clone(),
            named_entries([(r"\\.\DISPLAY1", "Dell U3223QE")]),
        );
        assert_eq!(
            friendly_name_of(&cache.entries_for(&changed_geometry), r"\\.\DISPLAY1"),
            Some("Dell U3223QE".into())
        );
    }

    #[test]
    fn failed_refresh_replaces_a_previous_label_with_raw_fallback() {
        let first = display(r"\\.\DISPLAY1", 1_920.0);
        let first_geometry = display_geometry_set(std::slice::from_ref(&first)).unwrap();
        let changed = display(r"\\.\DISPLAY1", 1_600.0);
        let changed_geometry = display_geometry_set(std::slice::from_ref(&changed)).unwrap();
        let mut cache = DisplayNameCache::default();

        assert!(cache.begin_query(&first_geometry));
        cache.finish_query(
            first_geometry,
            named_entries([(r"\\.\DISPLAY1", "Dell U2723QE")]),
        );
        assert!(cache.begin_query(&changed_geometry));
        cache.finish_query(changed_geometry.clone(), BTreeMap::new());

        assert!(cache.entries_for(&changed_geometry).is_empty());
        assert!(!cache.begin_query(&changed_geometry));
    }

    #[test]
    fn explicit_refresh_requeries_unchanged_geometry() {
        let display = display(r"\\.\DISPLAY1", 1_920.0);
        let geometry = display_geometry_set(std::slice::from_ref(&display)).unwrap();
        let mut cache = DisplayNameCache::default();

        assert!(cache.begin_query(&geometry));
        cache.finish_query(geometry.clone(), BTreeMap::new());
        cache.request_refresh();
        assert!(cache.begin_query(&geometry));
    }

    #[test]
    fn explicit_refresh_during_a_query_is_not_lost() {
        let display = display(r"\\.\DISPLAY1", 1_920.0);
        let geometry = display_geometry_set(std::slice::from_ref(&display)).unwrap();
        let mut cache = DisplayNameCache::default();

        assert!(cache.begin_query(&geometry));
        cache.request_refresh();
        cache.finish_query(geometry.clone(), BTreeMap::new());
        assert!(cache.begin_query(&geometry));
    }

    fn sample_edid(vendor: u16, product: u16, serial: u32) -> [u8; 128] {
        let mut edid = [0_u8; 128];
        edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        edid[8..10].copy_from_slice(&vendor.to_be_bytes());
        edid[10..12].copy_from_slice(&product.to_le_bytes());
        edid[12..16].copy_from_slice(&serial.to_le_bytes());
        edid
    }

    #[test]
    fn edid_monitor_identity_parses_a_well_formed_edid() {
        let edid = sample_edid(0x10ac, 0xd0e5, 0x3030_5455);
        assert_eq!(
            edid_monitor_identity(&edid),
            MonitorIdentity::new(0x10ac, 0xd0e5, 0x3030_5455)
        );
    }

    #[test]
    fn edid_monitor_identity_rejects_a_bad_header() {
        let mut edid = sample_edid(0x10ac, 0xd0e5, 0x3030_5455);
        edid[0] = 0x01;
        assert_eq!(edid_monitor_identity(&edid), None);
    }

    #[test]
    fn edid_monitor_identity_rejects_a_short_buffer() {
        let edid = sample_edid(0x10ac, 0xd0e5, 0x3030_5455);
        assert_eq!(edid_monitor_identity(&edid[..127]), None);
    }

    #[test]
    fn monitor_device_registry_path_derives_hwid_and_instance() {
        assert_eq!(
            monitor_device_registry_path(
                r"\\?\DISPLAY#DELA0E5#5&2c8b8a0&0&UID4352#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
            ),
            Some(
                r"SYSTEM\CurrentControlSet\Enum\DISPLAY\DELA0E5\5&2c8b8a0&0&UID4352\Device Parameters"
                    .into()
            )
        );
    }

    #[test]
    fn monitor_device_registry_path_rejects_an_injected_path_separator() {
        assert_eq!(
            monitor_device_registry_path(
                r"\\?\DISPLAY#DEL\A0E5#5&2c8b8a0&0&UID4352#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
            ),
            None
        );
    }

    #[test]
    fn monitor_device_registry_path_rejects_an_injected_parent_segment() {
        assert_eq!(
            monitor_device_registry_path(
                r"\\?\DISPLAY#DELA0E5#..#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
            ),
            None
        );
    }
}
