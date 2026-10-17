//! What the island shows, from snapshots of the app's state: a compact activity (a short-lived
//! notice, else a standing one, else nothing) and the open island's rows. Pure and clock-injected.

use std::time::{Duration, Instant};

const MAX_COMPUTER_ROWS: usize = 3;

/// How long each notice shows, and which may replace which.
const DIMMING: (u8, Duration) = (3, Duration::from_millis(1600));
const DROPPED: (u8, Duration) = (2, Duration::from_secs(4));
const CONNECTED: (u8, Duration) = (2, Duration::from_millis(2500));
const CLIPBOARD: (u8, Duration) = (1, Duration::from_millis(2500));
const UPDATE: (u8, Duration) = (1, Duration::from_secs(6));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Windows,
}

/// A computer's connection, as the sharing view names its phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Off,
    Connecting,
    Connected,
    Starting,
    Sharing,
    Reconnecting,
    Stopping,
    Error,
    NotPaired,
}

impl Phase {
    pub fn parse(phase: &str) -> Self {
        match phase {
            "connecting" => Self::Connecting,
            "connected" => Self::Connected,
            "starting" => Self::Starting,
            "sharing" => Self::Sharing,
            "reconnecting" => Self::Reconnecting,
            "stopping" => Self::Stopping,
            "error" => Self::Error,
            "notPaired" => Self::NotPaired,
            _ => Self::Off,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComputerInput {
    /// Lowercase.
    pub fingerprint: String,
    pub name: String,
    pub platform: Platform,
    pub enabled: bool,
    pub phase: Phase,
    /// The session is up but the other computer stopped answering.
    pub held: bool,
    /// Where this computer's session routes the pointer: false while this Mac's keyboard and
    /// pointer are on the other computer; true while input lands here, from either side.
    pub pointer_here: Option<bool>,
    /// Position among the computers switched on, for the Home card's tone.
    pub tone: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DimmingInput {
    pub dimmed: bool,
    pub level: u8,
    pub min_level: u8,
    pub max_level: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Sent,
    Received,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    Text,
    Image,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    pub direction: Direction,
    pub kind: TransferKind,
    pub bytes: u64,
    pub peer: String,
    pub age_seconds: u64,
}

impl Transfer {
    /// The same transfer seen again, as opposed to a new one.
    fn same_as(&self, earlier: &Transfer) -> bool {
        self.direction == earlier.direction
            && self.kind == earlier.kind
            && self.bytes == earlier.bytes
            && self.peer == earlier.peer
            && self.age_seconds >= earlier.age_seconds
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardInput {
    pub enabled: bool,
    pub last: Option<Transfer>,
}

/// One snapshot of everything the island reads.
#[derive(Clone, Debug, PartialEq)]
pub struct IslandInput {
    /// Paired computers, those switched on first.
    pub computers: Vec<ComputerInput>,
    pub paused: bool,
    /// The sharing view's lead phase, whether a session shares input, and whether one is held.
    pub phase: Phase,
    pub sharing_active: bool,
    pub held: bool,
    pub dimming: DimmingInput,
    /// None where clipboard sharing does not exist.
    pub clipboard: Option<ClipboardInput>,
    /// The version an update is ready to install, while one is.
    pub update_ready: Option<String>,
}

/// Text and indicator tones, each a dark-theme token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Foreground,
    Subtle,
    Success,
    Warning,
    Destructive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Symbol {
    MoonFill,
    SunMax,
    DocOnClipboard,
    ArrowDownCircle,
    ArrowRight,
    Laptop,
    Pc,
    OpenApp,
    Plus,
}

impl Symbol {
    pub fn system_name(self) -> &'static str {
        match self {
            Self::MoonFill => "moon.fill",
            Self::SunMax => "sun.max",
            Self::DocOnClipboard => "doc.on.clipboard",
            Self::ArrowDownCircle => "arrow.down.circle",
            Self::ArrowRight => "arrow.right",
            Self::Laptop => "laptopcomputer",
            Self::Pc => "pc",
            Self::OpenApp => "arrow.up.forward.app",
            Self::Plus => "plus",
        }
    }

    pub fn for_platform(platform: Platform) -> Self {
        match platform {
            Platform::Mac => Self::Laptop,
            Platform::Windows => Self::Pc,
        }
    }
}

/// The left wing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leading {
    Symbol(Symbol, Tone),
    Dot(Tone),
    Spinner,
}

/// The right wing.
#[derive(Clone, Debug, PartialEq)]
pub enum Trailing {
    Text(String),
    /// A share of the bar filled, 0..=1.
    Level(f64),
    Labeled(Symbol, String),
}

/// Which activity is up; a change of id replays the entrance, a change of content does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityId {
    Dimming,
    Dropped,
    Connected,
    Clipboard,
    Update,
    Reconnecting,
    Held,
    Controlling,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub id: ActivityId,
    pub leading: Leading,
    pub trailing: Trailing,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComputerRow {
    pub fingerprint: String,
    pub name: String,
    pub platform: Platform,
    pub tone: usize,
    pub status: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Computer(ComputerRow),
    SetUp,
    Dimming(DimmingInput),
    Clipboard {
        enabled: bool,
        detail: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expanded {
    pub status: String,
    pub status_tone: Tone,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Presentation {
    pub compact: Option<Activity>,
    pub expanded: Expanded,
    /// This Mac's keyboard and pointer are on another computer: hover and clicks never open it.
    pub input_away: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoticeKind {
    Dimming,
    Dropped,
    Connected,
    Clipboard,
    Update,
}

impl NoticeKind {
    fn rule(self) -> (u8, Duration) {
        match self {
            Self::Dimming => DIMMING,
            Self::Dropped => DROPPED,
            Self::Connected => CONNECTED,
            Self::Clipboard => CLIPBOARD,
            Self::Update => UPDATE,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Notice {
    kind: NoticeKind,
    activity: Activity,
    /// None while the pointer holds it.
    until: Option<Instant>,
}

/// The parts of a snapshot later snapshots are compared with.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    /// Each computer's last phase other than `Stopping`, which only passes through.
    phases: Vec<(String, Phase)>,
    enabled: Vec<String>,
    paused: bool,
    dimming: DimmingInput,
    transfer: Option<Transfer>,
}

#[derive(Default)]
pub struct Model {
    seen: Option<Seen>,
    notice: Option<Notice>,
    hovering: bool,
    update_shown: Option<String>,
}

impl Model {
    /// A model that stays quiet about the update version an earlier one already announced.
    pub fn remembering(update_shown: Option<String>) -> Self {
        Self {
            update_shown,
            ..Self::default()
        }
    }

    /// The update version announced so far, for the next model to remember.
    pub fn update_shown(&self) -> Option<&str> {
        self.update_shown.as_deref()
    }

    /// Takes a snapshot: compares it with the last one to raise notices, then presents both.
    pub fn update(&mut self, input: &IslandInput, now: Instant) -> Presentation {
        self.expire(now);
        let seen = Seen::of(input, self.seen.as_ref());
        if let Some(previous) = self.seen.take() {
            for (kind, activity) in notices(&previous, &seen, input) {
                self.raise(kind, activity, now);
            }
        }
        self.seen = Some(seen);
        if let Some(version) = &input.update_ready
            && self.update_shown.as_ref() != Some(version)
            && self.raise(NoticeKind::Update, update_activity(), now)
        {
            self.update_shown = Some(version.clone());
        }
        self.present(input)
    }

    /// The pointer resting on the island holds a notice; leaving gives it its full time again.
    pub fn set_hovering(&mut self, hovering: bool, now: Instant) {
        self.expire(now);
        if self.hovering == hovering {
            return;
        }
        self.hovering = hovering;
        if let Some(notice) = &mut self.notice {
            notice.until = (!hovering).then(|| now + notice.kind.rule().1);
        }
    }

    /// When the shown notice ends, so the driver wakes then.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.notice.as_ref().and_then(|notice| notice.until)
    }

    fn expire(&mut self, now: Instant) {
        if self
            .notice
            .as_ref()
            .and_then(|notice| notice.until)
            .is_some_and(|until| until <= now)
        {
            self.notice = None;
        }
    }

    /// True when the notice is now showing.
    fn raise(&mut self, kind: NoticeKind, activity: Activity, now: Instant) -> bool {
        let (priority, duration) = kind.rule();
        if self
            .notice
            .as_ref()
            .is_some_and(|shown| shown.kind.rule().0 > priority)
        {
            return false;
        }
        self.notice = Some(Notice {
            kind,
            activity,
            until: (!self.hovering).then(|| now + duration),
        });
        true
    }

    fn present(&self, input: &IslandInput) -> Presentation {
        let compact = self
            .notice
            .as_ref()
            .map(|notice| notice.activity.clone())
            .or_else(|| standing(input));
        Presentation {
            compact,
            expanded: expanded(input),
            input_away: controlled(input).is_some(),
        }
    }
}

impl Seen {
    fn of(input: &IslandInput, previous: Option<&Seen>) -> Self {
        let phases = input
            .computers
            .iter()
            .map(|computer| {
                let earlier = previous.and_then(|seen| phase_of(seen, &computer.fingerprint));
                let phase = match (computer.phase, earlier) {
                    (Phase::Stopping, Some(earlier)) => earlier,
                    (phase, _) => phase,
                };
                (computer.fingerprint.clone(), phase)
            })
            .collect();
        Self {
            phases,
            enabled: input
                .computers
                .iter()
                .filter(|computer| computer.enabled)
                .map(|computer| computer.fingerprint.clone())
                .collect(),
            paused: input.paused,
            dimming: input.dimming,
            transfer: input
                .clipboard
                .as_ref()
                .and_then(|clipboard| clipboard.last.clone()),
        }
    }
}

fn phase_of(seen: &Seen, fingerprint: &str) -> Option<Phase> {
    seen.phases
        .iter()
        .find(|(key, _)| key == fingerprint)
        .map(|(_, phase)| *phase)
}

/// Notices a pair of snapshots raises, lowest priority first so a later, higher one wins.
fn notices(previous: &Seen, seen: &Seen, input: &IslandInput) -> Vec<(NoticeKind, Activity)> {
    let mut raised = Vec::new();
    if let (Some(transfer), Some(clipboard)) = (&seen.transfer, &input.clipboard)
        && clipboard.last.is_some()
        && previous
            .transfer
            .as_ref()
            .is_none_or(|earlier| !transfer.same_as(earlier))
    {
        let name = name_of(input, &transfer.peer);
        let text = match transfer.direction {
            Direction::Received => format!("From {name}"),
            Direction::Sent => format!("To {name}"),
        };
        raised.push((
            NoticeKind::Clipboard,
            Activity {
                id: ActivityId::Clipboard,
                leading: Leading::Symbol(Symbol::DocOnClipboard, Tone::Foreground),
                trailing: Trailing::Text(text),
            },
        ));
    }
    for computer in &input.computers {
        let Some(before) = phase_of(previous, &computer.fingerprint) else {
            continue;
        };
        let Some(now) = phase_of(seen, &computer.fingerprint) else {
            continue;
        };
        if before != Phase::Sharing && now == Phase::Sharing {
            raised.push((
                NoticeKind::Connected,
                Activity {
                    id: ActivityId::Connected,
                    leading: Leading::Dot(Tone::Success),
                    trailing: Trailing::Text(computer.name.clone()),
                },
            ));
        }
        let user_stopped =
            !computer.enabled || seen.paused || !previous.enabled.contains(&computer.fingerprint);
        let lost = matches!(
            now,
            Phase::Reconnecting | Phase::Error | Phase::Off | Phase::Connecting
        );
        if before == Phase::Sharing && lost && !user_stopped {
            raised.push((
                NoticeKind::Dropped,
                Activity {
                    id: ActivityId::Dropped,
                    leading: Leading::Dot(Tone::Destructive),
                    trailing: Trailing::Text(format!("Lost {}", computer.name)),
                },
            ));
        }
    }
    let (was, is) = (previous.dimming, seen.dimming);
    if was.dimmed != is.dimmed || (is.dimmed && was.level != is.level) {
        raised.push((NoticeKind::Dimming, dimming_activity(is)));
    }
    raised
}

fn dimming_activity(dimming: DimmingInput) -> Activity {
    Activity {
        id: ActivityId::Dimming,
        leading: Leading::Symbol(
            if dimming.dimmed {
                Symbol::MoonFill
            } else {
                Symbol::SunMax
            },
            Tone::Foreground,
        ),
        trailing: if dimming.dimmed {
            Trailing::Level(f64::from(dimming.level) / 100.0)
        } else {
            Trailing::Text("Off".to_owned())
        },
    }
}

fn update_activity() -> Activity {
    Activity {
        id: ActivityId::Update,
        leading: Leading::Symbol(Symbol::ArrowDownCircle, Tone::Foreground),
        trailing: Trailing::Text("Update ready".to_owned()),
    }
}

fn name_of(input: &IslandInput, fingerprint: &str) -> String {
    input
        .computers
        .iter()
        .find(|computer| computer.fingerprint.eq_ignore_ascii_case(fingerprint))
        .map(|computer| computer.name.clone())
        .unwrap_or_else(|| "another computer".to_owned())
}

/// The computer this Mac's keyboard and pointer are on now.
fn controlled(input: &IslandInput) -> Option<&ComputerInput> {
    input.computers.iter().find(|computer| {
        computer.phase == Phase::Sharing && !computer.held && computer.pointer_here == Some(false)
    })
}

/// The activity shown while no notice is up: the first that applies.
fn standing(input: &IslandInput) -> Option<Activity> {
    if input
        .computers
        .iter()
        .any(|computer| computer.enabled && computer.phase == Phase::Reconnecting)
    {
        return Some(Activity {
            id: ActivityId::Reconnecting,
            leading: Leading::Spinner,
            trailing: Trailing::Text("Reconnecting".to_owned()),
        });
    }
    if let Some(computer) = input
        .computers
        .iter()
        .find(|computer| computer.phase == Phase::Sharing && computer.held)
    {
        return Some(Activity {
            id: ActivityId::Held,
            leading: Leading::Dot(Tone::Warning),
            trailing: Trailing::Text(format!("Waiting for {}", computer.name)),
        });
    }
    controlled(input).map(|computer| Activity {
        id: ActivityId::Controlling,
        leading: Leading::Symbol(Symbol::ArrowRight, Tone::Success),
        trailing: Trailing::Labeled(
            Symbol::for_platform(computer.platform),
            computer.name.clone(),
        ),
    })
}

/// The header's status, worded and toned as the tray and the window's pill word it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Attention,
    Reconnecting,
    Sharing,
    Connecting,
    Connected,
    Stopping,
    Paused,
    NotConnected,
}

impl Status {
    const ALL: [Self; 8] = [
        Self::Attention,
        Self::Reconnecting,
        Self::Sharing,
        Self::Connecting,
        Self::Connected,
        Self::Stopping,
        Self::Paused,
        Self::NotConnected,
    ];

    fn of(input: &IslandInput) -> Self {
        match input.phase {
            Phase::Error => Self::Attention,
            _ if input.sharing_active && input.held => Self::Reconnecting,
            _ if input.sharing_active => Self::Sharing,
            Phase::Reconnecting => Self::Reconnecting,
            Phase::Connecting | Phase::Starting => Self::Connecting,
            Phase::Connected => Self::Connected,
            Phase::Stopping => Self::Stopping,
            _ if input.paused => Self::Paused,
            _ => Self::NotConnected,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Attention => "Needs attention",
            Self::Reconnecting => "Reconnecting",
            Self::Sharing => "Sharing",
            Self::Connecting => "Connecting…",
            Self::Connected => "Connected",
            Self::Stopping => "Stopping…",
            Self::Paused => "Paused",
            Self::NotConnected => "Not connected",
        }
    }

    fn tone(self) -> Tone {
        match self {
            Self::Attention => Tone::Destructive,
            Self::Reconnecting | Self::Connecting | Self::Stopping => Tone::Warning,
            Self::Sharing | Self::Connected => Tone::Success,
            Self::Paused | Self::NotConnected => Tone::Subtle,
        }
    }
}

/// Every word the header can show, so the open island makes room for the widest once.
pub fn status_words() -> impl Iterator<Item = &'static str> {
    Status::ALL.into_iter().map(Status::word)
}

/// One computer's line, in the window's words for the same state.
fn computer_status(computer: &ComputerInput, paused: bool) -> String {
    if !computer.enabled {
        return "Off".to_owned();
    }
    if paused {
        return "Paused".to_owned();
    }
    match computer.phase {
        Phase::Sharing if computer.held => "Reconnecting…".to_owned(),
        Phase::Sharing if computer.pointer_here == Some(false) => {
            format!("Pointer on {}", computer.name)
        }
        Phase::Sharing => "Pointer on this Mac".to_owned(),
        Phase::Reconnecting => "Reconnecting…".to_owned(),
        Phase::Connected => "Connected".to_owned(),
        Phase::Stopping => "Stopping…".to_owned(),
        Phase::Error => "Can't connect".to_owned(),
        Phase::NotPaired => "Not paired".to_owned(),
        Phase::Off | Phase::Connecting | Phase::Starting => "Connecting…".to_owned(),
    }
}

fn expanded(input: &IslandInput) -> Expanded {
    let status = Status::of(input);
    let mut rows: Vec<Row> = input
        .computers
        .iter()
        .take(MAX_COMPUTER_ROWS)
        .map(|computer| {
            Row::Computer(ComputerRow {
                fingerprint: computer.fingerprint.clone(),
                name: computer.name.clone(),
                platform: computer.platform,
                tone: computer.tone,
                status: computer_status(computer, input.paused),
                enabled: computer.enabled,
            })
        })
        .collect();
    if rows.is_empty() {
        rows.push(Row::SetUp);
    }
    rows.push(Row::Dimming(input.dimming));
    if let Some(clipboard) = &input.clipboard {
        rows.push(Row::Clipboard {
            enabled: clipboard.enabled,
            detail: clipboard.last.as_ref().map(transfer_text),
        });
    }
    Expanded {
        status: status.word().to_owned(),
        status_tone: status.tone(),
        rows,
    }
}

/// "Sent an image (1.4 MB) 12 s ago", as the Home card words the last transfer.
pub fn transfer_text(transfer: &Transfer) -> String {
    let verb = match transfer.direction {
        Direction::Sent => "Sent",
        Direction::Received => "Received",
    };
    let kind = match transfer.kind {
        TransferKind::Text => "text",
        TransferKind::Image => "an image",
    };
    format!(
        "{verb} {kind} ({}) {}",
        human_bytes(transfer.bytes),
        age_text(transfer.age_seconds)
    )
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    value /= 1024.0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let rounded = if value < 10.0 {
        (value * 10.0).round() / 10.0
    } else {
        value.round()
    };
    format!("{rounded} {}", UNITS[unit])
}

fn age_text(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds} s ago");
    }
    let minutes = (seconds as f64 / 60.0).round() as u64;
    if minutes < 60 {
        return format!("{minutes} min ago");
    }
    format!("{} h ago", (minutes as f64 / 60.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn computer(name: &str, phase: Phase) -> ComputerInput {
        ComputerInput {
            fingerprint: name.to_ascii_lowercase(),
            name: name.to_owned(),
            platform: Platform::Windows,
            enabled: true,
            phase,
            held: false,
            pointer_here: (phase == Phase::Sharing).then_some(true),
            tone: 0,
        }
    }

    fn input(computers: Vec<ComputerInput>) -> IslandInput {
        IslandInput {
            computers,
            paused: false,
            phase: Phase::Off,
            sharing_active: false,
            held: false,
            dimming: DimmingInput {
                dimmed: false,
                level: 50,
                min_level: 10,
                max_level: 99,
            },
            clipboard: Some(ClipboardInput {
                enabled: true,
                last: None,
            }),
            update_ready: None,
        }
    }

    fn shown(presentation: &Presentation) -> Option<ActivityId> {
        presentation.compact.as_ref().map(|activity| activity.id)
    }

    fn text(presentation: &Presentation) -> String {
        match presentation
            .compact
            .as_ref()
            .map(|activity| &activity.trailing)
        {
            Some(Trailing::Text(text) | Trailing::Labeled(_, text)) => text.clone(),
            Some(Trailing::Level(level)) => format!("{level}"),
            None => String::new(),
        }
    }

    #[test]
    fn the_first_snapshot_raises_nothing_and_rests_idle() {
        let mut model = Model::default();
        let now = Instant::now();
        let presentation = model.update(&input(vec![computer("PC", Phase::Sharing)]), now);
        assert_eq!(presentation.compact, None);
        assert!(!presentation.input_away);
        assert_eq!(model.next_deadline(), None);
    }

    #[test]
    fn dimming_shows_for_its_time_and_updates_in_place() {
        let mut model = Model::default();
        let start = Instant::now();
        let mut state = input(vec![]);
        model.update(&state, start);
        state.dimming.dimmed = true;
        let presentation = model.update(&state, start);
        assert_eq!(shown(&presentation), Some(ActivityId::Dimming));
        assert_eq!(text(&presentation), "0.5");
        assert_eq!(model.next_deadline(), Some(start + DIMMING.1));
        // A new level updates the same notice in place and restarts its time.
        let later = start + Duration::from_millis(1000);
        state.dimming.level = 70;
        let presentation = model.update(&state, later);
        assert_eq!(shown(&presentation), Some(ActivityId::Dimming));
        assert_eq!(text(&presentation), "0.7");
        assert_eq!(model.next_deadline(), Some(later + DIMMING.1));
        // Undimming says Off; the level of an undimmed screen raises nothing.
        state.dimming.dimmed = false;
        assert_eq!(text(&model.update(&state, later)), "Off");
        let gone = later + DIMMING.1;
        assert_eq!(shown(&model.update(&state, gone)), None);
        state.dimming.level = 20;
        assert_eq!(shown(&model.update(&state, gone)), None);
    }

    #[test]
    fn a_higher_or_equal_priority_replaces_and_a_lower_one_is_dropped() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Connecting)]);
        model.update(&state, now);
        state.dimming.dimmed = true;
        model.update(&state, now);
        // Connected (2) does not replace dimming (3).
        state.computers[0].phase = Phase::Sharing;
        assert_eq!(shown(&model.update(&state, now)), Some(ActivityId::Dimming));
        // Once dimming ends, nothing replays.
        let after = now + DIMMING.1;
        assert_eq!(shown(&model.update(&state, after)), None);
        // Dropped (2) shows; then connected (2) replaces it.
        state.computers[0].phase = Phase::Reconnecting;
        assert_eq!(
            shown(&model.update(&state, after)),
            Some(ActivityId::Dropped)
        );
        assert_eq!(text(&model.update(&state, after)), "Lost PC");
        state.computers[0].phase = Phase::Sharing;
        assert_eq!(
            shown(&model.update(&state, after)),
            Some(ActivityId::Connected)
        );
        // Dimming (3) replaces connected.
        state.dimming.dimmed = false;
        assert_eq!(
            shown(&model.update(&state, after)),
            Some(ActivityId::Dimming)
        );
    }

    #[test]
    fn a_computer_the_user_switched_off_or_paused_is_not_lost() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        state.computers[0].enabled = false;
        state.computers[0].phase = Phase::Off;
        assert_eq!(shown(&model.update(&state, now)), None);
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        let mut model = Model::default();
        model.update(&state, now);
        state.paused = true;
        state.computers[0].phase = Phase::Off;
        assert_eq!(shown(&model.update(&state, now)), None);
    }

    #[test]
    fn stopping_passes_through_without_hiding_a_drop() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        state.computers[0].phase = Phase::Stopping;
        assert_eq!(shown(&model.update(&state, now)), None);
        state.computers[0].phase = Phase::Error;
        assert_eq!(shown(&model.update(&state, now)), Some(ActivityId::Dropped));
    }

    #[test]
    fn hovering_holds_a_notice_and_leaving_restarts_its_full_time() {
        let mut model = Model::default();
        let start = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Connecting)]);
        model.update(&state, start);
        state.computers[0].phase = Phase::Sharing;
        model.update(&state, start);
        model.set_hovering(true, start + Duration::from_millis(500));
        assert_eq!(model.next_deadline(), None);
        let long_after = start + Duration::from_secs(60);
        assert_eq!(
            shown(&model.update(&state, long_after)),
            Some(ActivityId::Connected)
        );
        model.set_hovering(false, long_after);
        assert_eq!(model.next_deadline(), Some(long_after + CONNECTED.1));
        assert_eq!(shown(&model.update(&state, long_after + CONNECTED.1)), None);
    }

    #[test]
    fn hovering_after_a_notice_ran_out_does_not_bring_it_back() {
        let mut model = Model::default();
        let start = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Connecting)]);
        model.update(&state, start);
        state.computers[0].phase = Phase::Sharing;
        model.update(&state, start);
        // The pointer arrives after the notice's time, before the next snapshot is read.
        let late = start + CONNECTED.1 + Duration::from_millis(1);
        model.set_hovering(true, late);
        assert_eq!(model.next_deadline(), None);
        assert_eq!(shown(&model.update(&state, late)), None);
    }

    #[test]
    fn a_notice_raised_while_hovering_waits_for_the_pointer_to_leave() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![]);
        model.update(&state, now);
        model.set_hovering(true, now);
        state.dimming.dimmed = true;
        model.update(&state, now);
        assert_eq!(model.next_deadline(), None);
    }

    #[test]
    fn clipboard_notices_name_the_other_computer_once_per_transfer() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        let transfer = Transfer {
            direction: Direction::Received,
            kind: TransferKind::Image,
            bytes: 1_468_006,
            peer: "pc".to_owned(),
            age_seconds: 0,
        };
        state.clipboard.as_mut().unwrap().last = Some(transfer.clone());
        let presentation = model.update(&state, now);
        assert_eq!(shown(&presentation), Some(ActivityId::Clipboard));
        assert_eq!(text(&presentation), "From PC");
        // Aging is the same transfer: no new notice once the first one ends.
        let later = now + CLIPBOARD.1;
        state
            .clipboard
            .as_mut()
            .unwrap()
            .last
            .as_mut()
            .unwrap()
            .age_seconds = 3;
        assert_eq!(shown(&model.update(&state, later)), None);
        // An identical copy sent again is new: its age starts over.
        state.clipboard.as_mut().unwrap().last = Some(Transfer {
            direction: Direction::Sent,
            age_seconds: 0,
            ..transfer
        });
        assert_eq!(text(&model.update(&state, later)), "To PC");
    }

    #[test]
    fn an_update_is_announced_once_per_version() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![]);
        state.update_ready = Some("0.2.9".to_owned());
        assert_eq!(shown(&model.update(&state, now)), Some(ActivityId::Update));
        let later = now + UPDATE.1;
        assert_eq!(shown(&model.update(&state, later)), None);
        state.update_ready = Some("0.3.0".to_owned());
        assert_eq!(
            shown(&model.update(&state, later)),
            Some(ActivityId::Update)
        );
    }

    #[test]
    fn a_new_model_stays_quiet_about_an_update_already_announced() {
        let mut first = Model::default();
        let now = Instant::now();
        let mut state = input(vec![]);
        state.update_ready = Some("0.2.9".to_owned());
        assert_eq!(shown(&first.update(&state, now)), Some(ActivityId::Update));
        let mut next = Model::remembering(first.update_shown().map(str::to_owned));
        assert_eq!(shown(&next.update(&state, now)), None);
        state.update_ready = Some("0.3.0".to_owned());
        assert_eq!(shown(&next.update(&state, now)), Some(ActivityId::Update));
        assert_eq!(next.update_shown(), Some("0.3.0"));
    }

    #[test]
    fn an_update_blocked_by_a_louder_notice_shows_once_that_ends() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![]);
        model.update(&state, now);
        state.dimming.dimmed = true;
        state.update_ready = Some("0.2.9".to_owned());
        assert_eq!(shown(&model.update(&state, now)), Some(ActivityId::Dimming));
        assert_eq!(
            shown(&model.update(&state, now + DIMMING.1)),
            Some(ActivityId::Update)
        );
    }

    #[test]
    fn standing_activities_follow_their_order() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut held = computer("Desk", Phase::Sharing);
        held.held = true;
        let mut away = computer("PC", Phase::Sharing);
        away.pointer_here = Some(false);
        let reconnecting = computer("Laptop", Phase::Reconnecting);
        let state = input(vec![away.clone(), held.clone(), reconnecting]);
        assert_eq!(
            shown(&model.update(&state, now)),
            Some(ActivityId::Reconnecting)
        );
        let state = input(vec![away.clone(), held]);
        let presentation = Model::default().update(&state, now);
        assert_eq!(shown(&presentation), Some(ActivityId::Held));
        assert_eq!(text(&presentation), "Waiting for Desk");
        let presentation = Model::default().update(&input(vec![away]), now);
        assert_eq!(shown(&presentation), Some(ActivityId::Controlling));
        assert!(presentation.input_away);
        assert_eq!(
            presentation.compact.unwrap().trailing,
            Trailing::Labeled(Symbol::Pc, "PC".to_owned())
        );
    }

    #[test]
    fn input_landing_here_from_the_other_computer_is_not_away() {
        // The receiving computer reports its pointer as local while the other keyboard drives it.
        let presentation =
            Model::default().update(&input(vec![computer("PC", Phase::Sharing)]), Instant::now());
        assert!(!presentation.input_away);
        let rows = &presentation.expanded.rows;
        assert!(matches!(&rows[0], Row::Computer(row) if row.status == "Pointer on this Mac"));
    }

    #[test]
    fn the_open_island_lists_three_computers_then_dimming_and_clipboard() {
        let mut computers: Vec<ComputerInput> = ["A", "B", "C", "D"]
            .into_iter()
            .map(|name| computer(name, Phase::Off))
            .collect();
        computers[0].phase = Phase::Sharing;
        computers[0].pointer_here = Some(false);
        computers[1].phase = Phase::Reconnecting;
        computers[2].enabled = false;
        let mut state = input(computers);
        state.sharing_active = true;
        state.phase = Phase::Sharing;
        state.clipboard.as_mut().unwrap().last = Some(Transfer {
            direction: Direction::Sent,
            kind: TransferKind::Image,
            bytes: 1_468_006,
            peer: "a".to_owned(),
            age_seconds: 12,
        });
        let expanded = Model::default().update(&state, Instant::now()).expanded;
        assert_eq!(
            (expanded.status.as_str(), expanded.status_tone),
            ("Sharing", Tone::Success)
        );
        let statuses: Vec<&str> = expanded
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Computer(row) => Some(row.status.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, ["Pointer on A", "Reconnecting…", "Off"]);
        assert!(matches!(expanded.rows[3], Row::Dimming(_)));
        assert_eq!(
            expanded.rows[4],
            Row::Clipboard {
                enabled: true,
                detail: Some("Sent an image (1.4 MB) 12 s ago".to_owned())
            }
        );
        // Without paired computers the first row sets one up; without the feature, no clipboard row.
        let mut empty = input(vec![]);
        empty.clipboard = None;
        let rows = Model::default()
            .update(&empty, Instant::now())
            .expanded
            .rows;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], Row::SetUp);
    }

    #[test]
    fn the_header_words_each_state_as_the_tray_does() {
        let mut state = input(vec![]);
        let header = |state: &IslandInput| {
            let expanded = Model::default().update(state, Instant::now()).expanded;
            (expanded.status, expanded.status_tone)
        };
        assert_eq!(header(&state), ("Not connected".to_owned(), Tone::Subtle));
        state.paused = true;
        assert_eq!(header(&state).0, "Paused");
        state.phase = Phase::Connecting;
        assert_eq!(header(&state), ("Connecting…".to_owned(), Tone::Warning));
        state.phase = Phase::Sharing;
        state.sharing_active = true;
        assert_eq!(header(&state).0, "Sharing");
        state.held = true;
        assert_eq!(header(&state).0, "Reconnecting");
        state.phase = Phase::Error;
        assert_eq!(
            header(&state),
            ("Needs attention".to_owned(), Tone::Destructive)
        );
    }

    #[test]
    fn every_header_status_is_listed_for_sizing() {
        let phases = [
            Phase::Off,
            Phase::Connecting,
            Phase::Connected,
            Phase::Starting,
            Phase::Sharing,
            Phase::Reconnecting,
            Phase::Stopping,
            Phase::Error,
            Phase::NotPaired,
        ];
        let mut reached = Vec::new();
        for (phase, flags) in phases
            .into_iter()
            .flat_map(|phase| (0..8u8).map(move |flags| (phase, flags)))
        {
            let mut state = input(vec![]);
            state.phase = phase;
            state.sharing_active = flags & 1 != 0;
            state.held = flags & 2 != 0;
            state.paused = flags & 4 != 0;
            let status = Status::of(&state);
            assert!(Status::ALL.contains(&status), "{status:?}");
            if !reached.contains(&status) {
                reached.push(status);
            }
        }
        assert_eq!(reached.len(), Status::ALL.len());
        assert_eq!(status_words().count(), Status::ALL.len());
    }

    #[test]
    fn transfer_lines_match_the_home_card() {
        let line = |bytes, age_seconds| {
            transfer_text(&Transfer {
                direction: Direction::Received,
                kind: TransferKind::Text,
                bytes,
                peer: String::new(),
                age_seconds,
            })
        };
        assert_eq!(line(512, 5), "Received text (512 B) 5 s ago");
        assert_eq!(line(15 * 1024, 90), "Received text (15 KB) 2 min ago");
        assert_eq!(
            line(3 * 1024 * 1024 * 1024, 7200),
            "Received text (3 GB) 2 h ago"
        );
    }
}
