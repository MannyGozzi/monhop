//! What the island shows, from snapshots of the app's state: a short-lived notice, else a standing
//! activity, else nothing. Pure and clock-injected.

use std::time::{Duration, Instant};

/// How long each notice shows, and which may replace which.
const DROPPED: (u8, Duration) = (2, Duration::from_secs(4));
const CONNECTED: (u8, Duration) = (2, Duration::from_millis(2500));
const CLIPBOARD: (u8, Duration) = (1, Duration::from_millis(2500));
/// The longest a connecting or reconnecting spinner stands in one episode.
const SPINNER: Duration = Duration::from_secs(15);

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

    /// On its way to a connection.
    fn linking(self) -> bool {
        matches!(self, Self::Connecting | Self::Starting | Self::Reconnecting)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComputerInput {
    /// Lowercase.
    pub fingerprint: String,
    pub name: String,
    pub enabled: bool,
    pub phase: Phase,
    /// The session is up but the other computer stopped answering.
    pub held: bool,
    /// Sessions the sharing core counted as dropped; None while its view does not list the
    /// computer.
    pub drops: Option<u64>,
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

/// One snapshot of everything the island reads.
#[derive(Clone, Debug, PartialEq)]
pub struct IslandInput {
    /// Paired computers, those switched on first.
    pub computers: Vec<ComputerInput>,
    pub paused: bool,
    /// The user is arranging displays; the island rests idle meanwhile.
    pub editing: bool,
    /// The last clipboard transfer, while the clipboard reports one.
    pub transfer: Option<Transfer>,
    /// The user turned clipboard notices on; while off, transfers raise nothing.
    pub clipboard_notices: bool,
}

/// The dots' tones, each a dark-theme token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warning,
    Destructive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Symbol {
    DocOnClipboard,
}

impl Symbol {
    pub fn system_name(self) -> &'static str {
        match self {
            Self::DocOnClipboard => "doc.on.clipboard",
        }
    }
}

/// The left wing; a symbol takes the text's color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leading {
    Symbol(Symbol),
    Dot(Tone),
    Spinner,
}

/// Which activity is up; a change of id replays the entrance, a change of words does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityId {
    Dropped,
    Connected,
    Clipboard,
    Reconnecting,
    Held,
    Connecting,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub id: ActivityId,
    pub leading: Leading,
    /// The right wing's words.
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoticeKind {
    Dropped,
    Connected,
    Clipboard,
}

impl NoticeKind {
    fn rule(self) -> (u8, Duration) {
        match self {
            Self::Dropped => DROPPED,
            Self::Connected => CONNECTED,
            Self::Clipboard => CLIPBOARD,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Notice {
    kind: NoticeKind,
    activity: Activity,
    until: Instant,
}

/// What the model remembers of one computer between snapshots.
#[derive(Clone, Debug, PartialEq)]
struct Track {
    fingerprint: String,
    /// The last phase other than `Stopping`, which only passes through.
    phase: Phase,
    /// Switched on, with sharing not paused.
    on: bool,
    /// As the sharing view listed it, so a count reappearing after an unlisted stretch is no drop.
    drops: Option<u64>,
    /// It left sharing without a drop and has not shared since: a deliberate restart, which starts
    /// no spinner. An error, a drop, switching it off or pausing ends it.
    restarting: bool,
    episode: Option<Episode>,
}

/// A stretch of linking a spinner may stand for: from rest or a drop until the computer shares, is
/// switched off or paused. Retries and further drops inside it neither end nor restart it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Episode {
    /// It began with a drop.
    reconnecting: bool,
    since: Instant,
}

/// The parts of a snapshot later snapshots are compared with.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    at: Instant,
    /// In the snapshot's order.
    computers: Vec<Track>,
    transfer: Option<Transfer>,
}

#[derive(Default)]
pub struct Model {
    seen: Option<Seen>,
    notice: Option<Notice>,
}

impl Model {
    /// Takes a snapshot: compares it with the last one to raise notices, then shows the notice up,
    /// else the standing activity. Arranging shows nothing and leaves no notice behind.
    pub fn update(&mut self, input: &IslandInput, now: Instant) -> Option<Activity> {
        self.expire(now);
        let previous = self.seen.take();
        let seen = Seen::of(input, previous.as_ref(), now);
        if input.editing {
            self.notice = None;
        } else if let Some(previous) = &previous {
            for (kind, activity) in notices(previous, &seen, input) {
                self.raise(kind, activity, now);
            }
        }
        let shown = if input.editing {
            None
        } else {
            self.notice
                .as_ref()
                .map(|notice| notice.activity.clone())
                .or_else(|| standing(input, &seen, now))
        };
        self.seen = Some(seen);
        shown
    }

