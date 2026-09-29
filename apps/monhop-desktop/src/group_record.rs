//! One saved layout for a group of computers, agreed by revision across every member.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

use monhop_core::{DeviceId, EdgeLink, MAX_DISPLAYS, Platform, Point, Topology};
use monhop_protocol::ControlPermissions;
use monhop_transport::{
    crypto::CertificateFingerprint,
    display_arrangement::{MemberBlock, group_topology},
    session_handshake::device_id_from_fingerprint,
    session_link::{MAX_LAYOUT_PAYLOAD_BYTES, MAX_SUMMARY_PAYLOAD_BYTES, payload_digest},
    session_setup::{DisplayTopology, InspectedPeer},
};
use serde::{Deserialize, Serialize};

use crate::sharing::{
    ArrangedDisplay, ArrangementRequest, DisplayPosition, LayoutRequest, parse_display, parse_link,
    validate_arrangement,
};
use crate::sharing_preferences::{
    ComputerPlatform, ControlMap, DisplaySnapshot, MAX_LINKS, PreferenceError,
    SHARING_PREFERENCES_VERSION, SharingPreferences, block_translation, fingerprint_key, id_map,
    pair_displays, remap_layout, same_display_geometry, same_displays, same_geometry, snapshots,
    topology_of, validate_displays, validate_fingerprint,
};

pub(crate) const GROUP_RECORD_VERSION: u8 = 1;
pub(crate) const MAX_GROUP_MEMBERS: usize = 8;
/// The layout one computer sends another over their link.
const SHARED_GROUP_VERSION: u8 = 3;
const RECORD_SUMMARY_VERSION: u8 = 1;
/// Room for the SharedGroup envelope, so every record that validates can also be sent.
const SHARED_GROUP_ENVELOPE_BYTES: usize = 256;
const MAX_RECORD_BYTES: usize = MAX_LAYOUT_PAYLOAD_BYTES - SHARED_GROUP_ENVELOPE_BYTES;
const OWN_SEAM: &str =
    "A crossing sits on an edge where one computer's own displays meet. Use a free edge.";

/// One layout for a set of computers. Members, displays and crossings are named by identity, so
/// every member reads the same meaning; where two copies meet, the greater `Stamp` wins.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupRecord {
    version: u8,
    /// Lamport revision, at least 1.
    revision: u64,
    /// The member that made this revision, as full uppercase hex.
    author: String,
    /// Strictly sorted by lowercase fingerprint.
    members: Vec<GroupMember>,
    /// Crossings between members, the shared picture, and each member's control entry.
    layout: LayoutRequest,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupMember {
    /// Full uppercase hex.
    fingerprint: String,
    platform: ComputerPlatform,
    displays: Vec<DisplaySnapshot>,
}

/// A record's place in the one order every member applies; the greater stamp wins. Revision
/// first, then the lower author DeviceId, then the lower content digest, so exchanging records
/// pairwise in any order converges on one content.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    revision: u64,
    author: DeviceId,
    content: [u8; 32],
}

impl Ord for Stamp {
    fn cmp(&self, other: &Self) -> Ordering {
        self.revision
            .cmp(&other.revision)
            .then_with(|| other.author.cmp(&self.author))
            .then_with(|| other.content.cmp(&self.content))
    }
}

impl PartialOrd for Stamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Stamp {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
}

/// The displays each computer shows as this computer knows them now: its own as read now and
/// each live peer's as that link reports them.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default)]
pub(crate) struct KnownDisplays {
    /// Keyed by lowercase fingerprint.
    members: BTreeMap<String, KnownMember>,
}

#[derive(Clone, Debug)]
struct KnownMember {
    platform: ComputerPlatform,
    displays: Vec<DisplaySnapshot>,
}

impl KnownDisplays {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn insert(
        &mut self,
        fingerprint: &str,
        platform: Platform,
        displays: &DisplayTopology,
    ) {
        self.members.insert(
            fingerprint_key(fingerprint),
            KnownMember {
                platform: platform.into(),
                displays: snapshots(displays),
            },
        );
    }

    /// Both ends of one link.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn of_link(inspection: &InspectedPeer) -> Self {
        let mut known = Self::default();
        known.insert(
            &inspection.local_fingerprint.full_hex(),
            inspection.local_platform,
            &inspection.local_displays,
        );
        known.insert(
            &inspection.peer_fingerprint.full_hex(),
            inspection.peer_platform,
            &inspection.peer_displays,
        );
        known
    }

    fn get(&self, member: &GroupMember) -> Option<&KnownMember> {
        self.members.get(&fingerprint_key(&member.fingerprint))
    }
}

/// A record rebuilt for the displays known now, and whether the rebuild left a crossing, a
/// position, or a display out against the record it was rebuilt from.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct AdaptedGroup {
    pub(crate) record: GroupRecord,
    pub(crate) left_out: bool,
}

/// The canonical form `content_digest` hashes.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Content {
    members: Vec<GroupMember>,
    layout: LayoutRequest,
}

