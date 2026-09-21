//! Inert, bounded sharing setup preferences. Loading never authorizes or starts a session.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use monhop_core::DeviceId;
use monhop_transport::{
    crypto::CertificateFingerprint,
    session_setup::{
        DisplayTopology, InspectedPeer, MAX_DISPLAY_NAME_BYTES, MAX_LOGICAL_ORIGIN_ABS,
        MAX_LOGICAL_SIZE, MAX_NATIVE_DIMENSION, MAX_SCALE_FACTOR, MIN_SCALE_FACTOR,
    },
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::sharing::{LayoutRequest, parse_display, parse_link, validated_layout};

/// One computer's saved setup record; also the wire format shared at Apply.
pub const SHARING_PREFERENCES_VERSION: u8 = 1;
/// The file holding every computer's record and which one is active.
pub const SETUP_FILE_VERSION: u8 = 2;
pub const MAX_SHARING_PREFERENCES_BYTES: u64 = 32 * 1024;
pub const MAX_SETUP_FILE_BYTES: u64 = 256 * 1024;
/// Paired computers this app keeps setups and names for.
pub const MAX_COMPUTERS: usize = 16;

const MAX_INTERFACE_ID_BYTES: usize = 512;
const MAX_LINKS: usize = 64;
const TEMPORARY_FILE_ATTEMPTS: usize = 64;

static NEXT_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SourcePlatform {
    Windows,
    Macos,
}

impl From<monhop_core::Platform> for SourcePlatform {
    fn from(platform: monhop_core::Platform) -> Self {
        match platform {
            monhop_core::Platform::Windows => Self::Windows,
            monhop_core::Platform::MacOs => Self::Macos,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisplaySnapshot {
    id: String,
    name: String,
    origin: [f64; 2],
    size: [f64; 2],
    native_size: [u32; 2],
    scale: f32,
    primary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    monitor: Option<String>,
}

impl DisplaySnapshot {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

/// The source computer is never stored by side or platform: both computers write identical
/// meaning, so it is recovered from the display list that owns the layout's starting display.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharingPreferences {
    version: u8,
    /// Written by earlier builds; accepted so their files still load, never written again.
    #[serde(default, skip_serializing)]
    source_platform: Option<SourcePlatform>,
    interface_id: String,
    local_fingerprint: String,
    peer_fingerprint: String,
    local_displays: Vec<DisplaySnapshot>,
    peer_displays: Vec<DisplaySnapshot>,
    layout: LayoutRequest,
    /// The sharing switch of version-1 files; read only to choose the active computer on migration.
    #[serde(default, skip_serializing)]
    sharing_enabled: bool,
}

/// Every paired computer's saved setup and which one MonHop shares input with. Loading never
/// authorizes or starts anything; a computer paired without a layout has no record here.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetupFile {
    version: u8,
    /// The physical network the last pairing or layout used; standing links reopen on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interface_id: Option<String>,
    /// Lowercase fingerprint of the active computer; may name a computer without a record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<String>,
    /// Keyed by the lowercase peer fingerprint of each record.
    #[serde(default)]
    computers: BTreeMap<String, SharingPreferences>,
}

impl Default for SetupFile {
    fn default() -> Self {
        Self {
            version: SETUP_FILE_VERSION,
            interface_id: None,
            active: None,
            computers: BTreeMap::new(),
        }
    }
}

/// Only the version is read first so a version-1 single-record file can still be migrated.
#[derive(Deserialize)]
struct VersionedFile {
    version: u8,
}

impl SetupFile {
    /// An absent file is empty; a version-1 file becomes one computer, active when its switch was on.
    pub fn load(path: &Path) -> io::Result<Self> {
        let Some(bytes) = read_bounded(path, MAX_SETUP_FILE_BYTES)? else {
            return Ok(Self::default());
        };
        let versioned: VersionedFile =
            serde_json::from_slice(&bytes).map_err(|_| invalid_data())?;
        let file = match versioned.version {
            SHARING_PREFERENCES_VERSION => {
                let mut record: SharingPreferences =
                    serde_json::from_slice(&bytes).map_err(|_| invalid_data())?;
                record.validate().map_err(|_| invalid_data())?;
                let mut file = Self::default();
                if std::mem::take(&mut record.sharing_enabled) {
                    file.active = Some(fingerprint_key(&record.peer_fingerprint));
                }
                file.insert(record);
                file
            }
            _ => serde_json::from_slice::<Self>(&bytes).map_err(|_| invalid_data())?,
        };
        file.validate().map_err(|_| invalid_data())?;
        Ok(file)
    }

    /// Writes the whole file atomically, creating the setup folder on first use.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.validate().map_err(|_| invalid_data())?;
        let bytes = serde_json::to_vec(self).map_err(|_| invalid_data())?;
        fs::create_dir_all(preference_parent(path))?;
        save_bounded(path, &bytes, MAX_SETUP_FILE_BYTES)
    }

    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    pub fn set_active(&mut self, fingerprint: Option<&CertificateFingerprint>) {
        self.active = fingerprint.map(|fingerprint| fingerprint_key(&fingerprint.full_hex()));
    }

    pub fn interface_id(&self) -> Option<&str> {
        self.interface_id.as_deref()
    }

    pub fn set_interface_id(&mut self, interface_id: &str) {
        self.interface_id = Some(interface_id.to_owned());
    }

    pub fn computer(&self, fingerprint: &str) -> Option<&SharingPreferences> {
        self.computers.get(&fingerprint_key(fingerprint))
    }

    pub fn active_computer(&self) -> Option<&SharingPreferences> {
        self.active
            .as_deref()
            .and_then(|active| self.computer(active))
    }

    /// Lowercase fingerprints of every computer with a saved record.
    pub fn fingerprints(&self) -> impl Iterator<Item = &str> {
        self.computers.keys().map(String::as_str)
    }

    /// Replaces that computer's record; the record's network becomes the file's network.
    pub fn insert(&mut self, setup: SharingPreferences) {
        self.interface_id = Some(setup.interface_id.clone());
        self.computers
            .insert(fingerprint_key(&setup.peer_fingerprint), setup);
    }

    /// Drops the record and, when it was the active computer, the active choice with it.
    pub fn remove(&mut self, fingerprint: &str) {
        let key = fingerprint_key(fingerprint);
        self.computers.remove(&key);
        if self.active.as_deref() == Some(key.as_str()) {
            self.active = None;
        }
    }

    fn validate(&self) -> Result<(), PreferenceError> {
        if self.version != SETUP_FILE_VERSION
            || self.computers.len() > MAX_COMPUTERS
            || self
                .interface_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > MAX_INTERFACE_ID_BYTES)
        {
            return Err(PreferenceError::Invalid);
        }
        if let Some(active) = &self.active {
            validate_fingerprint(&active.to_ascii_uppercase())?;
            if active.to_ascii_lowercase() != *active {
                return Err(PreferenceError::Invalid);
            }
        }
        for (key, setup) in &self.computers {
            setup.validate()?;
            if *key != fingerprint_key(&setup.peer_fingerprint) {
                return Err(PreferenceError::Invalid);
            }
        }
        Ok(())
    }
}

/// The lowercase form every view and file key uses; the input must already be a valid fingerprint.
pub(crate) fn fingerprint_key(fingerprint: &str) -> String {
    fingerprint.to_ascii_lowercase()
}

/// Accepts a fingerprint in any case from a command and returns its parsed form.
pub(crate) fn parse_fingerprint(value: &str) -> Result<CertificateFingerprint, String> {
    CertificateFingerprint::parse_full(value.trim())
        .map_err(|_| "That computer identity is not valid.".to_owned())
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SharedSetup {
    version: u8,
    sender_fingerprint: String,
    receiver_fingerprint: String,
    sender_displays: Vec<DisplaySnapshot>,
    receiver_displays: Vec<DisplaySnapshot>,
    layout: LayoutRequest,
}

pub(crate) fn shared_setup_bytes(
    inspection: &InspectedPeer,
    layout: LayoutRequest,
) -> Result<Vec<u8>, PreferenceError> {
    let saved = SharingPreferences::from_inspection(inspection, layout)?;
    let shared = SharedSetup {
        version: 1,
        sender_fingerprint: saved.local_fingerprint,
        receiver_fingerprint: saved.peer_fingerprint,
        sender_displays: saved.local_displays,
        receiver_displays: saved.peer_displays,
        layout: saved.layout,
    };
    let bytes = serde_json::to_vec(&shared).map_err(|_| PreferenceError::Invalid)?;
    if bytes.len() as u64 > MAX_SHARING_PREFERENCES_BYTES {
        return Err(PreferenceError::Invalid);
    }
    Ok(bytes)
}

pub(crate) fn shared_setup_for_inspection(
    fresh: &InspectedPeer,
    bytes: &[u8],
) -> Result<(InspectedPeer, SharingPreferences, LayoutRequest), PreferenceError> {
    if bytes.len() as u64 > MAX_SHARING_PREFERENCES_BYTES {
        return Err(PreferenceError::Invalid);
    }
    let shared: SharedSetup =
        serde_json::from_slice(bytes).map_err(|_| PreferenceError::Invalid)?;
    if shared.version != 1 {
        return Err(PreferenceError::Invalid);
    }
    let local = fresh.local_fingerprint.full_hex();
    let peer = fresh.peer_fingerprint.full_hex();
    let (local_displays, peer_displays) =
        if shared.sender_fingerprint == local && shared.receiver_fingerprint == peer {
            (&shared.sender_displays, &shared.receiver_displays)
        } else if shared.sender_fingerprint == peer && shared.receiver_fingerprint == local {
            (&shared.receiver_displays, &shared.sender_displays)
        } else {
            return Err(PreferenceError::InspectionChanged);
        };
    validate_displays(local_displays)?;
    validate_displays(peer_displays)?;
    if !same_display_geometry(local_displays, &snapshots(&fresh.local_displays))
        || !same_display_geometry(peer_displays, &snapshots(&fresh.peer_displays))
    {
        return Err(PreferenceError::InspectionChanged);
    }
    // Metadata negotiation has no input authority. Restore the reviewed source independently.
    let mut inspection = fresh.clone();
    inspection.source = source_device(&shared.layout, local_displays, peer_displays, fresh)
        .ok_or(PreferenceError::Invalid)?;
    let preferences = SharingPreferences::from_inspection(&inspection, shared.layout.clone())?;
    Ok((inspection, preferences, shared.layout))
}

/// The starting display decides which computer supplies input, on either side of the link.
fn source_device(
    layout: &LayoutRequest,
    local_displays: &[DisplaySnapshot],
    peer_displays: &[DisplaySnapshot],
    inspection: &InspectedPeer,
) -> Option<DeviceId> {
    let owns = |displays: &[DisplaySnapshot]| {
        displays
            .iter()
            .any(|display| display.id == layout.source_display)
    };
    match (owns(local_displays), owns(peer_displays)) {
        (true, false) => Some(inspection.local_device),
        (false, true) => Some(inspection.peer_device),
        _ => None,
    }
}

/// The displays both computers show while one of them is connected, so a card can draw what is
/// there now instead of what was there at the last Apply.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveDisplays {
    local_displays: Vec<DisplaySnapshot>,
    peer_displays: Vec<DisplaySnapshot>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSetupView {
    saved: bool,
    source_side: Option<&'static str>,
    local_displays: Vec<DisplaySnapshot>,
    peer_displays: Vec<DisplaySnapshot>,
    revision: String,
    layout: Option<LayoutRequest>,
    preview_layout: Option<LayoutRequest>,
    /// Null unless this computer is the one with a live link or session.
    live: Option<LiveDisplays>,
    message: &'static str,
}

impl SavedSetupView {
    /// `inspection` must be the live inspection of the same computer the record belongs to.
    pub fn from_saved(
        saved: Option<&SharingPreferences>,
        inspection: Option<&InspectedPeer>,
        revision: &str,
    ) -> Self {
        let layout = saved
            .zip(inspection)
            .and_then(|(saved, inspection)| saved.layout_for_inspection(inspection).ok());
        let message = if layout.is_some() {
            "Saved layout fits the connected displays."
        } else if saved.is_some() {
            "Layout saved. The displays are checked when this computer connects."
        } else {
            "No layout yet. Arrange the displays to start sharing."
        };
        Self {
            saved: saved.is_some(),
            source_side: saved.map(SharingPreferences::source_side),
            local_displays: saved
                .map(|saved| saved.local_displays().to_vec())
                .unwrap_or_default(),
            peer_displays: saved
                .map(|saved| saved.peer_displays().to_vec())
                .unwrap_or_default(),
            revision: revision.to_owned(),
            layout,
            preview_layout: saved.map(|saved| saved.layout.clone()),
            live: inspection.map(|inspection| LiveDisplays {
                local_displays: snapshots(&inspection.local_displays),
                peer_displays: snapshots(&inspection.peer_displays),
            }),
            message,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreferenceError {
    Invalid,
    InspectionChanged,
}

impl std::fmt::Display for PreferenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "The saved sharing setup is invalid.",
            Self::InspectionChanged => {
                "The saved sharing setup no longer matches the paired computers."
            }
        })
    }
}

impl std::error::Error for PreferenceError {}

impl SharingPreferences {
    pub fn from_inspection(
        inspection: &InspectedPeer,
        layout: LayoutRequest,
    ) -> Result<Self, PreferenceError> {
        let preferences = Self {
            version: SHARING_PREFERENCES_VERSION,
            source_platform: None,
            interface_id: inspection.interface_id.clone(),
            local_fingerprint: inspection.local_fingerprint.full_hex(),
            peer_fingerprint: inspection.peer_fingerprint.full_hex(),
            local_displays: snapshots(&inspection.local_displays),
            peer_displays: snapshots(&inspection.peer_displays),
            layout,
            sharing_enabled: false,
        };
        preferences.validate()?;
        preferences.validate_for_inspection(inspection)?;
        Ok(preferences)
    }

    pub fn interface_id(&self) -> &str {
        &self.interface_id
    }

    /// "local" or "peer" as seen from the computer that owns this file.
    pub fn source_side(&self) -> &'static str {
        if self
            .local_displays
            .iter()
            .any(|display| display.id == self.layout.source_display)
        {
            "local"
        } else {
            "peer"
        }
    }

    pub(crate) fn layout(&self) -> &LayoutRequest {
        &self.layout
    }

    pub(crate) fn peer_fingerprint(&self) -> &str {
        &self.peer_fingerprint
    }

    pub(crate) fn local_fingerprint(&self) -> &str {
        &self.local_fingerprint
    }

    /// Geometry-only comparison for a trial opened without a live link.
    pub(crate) fn matches_local_displays(&self, current: &DisplayTopology) -> bool {
        same_display_geometry(&self.local_displays, &snapshots(current))
    }

    pub fn local_displays(&self) -> &[DisplaySnapshot] {
        &self.local_displays
    }

    /// Names the edge the source pointer leaves for the first crossing, e.g. "the right edge of U2723QE".
    pub(crate) fn source_seam_hint(&self) -> Option<String> {
        if self.source_side() != "local" {
            return None;
        }
        self.layout.links.iter().find_map(|link| {
            let display = self
                .local_displays
                .iter()
                .find(|display| display.id == link.from_display)?;
            let name: String = display.name.chars().take(48).collect();
            Some(format!("the {} edge of {}", link.from_edge, name.trim()))
        })
    }

    pub fn peer_displays(&self) -> &[DisplaySnapshot] {
        &self.peer_displays
    }

    /// The same two computers, whatever their displays look like now.
    pub(crate) fn same_pair(&self, inspection: &InspectedPeer) -> bool {
        self.local_fingerprint == inspection.local_fingerprint.full_hex()
            && self.peer_fingerprint == inspection.peer_fingerprint.full_hex()
    }

    pub(crate) fn same_pair_as(&self, other: &Self) -> bool {
        self.local_fingerprint == other.local_fingerprint
            && self.peer_fingerprint == other.peer_fingerprint
    }

    /// The same monitors on each side, whatever their geometry, order, names, or OS ids.
    pub(crate) fn same_display_sets(&self, other: &Self) -> bool {
        same_displays(&self.local_displays, &other.local_displays).is_some()
            && same_displays(&self.peer_displays, &other.peer_displays).is_some()
    }

    /// The same monitors both computers show right now, whatever their geometry or OS ids.
    pub(crate) fn same_displays_as_inspection(&self, inspection: &InspectedPeer) -> bool {
        self.same_pair(inspection)
            && same_displays(&self.local_displays, &snapshots(&inspection.local_displays)).is_some()
            && same_displays(&self.peer_displays, &snapshots(&inspection.peer_displays)).is_some()
    }

    /// The same two computers showing the same displays, whichever side holds the input right now.
    pub(crate) fn fits_displays(&self, inspection: &InspectedPeer) -> bool {
        self.validate().is_ok()
            && self.same_pair(inspection)
            && same_display_geometry(&self.local_displays, &snapshots(&inspection.local_displays))
            && same_display_geometry(&self.peer_displays, &snapshots(&inspection.peer_displays))
    }

    /// This record with its display ids rewritten to the ids the same monitors carry now, when
    /// both computers show exactly the displays it was made with at the same geometry. An OS id
    /// is a per-connection number (a reconnected Mac display gets a new one), so a monitor keeps
    /// its layout through its EDID identity instead.
    pub(crate) fn remap_to_inspection(&self, inspection: &InspectedPeer) -> Option<Self> {
        if self.validate().is_err() || !self.same_pair(inspection) {
            return None;
        }
        let local = snapshots(&inspection.local_displays);
        let peer = snapshots(&inspection.peer_displays);
        let local_pairs = same_displays(&self.local_displays, &local)?;
        let peer_pairs = same_displays(&self.peer_displays, &peer)?;
        if !local_pairs
            .iter()
            .chain(&peer_pairs)
            .all(|(remembered, live)| same_geometry(remembered, live))
        {
            return None;
        }
        let layout = remap_layout(&self.layout, &id_map(local_pairs.iter().chain(&peer_pairs)));
        let mut inspected = inspection.clone();
        inspected.source = source_device(&layout, &local, &peer, inspection)?;
        Self::from_inspection(&inspected, layout).ok()
    }

    /// Like `remap_to_inspection` for this computer's displays alone; the other computer's are
    /// checked when the session connects.
    pub(crate) fn remap_to_local_displays(&self, current: &DisplayTopology) -> Option<Self> {
        if self.validate().is_err() {
            return None;
        }
        let local = snapshots(current);
        let pairs = same_displays(&self.local_displays, &local)?;
        if !pairs
            .iter()
            .all(|(remembered, live)| same_geometry(remembered, live))
        {
            return None;
        }
        let mut remapped = self.clone();
        remapped.layout = remap_layout(&self.layout, &id_map(pairs.iter()));
        remapped.local_displays = local;
        remapped.validate().ok()?;
        Some(remapped)
    }

    pub fn matches_inspection(&self, inspection: &InspectedPeer) -> bool {
        self.validate().is_ok()
            && source_device(
                &self.layout,
                &self.local_displays,
                &self.peer_displays,
                inspection,
            ) == Some(inspection.source)
            && self.interface_id == inspection.interface_id
            && self.local_fingerprint == inspection.local_fingerprint.full_hex()
            && self.peer_fingerprint == inspection.peer_fingerprint.full_hex()
            && same_display_geometry(&self.local_displays, &snapshots(&inspection.local_displays))
            && same_display_geometry(&self.peer_displays, &snapshots(&inspection.peer_displays))
    }

    pub fn layout_for_inspection(
        &self,
        inspection: &InspectedPeer,
    ) -> Result<LayoutRequest, PreferenceError> {
        self.validate_for_inspection(inspection)?;
        Ok(self.layout.clone())
    }

    fn validate_for_inspection(&self, inspection: &InspectedPeer) -> Result<(), PreferenceError> {
        if !self.matches_inspection(inspection) {
            return Err(PreferenceError::InspectionChanged);
        }
        validated_layout(inspection, &self.layout).map_err(|_| PreferenceError::Invalid)?;
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(), PreferenceError> {
        if self.version != SHARING_PREFERENCES_VERSION
            || self.interface_id.is_empty()
            || self.interface_id.len() > MAX_INTERFACE_ID_BYTES
        {
            return Err(PreferenceError::Invalid);
        }
        validate_fingerprint(&self.local_fingerprint)?;
        validate_fingerprint(&self.peer_fingerprint)?;
        validate_displays(&self.local_displays)?;
        validate_displays(&self.peer_displays)?;
        validate_layout(&self.layout, &self.local_displays, &self.peer_displays)
    }
}

/// The displays a display notice was raised for, so one change raises it once.
#[derive(Clone, Debug)]
pub(crate) struct DisplayGeometry {
    local: Vec<DisplaySnapshot>,
    peer: Vec<DisplaySnapshot>,
}

impl DisplayGeometry {
    pub(crate) fn of(inspection: &InspectedPeer) -> Self {
        Self {
            local: snapshots(&inspection.local_displays),
            peer: snapshots(&inspection.peer_displays),
        }
    }

    pub(crate) fn same(&self, other: &Self) -> bool {
        same_display_geometry(&self.local, &other.local)
            && same_display_geometry(&self.peer, &other.peer)
    }
}

/// Rebuilds an applied record for the displays connected now. A monitor is the same display
/// under a new OS id, so ids are rewritten first; then routes, positions, and hidden marks naming
/// a display that went away are dropped. A display that appeared joins a free arrangement beside
/// its computer's placed display at the OS's own offset when that spot is clear, and is left out
/// of the routes otherwise. None when the starting display went away or nothing valid is left.
pub(crate) fn adapt_to_inspection(
    record: &SharingPreferences,
    inspection: &InspectedPeer,
) -> Option<SharingPreferences> {
    if !record.same_pair(inspection) {
        return None;
    }
    let local = snapshots(&inspection.local_displays);
    let peer = snapshots(&inspection.peer_displays);
    let pairs: Vec<(&DisplaySnapshot, &DisplaySnapshot)> =
        pair_displays(&record.local_displays, &local)
            .into_iter()
            .chain(pair_displays(&record.peer_displays, &peer))
            .collect();
    let kept: BTreeSet<&str> = pairs.iter().map(|(_, live)| live.id.as_str()).collect();
    let present = |id: &str| kept.contains(id);
    let mut layout = remap_layout(&record.layout, &id_map(pairs.iter()));
    if !present(&layout.source_display) {
        return None;
    }
    layout
        .links
        .retain(|link| present(&link.from_display) && present(&link.to_display));
    let mut placed_appeared: Vec<String> = Vec::new();
    if let Some(arrangement) = layout.arrangement.as_mut() {
        arrangement
            .positions
            .retain(|position| present(&position.display));
        arrangement.hidden.retain(|id| present(id));
        if arrangement.mode == "free" {
            // Placed in id order, which is the same on both computers, so each display that
            // appeared lands (or is left out) identically on each side without a wire message.
            let mut appeared: Vec<(&[DisplaySnapshot], &DisplaySnapshot)> = [&local, &peer]
                .into_iter()
                .flat_map(|side| side.iter().map(move |display| (side.as_slice(), display)))
                .filter(|(_, display)| !present(&display.id))
                .collect();
            appeared.sort_by(|(_, a), (_, b)| a.id.cmp(&b.id));
            for (own, display) in appeared {
                match place_beside_sibling(arrangement, display, own, &local, &peer) {
                    Some((x, y)) => {
                        arrangement.positions.push(crate::sharing::DisplayPosition {
                            display: display.id.clone(),
                            x,
                            y,
                        });
                        placed_appeared.push(display.id.clone());
                    }
                    None => arrangement.hidden.push(display.id.clone()),
                }
            }
        }
    }
    let finish = |layout: LayoutRequest| {
        // The starting display decides which computer supplies input, as it does at Apply.
        let mut inspected = inspection.clone();
        inspected.source = source_device(&layout, &local, &peer, inspection)?;
        SharingPreferences::from_inspection(&inspected, layout).ok()
    };
    if placed_appeared.is_empty() {
        return finish(layout);
    }
    if let Some(adapted) = finish(layout.clone()) {
        return Some(adapted);
    }
    // A placed newcomer can still break the topology (an edge it shares with a kept crossing),
    // which the rectangle check cannot see; leaving it out is always as valid as before.
    if let Some(arrangement) = layout.arrangement.as_mut() {
        arrangement
            .positions
            .retain(|position| !placed_appeared.contains(&position.display));
        arrangement.hidden.extend(placed_appeared);
    }
    finish(layout)
}

/// A regular file of at most `limit` bytes, or None when absent; symlinks and oversize fail.
pub(crate) fn read_bounded(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_regular_file(&metadata)?;
    if metadata.len() > limit {
        return Err(invalid_data());
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid_data());
    }
    Ok(Some(bytes))
}