    /// When the shown notice ends or a spinner reaches its limit, so the driver wakes then.
    pub fn next_deadline(&self) -> Option<Instant> {
        let limits = self.seen.iter().flat_map(|seen| {
            seen.computers
                .iter()
                .filter_map(|track| track.episode)
                .map(|episode| episode.since + SPINNER)
                .filter(|limit| *limit > seen.at)
        });
        self.notice
            .as_ref()
            .map(|notice| notice.until)
            .into_iter()
            .chain(limits)
            .min()
    }

    fn expire(&mut self, now: Instant) {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.until <= now)
        {
            self.notice = None;
        }
    }

    fn raise(&mut self, kind: NoticeKind, activity: Activity, now: Instant) {
        let (priority, duration) = kind.rule();
        if self
            .notice
            .as_ref()
            .is_some_and(|shown| shown.kind.rule().0 > priority)
        {
            return;
        }
        self.notice = Some(Notice {
            kind,
            activity,
            until: now + duration,
        });
    }
}

impl Seen {
    fn of(input: &IslandInput, previous: Option<&Seen>, now: Instant) -> Self {
        let computers = input
            .computers
            .iter()
            .map(|computer| {
                let earlier = previous.and_then(|seen| seen.track(&computer.fingerprint));
                Track::of(computer, earlier, input, now)
            })
            .collect();
        Self {
            at: now,
            computers,
            transfer: input.transfer.clone(),
        }
    }

    fn track(&self, fingerprint: &str) -> Option<&Track> {
        self.computers
            .iter()
            .find(|track| track.fingerprint == fingerprint)
    }
}

impl Track {
    fn of(
        computer: &ComputerInput,
        earlier: Option<&Track>,
        input: &IslandInput,
        now: Instant,
    ) -> Self {
        let phase = match (computer.phase, earlier) {
            (Phase::Stopping, Some(earlier)) => earlier.phase,
            (phase, _) => phase,
        };
        let on = computer.enabled && !input.paused;
        let lost = !input.editing && earlier.is_some_and(|before| lost(before, computer, on));
        let restarting = on
            && !lost
            && !matches!(phase, Phase::Sharing | Phase::Error)
            && earlier.is_some_and(|before| {
                before.restarting || (before.on && before.phase == Phase::Sharing)
            });
        // Sharing counts as rest too: leaving it is either a drop or a restart.
        let from_rest = earlier.is_none_or(|before| !before.on || !before.phase.linking());
        let episode = if !on || phase == Phase::Sharing {
            None
        } else if lost {
            Some(Episode {
                reconnecting: true,
                since: now,
            })
        } else {
            earlier.and_then(|before| before.episode).or_else(|| {
                (!input.editing && !restarting && from_rest && phase.linking()).then_some(Episode {
                    reconnecting: false,
                    since: now,
                })
            })
        };
        Self {
            fingerprint: computer.fingerprint.clone(),
            phase,
            on,
            drops: computer.drops,
            restarting,
            episode,
        }
    }
}

/// The sharing core counted a new drop for a computer the user still has on, outside an episode:
/// a session that keeps failing before it shares is lost once. Phases never tell, since a
/// deliberate restart passes through the same ones.
fn lost(before: &Track, computer: &ComputerInput, on: bool) -> bool {
    on && before.episode.is_none()
        && before
            .drops
            .zip(computer.drops)
            .is_some_and(|(before, now)| now > before)
}