impl GroupMember {
    /// `fingerprint` in any case.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        fingerprint: &str,
        platform: Platform,
        displays: &DisplayTopology,
    ) -> Result<Self, PreferenceError> {
        Ok(Self {
            fingerprint: canonical_fingerprint(fingerprint)?,
            platform: platform.into(),
            displays: snapshots(displays),
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn platform(&self) -> ComputerPlatform {
        self.platform
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn displays(&self) -> &[DisplaySnapshot] {
        &self.displays
    }
}

impl GroupRecord {
    /// A validated record; `members` in any order, `author` in any case.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        revision: u64,
        author: &str,
        mut members: Vec<GroupMember>,
        layout: LayoutRequest,
    ) -> Result<Self, PreferenceError> {
        members.sort_by_key(|member| fingerprint_key(&member.fingerprint));
        let record = Self {
            version: GROUP_RECORD_VERSION,
            revision,
            author: canonical_fingerprint(author)?,
            members,
            layout,
        };
        record.validate()?;
        Ok(record)
    }

    /// Today's pairwise record as a two-member group. Both computers' copies came from the same
    /// bytes, so they get the same content; the old decider's copy (lower DeviceId) is revision 2
    /// and the other's revision 1, so a pair whose copies drifted converges to the decider's.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn from_pairwise(
        record: &SharingPreferences,
        local_platform: Platform,
        peer_platform: Platform,
    ) -> Result<Self, PreferenceError> {
        let decides = record.local_decides();
        let author = if decides {
            &record.local_fingerprint
        } else {
            &record.peer_fingerprint
        };
        let member = |fingerprint: &str, platform: Platform, displays: &[DisplaySnapshot]| {
            Ok::<_, PreferenceError>(GroupMember {
                fingerprint: canonical_fingerprint(fingerprint)?,
                platform: platform.into(),
                displays: displays.to_vec(),
            })
        };
        Self::new(
            if decides { 2 } else { 1 },
            author,
            vec![
                member(
                    &record.local_fingerprint,
                    local_platform,
                    &record.local_displays,
                )?,
                member(
                    &record.peer_fingerprint,
                    peer_platform,
                    &record.peer_displays,
                )?,
            ],
            record.layout.clone(),
        )
    }

    /// The pairwise record `local` keeps for a two-member group; the network is the file's.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn to_pairwise(
        &self,
        local: &str,
        interface_id: &str,
    ) -> Result<SharingPreferences, PreferenceError> {
        let index = self.index_of(local).ok_or(PreferenceError::Invalid)?;
        if self.members.len() != 2 {
            return Err(PreferenceError::Invalid);
        }
        let (local, peer) = (&self.members[index], &self.members[1 - index]);
        let record = SharingPreferences {
            version: SHARING_PREFERENCES_VERSION,
            interface_id: interface_id.to_owned(),
            local_fingerprint: local.fingerprint.clone(),
            peer_fingerprint: peer.fingerprint.clone(),
            local_displays: local.displays.clone(),
            peer_displays: peer.displays.clone(),
            layout: self.layout.clone(),
        };
        record.validate()?;
        Ok(record)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn author(&self) -> &str {
        &self.author
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn members(&self) -> &[GroupMember] {
        &self.members
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn layout(&self) -> &LayoutRequest {
        &self.layout
    }

    /// `fingerprint` in any case.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn has_member(&self, fingerprint: &str) -> bool {
        self.index_of(fingerprint).is_some()
    }

    fn index_of(&self, fingerprint: &str) -> Option<usize> {
        let key = fingerprint_key(fingerprint);
        self.members
            .iter()
            .position(|member| fingerprint_key(&member.fingerprint) == key)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn validate(&self) -> Result<(), PreferenceError> {
        self.check().map_err(|_| PreferenceError::Invalid)
    }

    /// SharingPreferences::validate and validated_layout for any number of members, with the
    /// messages validated_layout gives. Members need not be joined into one connected picture.
    fn check(&self) -> Result<(), String> {
        if self.version != GROUP_RECORD_VERSION
            || self.revision == 0
            || !(2..=MAX_GROUP_MEMBERS).contains(&self.members.len())
            || !self
                .members
                .iter()
                .any(|member| member.fingerprint == self.author)
        {
            return Err(invalid());
        }
        let mut owners = BTreeMap::new();
        for (index, member) in self.members.iter().enumerate() {
            validate_fingerprint(&member.fingerprint).map_err(|_| invalid())?;
            validate_displays(&member.displays).map_err(|_| invalid())?;
            if index > 0
                && fingerprint_key(&self.members[index - 1].fingerprint)
                    >= fingerprint_key(&member.fingerprint)
            {
                return Err(invalid());
            }
            for display in &member.displays {
                if owners
                    .insert(parse_display(&display.id).map_err(|_| invalid())?, index)
                    .is_some()
                {
                    return Err(invalid());
                }
            }
        }
        if owners.len() > MAX_DISPLAYS {
            return Err(invalid());
        }
        let layout = &self.layout;
        if layout.links.len() > MAX_LINKS {
            return Err("The layout supports at most 64 directed edges.".into());
        }
        let links = layout
            .links
            .iter()
            .map(parse_link)
            .collect::<Result<Vec<_>, _>>()?;
        if links.is_empty() {
            return Err("Move the displays together until they touch.".into());
        }
        let ids: Vec<(&str, usize)> = self
            .members
            .iter()
            .enumerate()
            .flat_map(|(index, member)| {
                member
                    .displays
                    .iter()
                    .map(move |display| (display.id.as_str(), index))
            })
            .collect();
        // Run once per member so every member keeps at least one display in use.
        for index in 0..self.members.len() {
            let arranged: Vec<ArrangedDisplay<'_>> = ids
                .iter()
                .map(|(id, owner)| ArrangedDisplay {
                    id,
                    local: *owner == index,
                })
                .collect();
            validate_arrangement(layout, &arranged)?;
        }
        let names_members = layout.control.len() == self.members.len()
            && self.members.iter().all(|member| {
                layout
                    .control
                    .contains_key(&fingerprint_key(&member.fingerprint))
            });
        if !names_members || !layout.control.values().any(|allowed| *allowed) {
            return Err("Choose which computer can control the other.".into());
        }
        if links
            .iter()
            .any(|link| !links.iter().any(|other| reverses(link, other)))
        {
            return Err("Every crossing must work in both directions.".into());
        }
        if links.iter().enumerate().any(|(index, link)| {
            links[index + 1..].iter().any(|other| {
                other.from_display == link.from_display
                    && other.from_edge == link.from_edge
                    && other.from_span.start() < link.from_span.end()
                    && link.from_span.start() < other.from_span.end()
            })
        }) {
            return Err("Crossings on the same display edge must not overlap.".into());
        }
        if links.iter().any(|link| {
            match (owners.get(&link.from_display), owners.get(&link.to_display)) {
                (Some(from), Some(to)) => from == to,
                _ => true,
            }
        }) || (0..self.members.len()).any(|index| self.topology_at(index).is_none())
        {
            return Err(OWN_SEAM.into());
        }
        if serde_json::to_vec(self).map_err(|_| invalid())?.len() > MAX_RECORD_BYTES {
            return Err("The layout is too large to share with the other computers.".into());
        }
        Ok(())
    }

    /// SHA-256 of this canonical JSON: `{"members":[...],"layout":{...}}` with members sorted by
    /// lowercase fingerprint, each member's displays sorted by numeric id with every name blank,
    /// links sorted by their own JSON text, positions sorted by numeric display id, hidden ids
    /// sorted numerically, and the control map as is. Revision and author are left out, so two
    /// computers that made the same layout agree on it whoever stamped it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn content_digest(&self) -> [u8; 32] {
        let mut members = self.members.clone();
        members.sort_by_key(|member| fingerprint_key(&member.fingerprint));
        for member in &mut members {
            member
                .displays
                .sort_by(|left, right| display_order(&left.id, &right.id));
            for display in &mut member.displays {
                display.name.clear();
            }
        }
        let mut layout = self.layout.clone();
        let mut links: Vec<_> = layout
            .links
            .drain(..)
            .map(|link| (serde_json::to_string(&link).unwrap_or_default(), link))
            .collect();
        links.sort_by(|left, right| left.0.cmp(&right.0));
        layout.links = links.into_iter().map(|(_, link)| link).collect();
        if let Some(arrangement) = layout.arrangement.as_mut() {
            arrangement
                .positions
                .sort_by(|left, right| display_order(&left.display, &right.display));
            arrangement
                .hidden
                .sort_by(|left, right| display_order(left, right));
        }
        payload_digest(&serde_json::to_vec(&Content { members, layout }).unwrap_or_default())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn stamp(&self) -> Stamp {
        Stamp {
            revision: self.revision,
            // Only a record that failed validation has an author that does not parse.
            author: device_of(&self.author).unwrap_or(DeviceId([u8::MAX; 16])),
            content: self.content_digest(),
        }
    }

    /// The winner of two copies of a group's record. Equal stamps mean equal content, so the
    /// copies differ at most in display names or list order and this one is kept.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn merge(self, other: Self) -> Self {
        if other.stamp() > self.stamp() {
            other
        } else {
            self
        }
    }

    /// The same content stamped as a new local change ("touch").
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn restamped(&self, revision: u64, author: &str) -> Result<Self, PreferenceError> {
        let record = Self {
            revision,
            author: canonical_fingerprint(author)?,
            ..self.clone()
        };
        record.validate()?;
        Ok(record)
    }

    /// The whole group's pointer topology as `local` runs it: its own block at zero, every other
    /// member's translated by `member_offset`, offline members included.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn topology_for(&self, local: &str) -> Option<Topology> {
        self.topology_at(self.index_of(local)?)
    }

    fn topology_at(&self, local: usize) -> Option<Topology> {
        let displays = self
            .members
            .iter()
            .map(|member| topology_of(&member.displays))
            .collect::<Option<Vec<_>>>()?;
        let blocks = self
            .members
            .iter()
            .zip(&displays)
            .enumerate()
            .map(|(index, (member, displays))| {
                Some(MemberBlock {
                    device: device_of(&member.fingerprint)?,
                    platform: core_platform(member.platform),
                    displays,
                    offset: if index == local {
                        Point::default()
                    } else {
                        self.member_offset(local, index)
                    },
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let links = self
            .layout
            .links
            .iter()
            .map(parse_link)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        let hidden = self
            .layout
            .arrangement
            .iter()
            .flat_map(|arrangement| &arrangement.hidden)
            .map(|id| parse_display(id))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        group_topology(&blocks, links, &hidden).ok()
    }

    /// `member`'s block offset from `local`'s. Crossings are explicit, so it only translates
    /// coordinates; one the session cannot reproduce exactly on both computers becomes zero.
    fn member_offset(&self, local: usize, member: usize) -> Point {
        let (here, there) = (
            &self.members[local].displays,
            &self.members[member].displays,
        );
        let Some((Some(from), Some(to))) = self.layout.arrangement.as_ref().map(|arrangement| {
            (
                block_translation(arrangement, here),
                block_translation(arrangement, there),
            )
        }) else {
            return Point::default();
        };
        let offset = Point::new((to[0] - from[0]).round(), (to[1] - from[1]).round());
        let exact = |displays: &[DisplaySnapshot], sign: f64| {
            displays.iter().all(|display| {
                let [x, y] = display.origin;
                (x + sign * offset.x) - x == sign * offset.x
                    && (y + sign * offset.y) - y == sign * offset.y
            })
        };
        if offset.is_finite() && exact(there, 1.0) && exact(here, -1.0) {
            offset
        } else {
            Point::default()
        }
    }

    /// Both link ends are members showing exactly the displays and platform their entries hold;
    /// other members are not this link's to judge.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn fits_link(&self, inspection: &InspectedPeer) -> bool {
        let known = KnownDisplays::of_link(inspection);
        self.validate().is_ok()
            && [&inspection.local_fingerprint, &inspection.peer_fingerprint]
                .into_iter()
                .all(|fingerprint| self.has_member(&fingerprint.full_hex()))
            && self.changed_members(&known).is_empty()
    }

    /// Members `known` shows with other displays or another platform than their entries hold.
    fn changed_members<'k>(&self, known: &'k KnownDisplays) -> Vec<(usize, &'k KnownMember)> {
        self.members
            .iter()
            .enumerate()
            .filter_map(|(index, member)| {
                let live = known.get(member)?;
                (live.platform != member.platform
                    || !same_display_geometry(&member.displays, &live.displays))
                .then_some((index, live))
            })
            .collect()
    }

    /// This record with the ids of every known member's displays rewritten to the ids the same
    /// monitors carry now, when each known member shows exactly the displays its entry was made
    /// with at the same geometry. Members `known` does not show are left as they are.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn remap_to(&self, known: &KnownDisplays) -> Option<Self> {
        self.validate().ok()?;
        let mut members = self.members.clone();
        let mut pairs = Vec::new();
        for (index, member) in self.members.iter().enumerate() {
            let Some(live) = known.get(member) else {
                continue;
            };
            let matched = same_displays(&member.displays, &live.displays)?;
            if live.platform != member.platform
                || !matched
                    .iter()
                    .all(|(remembered, now)| same_geometry(remembered, now))
            {
                return None;
            }
            pairs.extend(matched);
            members[index].displays.clone_from(&live.displays);
        }
        let record = Self {
            members,
            layout: remap_layout(&self.layout, &id_map(pairs.iter())),
            ..self.clone()
        };
        record.validate().ok().map(|()| record)
    }

    /// The record rebuilt for the displays `known` shows, touching only the members whose
    /// displays changed: ids follow monitor identity, what names a gone display drops, each changed
    /// block keeps its place, and a newcomer breaking a crossing is hidden. Two links that know the
    /// same change to one member rebuild the same content. None if nothing valid survives.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn adapt(&self, known: &KnownDisplays) -> Option<AdaptedGroup> {
        let changed = self.changed_members(known);
        if changed.is_empty() {
            return self.validate().is_ok().then(|| AdaptedGroup {
                record: self.clone(),
                left_out: false,
            });
        }
        // Measured on the record's own ids and geometry, before either is rewritten.
        let translations: Vec<Option<[f64; 2]>> = changed
            .iter()
            .map(|(index, _)| {
                self.layout.arrangement.as_ref().and_then(|arrangement| {
                    block_translation(arrangement, &self.members[*index].displays)
                })
            })
            .collect();
        let pairs: Vec<(&DisplaySnapshot, &DisplaySnapshot)> = changed
            .iter()
            .flat_map(|(index, live)| pair_displays(&self.members[*index].displays, &live.displays))
            .collect();
        let unchanged = self
            .members
            .iter()
            .enumerate()
            .filter(|(index, _)| !changed.iter().any(|(other, _)| other == index))
            .flat_map(|(_, member)| &member.displays);
        let kept: BTreeSet<&str> = pairs
            .iter()
            .map(|(_, live)| live.id.as_str())
            .chain(unchanged.map(|display| display.id.as_str()))
            .collect();
        let present = |id: &str| kept.contains(id);
        let mut layout = remap_layout(&self.layout, &id_map(pairs.iter()));
        let crossings = layout.links.len();
        layout
            .links
            .retain(|link| present(&link.from_display) && present(&link.to_display));
        let mut left_out = layout.links.len() != crossings;
        let appeared: Vec<String> = changed
            .iter()
            .flat_map(|(_, live)| &live.displays)
            .filter(|display| !present(&display.id))
            .map(|display| display.id.clone())
            .collect();
        if let Some(arrangement) = layout.arrangement.as_mut() {
            let placed = arrangement.positions.len();
            arrangement
                .positions
                .retain(|position| present(&position.display));
            left_out |= arrangement.positions.len() != placed;
            arrangement.hidden.retain(|id| present(id));
            for ((_, live), translation) in changed.iter().zip(&translations) {
                let Some([dx, dy]) = translation else {
                    continue;
                };
                for display in &live.displays {
                    if arrangement.hidden.contains(&display.id) {
                        continue;
                    }
                    arrangement
                        .positions
                        .retain(|position| position.display != display.id);
                    arrangement.positions.push(DisplayPosition {
                        display: display.id.clone(),
                        x: display.origin[0] + dx,
                        y: display.origin[1] + dy,
                    });
                }
            }
        }
        let mut members = self.members.clone();
        for (index, live) in &changed {
            members[*index].platform = live.platform;
            members[*index].displays.clone_from(&live.displays);
        }
        let mut record = Self {
            members,
            layout,
            ..self.clone()
        };
        if record.validate().is_ok() {
            return Some(AdaptedGroup { record, left_out });
        }
        if appeared.is_empty() {
            return None;
        }
        // A newcomer can share an edge with a kept crossing; leaving it unused is as valid as before.
        let arrangement = record
            .layout
            .arrangement
            .get_or_insert_with(|| ArrangementRequest {
                positions: Vec::new(),
                hidden: Vec::new(),
            });
        arrangement
            .positions
            .retain(|position| !appeared.contains(&position.display));
        arrangement.hidden.extend(appeared);
        record.validate().ok().map(|()| AdaptedGroup {
            record,
            left_out: true,
        })
    }

    /// The record as the displays in `known` show it, to draw and never save: itself while it
    /// fits them, else `adapt`'s rebuild, else those displays with what named a changed member's
    /// old displays dropped.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn preview_for(&self, known: &KnownDisplays) -> Self {
        let changed = self.changed_members(known);
        if changed.is_empty() {
            return self.clone();
        }
        if let Some(adapted) = self.adapt(known) {
            return adapted.record;
        }
        let gone: BTreeSet<&str> = changed
            .iter()
            .flat_map(|(index, _)| &self.members[*index].displays)
            .map(|display| display.id.as_str())
            .collect();
        let mut preview = self.clone();
        for (index, live) in &changed {
            preview.members[*index].platform = live.platform;
            preview.members[*index].displays.clone_from(&live.displays);
        }
        preview.layout.links.retain(|link| {
            !gone.contains(link.from_display.as_str()) && !gone.contains(link.to_display.as_str())
        });
        if let Some(arrangement) = preview.layout.arrangement.as_mut() {
            arrangement
                .positions
                .retain(|position| !gone.contains(position.display.as_str()));
            arrangement.hidden.retain(|id| !gone.contains(id.as_str()));
        }
        preview
    }

    /// This record carrying the labels `known` shows for its displays. Labels are cosmetic, so
    /// the content and every fit stay as they are.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn relabeled(&self, known: &KnownDisplays) -> Self {
        let mut record = self.clone();
        for member in &mut record.members {
            let Some(live) = known.get(member) else {
                continue;
            };
            for display in &mut member.displays {
                if let Some(now) = live.displays.iter().find(|now| now.id == display.id) {
                    display.name.clone_from(&now.name);
                }
            }
        }
        record
    }

    /// The group without `member`: its displays, the crossings and positions that name them, and
    /// its control entry go, stamped as `author`'s local change at `revision`. None when what is
    /// left is not a valid record (one member, no crossing, or nobody left who may control).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn without_member(&self, member: &str, revision: u64, author: &str) -> Option<Self> {
        let index = self.index_of(member)?;
        let mut record = self.clone();
        let removed = record.members.remove(index);
        let gone: BTreeSet<&str> = removed
            .displays
            .iter()
            .map(|display| display.id.as_str())
            .collect();
        record.layout.links.retain(|link| {
            !gone.contains(link.from_display.as_str()) && !gone.contains(link.to_display.as_str())
        });
        record.layout.control.remove(&fingerprint_key(member));
        if let Some(arrangement) = record.layout.arrangement.as_mut() {
            arrangement
                .positions
                .retain(|position| !gone.contains(position.display.as_str()));
            arrangement.hidden.retain(|id| !gone.contains(id.as_str()));
        }
        record.restamped(revision, author).ok()
    }
}

