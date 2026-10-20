//! Builds the island's snapshot from the same views the window reads (sharing, clipboard and the
//! saved computer names), so both always describe one state.

use serde::Deserialize;
use serde_json::Value;

use super::model::{ComputerInput, Direction, IslandInput, Phase, Transfer, TransferKind};

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SharingFeed {
    paused: bool,
    editing: bool,
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
    drops: u64,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ClipboardFeed {
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
    /// None where clipboard sharing does not exist.
    pub clipboard: Option<Value>,
    /// The saved computer list (`dashboard.json`), or null when unreadable.
    pub computers: Value,
}

fn parse<T: for<'de> Deserialize<'de> + Default>(value: &Value) -> T {
    T::deserialize(value).unwrap_or_default()
}

/// What the window calls a computer it has no saved name for.
fn default_name(platform: Option<&str>) -> &'static str {
    match platform {
        Some("macos") => "Mac",
        _ => "Windows PC",
    }
}

pub fn snapshot(views: &Views, clipboard_notices: bool) -> IslandInput {
    let sharing: SharingFeed = parse(&views.sharing);
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
                Some(listed) if !listed.platform.is_empty() => Some(listed.platform.as_str()),
                _ => peer.and_then(|peer| peer.platform.as_deref()),
            };
            let name = listed
                .map(|listed| listed.name.trim())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| default_name(platform))
                .to_owned();
            ComputerInput {
                fingerprint: fingerprint.clone(),
                name,
                enabled: enabled.contains(fingerprint),
                phase: peer.map_or(Phase::Off, |peer| Phase::parse(&peer.phase)),
                held: peer.is_some_and(|peer| peer.held),
                drops: peer.map(|peer| peer.drops),
            }
        })
        .collect();
    let transfer = views.clipboard.as_ref().and_then(|value| {
        let last = parse::<ClipboardFeed>(value).last?;
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
    });
    IslandInput {
        computers,
        paused: sharing.paused,
        editing: sharing.editing,
        transfer,
        clipboard_notices,
    }
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
                "editing": true,
                "enabled": [PC.to_uppercase()],
                "peers": [{
                    "fingerprint": PC,
                    "platform": "windows",
                    "phase": "sharing",
                    "held": true,
                    "drops": 2,
                    "diagnostics": { "sentEvents": "4", "activeIsLocal": false }
                }],
                "localDisplays": []
            }),
            clipboard: Some(json!({
                "enabled": true, "access": "allowed", "notice": null, "peers": [],
                "last": { "direction": "received", "kind": "image", "bytes": 2048, "peer": PC, "ageSeconds": 3 }
            })),
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
        let input = snapshot(&views(), true);
        assert_eq!(input.computers.len(), 2);
        let pc = &input.computers[0];
        assert_eq!(
            (pc.fingerprint.as_str(), pc.name.as_str()),
            (PC, "Gaming PC")
        );
        assert!(pc.enabled && pc.phase == Phase::Sharing && pc.held);
        assert_eq!(pc.drops, Some(2));
        let mac = &input.computers[1];
        assert_eq!((mac.name.as_str(), mac.enabled), ("Studio", false));
        // Not in the sharing view, so its drops are unknown rather than none.
        assert_eq!((mac.phase, mac.drops), (Phase::Off, None));
        let last = input.transfer.unwrap();
        assert_eq!(
            (last.direction, last.kind, last.bytes),
            (Direction::Received, TransferKind::Image, 2048)
        );
        assert!(input.clipboard_notices && !input.paused && input.editing);
    }

    #[test]
    fn missing_names_and_unknown_values_fall_back_to_the_window_defaults() {
        let mut views = views();
        views.computers = Value::Null;
        views.clipboard = Some(json!({
            "enabled": false,
            "last": { "direction": "sideways", "kind": "image", "bytes": 1, "peer": PC, "ageSeconds": 0 }
        }));
        let input = snapshot(&views, false);
        assert_eq!(input.computers.len(), 1);
        assert_eq!(input.computers[0].name, "Windows PC");
        assert!(input.transfer.is_none() && !input.clipboard_notices);
        views.clipboard = None;
        assert!(snapshot(&views, true).transfer.is_none());
        views.sharing = Value::Null;
        assert!(snapshot(&views, true).computers.is_empty());
    }
}