/// Reads, parses, and validates a bounded JSON file; an absent file is the default value.
pub(crate) fn load_bounded<T: DeserializeOwned + Default>(
    path: &Path,
    limit: u64,
    validate: impl FnOnce(&T) -> io::Result<()>,
) -> io::Result<T> {
    let Some(bytes) = read_bounded(path, limit)? else {
        return Ok(T::default());
    };
    let value: T = serde_json::from_slice(&bytes).map_err(|_| invalid_data())?;
    validate(&value)?;
    Ok(value)
}

// The setup file, the computer list, and the arrangement library share one bounded atomic write.
pub(crate) fn save_metadata(path: &Path, bytes: &[u8]) -> io::Result<()> {
    save_bounded(path, bytes, MAX_SHARING_PREFERENCES_BYTES)
}

/// Writes through a temporary file and a rename, so the target is whole or untouched.
pub(crate) fn save_bounded(path: &Path, bytes: &[u8], limit: u64) -> io::Result<()> {
    if bytes.len() as u64 > limit {
        return Err(invalid_data());
    }
    let parent = preference_parent(path);
    validate_parent(parent)?;
    validate_existing_target(path)?;

    let (temporary_path, mut temporary) = create_temporary_file(parent)?;
    let write_result = (|| -> io::Result<()> {
        temporary.write_all(bytes)?;
        temporary.sync_all()
    })();
    drop(temporary);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary_path, path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    Ok(())
}