/// The permissions a Share session between members `a` and `b` negotiates, keyed as today: the
/// lower DeviceId's entry is `lower_controls_higher` on both computers alike. None when neither
/// may control the other, since such a pair gets no Share connection.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn wire_control(control: &ControlMap, a: &str, b: &str) -> Option<ControlPermissions> {
    if fingerprint_key(a) == fingerprint_key(b) {
        return None;
    }
    let a_allowed = *control.get(&fingerprint_key(a))?;
    let b_allowed = *control.get(&fingerprint_key(b))?;
    let (lower, higher) = if device_of(a)? < device_of(b)? {
        (a_allowed, b_allowed)
    } else {
        (b_allowed, a_allowed)
    };
    (lower || higher).then_some(ControlPermissions {
        lower_controls_higher: lower,
        higher_controls_lower: higher,
    })
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SharedGroup {
    version: u8,
    sender_fingerprint: String,
    receiver_fingerprint: String,
    record: GroupRecord,
    /// The sender had to leave a crossing or a display out; neither computer remembers it.
    left_out: bool,
}

/// The payload proposing `record` over the link `inspection` describes, sent by its local end.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn shared_group_bytes(
    inspection: &InspectedPeer,
    record: &GroupRecord,
    left_out: bool,
) -> Result<Vec<u8>, PreferenceError> {
    record.validate()?;
    if !record.fits_link(inspection) {
        return Err(PreferenceError::InspectionChanged);
    }
    let shared = SharedGroup {
        version: SHARED_GROUP_VERSION,
        sender_fingerprint: inspection.local_fingerprint.full_hex(),
        receiver_fingerprint: inspection.peer_fingerprint.full_hex(),
        record: record.clone(),
        left_out,
    };
    let bytes = serde_json::to_vec(&shared).map_err(|_| PreferenceError::Invalid)?;
    if bytes.len() > MAX_LAYOUT_PAYLOAD_BYTES {
        return Err(PreferenceError::Invalid);
    }
    Ok(bytes)
}

/// The record a link peer proposed, and whether its sender left something out. Either end may have
/// sent it; both ends must be members whose entries match the displays and platforms the link
/// shows now, while other members' entries are only checked against the record's own bounds.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn shared_group_for_link(
    fresh: &InspectedPeer,
    bytes: &[u8],
) -> Result<(GroupRecord, bool), PreferenceError> {
    if bytes.len() > MAX_LAYOUT_PAYLOAD_BYTES {
        return Err(PreferenceError::Invalid);
    }
    let shared: SharedGroup =
        serde_json::from_slice(bytes).map_err(|_| PreferenceError::Invalid)?;
    if shared.version != SHARED_GROUP_VERSION {
        return Err(PreferenceError::Invalid);
    }
    let local = fresh.local_fingerprint.full_hex();
    let peer = fresh.peer_fingerprint.full_hex();
    let ends = (
        shared.sender_fingerprint.as_str(),
        shared.receiver_fingerprint.as_str(),
    );
    if ends != (local.as_str(), peer.as_str()) && ends != (peer.as_str(), local.as_str()) {
        return Err(PreferenceError::InspectionChanged);
    }
    shared.record.validate()?;
    if !shared.record.fits_link(fresh) {
        return Err(PreferenceError::InspectionChanged);
    }
    Ok((shared.record, shared.left_out))
}

/// What one computer tells each link peer about its active group: the members, and the stamp of
/// its record for them when it has one. Never a layout.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RecordSummary {
    version: u8,
    /// Sorted lowercase fingerprints.
    members: Vec<String>,
    record: Option<SummaryStamp>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SummaryStamp {
    revision: u64,
    /// Full uppercase hex, as the record names it.
    author: String,
    /// The content digest as lowercase hex.
    content: String,
}

