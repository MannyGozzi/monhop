//! The paired-computer list: names and addresses for presentation only. These records never
//! authenticate or authorize input; trust lives in OS-protected storage.

use std::{collections::BTreeSet, fs, io, net::SocketAddrV4, path::Path};

use monhop_core::Platform;
use monhop_transport::{crypto::CertificateFingerprint, session_setup::InspectedPeer};
use serde::{Deserialize, Serialize};

use crate::{
    pairing::PairedPeer,
    sharing_preferences::{
        MAX_COMPUTERS, SavedSetupView, SetupFile, SourcePlatform, fingerprint_key, load_bounded,
        save_metadata,
    },
};

const MAX_BYTES: u64 = 16 * 1024;
const MAX_NAME_CHARS: usize = 48;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Computer {
    /// Stored in the uppercase form the pairing exchange shows; views lowercase it.
    fingerprint: String,
    name: String,
    platform: SourcePlatform,
    address: Option<SocketAddrV4>,
}

/// The `dashboard.json` list, newest pairing first.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerList {
    version: u8,
    computers: Vec<Computer>,
}

impl Default for ComputerList {
    fn default() -> Self {
        Self {
            version: 1,
            computers: Vec::new(),
        }
    }
}

/// One paired computer as Home and Setup show it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerView {
    fingerprint: String,
    name: String,
    platform: SourcePlatform,
    address: Option<SocketAddrV4>,
    setup: SavedSetupView,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputersView {
    computers: Vec<ComputerView>,
    active: Option<String>,
    interface_id: Option<String>,
}

/// The displays the live link or session reports, valid for exactly one computer.
pub struct LiveInspection<'a> {
    pub fingerprint: &'a str,
    pub inspection: &'a InspectedPeer,
}

impl ComputersView {
    /// A computer with a trust record or a saved layout but no list entry (an older installation,
    /// or a list write that failed) is shown with a default name so it can be used or forgotten;
    /// nothing is written until the user renames or pairs.
    pub fn assemble(
        list: &ComputerList,
        trusted: &[PairedPeer],
        setup: &SetupFile,
        live: Option<LiveInspection<'_>>,
        revision: &str,
    ) -> Self {
        let mut computers = list.computers.clone();
        let mut seed = |fingerprint: String, address, platform| {
            let key = fingerprint_key(&fingerprint);
            if !computers
                .iter()
                .any(|computer| fingerprint_key(&computer.fingerprint) == key)
                && computers.len() < MAX_COMPUTERS
            {
                computers.push(Computer::new(fingerprint, address, platform));
            }
        };
        for peer in trusted {
            seed(
                peer.fingerprint.full_hex(),
                Some(peer.address),
                peer.platform,
            );
        }
        for fingerprint in setup.fingerprints() {
            seed(fingerprint.to_ascii_uppercase(), None, None);
        }
        let computers = computers
            .into_iter()
            .map(|computer| {
                let fingerprint = fingerprint_key(&computer.fingerprint);
                let inspection = live
                    .as_ref()
                    .filter(|live| live.fingerprint == fingerprint)
                    .map(|live| live.inspection);
                ComputerView {
                    setup: SavedSetupView::from_saved(
                        setup.computer(&fingerprint),
                        inspection,
                        revision,
                    ),
                    fingerprint,
                    name: computer.name,
                    platform: computer.platform,
                    address: computer.address,
                }
            })
            .collect();
        Self {
            computers,
            active: setup.active().map(str::to_owned),
            interface_id: setup.interface_id().map(str::to_owned),
        }
    }
}