/// Notices a pair of snapshots raises, lowest priority first so a later, higher one wins.
fn notices(previous: &Seen, seen: &Seen, input: &IslandInput) -> Vec<(NoticeKind, Activity)> {
    let mut raised = Vec::new();
    if input.clipboard_notices
        && let Some(transfer) = &seen.transfer
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
                leading: Leading::Symbol(Symbol::DocOnClipboard),
                text,
            },
        ));
    }
    for (computer, track) in input.computers.iter().zip(&seen.computers) {
        let Some(before) = previous.track(&computer.fingerprint) else {
            continue;
        };
        let lost = lost(before, computer, track.on);
        if lost {
            raised.push((
                NoticeKind::Dropped,
                Activity {
                    id: ActivityId::Dropped,
                    leading: Leading::Dot(Tone::Destructive),
                    text: format!("Lost {}", computer.name),
                },
            ));
        }
        // A drop already shared again by this snapshot ends as a connection.
        if track.phase == Phase::Sharing && (before.phase != Phase::Sharing || lost) {
            raised.push((
                NoticeKind::Connected,
                Activity {
                    id: ActivityId::Connected,
                    leading: Leading::Dot(Tone::Success),
                    text: computer.name.clone(),
                },
            ));
        }
    }
    raised
}

fn name_of(input: &IslandInput, fingerprint: &str) -> String {
    input
        .computers
        .iter()
        .find(|computer| computer.fingerprint.eq_ignore_ascii_case(fingerprint))
        .map(|computer| computer.name.clone())
        .unwrap_or_else(|| "another computer".to_owned())
}