impl RecordSummary {
    /// `members` in any case and order; `record`, when present, is the record for exactly them.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        members: &[&str],
        record: Option<&GroupRecord>,
    ) -> Result<Self, PreferenceError> {
        let mut keys: Vec<String> = members
            .iter()
            .map(|member| fingerprint_key(member))
            .collect();
        keys.sort();
        if record.is_some_and(|record| {
            !record
                .members
                .iter()
                .map(|member| fingerprint_key(&member.fingerprint))
                .eq(keys.iter().cloned())
        }) {
            return Err(PreferenceError::Invalid);
        }
        let summary = Self {
            version: RECORD_SUMMARY_VERSION,
            members: keys,
            record: record.map(|record| SummaryStamp {
                revision: record.revision,
                author: record.author.clone(),
                content: hex(&record.content_digest()),
            }),
        };
        summary.validate()?;
        Ok(summary)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>, PreferenceError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| PreferenceError::Invalid)?;
        if bytes.len() > MAX_SUMMARY_PAYLOAD_BYTES {
            return Err(PreferenceError::Invalid);
        }
        Ok(bytes)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, PreferenceError> {
        if bytes.is_empty() || bytes.len() > MAX_SUMMARY_PAYLOAD_BYTES {
            return Err(PreferenceError::Invalid);
        }
        let summary: Self = serde_json::from_slice(bytes).map_err(|_| PreferenceError::Invalid)?;
        summary.validate()?;
        Ok(summary)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn members(&self) -> &[String] {
        &self.members
    }

    /// None when the sender has no record for its group yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn stamp(&self) -> Option<Stamp> {
        let record = self.record.as_ref()?;
        Some(Stamp {
            revision: record.revision,
            author: device_of(&record.author)?,
            content: unhex(&record.content)?,
        })
    }

    fn validate(&self) -> Result<(), PreferenceError> {
        if self.version != RECORD_SUMMARY_VERSION
            || self.members.is_empty()
            || self.members.len() > MAX_GROUP_MEMBERS
            || self.members.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(PreferenceError::Invalid);
        }
        for member in &self.members {
            validate_fingerprint(&member.to_ascii_uppercase())?;
            if fingerprint_key(member) != *member {
                return Err(PreferenceError::Invalid);
            }
        }
        if let Some(record) = &self.record {
            validate_fingerprint(&record.author)?;
            if record.revision == 0
                || self.members.len() < 2
                || !self.members.contains(&fingerprint_key(&record.author))
                || unhex(&record.content).is_none()
            {
                return Err(PreferenceError::Invalid);
            }
        }
        Ok(())
    }
}

/// `other` crosses the same seam as `link`, the other way.
fn reverses(link: &EdgeLink, other: &EdgeLink) -> bool {
    other.from_display == link.to_display
        && other.from_edge == link.to_edge
        && other.from_span == link.to_span
        && other.to_display == link.from_display
        && other.to_edge == link.from_edge
        && other.to_span == link.from_span
}

fn invalid() -> String {
    PreferenceError::Invalid.to_string()
}

/// Numeric display ids first by value; anything else after, by text.
fn display_order(left: &str, right: &str) -> Ordering {
    let number = |id: &str| parse_display(id).ok().map(|id| id.0);
    number(left)
        .cmp(&number(right))
        .then_with(|| left.cmp(right))
}

fn canonical_fingerprint(value: &str) -> Result<String, PreferenceError> {
    CertificateFingerprint::parse_full(value)
        .map(|fingerprint| fingerprint.full_hex())
        .map_err(|_| PreferenceError::Invalid)
}

fn device_of(fingerprint: &str) -> Option<DeviceId> {
    CertificateFingerprint::parse_full(fingerprint)
        .ok()
        .map(device_id_from_fingerprint)
}