impl ComputerList {
    /// An absent file is an empty list; a damaged one is an error rather than a silent reset.
    pub fn load(path: &Path) -> Result<Self, String> {
        read_list(path).map_err(|_| {
            "Saved computer names could not be read. Your pairing and display setup are unchanged."
                .to_owned()
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec(self)
            .map_err(|_| "Computer history could not be saved.".to_owned())?;
        let parent = path
            .parent()
            .ok_or_else(|| "The setup folder could not be located.".to_owned())?;
        fs::create_dir_all(parent)
            .and_then(|()| save_metadata(path, &bytes))
            .map_err(|_| {
                "Computer history could not be saved. Your pairing and display setup are unchanged."
                    .to_owned()
            })
    }

    #[cfg(test)]
    pub fn contains(&self, fingerprint: CertificateFingerprint) -> bool {
        self.computers
            .iter()
            .any(|computer| computer.fingerprint == fingerprint.full_hex())
    }

    /// Moves the computer to the front with its current address; a known one keeps its name.
    /// A full list refuses a new computer rather than dropping the oldest silently.
    pub fn remember(
        &mut self,
        fingerprint: CertificateFingerprint,
        address: SocketAddrV4,
        platform: Option<Platform>,
    ) -> Result<(), String> {
        let fingerprint = fingerprint.full_hex();
        let known = self
            .computers
            .iter()
            .find(|computer| computer.fingerprint == fingerprint)
            .cloned();
        if known.is_none() {
            self.require_room()?;
        }
        let mut computer =
            known.unwrap_or_else(|| Computer::new(fingerprint.clone(), Some(address), platform));
        computer.address = Some(address);
        if let Some(platform) = platform {
            computer.platform = platform.into();
        }
        self.computers
            .retain(|computer| computer.fingerprint != fingerprint);
        self.computers.insert(0, computer);
        Ok(())
    }

    pub fn require_room(&self) -> Result<(), String> {
        if self.computers.len() >= MAX_COMPUTERS {
            return Err(format!(
                "{MAX_COMPUTERS} computers are already paired. Forget one before pairing another."
            ));
        }
        Ok(())
    }

    pub fn rename(
        &mut self,
        fingerprint: CertificateFingerprint,
        name: &str,
    ) -> Result<(), String> {
        let name = clean_name(name)?;
        let computer = self
            .computers
            .iter_mut()
            .find(|computer| computer.fingerprint == fingerprint.full_hex())
            .ok_or_else(|| "That computer is not in your saved list.".to_owned())?;
        computer.name = name;
        Ok(())
    }

    pub fn forget(&mut self, fingerprint: CertificateFingerprint) {
        self.computers
            .retain(|computer| computer.fingerprint != fingerprint.full_hex());
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != 1 || self.computers.len() > MAX_COMPUTERS {
            return Err(invalid());
        }
        let mut seen = BTreeSet::new();
        for computer in &self.computers {
            valid_fingerprint(&computer.fingerprint)?;
            if !seen.insert(&computer.fingerprint)
                || clean_name(&computer.name).ok().as_ref() != Some(&computer.name)
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

impl Computer {
    fn new(fingerprint: String, address: Option<SocketAddrV4>, platform: Option<Platform>) -> Self {
        // Pairings saved before platforms were exchanged are always the other platform.
        let platform = platform.unwrap_or(if cfg!(target_os = "macos") {
            Platform::Windows
        } else {
            Platform::MacOs
        });
        let name = match platform {
            Platform::Windows => "Windows PC",
            Platform::MacOs => "Mac",
        };
        Self {
            fingerprint,
            name: name.into(),
            platform: platform.into(),
            address,
        }
    }
}

fn clean_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err("Use a computer name between 1 and 48 characters, without line breaks.".into());
    }
    Ok(name.to_owned())
}

fn valid_fingerprint(value: &str) -> io::Result<()> {
    let parsed = CertificateFingerprint::parse_full(value).map_err(|_| invalid())?;
    if parsed.full_hex() != value {
        return Err(invalid());
    }
    Ok(())
}

fn read_list(path: &Path) -> io::Result<ComputerList> {
    load_bounded(path, MAX_BYTES, ComputerList::validate)
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid computer list")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "monhop-computers-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self) -> std::path::PathBuf {
            self.0.join("dashboard.json")
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn fingerprint(letter: char) -> CertificateFingerprint {
        CertificateFingerprint::parse_full(&letter.to_string().repeat(64)).unwrap()
    }
    fn endpoint() -> SocketAddrV4 {
        "192.168.1.9:24872".parse().unwrap()
    }

    #[test]
    fn first_launch_does_not_create_metadata_or_any_authority() {
        let directory = Directory::new();
        let list = ComputerList::load(&directory.file()).unwrap();
        let view = ComputersView::assemble(&list, &[], &SetupFile::default(), None, "0");
        assert!(view.computers.is_empty());
        assert!(view.active.is_none());
        assert!(!directory.file().exists());
    }

    #[test]
    fn restart_retains_names_but_no_keys_or_enabled_state() {
        let directory = Directory::new();
        let path = directory.file();
        let mut list = ComputerList::load(&path).unwrap();
        list.remember(fingerprint('A'), endpoint(), None).unwrap();
        list.rename(fingerprint('A'), "  Desk PC  ").unwrap();
        list.save(&path).unwrap();
        let reloaded = ComputerList::load(&path).unwrap();
        assert_eq!(reloaded.computers[0].name, "Desk PC");
        assert!(reloaded.contains(fingerprint('A')));
        let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(value["computers"][0].as_object().unwrap().len(), 4);
    }

    #[test]
    fn the_view_lowercases_fingerprints_and_binds_the_live_displays_to_one_computer() {
        let saved = crate::sharing_preferences::tests::preferences();
        let inspection = crate::sharing_preferences::tests::inspection(&saved);
        let mut setup = crate::sharing_preferences::tests::file_with(saved);
        setup.insert(crate::sharing_preferences::tests::preferences_for_peer('C'));
        let mut list = ComputerList::default();
        list.remember(fingerprint('C'), endpoint(), Some(Platform::MacOs))
            .unwrap();
        list.rename(fingerprint('C'), "Studio Mac").unwrap();
        let live = LiveInspection {
            fingerprint: &"b".repeat(64),
            inspection: &inspection,
        };
        let view =
            serde_json::to_value(ComputersView::assemble(&list, &[], &setup, Some(live), "7"))
                .unwrap();
        assert_eq!(view["active"], "b".repeat(64));
        assert_eq!(view["interfaceId"], "en0:4:192.168.1.4");
        let computers = view["computers"].as_array().unwrap();
        assert_eq!(computers.len(), 2);
        assert_eq!(computers[0]["fingerprint"], "c".repeat(64));
        assert_eq!(computers[0]["name"], "Studio Mac");
        assert_eq!(computers[0]["platform"], "macos");
        assert_eq!(computers[0]["setup"]["saved"], true);
        assert!(computers[0]["setup"]["layout"].is_null());
        // The live displays belong to the one connected computer; every other card has none.
        assert!(computers[0]["setup"]["live"].is_null());
        assert_eq!(computers[1]["setup"]["live"]["localDisplays"][0]["id"], "1");
        assert_eq!(computers[1]["setup"]["live"]["peerDisplays"][0]["id"], "2");
        // The seeded legacy entry carries the other platform's default name on either host.
        let (legacy_name, legacy_platform) = if cfg!(target_os = "macos") {
            ("Windows PC", "windows")
        } else {
            ("Mac", "macos")
        };
        assert_eq!(computers[1]["fingerprint"], "b".repeat(64));
        assert_eq!(computers[1]["name"], legacy_name);
        assert_eq!(computers[1]["platform"], legacy_platform);
        assert_eq!(computers[1]["setup"]["layout"]["sourceDisplay"], "2");
        assert_eq!(computers[1]["setup"]["revision"], "7");
        assert!(view["computers"][0]["setup"]["peerFingerprint"].is_null());
    }

    #[test]
    fn corrupt_or_oversized_lists_are_preserved() {
        let directory = Directory::new();
        let path = directory.file();
        for bytes in [b"{corrupt".to_vec(), vec![b' '; MAX_BYTES as usize + 1]] {
            fs::write(&path, &bytes).unwrap();
            assert!(ComputerList::load(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn a_trust_record_without_a_list_entry_is_shown_with_its_address_and_platform() {
        let list = ComputerList::default();
        let trusted = [PairedPeer {
            fingerprint: fingerprint('D'),
            address: endpoint(),
            platform: Some(Platform::MacOs),
        }];
        let view = serde_json::to_value(ComputersView::assemble(
            &list,
            &trusted,
            &SetupFile::default(),
            None,
            "1",
        ))
        .unwrap();
        let computers = view["computers"].as_array().unwrap();
        assert_eq!(computers.len(), 1);
        assert_eq!(computers[0]["fingerprint"], "d".repeat(64));
        assert_eq!(computers[0]["name"], "Mac");
        assert_eq!(computers[0]["platform"], "macos");
        assert_eq!(computers[0]["address"], "192.168.1.9:24872");
        assert_eq!(computers[0]["setup"]["saved"], false);
    }

    #[test]
    fn a_full_list_refuses_a_new_computer_and_keeps_known_names() {
        let mut list = ComputerList::default();
        for n in 0..MAX_COMPUTERS {
            list.remember(
                CertificateFingerprint::parse_full(&format!("{n:064X}")).unwrap(),
                endpoint(),
                None,
            )
            .unwrap();
        }
        assert_eq!(list.computers.len(), MAX_COMPUTERS);
        let refused = list.remember(fingerprint('E'), endpoint(), None);
        assert_eq!(
            refused,
            Err("16 computers are already paired. Forget one before pairing another.".to_owned())
        );
        assert!(list.require_room().is_err());
        assert!(!list.contains(fingerprint('E')));
        let newest = CertificateFingerprint::parse_full(&list.computers[0].fingerprint).unwrap();
        list.rename(newest, "Office").unwrap();
        list.remember(newest, endpoint(), Some(Platform::MacOs))
            .unwrap();
        assert_eq!(list.computers[0].name, "Office");
        assert_eq!(list.computers[0].platform, SourcePlatform::Macos);
        assert_eq!(list.computers.len(), MAX_COMPUTERS);
        list.validate().unwrap();
        list.forget(newest);
        assert!(!list.contains(newest));
        assert!(list.require_room().is_ok());
        assert_eq!(list.computers.len(), MAX_COMPUTERS - 1);
    }

    #[test]
    fn rejects_invalid_names_unknown_computers_and_extra_authority_fields() {
        for name in ["", " \n", "Desk\nPC", &"a".repeat(49)] {
            assert!(clean_name(name).is_err());
        }
        assert!(
            ComputerList::default()
                .rename(fingerprint('A'), "Desk")
                .is_err()
        );
        assert!(
            serde_json::from_str::<ComputerList>(r#"{"version":1,"computers":[],"enabled":true}"#)
                .is_err()
        );
        assert!(valid_fingerprint(&"a".repeat(64)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_list_is_not_read_or_replaced() {
        let directory = Directory::new();
        let target = directory.0.join("target.json");
        fs::write(&target, b"{}").unwrap();
        std::os::unix::fs::symlink(&target, directory.file()).unwrap();
        assert!(ComputerList::load(&directory.file()).is_err());
        assert!(ComputerList::default().save(&directory.file()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"{}");
    }
}