fn same_display_geometry(left: &[DisplaySnapshot], right: &[DisplaySnapshot]) -> bool {
    left.len() == right.len()
        && left.iter().all(|display| {
            right.iter().any(|candidate| {
                display.id == candidate.id
                    && display.origin == candidate.origin
                    && display.size == candidate.size
                    && display.native_size == candidate.native_size
                    && display.scale == candidate.scale
                    && display.primary == candidate.primary
            })
        })
}

/// Everything but the id, name, and monitor identity: what a layout's crossings depend on.
fn same_geometry(left: &DisplaySnapshot, right: &DisplaySnapshot) -> bool {
    left.origin == right.origin
        && left.size == right.size
        && left.native_size == right.native_size
        && left.scale == right.scale
        && left.primary == right.primary
}

/// The monitor's EDID identity when this list reports it for exactly one display. Two identical
/// monitors without serial numbers share one, which tells them apart no better than their ids.
fn usable_monitor<'a>(display: &'a DisplaySnapshot, list: &[DisplaySnapshot]) -> Option<&'a str> {
    let key = display.monitor.as_deref()?;
    (list
        .iter()
        .filter(|other| other.monitor.as_deref() == Some(key))
        .count()
        == 1)
        .then_some(key)
}

/// Pairs remembered displays with the live displays that are the same monitor: by EDID identity
/// where both lists report one for exactly one display, otherwise by OS id. Two displays that
/// report different identities never pair, even when one has taken the other's id.
fn pair_displays<'a>(
    remembered: &'a [DisplaySnapshot],
    live: &'a [DisplaySnapshot],
) -> Vec<(&'a DisplaySnapshot, &'a DisplaySnapshot)> {
    let mut pairs: Vec<(&DisplaySnapshot, &DisplaySnapshot)> = Vec::new();
    let mut paired = vec![false; remembered.len()];
    for (index, display) in remembered.iter().enumerate() {
        let Some(key) = usable_monitor(display, remembered) else {
            continue;
        };
        if let Some(candidate) = live
            .iter()
            .find(|candidate| usable_monitor(candidate, live) == Some(key))
        {
            pairs.push((display, candidate));
            paired[index] = true;
        }
    }
    for (index, display) in remembered.iter().enumerate() {
        if paired[index] {
            continue;
        }
        let by_id = live.iter().find(|candidate| {
            candidate.id == display.id
                && !pairs.iter().any(|(_, taken)| taken.id == candidate.id)
                && (display.monitor.is_none()
                    || candidate.monitor.is_none()
                    || display.monitor == candidate.monitor)
        });
        if let Some(candidate) = by_id {
            pairs.push((display, candidate));
        }
    }
    pairs
}