fn core_platform(platform: ComputerPlatform) -> Platform {
    match platform {
        ComputerPlatform::Windows => Platform::Windows,
        ComputerPlatform::Macos => Platform::MacOs,
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut text = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Exactly 64 lowercase hex digits.
fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sharing::{LinkRequest, validated_layout};
    use crate::sharing_preferences::tests::{
        BUILT_IN, EXTERNAL, inspection, opposite, preferences, preferences_for_peer,
        two_display_record,
    };
    use monhop_core::DisplayId;
    use monhop_transport::session_setup::MAX_DISPLAY_NAME_BYTES;

    fn fingerprint(owner: char) -> String {
        owner.to_string().repeat(64)
    }

    fn key(owner: char) -> String {
        fingerprint(owner).to_ascii_lowercase()
    }

    fn display(id: &str, origin: [f64; 2], size: [f64; 2], primary: bool) -> DisplaySnapshot {
        DisplaySnapshot {
            id: id.into(),
            name: format!("Display {id}"),
            origin,
            size,
            native_size: [size[0] as u32, size[1] as u32],
            scale: 1.0,
            primary,
            monitor: None,
        }
    }

    fn member(
        owner: char,
        platform: ComputerPlatform,
        displays: Vec<DisplaySnapshot>,
    ) -> GroupMember {
        GroupMember {
            fingerprint: fingerprint(owner),
            platform,
            displays,
        }
    }

    /// A crossing between parts of two edges, in both directions.
    fn spanned(
        from: &str,
        from_edge: &str,
        from_span: [f64; 2],
        to: &str,
        to_edge: &str,
        to_span: [f64; 2],
    ) -> [LinkRequest; 2] {
        let link = |from: &str, from_edge: &str, from_span, to: &str, to_edge: &str, to_span| {
            LinkRequest {
                from_display: from.into(),
                from_edge: from_edge.into(),
                from_span,
                to_display: to.into(),
                to_edge: to_edge.into(),
                to_span,
                hysteresis: 1.0,
            }
        };
        [
            link(from, from_edge, from_span, to, to_edge, to_span),
            link(to, to_edge, to_span, from, from_edge, from_span),
        ]
    }

    fn crossing(from: &str, from_edge: &str, to: &str, to_edge: &str) -> [LinkRequest; 2] {
        spanned(from, from_edge, [0.0, 1.0], to, to_edge, [0.0, 1.0])
    }

    fn position(display: &str, x: f64, y: f64) -> DisplayPosition {
        DisplayPosition {
            display: display.into(),
            x,
            y,
        }
    }

    fn everyone(owners: &[char]) -> ControlMap {
        owners.iter().map(|owner| (key(*owner), true)).collect()
    }

    /// A Mac (display 1), a PC (2), and a PC with two side-by-side displays (3, 4), pictured as
    /// C | A | B, with crossings A-B, C-A and B-C.
    fn trio() -> GroupRecord {
        GroupRecord::new(
            1,
            &fingerprint('A'),
            vec![
                member(
                    'C',
                    ComputerPlatform::Windows,
                    vec![
                        display("3", [0.0, 0.0], [1920.0, 1080.0], true),
                        display("4", [1920.0, 0.0], [1920.0, 1080.0], false),
                    ],
                ),
                member(
                    'A',
                    ComputerPlatform::Macos,
                    vec![display("1", [0.0, 0.0], [1920.0, 1080.0], true)],
                ),
                member(
                    'B',
                    ComputerPlatform::Windows,
                    vec![display("2", [0.0, 0.0], [2560.0, 1440.0], true)],
                ),
            ],
            LayoutRequest {
                links: [
                    crossing("1", "right", "2", "left"),
                    crossing("4", "right", "1", "left"),
                    crossing("2", "bottom", "3", "top"),
                ]
                .concat(),
                arrangement: Some(ArrangementRequest {
                    positions: vec![
                        position("1", 0.0, 0.0),
                        position("2", 1920.0, 0.0),
                        position("3", -3840.0, 0.0),
                        position("4", -1920.0, 0.0),
                    ],
                    hidden: Vec::new(),
                }),
                control: everyone(&['A', 'B', 'C']),
            },
        )
        .expect("the fixture is a valid record")
    }

    /// `count` computers with one display each, every neighbouring pair crossing right to left.
    /// Built without validation so a count past the bound can be checked.
    fn ring(count: usize) -> GroupRecord {
        let owners: Vec<char> = "123456789".chars().take(count).collect();
        GroupRecord {
            version: GROUP_RECORD_VERSION,
            revision: 1,
            author: fingerprint(owners[0]),
            members: owners
                .iter()
                .enumerate()
                .map(|(index, owner)| {
                    let id = (index + 1).to_string();
                    member(
                        *owner,
                        ComputerPlatform::Windows,
                        vec![display(&id, [0.0, 0.0], [1920.0, 1080.0], true)],
                    )
                })
                .collect(),
            layout: LayoutRequest {
                links: (1..count)
                    .flat_map(|index| {
                        crossing(
                            &index.to_string(),
                            "right",
                            &(index + 1).to_string(),
                            "left",
                        )
                    })
                    .collect(),
                arrangement: None,
                control: everyone(&owners),
            },
        }
    }

    /// Every count at its bound, with the widest ids, numbers and names: eight members, sixteen
    /// displays, sixty-four crossings, every display placed.
    fn maximal_record(name: &str) -> GroupRecord {
        let owners: Vec<char> = "01234567".chars().collect();
        let id = |index: usize| (u64::MAX - index as u64).to_string();
        let members = owners
            .iter()
            .enumerate()
            .map(|(index, owner)| {
                let displays = [0, 1].map(|second| DisplaySnapshot {
                    id: id(2 * index + second),
                    name: name.to_owned(),
                    origin: [
                        -999_999_999.123_456_7,
                        if second == 0 {
                            -999_999_999.123_456_7
                        } else {
                            0.123_456_789_012_345_67
                        },
                    ],
                    size: [999_999.123_456_789_1, 999_999.123_456_789_1],
                    native_size: [65_535, 65_535],
                    scale: 15.999_999,
                    primary: second == 0,
                    monitor: Some(format!("ffff-ffff-{:08x}", 2 * index + second)),
                });
                member(*owner, ComputerPlatform::Windows, displays.to_vec())
            })
            .collect();
        let first = [0.012_345_678_901_234_568, 0.498_765_432_109_876_54];
        let second = [0.501_234_567_890_123_4, 0.987_654_321_098_765_4];
        // Each display's right edge leads to the next member's display over its first half and to
        // the member after that over its second half.
        let links = (0..32)
            .flat_map(|link| {
                let from = link % 16;
                let (to, span) = if link < 16 {
                    ((from + 2) % 16, first)
                } else {
                    ((from + 4) % 16, second)
                };
                spanned(&id(from), "right", span, &id(to), "left", span).map(|mut crossing| {
                    crossing.hysteresis = 1.234_567_890_123_456_7;
                    crossing
                })
            })
            .collect();
        let positions = (0..16)
            .map(|index| {
                position(
                    &id(index),
                    -19_999_999.123_456_78 + index as f64,
                    -19_999_999.123_456_78,
                )
            })
            .collect();
        GroupRecord::new(
            u64::MAX,
            &fingerprint('7'),
            members,
            LayoutRequest {
                links,
                arrangement: Some(ArrangementRequest {
                    positions,
                    hidden: Vec::new(),
                }),
                control: owners
                    .iter()
                    .map(|owner| (key(*owner), *owner == '0'))
                    .collect(),
            },
        )
        .expect("the maximal record is valid")
    }

    fn entry(record: &GroupRecord, owner: char) -> &GroupMember {
        &record.members()[record
            .index_of(&fingerprint(owner))
            .expect("a member of the record")]
    }

    /// The link between two members as `local` inspects it, showing exactly their entries.
    fn link_between(record: &GroupRecord, local: char, peer: char) -> InspectedPeer {
        let parsed = |owner: char| CertificateFingerprint::parse_full(&fingerprint(owner)).unwrap();
        let live = |owner: char| topology_of(entry(record, owner).displays()).unwrap();
        InspectedPeer {
            local_device: device_id_from_fingerprint(parsed(local)),
            peer_device: device_id_from_fingerprint(parsed(peer)),
            local_fingerprint: parsed(local),
            peer_fingerprint: parsed(peer),
            local_platform: core_platform(entry(record, local).platform()),
            peer_platform: core_platform(entry(record, peer).platform()),
            local_displays: live(local),
            peer_displays: live(peer),
            interface_id: "en0:4:192.168.1.4".into(),
        }
    }

    fn known_of(members: &[(char, ComputerPlatform, &[DisplaySnapshot])]) -> KnownDisplays {
        let mut known = KnownDisplays::default();
        for (owner, platform, displays) in members {
            known.insert(
                &fingerprint(*owner),
                core_platform(*platform),
                &topology_of(displays).unwrap(),
            );
        }
        known
    }

    /// The same pair seen from the other computer.
    fn mirrored(record: &SharingPreferences) -> SharingPreferences {
        let mut mirrored = record.clone();
        std::mem::swap(
            &mut mirrored.local_fingerprint,
            &mut mirrored.peer_fingerprint,
        );
        std::mem::swap(&mut mirrored.local_displays, &mut mirrored.peer_displays);
        mirrored
    }

    /// The same displays, machines, and crossings from each display in the same order: link order
    /// only matters among links that leave one display, where the first match wins.
    fn assert_same_topology(today: &Topology, group: &Topology) {
        assert!(today.displays().eq(group.displays()));
        assert_eq!(today.links().len(), group.links().len());
        for display in today.displays() {
            assert_eq!(
                today.machine_for_display(display.id),
                group.machine_for_display(display.id)
            );
            let leaving = |topology: &Topology| {
                topology
                    .links()
                    .iter()
                    .filter(|link| link.from_display == display.id)
                    .copied()
                    .collect::<Vec<_>>()
            };
            assert_eq!(leaving(today), leaving(group));
        }
    }

    fn permutations(items: &[usize]) -> Vec<Vec<usize>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        (0..items.len())
            .flat_map(|index| {
                let mut rest = items.to_vec();
                let first = rest.remove(index);
                permutations(&rest).into_iter().map(move |mut order| {
                    order.insert(0, first);
                    order
                })
            })
            .collect()
    }

    #[test]
    fn a_three_member_record_round_trips_through_the_wire_and_the_file() {
        let record = trio();
        assert_eq!(
            record
                .members()
                .iter()
                .map(GroupMember::fingerprint)
                .collect::<Vec<_>>(),
            [fingerprint('A'), fingerprint('B'), fingerprint('C')]
        );

        // The file keeps the record exactly.
        let text = serde_json::to_vec(&record).unwrap();
        let restored: GroupRecord = serde_json::from_slice(&text).unwrap();
        assert_eq!(restored, record);
        assert_eq!(restored.validate(), Ok(()));
        assert_eq!(restored.stamp(), record.stamp());
        let mut unknown = serde_json::to_value(&record).unwrap();
        unknown["interfaceId"] = serde_json::json!("en0:4:192.168.1.4");
        assert!(serde_json::from_value::<GroupRecord>(unknown).is_err());

        // Every link of the group carries it, sent from either end.
        for (sender, receiver) in [('A', 'B'), ('A', 'C'), ('C', 'B')] {
            let link = link_between(&record, sender, receiver);
            assert!(record.fits_link(&link));
            let bytes = shared_group_bytes(&link, &record, true).unwrap();
            for current in [link.clone(), opposite(&link)] {
                assert_eq!(
                    shared_group_for_link(&current, &bytes),
                    Ok((record.clone(), true))
                );
            }
        }

        // Only the link between sender and receiver, showing their displays now, stages it.
        let link = link_between(&record, 'B', 'A');
        let bytes = shared_group_bytes(&opposite(&link), &record, false).unwrap();
        assert_eq!(
            shared_group_for_link(&link_between(&record, 'C', 'A'), &bytes),
            Err(PreferenceError::InspectionChanged)
        );
        let mut outsider = link.clone();
        outsider.local_fingerprint = CertificateFingerprint::parse_full(&fingerprint('D')).unwrap();
        outsider.local_device = device_id_from_fingerprint(outsider.local_fingerprint);
        assert_eq!(
            shared_group_for_link(&outsider, &bytes),
            Err(PreferenceError::InspectionChanged)
        );
        let mut moved = record.clone();
        moved.members[1].displays[0].origin = [0.0, 100.0];
        assert_eq!(
            shared_group_for_link(&link_between(&moved, 'B', 'A'), &bytes),
            Err(PreferenceError::InspectionChanged)
        );
        assert_eq!(
            shared_group_bytes(&link_between(&moved, 'B', 'A'), &record, false),
            Err(PreferenceError::InspectionChanged)
        );
        // Another member's entry is not this link's to judge; only the record's bounds are.
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let mut elsewhere = value.clone();
        elsewhere["record"]["members"][2]["displays"][0]["origin"] = serde_json::json!([0.0, -5.0]);
        let (staged, _) =
            shared_group_for_link(&link, &serde_json::to_vec(&elsewhere).unwrap()).unwrap();
        assert_eq!(entry(&staged, 'C').displays()[0].origin, [0.0, -5.0]);
        let mut broken = value.clone();
        broken["record"]["members"][2]["displays"][0]["size"] = serde_json::json!([0.0, 1080.0]);
        assert_eq!(
            shared_group_for_link(&link, &serde_json::to_vec(&broken).unwrap()),
            Err(PreferenceError::Invalid)
        );
        for (field, bad) in [
            ("version", serde_json::json!(2)),
            ("senderFingerprint", serde_json::json!(fingerprint('D'))),
            ("interfaceId", serde_json::json!("en0:4:192.168.1.4")),
        ] {
            let mut corrupted = value.clone();
            corrupted[field] = bad;
            assert!(
                shared_group_for_link(&link, &serde_json::to_vec(&corrupted).unwrap()).is_err(),
                "{field}"
            );
        }
        assert!(shared_group_for_link(&link, &vec![b' '; MAX_LAYOUT_PAYLOAD_BYTES + 1]).is_err());

        // The summary names the members and the record's stamp, never the layout.
        let summary = RecordSummary::new(
            &[&fingerprint('C'), &key('A'), &fingerprint('B')],
            Some(&record),
        )
        .unwrap();
        let bytes = summary.to_bytes().unwrap();
        let parsed = RecordSummary::parse(&bytes).unwrap();
        assert_eq!(parsed, summary);
        assert_eq!(
            parsed.members().to_vec(),
            vec![key('A'), key('B'), key('C')]
        );
        assert_eq!(parsed.stamp(), Some(record.stamp()));
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains(&hex(&record.content_digest())));
        assert!(!text.contains("links"));
        let unarranged = RecordSummary::new(&[&fingerprint('A'), &fingerprint('D')], None).unwrap();
        assert_eq!(
            RecordSummary::parse(&unarranged.to_bytes().unwrap())
                .unwrap()
                .stamp(),
            None
        );
        assert!(
            RecordSummary::new(&[&fingerprint('A'), &fingerprint('B')], Some(&record)).is_err()
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let corruptions: [fn(&mut serde_json::Value); 9] = [
            |summary| summary["version"] = serde_json::json!(2),
            |summary| summary["members"][0] = serde_json::json!(fingerprint('A')),
            |summary| summary["members"] = serde_json::json!([key('B'), key('A'), key('C')]),
            |summary| summary["members"] = serde_json::json!([key('A')]),
            |summary| summary["record"]["author"] = serde_json::json!(fingerprint('D')),
            |summary| summary["record"]["revision"] = serde_json::json!(0),
            |summary| summary["record"]["content"] = serde_json::json!("0".repeat(63)),
            |summary| summary["record"]["content"] = serde_json::json!("F".repeat(64)),
            |summary| summary["layout"] = serde_json::json!({}),
        ];
        for (index, corrupt) in corruptions.into_iter().enumerate() {
            let mut corrupted = value.clone();
            corrupt(&mut corrupted);
            assert!(
                RecordSummary::parse(&serde_json::to_vec(&corrupted).unwrap()).is_err(),
                "{index}"
            );
        }
        assert!(RecordSummary::parse(b"").is_err());
        assert!(RecordSummary::parse(&vec![b' '; MAX_SUMMARY_PAYLOAD_BYTES + 1]).is_err());
    }

    #[test]
    fn a_two_member_group_builds_todays_outbound_topology() {
        let mut placed = two_display_record();
        placed.layout.arrangement = Some(ArrangementRequest {
            positions: vec![
                position("1", 0.0, 0.0),
                position("3", 0.0, 1080.0),
                position("2", -2560.0, 540.0),
            ],
            hidden: Vec::new(),
        });
        let mut one_unused = two_display_record();
        one_unused.layout.arrangement = Some(ArrangementRequest {
            positions: vec![position("3", 0.0, 1080.0), position("2", -2560.0, 540.0)],
            hidden: vec!["1".into()],
        });
        for record in [
            preferences(),
            two_display_record(),
            placed,
            one_unused,
            preferences_for_peer('0'),
        ] {
            let here = inspection(&record);
            let group =
                GroupRecord::from_pairwise(&record, here.local_platform, here.peer_platform)
                    .unwrap();
            for (side, local) in [
                (here.clone(), &record.local_fingerprint),
                (opposite(&here), &record.peer_fingerprint),
            ] {
                let today = validated_layout(&side, &record.layout).unwrap();
                assert_same_topology(&today, &group.topology_for(local).unwrap());
            }
        }
    }

    #[test]
    fn validation_matches_the_pairwise_rules_for_two_members() {
        // This computer's displays 1 above 3, the other computer's display 2 crossed from 3.
        let base = two_display_record();
        let with = |change: &dyn Fn(&mut LayoutRequest)| {
            let mut layout = base.layout.clone();
            change(&mut layout);
            layout
        };
        let placed = |hidden: &[&str]| {
            Some(ArrangementRequest {
                positions: [("1", 0.0, 0.0), ("3", 0.0, 1080.0), ("2", -2560.0, 540.0)]
                    .into_iter()
                    .filter(|(id, _, _)| !hidden.contains(id))
                    .map(|(id, x, y)| position(id, x, y))
                    .collect(),
                hidden: hidden.iter().map(|id| (*id).to_owned()).collect(),
            })
        };
        let cases: Vec<(&str, LayoutRequest)> = vec![
            ("as saved", base.layout.clone()),
            ("placed", with(&|layout| layout.arrangement = placed(&[]))),
            (
                "a display unused",
                with(&|layout| layout.arrangement = placed(&["1"])),
            ),
            (
                "one direction of control",
                with(&|layout| {
                    layout.control.insert(key('B'), false);
                }),
            ),
            (
                "split crossings",
                with(&|layout| {
                    layout.links = [
                        spanned("3", "right", [0.0, 0.5], "2", "left", [0.0, 0.5]),
                        spanned("3", "right", [0.5, 1.0], "2", "left", [0.5, 1.0]),
                    ]
                    .concat();
                }),
            ),
            ("no crossing", with(&|layout| layout.links.clear())),
            (
                "one way",
                with(&|layout| {
                    layout.links.remove(1);
                }),
            ),
            (
                "on one computer's own displays",
                with(&|layout| layout.links.extend(crossing("1", "bottom", "3", "top"))),
            ),
            (
                "onto an inherited seam",
                with(&|layout| layout.links.extend(crossing("3", "top", "2", "right"))),
            ),
            (
                "overlapping on one edge",
                with(&|layout| {
                    layout.links.extend(spanned(
                        "3",
                        "right",
                        [0.5, 1.0],
                        "2",
                        "right",
                        [0.0, 1.0],
                    ));
                }),
            ),
            (
                "an unknown display",
                with(&|layout| layout.links.extend(crossing("9", "right", "2", "right"))),
            ),
            (
                "onto an unused display",
                with(&|layout| layout.arrangement = placed(&["3"])),
            ),
            (
                "every display of one computer unused",
                with(&|layout| layout.arrangement = placed(&["1", "3"])),
            ),
            (
                "nobody may control",
                with(&|layout| {
                    layout.control.insert(key('A'), false);
                    layout.control.insert(key('B'), false);
                }),
            ),
            (
                "a third computer in control",
                with(&|layout| {
                    layout.control.insert(key('C'), true);
                }),
            ),
            (
                "an uppercase control key",
                with(&|layout| {
                    let allowed = layout.control.remove(&key('B')).unwrap();
                    layout.control.insert(fingerprint('B'), allowed);
                }),
            ),
            (
                "placed twice",
                with(&|layout| {
                    layout.arrangement = placed(&[]);
                    let arrangement = layout.arrangement.as_mut().unwrap();
                    arrangement.positions.push(position("1", 5.0, 5.0));
                }),
            ),
            (
                "placed out of range",
                with(&|layout| {
                    layout.arrangement = placed(&[]);
                    layout.arrangement.as_mut().unwrap().positions[0].x = 3.0e7;
                }),
            ),
            (
                "hidden and placed",
                with(&|layout| {
                    layout.arrangement = placed(&[]);
                    let arrangement = layout.arrangement.as_mut().unwrap();
                    arrangement.hidden.push("1".into());
                }),
            ),
            (
                "an unknown edge",
                with(&|layout| layout.links[0].from_edge = "middle".into()),
            ),
            (
                "too many crossings",
                with(&|layout| {
                    let first = layout.links[0].clone();
                    layout.links.resize(MAX_LINKS + 1, first);
                }),
            ),
            (
                "hysteresis wider than a display",
                with(&|layout| {
                    for link in &mut layout.links {
                        link.hysteresis = 5000.0;
                    }
                }),
            ),
        ];
        let mut accepted = 0;
        for (case, layout) in &cases {
            let pairwise = SharingPreferences::from_inspection(&inspection(&base), layout.clone());
            let mut record = base.clone();
            record.layout = layout.clone();
            let group = GroupRecord::from_pairwise(&record, Platform::MacOs, Platform::Windows);
            assert_eq!(group.is_ok(), pairwise.is_ok(), "{case}");
            accepted += usize::from(pairwise.is_ok());
        }
        assert_eq!(accepted, 5);
    }

    #[test]
    fn display_ids_unique_across_members_and_at_most_sixteen() {
        let mut unique = trio();
        unique
            .layout
            .links
            .retain(|link| link.from_display != "4" && link.to_display != "4");
        unique.layout.arrangement = None;
        unique.members[2].displays[1].id = "5".into();
        assert_eq!(unique.validate(), Ok(()));
        // Each member's own list is fine, but another member already has a display 2.
        let mut clash = unique.clone();
        clash.members[2].displays[1].id = "2".into();
        assert_eq!(clash.validate(), Err(PreferenceError::Invalid));

        // Sixteen displays across the whole group, however they are shared out, and no more.
        let full = maximal_record("Display");
        let count = |record: &GroupRecord| {
            record
                .members()
                .iter()
                .map(|member| member.displays().len())
                .sum::<usize>()
        };
        assert_eq!(count(&full), MAX_DISPLAYS);
        assert_eq!(full.validate(), Ok(()));
        let mut over = full.clone();
        over.members[0]
            .displays
            .push(display("17", [0.0, 5.0e8], [100.0, 100.0], false));
        assert_eq!(count(&over), MAX_DISPLAYS + 1);
        assert_eq!(over.validate(), Err(PreferenceError::Invalid));

        // And at most eight members.
        assert_eq!(ring(MAX_GROUP_MEMBERS).validate(), Ok(()));
        assert_eq!(
            ring(MAX_GROUP_MEMBERS + 1).validate(),
            Err(PreferenceError::Invalid)
        );
    }

    #[test]
    fn a_pair_where_neither_may_control_has_no_wire_permissions() {
        let (a, b, c) = (fingerprint('A'), fingerprint('B'), fingerprint('C'));
        assert!(device_of(&a) < device_of(&b));
        let control: ControlMap = [(key('A'), true), (key('B'), false), (key('C'), false)]
            .into_iter()
            .collect();
        let mut record = trio();
        record.layout.control = control.clone();
        assert_eq!(record.validate(), Ok(()));
        let expected = Some(ControlPermissions {
            lower_controls_higher: true,
            higher_controls_lower: false,
        });
        assert_eq!(wire_control(&control, &a, &b), expected);
        assert_eq!(wire_control(&control, &b, &key('A')), expected);
        // B and C may control nobody, so their pair negotiates nothing and gets no connection.
        assert_eq!(wire_control(&control, &b, &c), None);
        assert_eq!(wire_control(&control, &c, &b), None);
        assert_eq!(wire_control(&control, &a, &a), None);
        assert_eq!(wire_control(&control, &a, &fingerprint('D')), None);

        // A two-member map gives exactly today's permissions, from either side.
        for (a_allowed, b_allowed) in [(true, true), (true, false), (false, true)] {
            let pair: ControlMap = [(key('A'), a_allowed), (key('B'), b_allowed)]
                .into_iter()
                .collect();
            for (local, peer) in [(&a, &b), (&b, &a)] {
                assert_eq!(
                    wire_control(&pair, local, peer),
                    Some(crate::sharing_preferences::wire_control(&pair, local, peer).unwrap())
                );
            }
        }
    }

    #[test]
    fn stamps_order_by_revision_then_lower_device_then_content() {
        let device = |owner: char| device_of(&fingerprint(owner)).unwrap();
        assert!(device('A') < device('B'));
        let stamp = |revision, owner, content| Stamp {
            revision,
            author: device(owner),
            content: [content; 32],
        };
        assert!(stamp(2, 'B', 9) > stamp(1, 'A', 0));
        assert!(stamp(1, 'A', 9) > stamp(1, 'B', 0));
        assert!(stamp(1, 'A', 0) > stamp(1, 'A', 9));
        assert_eq!(stamp(1, 'A', 0).cmp(&stamp(1, 'A', 0)), Ordering::Equal);

        let record = trio();
        assert_eq!(record.stamp().revision(), 1);
        assert_eq!(record.stamp().content, record.content_digest());
        assert_eq!(record.stamp().author, device('A'));
        let newer = record.restamped(2, &fingerprint('C')).unwrap();
        assert!(newer.stamp() > record.stamp());
        assert_eq!(newer.clone().merge(record.clone()), newer);
        assert_eq!(record.clone().merge(newer.clone()), newer);
        assert!(record.restamped(0, &fingerprint('A')).is_err());
        assert!(record.restamped(2, &fingerprint('D')).is_err());
    }

    #[test]
    fn conflicting_revisions_converge_under_any_pairwise_exchange_order() {
        let base = trio();
        let mut no_c = base.clone();
        no_c.layout.control.insert(key('C'), false);
        let mut moved = base.clone();
        moved.layout.arrangement.as_mut().unwrap().positions[1].y = 100.0;
        let copies = [
            no_c.restamped(3, &fingerprint('B')).unwrap(),
            moved.restamped(3, &fingerprint('B')).unwrap(),
            base.restamped(2, &fingerprint('A')).unwrap(),
            base.restamped(3, &fingerprint('C')).unwrap(),
        ];
        // Revision 3 beats 2, B beats C as the lower device, and the lower content settles B's two.
        assert!(device_of(&fingerprint('B')) < device_of(&fingerprint('C')));
        assert_ne!(copies[0].content_digest(), copies[1].content_digest());
        let expected = if copies[0].content_digest() < copies[1].content_digest() {
            &copies[0]
        } else {
            &copies[1]
        };
        assert!(copies.iter().all(|copy| copy.stamp() <= expected.stamp()));
        let exchanges: Vec<(usize, usize)> = (0..copies.len())
            .flat_map(|a| (a + 1..copies.len()).map(move |b| (a, b)))
            .collect();
        for order in permutations(&(0..exchanges.len()).collect::<Vec<_>>()) {
            let mut held = copies.clone();
            for exchange in order {
                let (a, b) = exchanges[exchange];
                let at_a = held[a].clone().merge(held[b].clone());
                let at_b = held[b].clone().merge(held[a].clone());
                assert_eq!(at_a, at_b);
                held[a] = at_a;
                held[b] = at_b;
            }
            assert!(held.iter().all(|copy| copy == expected));
        }
    }

    #[test]
    fn a_members_display_change_is_refit_in_its_own_block_and_leaves_others_alone() {
        let record = trio();
        // C's second display moved down, and a third appeared under its first.
        let now = vec![
            display("3", [0.0, 0.0], [1920.0, 1080.0], true),
            display("4", [1920.0, 540.0], [1920.0, 1080.0], false),
            display("5", [0.0, 1080.0], [1920.0, 1080.0], false),
        ];
        let a = entry(&record, 'A').displays().to_vec();
        for known in [
            known_of(&[('C', ComputerPlatform::Windows, &now)]),
            known_of(&[
                ('A', ComputerPlatform::Macos, &a),
                ('C', ComputerPlatform::Windows, &now),
            ]),
        ] {
            let AdaptedGroup {
                record: adapted,
                left_out,
            } = record
                .adapt(&known)
                .expect("the change fits in C's own block");
            assert!(!left_out);
            assert_eq!(entry(&adapted, 'A'), entry(&record, 'A'));
            assert_eq!(entry(&adapted, 'B'), entry(&record, 'B'));
            assert_eq!(entry(&adapted, 'C').displays(), now.as_slice());
            assert_eq!(adapted.layout().links, record.layout().links);
            assert_eq!(adapted.layout().control, record.layout().control);
            assert_eq!(
                (adapted.revision(), adapted.author()),
                (record.revision(), record.author())
            );
            let mut placed: Vec<(&str, f64, f64)> = adapted
                .layout()
                .arrangement
                .as_ref()
                .unwrap()
                .positions
                .iter()
                .map(|position| (position.display.as_str(), position.x, position.y))
                .collect();
            placed.sort_by(|left, right| left.0.cmp(right.0));
            assert_eq!(
                placed,
                vec![
                    ("1", 0.0, 0.0),
                    ("2", 1920.0, 0.0),
                    ("3", -3840.0, 0.0),
                    ("4", -1920.0, 540.0),
                    ("5", -3840.0, 1080.0),
                ]
            );
            for owner in ['A', 'B', 'C'] {
                assert!(adapted.topology_for(&fingerprint(owner)).is_some());
            }
            // A card draws the refit before anything is saved; a fitting record is drawn as is.
            assert_eq!(record.preview_for(&known), adapted);
            assert_eq!(adapted.preview_for(&known), adapted);
        }
        // Nothing changed: the record stands as it is.
        let unchanged = known_of(&[('A', ComputerPlatform::Macos, &a)]);
        assert_eq!(
            record.adapt(&unchanged).map(|adapted| adapted.record),
            Some(record.clone())
        );

        // A monitor that came back under a new id is the same display: only its id changes.
        let mut monitored = record.clone();
        monitored.members[2].displays[0].monitor = Some(BUILT_IN.into());
        monitored.members[2].displays[1].monitor = Some(EXTERNAL.into());
        let mut renumbered = monitored.members[2].displays.clone();
        renumbered[1].id = "9".into();
        let known = known_of(&[('C', ComputerPlatform::Windows, &renumbered)]);
        let remapped = monitored
            .remap_to(&known)
            .expect("the same monitors under new ids");
        assert_eq!(entry(&remapped, 'C').displays(), renumbered.as_slice());
        assert!(
            remapped
                .layout()
                .links
                .iter()
                .all(|link| link.from_display != "4" && link.to_display != "4")
        );
        assert_eq!(
            remapped.layout().links.len(),
            monitored.layout().links.len()
        );
        assert_eq!(entry(&remapped, 'A'), entry(&monitored, 'A'));
        // A display that moved is refit, never remapped.
        assert!(
            record
                .remap_to(&known_of(&[('C', ComputerPlatform::Windows, &now)]))
                .is_none()
        );

        // With every crossing gone, a card still draws the displays there are now.
        let pair =
            GroupRecord::from_pairwise(&preferences(), Platform::MacOs, Platform::Windows).unwrap();
        let other = vec![display("7", [0.0, 0.0], [1920.0, 1080.0], true)];
        let known = known_of(&[('B', ComputerPlatform::Windows, &other)]);
        assert!(pair.adapt(&known).is_none());
        let preview = pair.preview_for(&known);
        assert_eq!(entry(&preview, 'B').displays(), other.as_slice());
        assert_eq!(entry(&preview, 'A'), entry(&pair, 'A'));
        assert!(preview.layout().links.is_empty());
    }

    #[test]
    fn two_links_refitting_one_member_produce_the_same_content() {
        let record = trio();
        let mut a = entry(&record, 'A').displays().to_vec();
        // A's own link reports a label the record never had.
        a[0].name = "Studio Display".into();
        let b = entry(&record, 'B').displays().to_vec();
        let moved = vec![
            display("3", [0.0, 0.0], [1920.0, 1080.0], true),
            display("4", [1920.0, 540.0], [1920.0, 1080.0], false),
        ];
        let unplugged = vec![display("3", [0.0, 0.0], [1920.0, 1080.0], true)];
        for (now, lost) in [(moved, false), (unplugged, true)] {
            let from_a = record
                .adapt(&known_of(&[
                    ('A', ComputerPlatform::Macos, &a),
                    ('C', ComputerPlatform::Windows, &now),
                ]))
                .unwrap();
            let from_b = record
                .adapt(&known_of(&[
                    ('B', ComputerPlatform::Windows, &b),
                    ('C', ComputerPlatform::Windows, &now),
                ]))
                .unwrap();
            assert_eq!(
                from_a.record.content_digest(),
                from_b.record.content_digest()
            );
            assert_eq!(from_a.record, from_b.record);
            assert_eq!((from_a.left_out, from_b.left_out), (lost, lost));
        }
    }

    #[test]
    fn content_digest_ignores_names_revision_and_author() {
        let record = trio();
        let digest = record.content_digest();
        let labeled = vec![DisplaySnapshot {
            name: "U2723QE".into(),
            ..entry(&record, 'B').displays()[0].clone()
        }];
        let relabeled = record.relabeled(&known_of(&[('B', ComputerPlatform::Windows, &labeled)]));
        assert_eq!(entry(&relabeled, 'B').displays()[0].name(), "U2723QE");
        assert_ne!(relabeled, record);
        let restamped = record.restamped(9, &key('C')).unwrap();
        assert_eq!(restamped.author(), fingerprint('C'));
        let mut reordered = record.clone();
        reordered.layout.links.reverse();
        reordered
            .layout
            .arrangement
            .as_mut()
            .unwrap()
            .positions
            .reverse();
        reordered.members[2].displays.reverse();
        for same in [&relabeled, &restamped, &reordered] {
            assert_eq!(same.validate(), Ok(()));
            assert_eq!(same.content_digest(), digest);
        }
        // Everything a session or the picture reads is content.
        let changes: [fn(&mut GroupRecord); 6] = [
            |record| record.members[0].displays[0].origin = [0.0, 10.0],
            |record| {
                record.layout.control.insert(key('C'), false);
            },
            |record| record.layout.links.truncate(4),
            |record| record.layout.arrangement.as_mut().unwrap().positions[0].x = 10.0,
            |record| record.members[1].platform = ComputerPlatform::Macos,
            |record| record.members[0].displays[0].monitor = Some(EXTERNAL.into()),
        ];
        for (index, change) in changes.into_iter().enumerate() {
            let mut changed = record.clone();
            change(&mut changed);
            assert_ne!(changed.content_digest(), digest, "{index}");
        }
    }

    #[test]
    fn per_member_offsets_fall_back_to_zero_when_inexact() {
        // B's display sits just off a whole number, so moving it by the rounded offset 2048 cannot
        // be undone exactly on the other computer.
        let odd = 1.0 + 2.0_f64.powi(-42);
        assert_ne!((odd + 2048.0) - odd, 2048.0);
        let mut record = trio();
        record.members[1].displays[0].origin = [odd, 0.0];
        record.layout.arrangement.as_mut().unwrap().positions[1].x = 2049.0;
        assert_eq!(record.validate(), Ok(()));
        let (a, b, c) = (0, 1, 2);
        assert_eq!(record.member_offset(a, b), Point::default());
        assert_eq!(record.member_offset(b, a), Point::default());
        assert_eq!(record.member_offset(a, c), Point::new(-3840.0, 0.0));
        assert_eq!(record.member_offset(c, a), Point::new(3840.0, 0.0));
        let topology = record.topology_for(&fingerprint('A')).unwrap();
        let origin = |id: u64| topology.display(DisplayId(id)).unwrap().origin;
        assert_eq!(origin(1), Point::new(0.0, 0.0));
        assert_eq!(origin(2), Point::new(odd, 0.0));
        assert_eq!(origin(3), Point::new(-3840.0, 0.0));
        assert_eq!(origin(4), Point::new(-1920.0, 0.0));

        // For two members this is today's peer offset, exact or not.
        for x in [2049.0, 2048.0 + odd] {
            let mut pair = preferences();
            pair.peer_displays[0].origin = [odd, 0.0];
            pair.layout.arrangement = Some(ArrangementRequest {
                positions: vec![position("1", 0.0, 0.0), position("2", x, 0.0)],
                hidden: Vec::new(),
            });
            let here = inspection(&pair);
            let group =
                GroupRecord::from_pairwise(&pair, here.local_platform, here.peer_platform).unwrap();
            let topology = group.topology_for(&pair.local_fingerprint).unwrap();
            assert_same_topology(&validated_layout(&here, &pair.layout).unwrap(), &topology);
        }
    }

    #[test]
    fn a_maximal_record_fits_the_link_payload() {
        // Control characters take six bytes each once written as JSON.
        for name in [
            "M".repeat(MAX_DISPLAY_NAME_BYTES),
            "\u{1}".repeat(MAX_DISPLAY_NAME_BYTES),
        ] {
            let record = maximal_record(&name);
            let json = serde_json::to_vec(&record).unwrap();
            let link = link_between(&record, '0', '1');
            let shared = shared_group_bytes(&link, &record, false).unwrap();
            assert!(shared.len() <= MAX_LAYOUT_PAYLOAD_BYTES);
            assert!(shared.len() - json.len() <= SHARED_GROUP_ENVELOPE_BYTES);
            assert_eq!(
                shared_group_for_link(&opposite(&link), &shared).map(|(staged, _)| staged),
                Ok(record.clone())
            );
            let members: Vec<&str> = record
                .members()
                .iter()
                .map(GroupMember::fingerprint)
                .collect();
            let summary = RecordSummary::new(&members, Some(&record))
                .unwrap()
                .to_bytes()
                .unwrap();
            assert!(summary.len() <= MAX_SUMMARY_PAYLOAD_BYTES);
            assert!(RecordSummary::parse(&summary).is_ok());
        }
        // Past the byte bound a record is refused, so every valid one can be sent.
        let mut oversized = maximal_record("M");
        oversized.members[0].displays[0].monitor = Some("f".repeat(MAX_LAYOUT_PAYLOAD_BYTES));
        assert_eq!(oversized.validate(), Err(PreferenceError::Invalid));
    }

    #[test]
    fn removing_a_member_keeps_the_remaining_crossings() {
        let record = trio();
        let without_c = record
            .without_member(&key('C'), 2, &fingerprint('A'))
            .expect("A and B still cross");
        assert!(!without_c.has_member(&fingerprint('C')));
        assert_eq!(without_c.members().len(), 2);
        assert_eq!(
            without_c.layout().links,
            crossing("1", "right", "2", "left").to_vec()
        );
        assert_eq!(without_c.layout().control, everyone(&['A', 'B']));
        let placed: Vec<&str> = without_c
            .layout()
            .arrangement
            .as_ref()
            .unwrap()
            .positions
            .iter()
            .map(|position| position.display.as_str())
            .collect();
        assert_eq!(placed, ["1", "2"]);
        assert_eq!(
            (without_c.revision(), without_c.author()),
            (2, fingerprint('A').as_str())
        );
        assert_eq!(entry(&without_c, 'A'), entry(&record, 'A'));
        assert_eq!(entry(&without_c, 'B'), entry(&record, 'B'));
        let without_a = record
            .without_member(&fingerprint('A'), 2, &fingerprint('B'))
            .expect("B and C still cross");
        assert_eq!(
            without_a.layout().links,
            crossing("2", "bottom", "3", "top").to_vec()
        );
        // The derived record is a local change by a member that is left.
        assert!(
            record
                .without_member(&fingerprint('A'), 2, &fingerprint('A'))
                .is_none()
        );
        assert!(
            record
                .without_member(&fingerprint('D'), 2, &fingerprint('A'))
                .is_none()
        );
        // Nothing valid is left: no crossing, nobody who may control, or a single computer.
        let mut through_a = record.clone();
        through_a.layout.links.truncate(4);
        assert_eq!(through_a.validate(), Ok(()));
        assert!(
            through_a
                .without_member(&fingerprint('A'), 2, &fingerprint('B'))
                .is_none()
        );
        let mut only_c = record.clone();
        only_c.layout.control = [(key('A'), false), (key('B'), false), (key('C'), true)]
            .into_iter()
            .collect();
        assert_eq!(only_c.validate(), Ok(()));
        assert!(
            only_c
                .without_member(&fingerprint('C'), 2, &fingerprint('A'))
                .is_none()
        );
        assert!(
            without_c
                .without_member(&fingerprint('B'), 3, &fingerprint('A'))
                .is_none()
        );
    }

    #[test]
    fn from_pairwise_gives_both_computers_the_same_content() {
        for record in [
            preferences(),
            two_display_record(),
            preferences_for_peer('0'),
        ] {
            let here = inspection(&record);
            let mine = GroupRecord::from_pairwise(&record, here.local_platform, here.peer_platform)
                .unwrap();
            let theirs = GroupRecord::from_pairwise(
                &mirrored(&record),
                here.peer_platform,
                here.local_platform,
            )
            .unwrap();
            assert_eq!(mine.content_digest(), theirs.content_digest());
            assert_eq!(mine.members(), theirs.members());
            assert_eq!(mine.author(), theirs.author());
            let (decider, other, decider_fingerprint) = if record.local_decides() {
                (&mine, &theirs, &record.local_fingerprint)
            } else {
                (&theirs, &mine, &record.peer_fingerprint)
            };
            assert_eq!((decider.revision(), other.revision()), (2, 1));
            assert_eq!(decider.author(), decider_fingerprint);
            assert_eq!(mine.clone().merge(theirs.clone()), *decider);
            assert_eq!(theirs.clone().merge(mine.clone()), *decider);
            // Both computers get today's pairwise record back, and today's permissions.
            assert_eq!(
                mine.to_pairwise(&record.local_fingerprint, record.interface_id()),
                Ok(record.clone())
            );
            assert_eq!(
                theirs.to_pairwise(
                    &record.peer_fingerprint.to_ascii_lowercase(),
                    record.interface_id()
                ),
                Ok(mirrored(&record))
            );
            assert_eq!(
                wire_control(
                    &mine.layout().control,
                    &record.local_fingerprint,
                    &record.peer_fingerprint
                ),
                record.wire_control().ok()
            );
            assert_eq!(
                mine.to_pairwise(&fingerprint('D'), record.interface_id()),
                Err(PreferenceError::Invalid)
            );
        }
        assert!(
            trio()
                .to_pairwise(&fingerprint('A'), "en0:4:192.168.1.4")
                .is_err()
        );

        // Copies that drifted apart converge to the old decider's.
        let record = preferences();
        assert!(record.local_decides());
        let mut drifted = mirrored(&record);
        drifted.layout.control.insert(key('A'), false);
        let mine = GroupRecord::from_pairwise(&record, Platform::MacOs, Platform::Windows).unwrap();
        let theirs =
            GroupRecord::from_pairwise(&drifted, Platform::Windows, Platform::MacOs).unwrap();
        assert_ne!(mine.content_digest(), theirs.content_digest());
        assert_eq!(theirs.merge(mine.clone()), mine);

        // A member built from live displays is the same entry.
        let local = GroupMember::new(
            &key('A'),
            Platform::MacOs,
            &topology_of(&record.local_displays).unwrap(),
        )
        .unwrap();
        assert_eq!(&local, entry(&mine, 'A'));
    }
}