/// The activity shown while no notice is up: the first that applies. A spinner stands only for the
/// start of its episode.
fn standing(input: &IslandInput, seen: &Seen, now: Instant) -> Option<Activity> {
    let spinning = |reconnecting: bool| {
        seen.computers
            .iter()
            .filter_map(|track| track.episode)
            .any(|episode| episode.reconnecting == reconnecting && now < episode.since + SPINNER)
    };
    let spinner = |id, text: &str| Activity {
        id,
        leading: Leading::Spinner,
        text: text.to_owned(),
    };
    if spinning(true) {
        return Some(spinner(ActivityId::Reconnecting, "Reconnecting"));
    }
    if let Some(computer) = input
        .computers
        .iter()
        .find(|computer| computer.phase == Phase::Sharing && computer.held)
    {
        return Some(Activity {
            id: ActivityId::Held,
            leading: Leading::Dot(Tone::Warning),
            text: format!("Waiting for {}", computer.name),
        });
    }
    spinning(false).then(|| spinner(ActivityId::Connecting, "Connecting"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn computer(name: &str, phase: Phase) -> ComputerInput {
        ComputerInput {
            fingerprint: name.to_ascii_lowercase(),
            name: name.to_owned(),
            enabled: true,
            phase,
            held: false,
            drops: Some(0),
        }
    }

    fn input(computers: Vec<ComputerInput>) -> IslandInput {
        IslandInput {
            computers,
            paused: false,
            editing: false,
            transfer: None,
            clipboard_notices: true,
        }
    }

    /// The sharing core counts a drop of the computer's session.
    fn lose(computer: &mut ComputerInput) {
        computer.drops = computer.drops.map(|drops| drops + 1);
    }

    fn shown(activity: &Option<Activity>) -> Option<ActivityId> {
        activity.as_ref().map(|activity| activity.id)
    }

    fn text(activity: &Option<Activity>) -> &str {
        activity
            .as_ref()
            .map_or("", |activity| activity.text.as_str())
    }

    fn received(peer: &str) -> Transfer {
        Transfer {
            direction: Direction::Received,
            kind: TransferKind::Image,
            bytes: 1_468_006,
            peer: peer.to_owned(),
            age_seconds: 0,
        }
    }

    /// Feeds the phases in turn on one clock, returning what each snapshot shows.
    fn walk(
        model: &mut Model,
        state: &mut IslandInput,
        now: Instant,
        phases: &[Phase],
    ) -> Vec<Option<ActivityId>> {
        phases
            .iter()
            .map(|phase| {
                state.computers[0].phase = *phase;
                shown(&model.update(state, now))
            })
            .collect()
    }

    #[test]
    fn the_first_snapshot_raises_nothing_and_rests_idle() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        state.transfer = Some(received("pc"));
        assert_eq!(model.update(&state, now), None);
        assert_eq!(model.next_deadline(), None);
    }

    #[test]
    fn a_computer_linking_at_launch_shows_connecting_from_the_first_snapshot() {
        let now = Instant::now();
        for phase in [Phase::Connecting, Phase::Starting] {
            let activity = Model::default().update(&input(vec![computer("PC", phase)]), now);
            assert_eq!(
                (shown(&activity), text(&activity)),
                (Some(ActivityId::Connecting), "Connecting")
            );
        }
    }

    #[test]
    fn connecting_shows_from_switching_on_until_sharing_then_connected_names_it() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Off)]);
        state.computers[0].enabled = false;
        assert_eq!(model.update(&state, now), None);
        state.computers[0].enabled = true;
        let walked = walk(
            &mut model,
            &mut state,
            now,
            &[
                Phase::Off,
                Phase::Connecting,
                Phase::Connected,
                Phase::Starting,
            ],
        );
        let connecting = Some(ActivityId::Connecting);
        assert_eq!(walked, [None, connecting, connecting, connecting]);
        state.computers[0].phase = Phase::Sharing;
        let activity = model.update(&state, now);
        assert_eq!(
            (shown(&activity), text(&activity)),
            (Some(ActivityId::Connected), "PC")
        );
        assert_eq!(model.next_deadline(), Some(now + CONNECTED.1));
        assert_eq!(model.update(&state, now + CONNECTED.1), None);
    }

    #[test]
    fn a_link_that_rests_up_without_connecting_first_shows_nothing() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Connected)]);
        assert_eq!(
            walk(
                &mut model,
                &mut state,
                now,
                &[Phase::Connected, Phase::Connected]
            ),
            [None, None]
        );
    }

    #[test]
    fn arranging_from_sharing_stays_quiet_until_done_shares_again() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        // Arrange ends the session and opens a link; editing turns on about a second later.
        assert_eq!(
            walk(
                &mut model,
                &mut state,
                now,
                &[Phase::Stopping, Phase::Connecting, Phase::Connected]
            ),
            [None, None, None]
        );
        state.editing = true;
        assert_eq!(
            walk(
                &mut model,
                &mut state,
                now,
                &[Phase::Connected, Phase::Reconnecting, Phase::Connected]
            ),
            [None, None, None]
        );
        // Done closes the link and the supervisor starts a session.
        state.editing = false;
        assert_eq!(
            walk(
                &mut model,
                &mut state,
                now,
                &[Phase::Stopping, Phase::Off, Phase::Starting]
            ),
            [None, None, None]
        );
        state.computers[0].phase = Phase::Sharing;
        let activity = model.update(&state, now);
        assert_eq!(
            (shown(&activity), text(&activity)),
            (Some(ActivityId::Connected), "PC")
        );
    }

    #[test]
    fn a_deliberate_restart_shows_no_drop_or_spinner_and_names_the_computer_once_it_shares() {
        let now = Instant::now();
        // A control change over a link, a layout change, and the other computer pausing, which
        // leaves this one retrying.
        let restarts: [&[Phase]; 3] = [
            &[
                Phase::Stopping,
                Phase::Off,
                Phase::Connecting,
                Phase::Connected,
                Phase::Stopping,
                Phase::Off,
                Phase::Starting,
            ],
            &[Phase::Stopping, Phase::Off, Phase::Starting],
            &[Phase::Starting; 40],
        ];
        for phases in restarts {
            let mut model = Model::default();
            let mut state = input(vec![computer("PC", Phase::Sharing)]);
            model.update(&state, now);
            let mut at = now;
            for phase in phases {
                at += Duration::from_secs(1);
                state.computers[0].phase = *phase;
                assert_eq!(model.update(&state, at), None, "{phase:?}");
            }
            state.computers[0].phase = Phase::Sharing;
            let activity = model.update(&state, at);
            assert_eq!(
                (shown(&activity), text(&activity)),
                (Some(ActivityId::Connected), "PC")
            );
        }
    }

    #[test]
    fn a_counted_drop_is_lost_then_reconnecting_through_retries_until_it_shares() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        // The session reports starting a moment before the core counts its drop.
        state.computers[0].phase = Phase::Starting;
        assert_eq!(model.update(&state, now), None);
        lose(&mut state.computers[0]);
        let activity = model.update(&state, now);
        assert_eq!(
            (shown(&activity), text(&activity)),
            (Some(ActivityId::Dropped), "Lost PC")
        );
        assert_eq!(model.next_deadline(), Some(now + DROPPED.1));
        assert_eq!(
            walk(
                &mut model,
                &mut state,
                now + DROPPED.1,
                &[
                    Phase::Error,
                    Phase::Off,
                    Phase::Starting,
                    Phase::Connecting,
                    Phase::Connected
                ]
            ),
            [Some(ActivityId::Reconnecting); 5]
        );
        // Sessions that come up and fail again before sharing raise nothing and keep the
        // episode's start, so its spinner still rests at the limit.
        lose(&mut state.computers[0]);
        assert_eq!(
            walk(&mut model, &mut state, now + DROPPED.1, &[Phase::Error]),
            [Some(ActivityId::Reconnecting)]
        );
        let rested = now + SPINNER;
        lose(&mut state.computers[0]);
        assert_eq!(
            walk(&mut model, &mut state, rested, &[Phase::Starting]),
            [None]
        );
        assert_eq!(model.next_deadline(), None);
        state.computers[0].phase = Phase::Sharing;
        assert_eq!(
            shown(&model.update(&state, rested)),
            Some(ActivityId::Connected)
        );
        // Shared again, the next drop is a new loss.
        let later = rested + CONNECTED.1;
        lose(&mut state.computers[0]);
        assert_eq!(
            walk(&mut model, &mut state, later, &[Phase::Starting]),
            [Some(ActivityId::Dropped)]
        );
    }

    #[test]
    fn a_drop_already_shared_again_by_the_next_snapshot_ends_as_a_connection() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        lose(&mut state.computers[0]);
        let activity = model.update(&state, now);
        assert_eq!(
            (shown(&activity), text(&activity)),
            (Some(ActivityId::Connected), "PC")
        );
        assert_eq!(model.update(&state, now + CONNECTED.1), None);
    }

    #[test]
    fn retrying_a_computer_that_is_asleep_shows_one_spinner_then_rests() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Starting)]);
        let first = model.update(&state, now);
        assert_eq!(shown(&first), Some(ActivityId::Connecting));
        for second in 1..=60 {
            let at = now + Duration::from_secs(second);
            state.computers[0].phase = if second % 2 == 1 {
                Phase::Error
            } else {
                Phase::Starting
            };
            // Unchanged throughout, so the island never replays its entrance.
            let expected = if at < now + SPINNER {
                first.clone()
            } else {
                None
            };
            assert_eq!(model.update(&state, at), expected, "after {second} s");
        }
        assert_eq!(model.next_deadline(), None);
        state.computers[0].phase = Phase::Sharing;
        assert_eq!(
            shown(&model.update(&state, now + Duration::from_secs(61))),
            Some(ActivityId::Connected)
        );
    }

    #[test]
    fn a_restart_that_errors_then_retries_shows_one_connecting_spinner_that_rests() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        assert_eq!(
            walk(
                &mut model,
                &mut state,
                now,
                &[Phase::Stopping, Phase::Off, Phase::Error]
            ),
            [None, None, None]
        );
        state.computers[0].phase = Phase::Starting;
        let first = model.update(&state, now);
        assert_eq!(shown(&first), Some(ActivityId::Connecting));
        for second in 1..=20 {
            let at = now + Duration::from_secs(second);
            state.computers[0].phase = if second % 2 == 1 {
                Phase::Error
            } else {
                Phase::Starting
            };
            let expected = if at < now + SPINNER {
                first.clone()
            } else {
                None
            };
            assert_eq!(model.update(&state, at), expected, "after {second} s");
        }
    }

    #[test]
    fn a_spinner_hidden_while_arranging_shows_again_only_within_its_limit() {
        let now = Instant::now();
        for (back, expected) in [
            (Duration::from_secs(10), Some(ActivityId::Connecting)),
            (SPINNER, None),
        ] {
            let mut model = Model::default();
            let mut state = input(vec![computer("PC", Phase::Connecting)]);
            assert_eq!(
                shown(&model.update(&state, now)),
                Some(ActivityId::Connecting)
            );
            state.editing = true;
            assert_eq!(model.update(&state, now + Duration::from_secs(5)), None);
            state.editing = false;
            assert_eq!(shown(&model.update(&state, now + back)), expected);
        }
    }

    #[test]
    fn a_count_the_view_stops_listing_is_no_drop_when_it_comes_back() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        // Switched off, the view no longer lists it; back on, it lists a higher count.
        let pc = &mut state.computers[0];
        (pc.enabled, pc.phase, pc.drops) = (false, Phase::Off, None);
        assert_eq!(model.update(&state, now), None);
        let pc = &mut state.computers[0];
        (pc.enabled, pc.phase, pc.drops) = (true, Phase::Starting, Some(3));
        assert_eq!(
            shown(&model.update(&state, now)),
            Some(ActivityId::Connecting)
        );
    }

    #[test]
    fn the_next_deadline_is_the_earlier_of_a_notice_ending_and_a_spinner_resting() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Connecting)]);
        model.update(&state, now);
        assert_eq!(model.next_deadline(), Some(now + SPINNER));
        let copied = now + Duration::from_secs(1);
        state.transfer = Some(received("pc"));
        model.update(&state, copied);
        assert_eq!(model.next_deadline(), Some(copied + CLIPBOARD.1));
        // A spinner past its limit wakes nothing.
        assert_eq!(model.update(&state, now + SPINNER), None);
        assert_eq!(model.next_deadline(), None);
    }

    #[test]
    fn switching_off_or_pausing_ends_an_episode_and_turning_back_on_connects_afresh() {
        let now = Instant::now();
        for pause in [false, true] {
            let mut model = Model::default();
            let mut state = input(vec![computer("PC", Phase::Sharing)]);
            model.update(&state, now);
            state.computers[0].phase = Phase::Error;
            lose(&mut state.computers[0]);
            assert_eq!(shown(&model.update(&state, now)), Some(ActivityId::Dropped));
            let later = now + DROPPED.1;
            assert_eq!(
                walk(&mut model, &mut state, later, &[Phase::Starting]),
                [Some(ActivityId::Reconnecting)]
            );
            let set_on = |state: &mut IslandInput, on: bool| {
                if pause {
                    state.paused = !on;
                } else {
                    state.computers[0].enabled = on;
                }
            };
            set_on(&mut state, false);
            assert_eq!(
                walk(
                    &mut model,
                    &mut state,
                    later,
                    &[Phase::Stopping, Phase::Off]
                ),
                [None, None]
            );
            set_on(&mut state, true);
            assert_eq!(
                walk(
                    &mut model,
                    &mut state,
                    later,
                    &[Phase::Off, Phase::Starting]
                ),
                [None, Some(ActivityId::Connecting)]
            );
        }
    }

    #[test]
    fn a_link_reconnecting_by_itself_is_no_drop() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Connected)]);
        assert_eq!(
            walk(
                &mut model,
                &mut state,
                now,
                &[Phase::Connected, Phase::Reconnecting, Phase::Connected]
            ),
            [
                None,
                Some(ActivityId::Connecting),
                Some(ActivityId::Connecting)
            ]
        );
    }

    #[test]
    fn a_computer_the_user_switched_off_or_paused_is_not_lost_or_connecting() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        state.computers[0].enabled = false;
        state.computers[0].phase = Phase::Off;
        // A drop counted as the switch went off is no news either.
        lose(&mut state.computers[0]);
        assert_eq!(model.update(&state, now), None);
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        let mut model = Model::default();
        model.update(&state, now);
        state.paused = true;
        state.computers[0].phase = Phase::Starting;
        assert_eq!(model.update(&state, now), None);
    }

    #[test]
    fn only_the_cores_count_reports_a_drop() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        assert_eq!(
            walk(
                &mut model,
                &mut state,
                now,
                &[Phase::Stopping, Phase::Error, Phase::Error]
            ),
            [None, None, None]
        );
        lose(&mut state.computers[0]);
        assert_eq!(shown(&model.update(&state, now)), Some(ActivityId::Dropped));
    }

    #[test]
    fn arranging_rests_idle_and_starts_no_episode() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![
            computer("PC", Phase::Sharing),
            computer("Laptop", Phase::Off),
        ]);
        state.computers[1].enabled = false;
        model.update(&state, now);
        state.transfer = Some(received("pc"));
        assert_eq!(
            shown(&model.update(&state, now)),
            Some(ActivityId::Clipboard)
        );
        state.editing = true;
        assert_eq!(model.update(&state, now), None);
        state.computers[1].enabled = true;
        state.computers[1].phase = Phase::Connecting;
        assert_eq!(model.update(&state, now), None);
        state.computers[1].phase = Phase::Connected;
        assert_eq!(model.update(&state, now), None);
        // Neither the notice nor a spinner comes back once arranging ends.
        state.editing = false;
        assert_eq!(model.update(&state, now), None);
    }

    #[test]
    fn a_higher_or_equal_priority_replaces_and_a_lower_one_is_dropped() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Starting)]);
        model.update(&state, now);
        state.computers[0].phase = Phase::Sharing;
        assert_eq!(
            shown(&model.update(&state, now)),
            Some(ActivityId::Connected)
        );
        // A copy (1) does not replace connected (2); nothing replays once connected ends.
        state.transfer = Some(received("pc"));
        assert_eq!(
            shown(&model.update(&state, now)),
            Some(ActivityId::Connected)
        );
        let after = now + CONNECTED.1;
        assert_eq!(model.update(&state, after), None);
        // Dropped (2) shows, then connected (2) replaces it.
        state.computers[0].phase = Phase::Starting;
        lose(&mut state.computers[0]);
        let activity = model.update(&state, after);
        assert_eq!(
            (shown(&activity), text(&activity)),
            (Some(ActivityId::Dropped), "Lost PC")
        );
        state.computers[0].phase = Phase::Sharing;
        assert_eq!(
            shown(&model.update(&state, after)),
            Some(ActivityId::Connected)
        );
    }

    #[test]
    fn standing_activities_follow_their_order() {
        let now = Instant::now();
        let mut held = computer("Desk", Phase::Sharing);
        held.held = true;
        let connecting = computer("Laptop", Phase::Connecting);
        let mut model = Model::default();
        let mut state = input(vec![
            computer("PC", Phase::Sharing),
            held.clone(),
            connecting.clone(),
        ]);
        model.update(&state, now);
        state.computers[0].phase = Phase::Starting;
        lose(&mut state.computers[0]);
        assert_eq!(shown(&model.update(&state, now)), Some(ActivityId::Dropped));
        let later = now + DROPPED.1;
        assert_eq!(
            shown(&model.update(&state, later)),
            Some(ActivityId::Reconnecting)
        );
        let activity = Model::default().update(&input(vec![held.clone(), connecting.clone()]), now);
        assert_eq!(
            (shown(&activity), text(&activity)),
            (Some(ActivityId::Held), "Waiting for Desk")
        );
        let activity = Model::default().update(&input(vec![connecting]), now);
        assert_eq!(
            (shown(&activity), text(&activity)),
            (Some(ActivityId::Connecting), "Connecting")
        );
    }

    #[test]
    fn clipboard_notices_name_the_other_computer_once_per_transfer() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        model.update(&state, now);
        let transfer = received("pc");
        state.transfer = Some(transfer.clone());
        let activity = model.update(&state, now);
        assert_eq!(
            (shown(&activity), text(&activity)),
            (Some(ActivityId::Clipboard), "From PC")
        );
        // Aging is the same transfer: no new notice once the first one ends.
        let later = now + CLIPBOARD.1;
        state.transfer.as_mut().unwrap().age_seconds = 3;
        assert_eq!(model.update(&state, later), None);
        // An identical copy sent again is new: its age starts over.
        state.transfer = Some(Transfer {
            direction: Direction::Sent,
            ..transfer
        });
        assert_eq!(text(&model.update(&state, later)), "To PC");
    }

    #[test]
    fn clipboard_copies_stay_quiet_while_notices_are_off_and_after_they_turn_on() {
        let mut model = Model::default();
        let now = Instant::now();
        let mut state = input(vec![computer("PC", Phase::Sharing)]);
        state.clipboard_notices = false;
        model.update(&state, now);
        state.transfer = Some(received("pc"));
        assert_eq!(model.update(&state, now), None);
        // Turned on, the copy made while off is old news; the next one shows.
        state.clipboard_notices = true;
        assert_eq!(model.update(&state, now), None);
        state.transfer = Some(Transfer {
            direction: Direction::Sent,
            ..received("pc")
        });
        assert_eq!(
            shown(&model.update(&state, now)),
            Some(ActivityId::Clipboard)
        );
    }
}