/// Every display on both sides paired with the same monitor on the other, whatever the geometry.
fn same_displays<'a>(
    remembered: &'a [DisplaySnapshot],
    live: &'a [DisplaySnapshot],
) -> Option<Vec<(&'a DisplaySnapshot, &'a DisplaySnapshot)>> {
    let pairs = pair_displays(remembered, live);
    (remembered.len() == live.len() && pairs.len() == remembered.len()).then_some(pairs)
}

/// Remembered id to live id, for every paired display.
fn id_map<'a>(
    pairs: impl Iterator<Item = &'a (&'a DisplaySnapshot, &'a DisplaySnapshot)>,
) -> BTreeMap<String, String> {
    pairs
        .map(|(remembered, live)| (remembered.id.clone(), live.id.clone()))
        .collect()
}

/// The layout with every display id it names rewritten through `map`; unmapped ids stand.
fn remap_layout(layout: &LayoutRequest, map: &BTreeMap<String, String>) -> LayoutRequest {
    let rename = |id: &String| map.get(id).cloned().unwrap_or_else(|| id.clone());
    let mut remapped = layout.clone();
    remapped.source_display = rename(&layout.source_display);
    for link in &mut remapped.links {
        link.from_display = rename(&link.from_display);
        link.to_display = rename(&link.to_display);
    }
    if let Some(arrangement) = remapped.arrangement.as_mut() {
        for position in &mut arrangement.positions {
            position.display = rename(&position.display);
        }
        for hidden in &mut arrangement.hidden {
            *hidden = rename(hidden);
        }
    }
    remapped
}

/// Where a display that appeared goes in a free arrangement: beside its computer's primary (or
/// first placed) display, at the offset the OS puts between them, so the picture follows the OS
/// arrangement. None when nothing of that computer is placed or the spot overlaps a placed display.
fn place_beside_sibling(
    arrangement: &crate::sharing::ArrangementRequest,
    appeared: &DisplaySnapshot,
    own: &[DisplaySnapshot],
    local: &[DisplaySnapshot],
    peer: &[DisplaySnapshot],
) -> Option<(f64, f64)> {
    let placed = |display: &DisplaySnapshot| {
        arrangement
            .positions
            .iter()
            .find(|position| position.display == display.id)
    };
    let (sibling, anchor) = own
        .iter()
        .filter(|display| display.primary)
        .chain(own)
        .find_map(|display| placed(display).map(|position| (display, position)))?;
    let x = anchor.x + (appeared.origin[0] - sibling.origin[0]);
    let y = anchor.y + (appeared.origin[1] - sibling.origin[1]);
    if !x.is_finite()
        || !y.is_finite()
        || x.abs() > crate::sharing::MAX_ARRANGEMENT_COORDINATE
        || y.abs() > crate::sharing::MAX_ARRANGEMENT_COORDINATE
    {
        return None;
    }
    let overlaps = arrangement.positions.iter().any(|position| {
        local
            .iter()
            .chain(peer)
            .find(|display| display.id == position.display)
            .is_some_and(|display| {
                x < position.x + display.size[0]
                    && position.x < x + appeared.size[0]
                    && y < position.y + display.size[1]
                    && position.y < y + appeared.size[1]
            })
    });
    (!overlaps).then_some((x, y))
}

fn snapshots(topology: &DisplayTopology) -> Vec<DisplaySnapshot> {
    topology
        .displays()
        .iter()
        .map(|display| DisplaySnapshot {
            id: display.id.0.to_string(),
            name: display.name.clone(),
            origin: [display.logical_origin.x, display.logical_origin.y],
            size: [display.logical_size.x, display.logical_size.y],
            native_size: [display.native_width, display.native_height],
            scale: display.scale_factor,
            primary: display.is_primary,
            monitor: crate::sharing::monitor_key(display),
        })
        .collect()
}

fn validate_fingerprint(value: &str) -> Result<(), PreferenceError> {
    let fingerprint =
        CertificateFingerprint::parse_full(value).map_err(|_| PreferenceError::Invalid)?;
    if fingerprint.full_hex() == value {
        Ok(())
    } else {
        Err(PreferenceError::Invalid)
    }
}

fn validate_displays(displays: &[DisplaySnapshot]) -> Result<(), PreferenceError> {
    if displays.is_empty() || displays.len() > monhop_core::MAX_DISPLAYS {
        return Err(PreferenceError::Invalid);
    }
    let mut ids = BTreeSet::new();
    let mut primary_count = 0_u8;
    for display in displays {
        let id = parse_display(&display.id).map_err(|_| PreferenceError::Invalid)?;
        if id.0.to_string() != display.id
            || !ids.insert(id)
            || display.name.len() > MAX_DISPLAY_NAME_BYTES
            || display.native_size[0] == 0
            || display.native_size[1] == 0
            || display.native_size[0] > MAX_NATIVE_DIMENSION
            || display.native_size[1] > MAX_NATIVE_DIMENSION
            || !display.origin.iter().all(|value| value.is_finite())
            || !display.size.iter().all(|value| value.is_finite())
            || !display.scale.is_finite()
            || display.origin[0].abs() > MAX_LOGICAL_ORIGIN_ABS
            || display.origin[1].abs() > MAX_LOGICAL_ORIGIN_ABS
            || display.size[0] <= 0.0
            || display.size[1] <= 0.0
            || display.size[0] > MAX_LOGICAL_SIZE
            || display.size[1] > MAX_LOGICAL_SIZE
            || !(MIN_SCALE_FACTOR..=MAX_SCALE_FACTOR).contains(&display.scale)
            || !(display.origin[0] + display.size[0]).is_finite()
            || !(display.origin[1] + display.size[1]).is_finite()
        {
            return Err(PreferenceError::Invalid);
        }
        primary_count += u8::from(display.primary);
    }
    if primary_count == 1 {
        Ok(())
    } else {
        Err(PreferenceError::Invalid)
    }
}

fn validate_layout(
    layout: &LayoutRequest,
    local_displays: &[DisplaySnapshot],
    peer_displays: &[DisplaySnapshot],
) -> Result<(), PreferenceError> {
    if layout.links.len() > MAX_LINKS {
        return Err(PreferenceError::Invalid);
    }
    let source_display =
        parse_display(&layout.source_display).map_err(|_| PreferenceError::Invalid)?;
    if layout.source_display != source_display.0.to_string()
        || !local_displays
            .iter()
            .chain(peer_displays)
            .any(|display| display.id == layout.source_display)
        || layout.links.iter().any(|link| parse_link(link).is_err())
    {
        return Err(PreferenceError::Invalid);
    }
    let arranged: Vec<crate::sharing::ArrangedDisplay<'_>> = local_displays
        .iter()
        .map(|display| (display, true))
        .chain(peer_displays.iter().map(|display| (display, false)))
        .map(|(display, local)| crate::sharing::ArrangedDisplay {
            id: &display.id,
            local,
        })
        .collect();
    crate::sharing::validate_arrangement(layout, &arranged).map_err(|_| PreferenceError::Invalid)
}

fn preference_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn validate_parent(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_data());
    }
    Ok(())
}

fn validate_existing_target(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_regular_file(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_regular_file(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid_data());
    }
    Ok(())
}

