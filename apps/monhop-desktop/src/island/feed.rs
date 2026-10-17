//! Builds the island's snapshot from the same views the window reads (sharing, dimming,
//! clipboard, updates and the saved computer names), so both always describe one state.

use serde::Deserialize;
use serde_json::Value;

use super::model::{
    ClipboardInput, ComputerInput, DimmingInput, Direction, IslandInput, Phase, Platform, Transfer,
    TransferKind,
};

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SharingFeed {
    phase: String,
    sharing_active: bool,
    held: bool,
    paused: bool,
    enabled: Vec<String>,
    peers: Vec<PeerFeed>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PeerFeed {
    fingerprint: String,
    platform: Option<String>,
    phase: String,
    held: bool,
    diagnostics: DiagnosticsFeed,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct DiagnosticsFeed {
    active_is_local: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DimmingFeed {
    dimmed: bool,
    level: u8,
    min_level: u8,
    max_level: u8,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ClipboardFeed {
    enabled: bool,
    last: Option<LastFeed>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LastFeed {
    direction: String,
    kind: String,
    bytes: u64,
    peer: String,
    age_seconds: u64,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct UpdatesFeed {
    phase: String,
    available_version: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ListFeed {
    computers: Vec<ListedComputer>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ListedComputer {
    fingerprint: String,
    name: String,
    platform: String,
}

/// The serialized views, exactly as the window receives them.
pub struct Views {
    pub sharing: Value,
    pub dimming: Value,
    /// None where clipboard sharing does not exist.
    pub clipboard: Option<Value>,
    pub updates: Value,
    /// The saved computer list (`dashboard.json`), or null when unreadable.
    pub computers: Value,
}

fn parse<T: for<'de> Deserialize<'de> + Default>(value: &Value) -> T {
    T::deserialize(value).unwrap_or_default()
}

fn platform(name: Option<&str>) -> Platform {
    match name {
        Some("macos") => Platform::Mac,
        _ => Platform::Windows,
    }
}

/// What the window calls a computer it has no saved name for.
fn default_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Mac => "Mac",
        Platform::Windows => "Windows PC",
    }
}

pub fn snapshot(views: &Views) -> Option<IslandInput> {
    let sharing: SharingFeed = parse(&views.sharing);
    let dimming = DimmingFeed::deserialize(&views.dimming).ok()?;
    let updates: UpdatesFeed = parse(&views.updates);
    let list: ListFeed = parse(&views.computers);
    let enabled: Vec<String> = sharing
        .enabled
        .iter()
        .map(|fingerprint| fingerprint.to_ascii_lowercase())
        .collect();
    let peer = |fingerprint: &str| {
        sharing
            .peers
            .iter()
            .find(|peer| peer.fingerprint.eq_ignore_ascii_case(fingerprint))
    };
    let listed = |fingerprint: &str| {
        list.computers
            .iter()
            .find(|computer| computer.fingerprint.eq_ignore_ascii_case(fingerprint))
    };
    // Switched-on computers first, in the window's order, then the rest of the saved list.
    let mut order: Vec<String> = enabled.clone();
    for computer in &list.computers {
        let key = computer.fingerprint.to_ascii_lowercase();
        if !key.is_empty() && !order.contains(&key) {
            order.push(key);
        }
    }
    let computers = order
        .iter()
        .map(|fingerprint| {
            let peer = peer(fingerprint);
            let listed = listed(fingerprint);
            let platform = match listed {
                Some(listed) if !listed.platform.is_empty() => platform(Some(&listed.platform)),
                _ => platform(peer.and_then(|peer| peer.platform.as_deref())),
            };
            let name = listed
                .map(|listed| listed.name.trim())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| default_name(platform))
                .to_owned();
            let position = enabled.iter().position(|key| key == fingerprint);
            ComputerInput {
                fingerprint: fingerprint.clone(),
                name,
                platform,
                enabled: position.is_some(),
                phase: peer.map_or(Phase::Off, |peer| Phase::parse(&peer.phase)),
                held: peer.is_some_and(|peer| peer.held),
                pointer_here: peer.and_then(|peer| peer.diagnostics.active_is_local),
                tone: position.unwrap_or(0),
            }
        })
        .collect();
    let clipboard = views.clipboard.as_ref().map(|value| {
        let feed: ClipboardFeed = parse(value);
        ClipboardInput {
            enabled: feed.enabled,
            last: feed.last.and_then(|last| {
                Some(Transfer {
                    direction: match last.direction.as_str() {
                        "sent" => Direction::Sent,
                        "received" => Direction::Received,
                        _ => return None,
                    },
                    kind: match last.kind.as_str() {
                        "text" => TransferKind::Text,
                        "image" => TransferKind::Image,
                        _ => return None,
                    },
                    bytes: last.bytes,
                    peer: last.peer.to_ascii_lowercase(),
                    age_seconds: last.age_seconds,
                })
            }),
        }
    });
    Some(IslandInput {
        computers,
        paused: sharing.paused,
        phase: Phase::parse(&sharing.phase),
        sharing_active: sharing.sharing_active,
        held: sharing.held,
        dimming: DimmingInput {
            dimmed: dimming.dimmed,
            level: dimming.level,
            min_level: dimming.min_level,
            max_level: dimming.max_level,
        },
        clipboard,
        update_ready: (updates.phase == "ready")
            .then(|| updates.available_version.unwrap_or_default()),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PC: &str = "aaaa";
    const MAC: &str = "bbbb";

    fn views() -> Views {
        Views {
            sharing: json!({
                "phase": "sharing",
                "sharingActive": true,
                "held": false,
                "paused": false,
                "enabled": [PC.to_uppercase()],
                "peers": [{
                    "fingerprint": PC,
                    "platform": "windows",
                    "phase": "sharing",
                    "held": false,
                    "diagnostics": { "sentEvents": "4", "activeIsLocal": false }
                }],
                "localDisplays": []
            }),
            dimming: json!({
                "enabled": true, "level": 50, "minLevel": 10, "maxLevel": 99,
                "dimmed": true, "shortcut": "Control + Option + 0", "error": null
            }),
            clipboard: Some(json!({
                "enabled": true, "access": "allowed", "notice": null, "peers": [],
                "last": { "direction": "received", "kind": "image", "bytes": 2048, "peer": PC, "ageSeconds": 3 }
            })),
            updates: json!({ "phase": "ready", "availableVersion": "0.2.9", "automatic": true }),
            computers: json!({
                "version": 1,
                "computers": [
                    { "fingerprint": MAC.to_uppercase(), "name": "Studio", "platform": "macos", "address": null },
                    { "fingerprint": PC.to_uppercase(), "name": "Gaming PC", "platform": "windows", "address": null }
                ]
            }),
        }
    }

    #[test]
    fn the_window_views_become_one_snapshot() {
        let input = snapshot(&views()).unwrap();
        assert_eq!(input.computers.len(), 2);
        let pc = &input.computers[0];
        assert_eq!(
            (pc.fingerprint.as_str(), pc.name.as_str()),
            (PC, "Gaming PC")
        );
        assert!(pc.enabled && pc.phase == Phase::Sharing && pc.pointer_here == Some(false));
        assert_eq!(pc.platform, Platform::Windows);
        let mac = &input.computers[1];
        assert_eq!(
            (mac.name.as_str(), mac.platform, mac.enabled),
            ("Studio", Platform::Mac, false)
        );
        assert_eq!(mac.phase, Phase::Off);
        assert!(input.sharing_active && input.phase == Phase::Sharing);
        assert!(input.dimming.dimmed && input.dimming.level == 50);
        let last = input.clipboard.unwrap().last.unwrap();
        assert_eq!(
            (last.direction, last.kind, last.bytes),
            (Direction::Received, TransferKind::Image, 2048)
        );
        assert_eq!(input.update_ready.as_deref(), Some("0.2.9"));
    }

    #[test]
    fn missing_names_and_unknown_values_fall_back_to_the_window_defaults() {
        let mut views = views();
        views.computers = Value::Null;
        views.clipboard = Some(json!({
            "enabled": false,
            "last": { "direction": "sideways", "kind": "image", "bytes": 1, "peer": PC, "ageSeconds": 0 }
        }));
        views.updates = json!({ "phase": "downloading" });
        let input = snapshot(&views).unwrap();
        assert_eq!(input.computers.len(), 1);
        assert_eq!(input.computers[0].name, "Windows PC");
        let clipboard = input.clipboard.unwrap();
        assert!(!clipboard.enabled && clipboard.last.is_none());
        assert_eq!(input.update_ready, None);
        views.clipboard = None;
        assert!(snapshot(&views).unwrap().clipboard.is_none());
        views.dimming = Value::Null;
        assert!(snapshot(&views).is_none(), "no dimming view, no snapshot");
    }
}