fn create_temporary_file(parent: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMPORARY_FILE_ATTEMPTS {
        let sequence = NEXT_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".monhop-sharing-{}-{sequence}.tmp",
            std::process::id()
        ));
        let options = temporary_open_options();
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a sharing preferences temporary file",
    ))
}

fn temporary_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    options
}

fn invalid_data() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid sharing setup file")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use monhop_core::Platform;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "monhop-sharing-preferences-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("test directory must be created");
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_seam_hint_names_the_local_edge_only_when_this_computer_sends() {
        let receiver = preferences();
        assert_eq!(receiver.source_side(), "peer");
        assert_eq!(receiver.source_seam_hint(), None);
        let mut sender = preferences();
        sender.layout.source_display = "1".into();
        sender.layout.links = vec![crate::sharing::LinkRequest {
            from_display: "1".into(),
            from_edge: "right".into(),
            from_span: [0.0, 1.0],
            to_display: "2".into(),
            to_edge: "left".into(),
            to_span: [0.0, 1.0],
            hysteresis: 0.0,
        }];
        assert_eq!(
            sender.source_seam_hint().as_deref(),
            Some("the right edge of Local display")
        );
    }

    impl SharingPreferences {
        pub(crate) fn set_interface_id_for_test(&mut self, interface_id: &str) {
            self.interface_id = interface_id.to_owned();
        }

        /// Replaces this computer's displays with one 1920x1080 display per id, stacked so the
        /// side edges the fixture layout crosses on stay free.
        pub(crate) fn set_local_displays_for_test(&mut self, ids: &[&str]) {
            self.local_displays = ids
                .iter()
                .enumerate()
                .map(|(index, id)| DisplaySnapshot {
                    id: (*id).to_owned(),
                    name: format!("Display {id}"),
                    origin: [0.0, 1080.0 * index as f64],
                    size: [1920.0, 1080.0],
                    native_size: [1920, 1080],
                    scale: 1.0,
                    primary: index == 0,
                    monitor: None,
                })
                .collect();
        }

        pub(crate) fn set_local_display_names_for_test(&mut self, name: &str) {
            for display in &mut self.local_displays {
                display.name = name.to_owned();
            }
        }

        /// Gives this computer's displays, in order, the monitor identities `keys` name.
        pub(crate) fn set_local_display_monitors_for_test(&mut self, keys: &[&str]) {
            for (display, key) in self.local_displays.iter_mut().zip(keys) {
                display.monitor = Some((*key).to_owned());
            }
        }

        /// The display list alone with `from` renumbered, as the OS does at a reconnect; the
        /// layout is left naming the old id.
        pub(crate) fn set_local_display_id_for_test(&mut self, from: &str, to: &str) {
            for display in &mut self.local_displays {
                if display.id == from {
                    display.id = to.to_owned();
                }
            }
        }

        pub(crate) fn set_peer_fingerprint_for_test(&mut self, fingerprint_char: char) {
            self.peer_fingerprint = fingerprint_char.to_string().repeat(64);
        }

        pub(crate) fn move_local_display_for_test(&mut self, id: &str, origin: [f64; 2]) {
            for display in &mut self.local_displays {
                if display.id == id {
                    display.origin = origin;
                }
            }
        }
    }

    /// The identity `crate::sharing::monitor_key` writes, read back for a test inspection.
    fn monitor_from_key(key: &str) -> Option<monhop_core::MonitorIdentity> {
        let mut parts = key.split('-');
        let vendor = u16::from_str_radix(parts.next()?, 16).ok()?;
        let product = u16::from_str_radix(parts.next()?, 16).ok()?;
        let serial = u32::from_str_radix(parts.next()?, 16).ok()?;
        monhop_core::MonitorIdentity::new(vendor, product, serial)
    }

    pub(crate) const BUILT_IN: &str = "0610-a050-00000000";
    pub(crate) const EXTERNAL: &str = "10ac-4173-00000001";

    /// A Mac with its built-in display and one external monitor, crossed to from the other
    /// computer's display 2; the external is display 3 until the OS renumbers it.
    pub(crate) fn two_display_record() -> SharingPreferences {
        let mut record = preferences();
        record.set_local_displays_for_test(&["1", "3"]);
        record.set_local_display_monitors_for_test(&[BUILT_IN, EXTERNAL]);
        record.layout.links = vec![
            link("2", "left", "3", "right"),
            link("3", "right", "2", "left"),
        ];
        record.validate().expect("the fixture is a valid record");
        record
    }

    #[test]
    fn a_reconnected_monitor_with_a_new_id_fits_through_its_identity() {
        let record = two_display_record();
        let mut live = record.clone();
        live.set_local_display_id_for_test("3", "7");
        let inspected = inspection(&live);
        assert!(!record.fits_displays(&inspected));
        let fitted = record
            .remap_to_inspection(&inspected)
            .expect("the same monitors fit under new ids");
        assert!(fitted.fits_displays(&inspected));
        assert_eq!(fitted.layout().links[0].to_display, "7");
        assert_eq!(fitted.layout().links[1].from_display, "7");
        assert_eq!(fitted.layout().source_display, "2");
        assert!(record.same_display_sets(&fitted));
        assert!(record.same_displays_as_inspection(&inspected));
        let fitted_here = record
            .remap_to_local_displays(&inspected.local_displays)
            .expect("this computer's displays alone fit too");
        assert_eq!(fitted_here.layout(), fitted.layout());
        assert_eq!(fitted_here.local_displays()[1].id, "7");
        // Without identities, only the id can tell displays apart, as before.
        let mut anonymous = record.clone();
        for display in &mut anonymous.local_displays {
            display.monitor = None;
        }
        assert_eq!(anonymous.remap_to_inspection(&inspected), None);
        assert!(
            anonymous
                .remap_to_inspection(&inspection(&anonymous))
                .is_some()
        );
    }

    #[test]
    fn a_different_monitor_on_a_reused_id_is_a_new_display() {
        let record = two_display_record();
        let mut live = record.clone();
        live.local_displays[1].monitor = Some("04d9-0001-00000000".into());
        let inspected = inspection(&live);
        assert_eq!(record.remap_to_inspection(&inspected), None);
        assert!(!record.same_displays_as_inspection(&inspected));
        // The crossing led to the monitor that left; with none left, nothing can continue.
        assert_eq!(adapt_to_inspection(&record, &inspected), None);
    }

    #[test]
    fn identical_monitors_without_serials_are_told_apart_by_id() {
        let mut record = two_display_record();
        record.set_local_display_monitors_for_test(&[EXTERNAL, EXTERNAL]);
        assert!(record.remap_to_inspection(&inspection(&record)).is_some());
        let mut renumbered = record.clone();
        renumbered.set_local_display_id_for_test("3", "7");
        assert_eq!(record.remap_to_inspection(&inspection(&renumbered)), None);
    }

    #[test]
    fn a_duplicate_identity_never_pairs_with_a_different_monitor_on_its_id() {
        // Two identical monitors without serials, then the second swapped for another model
        // that took over its id: the crossing made for the old one must not carry onto it.
        let mut record = two_display_record();
        record.set_local_display_monitors_for_test(&[EXTERNAL, EXTERNAL]);
        let mut live = record.clone();
        live.local_displays[1].monitor = Some("04d9-0001-00000000".into());
        let inspected = inspection(&live);
        assert_eq!(record.remap_to_inspection(&inspected), None);
        assert_eq!(adapt_to_inspection(&record, &inspected), None);
        // The identical monitor that stayed keeps pairing by id.
        let mut still = record.clone();
        still.local_displays[1].monitor = None;
        assert!(record.remap_to_inspection(&inspection(&still)).is_some());
    }

    #[test]
    fn displays_that_appear_on_both_computers_are_placed_in_id_order() {
        let record = free_record((1920.0, 0.0));
        let mut changed = record.clone();
        // Display 9 appears under this computer's display 1; display 5 appears on the other
        // computer where its picture would overlap 9. Id order places 5 and hides 9, whichever
        // computer runs this.
        changed.set_local_displays_for_test(&["1", "9"]);
        changed.peer_displays.push(DisplaySnapshot {
            id: "5".into(),
            name: "Second peer display".into(),
            origin: [-1920.0, 1440.0],
            size: [1920.0, 1080.0],
            native_size: [1920, 1080],
            scale: 1.0,
            primary: false,
            monitor: None,
        });
        let adapted = adapt_to_inspection(&record, &inspection(&changed))
            .expect("a free layout survives displays that appeared on both computers");
        let (positions, hidden) = arrangement_of(&adapted);
        assert_eq!(hidden, vec!["9".to_owned()]);
        let placed: Vec<(&str, f64, f64)> = positions
            .iter()
            .map(|position| (position.display.as_str(), position.x, position.y))
            .collect();
        assert_eq!(
            placed,
            vec![("1", 0.0, 0.0), ("2", 1920.0, 0.0), ("5", 0.0, 1440.0)]
        );
    }

    #[test]
    fn a_placed_newcomer_that_breaks_the_topology_is_left_out_instead() {
        // The crossing leaves display 1's whole right edge, while the picture keeps the other
        // computer's display far right. Display 9 appears exactly right of 1: its spot is clear,
        // but its inherited seam would share that edge with the crossing.
        let record = free_record((5000.0, 0.0));
        let mut changed = record.clone();
        changed.set_local_displays_for_test(&["1", "9"]);
        changed.move_local_display_for_test("9", [1920.0, 0.0]);
        let adapted = adapt_to_inspection(&record, &inspection(&changed))
            .expect("the layout without the newcomer is as valid as before");
        let (positions, hidden) = arrangement_of(&adapted);
        assert_eq!(hidden, vec!["9".to_owned()]);
        assert_eq!(positions.len(), 2);
        assert!(adapted.fits_displays(&inspection(&changed)));
    }

    #[test]
    fn a_reconnected_monitor_that_moved_keeps_its_crossings() {
        let record = two_display_record();
        let mut live = record.clone();
        live.set_local_display_id_for_test("3", "7");
        live.move_local_display_for_test("7", [1920.0, 0.0]);
        let inspected = inspection(&live);
        assert_eq!(record.remap_to_inspection(&inspected), None);
        assert!(record.same_displays_as_inspection(&inspected));
        let adapted = adapt_to_inspection(&record, &inspected)
            .expect("a moved monitor keeps the crossings made for it");
        assert_eq!(adapted.layout().links.len(), 2);
        assert_eq!(adapted.layout().links[0].to_display, "7");
        assert_eq!(adapted.local_displays()[1].origin, [1920.0, 0.0]);
        assert!(adapted.fits_displays(&inspected));
    }

    fn link(from: &str, from_edge: &str, to: &str, to_edge: &str) -> crate::sharing::LinkRequest {
        crate::sharing::LinkRequest {
            from_display: from.into(),
            from_edge: from_edge.into(),
            from_span: [0.0, 1.0],
            to_display: to.into(),
            to_edge: to_edge.into(),
            to_span: [0.0, 1.0],
            hysteresis: 1.0,
        }
    }

    #[test]
    fn a_display_that_appeared_keeps_the_grouped_arrangement_going() {
        let record = preferences();
        let mut changed = record.clone();
        changed.set_local_displays_for_test(&["1", "3"]);
        let adapted = adapt_to_inspection(&record, &inspection(&changed))
            .expect("a grouped layout survives a display that appeared");
        assert_eq!(adapted.local_displays().len(), 2);
        assert_eq!(adapted.layout(), record.layout());
        assert!(adapted.fits_displays(&inspection(&changed)));
    }

    fn free_record(peer_position: (f64, f64)) -> SharingPreferences {
        let mut record = preferences();
        record.layout.arrangement = Some(crate::sharing::ArrangementRequest {
            mode: "free".into(),
            positions: vec![
                crate::sharing::DisplayPosition {
                    display: "1".into(),
                    x: 0.0,
                    y: 0.0,
                },
                crate::sharing::DisplayPosition {
                    display: "2".into(),
                    x: peer_position.0,
                    y: peer_position.1,
                },
            ],
            hidden: Vec::new(),
        });
        assert!(adapt_to_inspection(&record, &inspection(&record)).is_some());
        record
    }

    fn arrangement_of(
        adapted: &SharingPreferences,
    ) -> (Vec<crate::sharing::DisplayPosition>, Vec<String>) {
        let arrangement = adapted
            .layout()
            .arrangement
            .as_ref()
            .expect("the free arrangement survives");
        (arrangement.positions.clone(), arrangement.hidden.clone())
    }

    #[test]
    fn a_display_that_appeared_in_a_free_arrangement_is_placed_where_the_os_puts_it() {
        let record = free_record((1920.0, 0.0));
        let mut changed = record.clone();
        // The fixture stacks display 3 directly under display 1, as the OS reports it.
        changed.set_local_displays_for_test(&["1", "3"]);
        let adapted = adapt_to_inspection(&record, &inspection(&changed))
            .expect("a free layout survives a display that appeared");
        let (positions, hidden) = arrangement_of(&adapted);
        assert_eq!(hidden, Vec::<String>::new());
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[2].display, "3");
        assert_eq!((positions[2].x, positions[2].y), (0.0, 1080.0));
        assert!(adapted.fits_displays(&inspection(&changed)));
    }

    #[test]
    fn a_display_that_appeared_where_the_picture_is_taken_is_left_out_of_the_routes() {
        // The other computer's display was placed under display 1, where the OS puts display 3.
        let record = free_record((0.0, 1080.0));
        let mut changed = record.clone();
        changed.set_local_displays_for_test(&["1", "3"]);
        let adapted = adapt_to_inspection(&record, &inspection(&changed))
            .expect("a free layout survives a display that appeared");
        let (positions, hidden) = arrangement_of(&adapted);
        assert_eq!(hidden, vec!["3".to_owned()]);
        assert_eq!(positions.len(), 2);
    }

    #[test]
    fn a_crossing_display_that_went_away_stops_the_adaptation() {
        let mut record = preferences();
        record.set_local_displays_for_test(&["1", "3"]);
        record.layout.links = vec![
            link("2", "left", "3", "right"),
            link("3", "right", "2", "left"),
        ];
        assert!(adapt_to_inspection(&record, &inspection(&record)).is_some());
        let mut changed = record.clone();
        changed.set_local_displays_for_test(&["1"]);
        assert_eq!(adapt_to_inspection(&record, &inspection(&changed)), None);
    }

    #[test]
    fn a_starting_display_that_went_away_stops_the_adaptation() {
        let mut record = preferences();
        record.set_local_displays_for_test(&["1", "3"]);
        record.layout.source_display = "3".into();
        record.layout.links = vec![
            link("3", "right", "2", "left"),
            link("2", "left", "3", "right"),
        ];
        assert!(adapt_to_inspection(&record, &inspection(&record)).is_some());
        let mut changed = record.clone();
        changed.set_local_displays_for_test(&["1"]);
        assert_eq!(adapt_to_inspection(&record, &inspection(&changed)), None);
    }

    #[test]
    fn a_display_that_moved_refreshes_the_saved_geometry_and_keeps_sharing() {
        let record = preferences();
        let mut moved = record.clone();
        moved.local_displays[0].origin = [0.0, 240.0];
        let inspected = inspection(&moved);
        assert!(!record.fits_displays(&inspected));
        let adapted = adapt_to_inspection(&record, &inspected)
            .expect("a display that moved keeps the layout");
        assert_eq!(adapted.local_displays()[0].origin, [0.0, 240.0]);
        assert_eq!(adapted.layout(), record.layout());
        assert!(adapted.fits_displays(&inspected));
    }

    /// The same setup with the keyboard on this computer; `preferences()` puts it on the peer.
    pub(crate) fn preferences_with_local_source() -> SharingPreferences {
        let mut saved = preferences();
        saved.layout.source_display = "1".into();
        saved.validate().expect("the flipped fixture stays valid");
        saved
    }

    /// The same setup made with another paired computer.
    pub(crate) fn preferences_for_peer(fingerprint_char: char) -> SharingPreferences {
        let mut saved = preferences();
        saved.peer_fingerprint = fingerprint_char.to_string().repeat(64);
        saved
    }

    pub(crate) fn preferences() -> SharingPreferences {
        SharingPreferences {
            version: SHARING_PREFERENCES_VERSION,
            source_platform: None,
            interface_id: "en0:4:192.168.1.4".into(),
            local_fingerprint: "A".repeat(64),
            peer_fingerprint: "B".repeat(64),
            local_displays: vec![DisplaySnapshot {
                id: "1".into(),
                name: "Local display".into(),
                origin: [0.0, 0.0],
                size: [1920.0, 1080.0],
                native_size: [1920, 1080],
                scale: 1.0,
                primary: true,
                monitor: None,
            }],
            peer_displays: vec![DisplaySnapshot {
                id: "2".into(),
                name: "Peer display".into(),
                origin: [0.0, 0.0],
                size: [2560.0, 1440.0],
                native_size: [2560, 1440],
                scale: 1.0,
                primary: true,
                monitor: None,
            }],
            sharing_enabled: false,
            layout: LayoutRequest {
                source_display: "2".into(),
                links: vec![
                    crate::sharing::LinkRequest {
                        from_display: "1".into(),
                        from_edge: "right".into(),
                        from_span: [0.0, 1.0],
                        to_display: "2".into(),
                        to_edge: "left".into(),
                        to_span: [0.0, 1.0],
                        hysteresis: 1.0,
                    },
                    crate::sharing::LinkRequest {
                        from_display: "2".into(),
                        from_edge: "left".into(),
                        from_span: [0.0, 1.0],
                        to_display: "1".into(),
                        to_edge: "right".into(),
                        to_span: [0.0, 1.0],
                        hysteresis: 1.0,
                    },
                ],
                arrangement: None,
            },
        }
    }

    pub(crate) fn inspection(saved: &SharingPreferences) -> InspectedPeer {
        let topology = |displays: &[DisplaySnapshot]| {
            DisplayTopology::new(
                displays
                    .iter()
                    .map(
                        |display| monhop_transport::session_setup::DisplayDescription {
                            id: parse_display(&display.id).unwrap(),
                            name: display.name.clone(),
                            native_width: display.native_size[0],
                            native_height: display.native_size[1],
                            logical_origin: monhop_core::Point::new(
                                display.origin[0],
                                display.origin[1],
                            ),
                            logical_size: monhop_core::Point::new(display.size[0], display.size[1]),
                            scale_factor: display.scale,
                            is_primary: display.primary,
                            monitor: display.monitor.as_deref().and_then(monitor_from_key),
                        },
                    )
                    .collect(),
            )
            .unwrap()
        };
        InspectedPeer {
            local_device: monhop_core::DeviceId([1; 16]),
            peer_device: monhop_core::DeviceId([2; 16]),
            source: monhop_core::DeviceId([2; 16]),
            local_fingerprint: CertificateFingerprint::parse_full(&saved.local_fingerprint)
                .unwrap(),
            peer_fingerprint: CertificateFingerprint::parse_full(&saved.peer_fingerprint).unwrap(),
            local_platform: Platform::MacOs,
            peer_platform: Platform::Windows,
            local_displays: topology(&saved.local_displays),
            peer_displays: topology(&saved.peer_displays),
            interface_id: saved.interface_id.clone(),
        }
    }

    /// A file holding exactly `setup`, active.
    pub(crate) fn file_with(setup: SharingPreferences) -> SetupFile {
        let mut file = SetupFile::default();
        file.set_active(Some(
            &CertificateFingerprint::parse_full(&setup.peer_fingerprint).unwrap(),
        ));
        file.insert(setup);
        file
    }

    fn saved_computer(path: &Path) -> Option<SharingPreferences> {
        SetupFile::load(path)
            .unwrap()
            .computer(&"B".repeat(64))
            .cloned()
    }

    #[test]
    fn fractional_geometry_round_trip_preserves_exact_inspection_binding() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let mut saved = preferences();
        saved.peer_displays[0].size[1] = 1600.0 / 1.75;
        saved.peer_displays[0].scale = 1.75;
        let inspected = inspection(&saved);
        let expected =
            SharingPreferences::from_inspection(&inspected, saved.layout.clone()).unwrap();
        file_with(expected.clone()).save(&path).unwrap();
        let restored = saved_computer(&path).unwrap();
        assert_eq!(restored, expected);
        assert_eq!(restored.layout_for_inspection(&inspected), Ok(saved.layout));
    }

    #[test]
    fn the_file_keeps_one_record_per_computer_and_the_active_choice() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let mut file = file_with(preferences());
        file.insert(preferences_for_peer('C'));
        file.save(&path).unwrap();
        let loaded = SetupFile::load(&path).unwrap();
        assert_eq!(loaded, file);
        assert_eq!(loaded.active(), Some("b".repeat(64).as_str()));
        assert_eq!(loaded.interface_id(), Some("en0:4:192.168.1.4"));
        assert_eq!(loaded.fingerprints().count(), 2);
        assert_eq!(
            loaded.computer(&"c".repeat(64)),
            Some(&preferences_for_peer('C'))
        );
        assert_eq!(loaded.active_computer(), Some(&preferences()));
        let text = String::from_utf8(fs::read(&path).unwrap()).unwrap();
        assert!(text.contains("\"version\":2"));
        assert!(!text.contains("sharingEnabled"));
        assert!(text.contains(&format!("\"active\":\"{}\"", "b".repeat(64))));
        assert!(text.contains(&format!("\"{}\":{{", "b".repeat(64))));

        let mut file = loaded;
        file.remove(&"B".repeat(64));
        assert_eq!(file.active(), None);
        assert_eq!(file.fingerprints().count(), 1);
        file.remove(&"c".repeat(64));
        assert!(file.active_computer().is_none());
        file.save(&path).unwrap();
        assert_eq!(SetupFile::load(&path).unwrap(), file);
    }

    #[test]
    fn an_active_computer_may_have_no_record_yet() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let mut file = SetupFile::default();
        file.set_active(Some(
            &CertificateFingerprint::parse_full(&"D".repeat(64)).unwrap(),
        ));
        file.set_interface_id("en0:4:192.168.1.4");
        file.save(&path).unwrap();
        let loaded = SetupFile::load(&path).unwrap();
        assert_eq!(loaded.active(), Some("d".repeat(64).as_str()));
        assert!(loaded.active_computer().is_none());
        assert_eq!(loaded.interface_id(), Some("en0:4:192.168.1.4"));
    }

    #[test]
    fn a_version_one_file_becomes_one_computer_active_when_its_switch_was_on() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        for (enabled, active) in [(true, Some("b".repeat(64))), (false, None)] {
            let mut legacy = serde_json::to_value(preferences()).unwrap();
            legacy["sharingEnabled"] = serde_json::Value::Bool(enabled);
            write_raw(&path, &serde_json::to_vec(&legacy).unwrap());
            let loaded = SetupFile::load(&path).unwrap();
            assert_eq!(loaded.active(), active.as_deref());
            assert_eq!(loaded.computer(&"b".repeat(64)), Some(&preferences()));
            assert_eq!(loaded.interface_id(), Some("en0:4:192.168.1.4"));
            assert_eq!(
                fs::read(&path).unwrap(),
                serde_json::to_vec(&legacy).unwrap()
            );
        }
    }

    #[test]
    fn restored_layout_requires_same_identity_source_network_and_displays() {
        let saved = preferences();
        let current = inspection(&saved);
        assert_eq!(
            saved.layout_for_inspection(&current),
            Ok(saved.layout.clone())
        );
        let changes: [fn(&mut InspectedPeer); 7] = [
            |peer| {
                peer.local_fingerprint =
                    CertificateFingerprint::parse_full(&"C".repeat(64)).unwrap()
            },
            |peer| {
                peer.peer_fingerprint = CertificateFingerprint::parse_full(&"D".repeat(64)).unwrap()
            },
            |peer| peer.source = peer.local_device,
            |peer| peer.interface_id.push('x'),
            |peer| {
                let mut changed = preferences();
                changed.local_displays[0].size[0] += 1.0;
                peer.local_displays = inspection(&changed).local_displays;
            },
            |peer| {
                let mut changed = preferences();
                changed.peer_displays[0].origin[1] += 1.0;
                peer.peer_displays = inspection(&changed).peer_displays;
            },
            |peer| {
                let mut changed = preferences();
                changed.peer_displays[0].scale = 2.0;
                peer.peer_displays = inspection(&changed).peer_displays;
            },
        ];
        for change in changes {
            let mut changed = current.clone();
            change(&mut changed);
            assert_eq!(
                saved.layout_for_inspection(&changed),
                Err(PreferenceError::InspectionChanged)
            );
            let view = SavedSetupView::from_saved(Some(&saved), Some(&changed), "4");
            assert!(view.saved);
            assert!(view.layout.is_none());
        }
        let without_inspection = SavedSetupView::from_saved(Some(&saved), None, "5");
        assert!(without_inspection.layout.is_none());
        assert_eq!(
            without_inspection.preview_layout,
            Some(saved.layout.clone())
        );
    }

    #[test]
    fn display_names_do_not_invalidate_a_saved_layout() {
        let saved = preferences();
        let mut renamed = saved.clone();
        renamed.local_displays[0].name = "Built-in Retina Display".into();
        renamed.peer_displays[0].name = "U2723QE".into();
        let current = inspection(&renamed);
        assert!(saved.matches_inspection(&current));
        assert!(inspection(&saved).matches(&current));
        assert_eq!(
            saved.layout_for_inspection(&current),
            Ok(saved.layout.clone())
        );
    }

    #[test]
    fn saving_requires_a_crossing_and_a_route_back_to_the_source() {
        let saved = preferences();
        let current = inspection(&saved);
        let mut no_crossing = saved.layout.clone();
        no_crossing.links.clear();
        assert!(SharingPreferences::from_inspection(&current, no_crossing).is_err());
        let mut no_return = saved.layout.clone();
        no_return.links.remove(0);
        assert!(SharingPreferences::from_inspection(&current, no_return).is_err());
        let mut wrong_source = saved.layout.clone();
        wrong_source.source_display = "1".into();
        assert!(SharingPreferences::from_inspection(&current, wrong_source).is_err());
    }

    #[test]
    fn invalid_saved_file_is_not_automatically_repaired() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let corrupt = b"{unrecognized user setup}";
        write_raw(&path, corrupt);
        assert!(SetupFile::load(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
    }

    fn write_raw(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).expect("fixture file must be written");
    }

    #[test]
    fn round_trip_keeps_inert_setup_data() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let expected = preferences();
        file_with(expected.clone())
            .save(&path)
            .expect("preferences must save");
        assert_eq!(saved_computer(&path), Some(expected));
    }

    #[test]
    fn inserting_the_same_computer_replaces_its_complete_record() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let mut replacement = preferences();
        replacement.interface_id = "en1:7:192.168.1.5".into();
        let mut file = file_with(preferences());
        file.insert(replacement.clone());
        file.save(&path).unwrap();
        let loaded = SetupFile::load(&path).unwrap();
        assert_eq!(loaded.fingerprints().count(), 1);
        assert_eq!(loaded.computer(&"b".repeat(64)), Some(&replacement));
        assert_eq!(loaded.interface_id(), Some("en1:7:192.168.1.5"));
    }

    #[test]
    fn missing_file_is_inert() {
        let directory = TestDirectory::new();
        let loaded = SetupFile::load(&directory.path("missing.json")).unwrap();
        assert_eq!(loaded, SetupFile::default());
        assert!(loaded.active().is_none());
        assert!(!directory.path("missing.json").exists());
    }

    #[test]
    fn a_file_written_by_an_earlier_build_with_a_source_platform_still_loads() {
        let directory = TestDirectory::new();
        let path = directory.path("legacy.json");
        let mut legacy = serde_json::to_value(preferences()).unwrap();
        legacy.as_object_mut().unwrap().insert(
            "sourcePlatform".into(),
            serde_json::Value::String("windows".into()),
        );
        write_raw(&path, &serde_json::to_vec(&legacy).unwrap());
        let loaded = saved_computer(&path).unwrap();
        let written = serde_json::to_string(&loaded).unwrap();
        assert_eq!(written, serde_json::to_string(&preferences()).unwrap());
        assert!(!written.contains("sourcePlatform"));
    }

    #[test]
    fn corrupt_oversize_unknown_and_wrong_version_records_are_rejected() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        write_raw(&path, b"{");
        assert!(SetupFile::load(&path).is_err());

        write_raw(&path, &vec![b'x'; MAX_SETUP_FILE_BYTES as usize + 1]);
        assert!(SetupFile::load(&path).is_err());

        for (level, field) in [("file", "enabled"), ("record", "enabled")] {
            let mut unknown = serde_json::to_value(file_with(preferences())).unwrap();
            let target = if level == "file" {
                unknown.as_object_mut().unwrap()
            } else {
                unknown["computers"][&"b".repeat(64)]
                    .as_object_mut()
                    .unwrap()
            };
            target.insert(field.into(), serde_json::Value::Bool(false));
            write_raw(&path, &serde_json::to_vec(&unknown).unwrap());
            assert!(SetupFile::load(&path).is_err(), "{level}");
        }

        let mut wrong_version = serde_json::to_value(file_with(preferences())).unwrap();
        wrong_version["version"] = serde_json::json!(3);
        write_raw(&path, &serde_json::to_vec(&wrong_version).unwrap());
        assert!(SetupFile::load(&path).is_err());

        let mut wrong_key = serde_json::to_value(file_with(preferences())).unwrap();
        let record = wrong_key["computers"][&"b".repeat(64)].take();
        wrong_key["computers"][&"c".repeat(64)] = record;
        write_raw(&path, &serde_json::to_vec(&wrong_key).unwrap());
        assert!(SetupFile::load(&path).is_err());

        let mut upper_active = serde_json::to_value(file_with(preferences())).unwrap();
        upper_active["active"] = serde_json::json!("B".repeat(64));
        write_raw(&path, &serde_json::to_vec(&upper_active).unwrap());
        assert!(SetupFile::load(&path).is_err());
    }

    #[test]
    fn invalid_geometry_and_nonfinite_values_do_not_replace_a_valid_file() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let valid = preferences();
        file_with(valid.clone()).save(&path).unwrap();

        let mut nonfinite = valid.clone();
        nonfinite.local_displays[0].origin[0] = f64::NAN;
        assert!(file_with(nonfinite).save(&path).is_err());

        let mut invalid_geometry = valid.clone();
        invalid_geometry.peer_displays[0].native_size[0] = 0;
        assert!(file_with(invalid_geometry).save(&path).is_err());

        assert_eq!(saved_computer(&path), Some(valid));
    }

    #[test]
    fn display_and_layout_bounds_are_rejected() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let mut too_many_displays = preferences();
        for id in 3..=18 {
            too_many_displays.local_displays.push(DisplaySnapshot {
                id: id.to_string(),
                name: format!("Display {id}"),
                origin: [f64::from(id) * 100.0, 0.0],
                size: [100.0, 100.0],
                native_size: [100, 100],
                scale: 1.0,
                primary: false,
                monitor: None,
            });
        }
        assert!(file_with(too_many_displays).save(&path).is_err());

        let mut too_many_links = preferences();
        too_many_links.layout.links = vec![too_many_links.layout.links[0].clone(); MAX_LINKS + 1];
        assert!(file_with(too_many_links).save(&path).is_err());

        let mut too_many_computers = SetupFile::default();
        for index in 0..=MAX_COMPUTERS {
            let mut setup = preferences();
            setup.peer_fingerprint = format!("{index:064X}");
            too_many_computers.insert(setup);
        }
        assert!(too_many_computers.save(&path).is_err());
    }

    #[test]
    fn record_has_no_enabled_state() {
        let encoded = serde_json::to_string(&preferences()).unwrap();
        assert!(!encoded.contains("enabled"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_paths_are_rejected_without_touching_the_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let target = directory.path("target.json");
        let link = directory.path("link.json");
        let expected = preferences();
        file_with(expected.clone()).save(&target).unwrap();
        symlink(&target, &link).unwrap();
        assert!(SetupFile::load(&link).is_err());
        assert!(file_with(preferences()).save(&link).is_err());
        assert_eq!(
            SetupFile::load(&target).unwrap().computer(&"b".repeat(64)),
            Some(&expected)
        );
    }

    fn opposite(inspection: &InspectedPeer) -> InspectedPeer {
        InspectedPeer {
            local_device: inspection.peer_device,
            peer_device: inspection.local_device,
            source: inspection.source,
            local_fingerprint: inspection.peer_fingerprint,
            peer_fingerprint: inspection.local_fingerprint,
            local_platform: inspection.peer_platform,
            peer_platform: inspection.local_platform,
            local_displays: inspection.peer_displays.clone(),
            peer_displays: inspection.local_displays.clone(),
            interface_id: "windows-physical-interface".into(),
        }
    }

    #[test]
    fn shared_setup_maps_both_perspectives_without_importing_the_network() {
        let saved = preferences();
        let sender = inspection(&saved);
        let bytes = shared_setup_bytes(&sender, saved.layout.clone()).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("interfaceId"));
        assert!(!text.contains("enabled"));
        for current in [sender.clone(), opposite(&sender)] {
            let (fresh, restored, layout) = shared_setup_for_inspection(&current, &bytes).unwrap();
            assert_eq!(fresh.source, sender.source);
            assert_eq!(restored.interface_id, current.interface_id);
            assert_eq!(
                restored.local_fingerprint,
                current.local_fingerprint.full_hex()
            );
            assert_eq!(layout, saved.layout);
            assert!(restored.matches_inspection(&fresh));
        }
    }

    #[test]
    fn shared_source_is_independent_of_metadata_handshake_source() {
        let saved = preferences();
        let mut sender = inspection(&saved);
        sender.source = sender.local_device;
        let mut layout = saved.layout;
        layout.source_display = sender.local_displays.displays()[0].id.0.to_string();
        let bytes = shared_setup_bytes(&sender, layout.clone()).unwrap();
        let mut receiver = opposite(&sender);
        receiver.source = receiver.local_device;
        let (fresh, restored, received) = shared_setup_for_inspection(&receiver, &bytes).unwrap();
        assert_eq!(fresh.source, sender.local_device);
        assert_eq!(restored.source_side(), "peer");
        assert_eq!(received, layout);
    }

    #[test]
    fn shared_setup_rejects_stale_or_untrusted_metadata_before_saving() {
        let saved = preferences();
        let sender = inspection(&saved);
        let bytes = shared_setup_bytes(&sender, saved.layout).unwrap();
        let receiver = opposite(&sender);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for (field, bad) in [
            ("version", serde_json::json!(2)),
            ("senderFingerprint", serde_json::json!("00".repeat(32))),
            ("receiverFingerprint", serde_json::json!("00".repeat(32))),
            ("sourcePlatform", serde_json::json!("windows")),
            ("interfaceId", serde_json::json!("remote-interface")),
            ("enabled", serde_json::json!(true)),
        ] {
            let mut corrupted = value.clone();
            corrupted[field] = bad;
            assert!(
                shared_setup_for_inspection(&receiver, &serde_json::to_vec(&corrupted).unwrap())
                    .is_err(),
                "{field}"
            );
        }
        let mut stale = value.clone();
        stale["senderDisplays"][0]["origin"][0] = serde_json::json!(100.5);
        assert!(
            shared_setup_for_inspection(&receiver, &serde_json::to_vec(&stale).unwrap()).is_err()
        );
        let mut invalid = value;
        invalid["layout"]["links"] = serde_json::json!([]);
        assert!(
            shared_setup_for_inspection(&receiver, &serde_json::to_vec(&invalid).unwrap()).is_err()
        );
        let mut trailing = bytes;
        trailing.extend_from_slice(b"{}");
        assert!(shared_setup_for_inspection(&receiver, &trailing).is_err());
        assert!(
            shared_setup_for_inspection(
                &receiver,
                &vec![b' '; MAX_SHARING_PREFERENCES_BYTES as usize + 1]
            )
            .is_err()
        );
    }
}
