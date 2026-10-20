//! `--check-island`: the island alone on the real screen, playing a fixed script of synthetic
//! snapshots through the real model, while the drawn outline is sampled every frame. It starts
//! nothing else MonHop runs, reads no saved state and saves nothing.

use std::{
    cell::RefCell,
    collections::VecDeque,
    time::{Duration, Instant},
};

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

use super::{
    macos::{self, Context, Motion, Probe},
    model::{
        Activity, ActivityId, ComputerInput, Direction, IslandInput, Model, Phase, Transfer,
        TransferKind,
    },
    tokens,
};

const GENERATION: u64 = 1;
/// Each settled state stays this long before the next step.
const HOLD: f64 = 1.2;
/// A step that has not come to rest by then fails.
const SETTLE_LIMIT: f64 = 3.0;
const DEADLINE: f64 = 40.0;
const SAMPLE: f64 = 1.0 / 240.0;
/// Presentation values are evaluated at the time they are read, so between two samples no point
/// moves farther than the motion's peak speed allows, plus this rounding.
const JUMP_SLACK: f64 = 1.0;
/// A settled outline lies within this many points of its target.
const REST_TOLERANCE: f64 = 0.5;
/// The interrupted step brings the island back to rest this long after it starts growing, about
/// halfway.
const INTERRUPT_AFTER: f64 = 0.08;
/// The synthetic clock passes this between steps: every notice has ended, but a spinner started
/// one step earlier still stands.
const BETWEEN: Duration = Duration::from_secs(10);
/// A second notice of the same kind arrives while the first is still up.
const SOON: Duration = Duration::from_millis(500);

pub fn run() -> ! {
    let Some(mtm) = MainThreadMarker::new() else {
        finish(Err("the check must run on the main thread".to_owned()));
    };
    if let Err(error) = tokens::tokens() {
        finish(Err(error.to_string()));
    }
    if !macos::has_notch() {
        finish(Err(
            "no display has a camera notch, the only place the island shows".to_owned(),
        ));
    }
    let steps = match script() {
        Ok(steps) => steps,
        Err(reason) => finish(Err(format!("script: {reason}"))),
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    macos::start(
        mtm,
        GENERATION,
        Context {
            motion: Motion::Always,
            showing: Box::new(|_| {}),
        },
    );
    let check = RefCell::new(Check::new(steps));
    let _sampler = macos::every(SAMPLE, move || {
        let verdict = check.borrow_mut().tick(mtm);
        if let Some(verdict) = verdict {
            finish(verdict);
        }
    });
    app.run();
    finish(Err(
        "the event loop ended before the check finished".to_owned()
    ))
}

fn finish(verdict: Result<(), String>) -> ! {
    match verdict {
        Ok(()) => {
            println!("MonHop island check: PASS");
            std::process::exit(0)
        }
        Err(reason) => {
            println!("MonHop island check: FAIL: {reason}");
            std::process::exit(1)
        }
    }
}

enum Order {
    Show(Option<Activity>),
    /// Shows this while the island must still be moving.
    Interrupt(Option<Activity>),
}

struct Step {
    name: &'static str,
    /// Seconds after the step starts, in order.
    orders: VecDeque<(f64, Order)>,
    /// The content on show updates where it is: never rebuilt, and its band never grows or
    /// shrinks.
    in_place: bool,
}

/// The model fed synthetic snapshots on a clock of its own.
struct Feed {
    model: Model,
    clock: Instant,
    input: IslandInput,
}

impl Feed {
    fn next(&mut self, after: Duration, change: impl FnOnce(&mut IslandInput)) -> Option<Activity> {
        change(&mut self.input);
        self.clock += after;
        self.model.update(&self.input, self.clock)
    }
}

fn computer(name: &str, enabled: bool) -> ComputerInput {
    ComputerInput {
        fingerprint: name.to_ascii_lowercase(),
        name: name.to_owned(),
        enabled,
        phase: Phase::Off,
        held: false,
        drops: Some(0),
    }
}

fn studio(input: &mut IslandInput) -> &mut ComputerInput {
    &mut input.computers[0]
}

fn desk(input: &mut IslandInput) -> &mut ComputerInput {
    &mut input.computers[1]
}

/// The computer's session drops, as the sharing core counts it.
fn lose(computer: &mut ComputerInput, phase: Phase) {
    computer.phase = phase;
    computer.drops = computer.drops.map(|drops| drops + 1);
}

fn copied(direction: Direction) -> Transfer {
    Transfer {
        direction,
        kind: TransferKind::Image,
        bytes: 1_468_006,
        peer: "studio".to_owned(),
        age_seconds: 0,
    }
}

fn shows(activity: &Option<Activity>, expected: Option<ActivityId>) -> Result<(), String> {
    let shown = activity.as_ref().map(|activity| activity.id);
    if shown == expected {
        Ok(())
    } else {
        Err(format!("expected {expected:?}, the model shows {shown:?}"))
    }
}

/// Idle and every activity the model shows: connecting, connected, a copy with notices off and
/// on (then the other way in place), a drop, reconnecting, waiting, a second computer connecting,
/// two drops in a row in place, and a growth interrupted mid-flight by a return to rest.
fn script() -> Result<Vec<Step>, String> {
    let mut feed = Feed {
        model: Model::default(),
        clock: Instant::now(),
        input: IslandInput {
            computers: vec![computer("Studio", true), computer("Desk PC", false)],
            paused: false,
            editing: false,
            transfer: None,
            clipboard_notices: false,
        },
    };
    use ActivityId::{Clipboard, Connected, Connecting, Dropped, Held, Reconnecting};
    let mut steps = Vec::new();
    let mut show = |name, activity: Option<Activity>, expected, in_place| {
        shows(&activity, expected).map_err(|reason| format!("{name}: {reason}"))?;
        steps.push(Step {
            name,
            orders: VecDeque::from([(0.0, Order::Show(activity))]),
            in_place,
        });
        Ok::<(), String>(())
    };

    show("idle", feed.next(Duration::ZERO, |_| {}), None, false)?;
    let connecting = feed.next(BETWEEN, |input| studio(input).phase = Phase::Connecting);
    show("connecting", connecting.clone(), Some(Connecting), false)?;
    let linked = feed.next(BETWEEN, |input| studio(input).phase = Phase::Connected);
    shows(&linked, Some(Connecting)).map_err(|reason| format!("link up: {reason}"))?;
    let connected = feed.next(BETWEEN, |input| studio(input).phase = Phase::Sharing);
    show("connected", connected, Some(Connected), false)?;
    let quiet = feed.next(BETWEEN, |input| {
        input.transfer = Some(copied(Direction::Received))
    });
    show("a copy with notices off", quiet, None, false)?;
    feed.next(BETWEEN, |input| input.clipboard_notices = true);
    let sent = feed.next(BETWEEN, |input| {
        input.transfer = Some(copied(Direction::Sent))
    });
    show("clipboard", sent, Some(Clipboard), false)?;
    let received = feed.next(SOON, |input| {
        input.transfer = Some(copied(Direction::Received))
    });
    show(
        "clipboard, the other way in place",
        received,
        Some(Clipboard),
        true,
    )?;
    let dropped = feed.next(BETWEEN, |input| lose(studio(input), Phase::Starting));
    show("dropped", dropped, Some(Dropped), false)?;
    let reconnecting = feed.next(BETWEEN, |_| {});
    show("reconnecting", reconnecting, Some(Reconnecting), false)?;
    feed.next(BETWEEN, |input| {
        let studio = studio(input);
        studio.phase = Phase::Sharing;
        studio.held = true;
    });
    let held = feed.next(BETWEEN, |_| {});
    show("held", held, Some(Held), false)?;
    let sharing = feed.next(BETWEEN, |input| studio(input).held = false);
    show("sharing, at rest", sharing, None, false)?;
    let other = feed.next(BETWEEN, |input| {
        let desk = desk(input);
        desk.enabled = true;
        desk.phase = Phase::Starting;
    });
    show(
        "another computer connecting",
        other,
        Some(Connecting),
        false,
    )?;
    let both = feed.next(BETWEEN, |input| desk(input).phase = Phase::Sharing);
    show("both sharing", both, Some(Connected), false)?;
    let first = feed.next(BETWEEN, |input| lose(studio(input), Phase::Starting));
    show("one drops", first, Some(Dropped), false)?;
    let second = feed.next(SOON, |input| lose(desk(input), Phase::Error));
    show("the other drops, in place", second, Some(Dropped), true)?;
    let rest = feed.next(BETWEEN, |input| studio(input).phase = Phase::Sharing);
    shows(&rest, Some(Connected)).map_err(|reason| format!("recovered: {reason}"))?;
    let rest = feed.next(BETWEEN, |_| {});
    show("recovered, at rest", rest, None, false)?;
    steps.push(Step {
        name: "connecting, interrupted mid-flight by rest",
        orders: VecDeque::from([
            (0.0, Order::Show(connecting)),
            (INTERRUPT_AFTER, Order::Interrupt(None)),
        ]),
        in_place: false,
    });
    Ok(steps)
}

struct Running {
    step: Step,
    began: Instant,
    /// When the last order went out.
    ordered: f64,
    rested_since: Option<Instant>,
    /// The content on show before the first order.
    before: Option<Option<u64>>,
}

struct Check {
    steps: VecDeque<Step>,
    running: Option<Running>,
    started: Instant,
    last: Option<(Instant, Probe)>,
}

impl Check {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: steps.into(),
            running: None,
            started: Instant::now(),
            last: None,
        }
    }

    /// One sample: None while the check goes on, then its verdict.
    fn tick(&mut self, mtm: MainThreadMarker) -> Option<Result<(), String>> {
        let now = Instant::now();
        if now.duration_since(self.started).as_secs_f64() > DEADLINE {
            return Some(Err(format!(
                "the script did not finish within {DEADLINE} s"
            )));
        }
        let Some(probe) = macos::probe(mtm) else {
            return Some(Err("the island is not on screen".to_owned()));
        };
        let name = self
            .running
            .as_ref()
            .map_or("start", |running| running.step.name);
        if let Err(reason) = self
            .continuous(now, &probe)
            .and_then(|()| self.during(&probe))
        {
            return Some(Err(format!("{name}: {reason}")));
        }
        if !probe.fits {
            return Some(Err(format!("{name}: the outline left its window")));
        }
        let verdict = self.advance(mtm, now, &probe);
        self.last = Some((now, probe));
        verdict
    }

    /// No point of the drawn outline moves faster than the motion under way allows.
    fn continuous(&self, now: Instant, probe: &Probe) -> Result<(), String> {
        let Some((then, previous)) = &self.last else {
            return Ok(());
        };
        let elapsed = now.duration_since(*then).as_secs_f64();
        let speed = previous.peak_speed.max(probe.peak_speed);
        let allowed = speed * elapsed + JUMP_SLACK;
        let jump = previous.shown.travel(&probe.shown);
        if jump > allowed {
            return Err(format!(
                "the outline jumped {jump:.1} pt in {:.0} ms (at most {allowed:.1} pt)",
                elapsed * 1000.0
            ));
        }
        Ok(())
    }

    /// What must hold on every frame of the running step.
    fn during(&self, probe: &Probe) -> Result<(), String> {
        let in_place = self
            .running
            .as_ref()
            .is_some_and(|running| running.step.in_place);
        if in_place && probe.band_scaling {
            return Err("the band grew or shrank while the content updated in place".to_owned());
        }
        Ok(())
    }

    fn advance(
        &mut self,
        mtm: MainThreadMarker,
        now: Instant,
        probe: &Probe,
    ) -> Option<Result<(), String>> {
        let Some(running) = &mut self.running else {
            return match self.steps.pop_front() {
                Some(step) => {
                    self.running = Some(Running {
                        step,
                        began: now,
                        ordered: 0.0,
                        rested_since: None,
                        before: None,
                    });
                    self.advance(mtm, now, probe)
                }
                None => Some(Ok(())),
            };
        };
        let name = running.step.name;
        let elapsed = now.duration_since(running.began).as_secs_f64();
        if running
            .step
            .orders
            .front()
            .is_some_and(|(at, _)| *at <= elapsed)
            && let Some((at, order)) = running.step.orders.pop_front()
        {
            running.ordered = at;
            running.rested_since = None;
            running.before.get_or_insert(probe.content);
            match order {
                Order::Show(activity) => macos::render(mtm, GENERATION, activity),
                Order::Interrupt(activity) => {
                    if !probe.moving {
                        return Some(Err(format!(
                            "{name}: the island was not moving when interrupted"
                        )));
                    }
                    macos::render(mtm, GENERATION, activity);
                }
            }
            return None;
        }
        if !running.step.orders.is_empty() {
            return None;
        }
        if !probe.at_rest {
            running.rested_since = None;
            if elapsed - running.ordered > SETTLE_LIMIT {
                return Some(Err(format!("{name}: never came to rest")));
            }
            return None;
        }
        let rested = *running.rested_since.get_or_insert(now);
        if now.duration_since(rested).as_secs_f64() < HOLD {
            return None;
        }
        let off = probe.shown.travel(&probe.target);
        if off > REST_TOLERANCE {
            return Some(Err(format!(
                "{name}: settled {off:.2} pt away from its target"
            )));
        }
        let (height, notch) = (probe.target.bounds().height, probe.idle.bounds().height);
        if (height - notch).abs() > REST_TOLERANCE {
            return Some(Err(format!(
                "{name}: the island hangs {height:.1} pt, not the notch's {notch:.1} pt"
            )));
        }
        if running.step.in_place
            && let Some(content) = running.before
            && probe.content != content
        {
            return Some(Err(format!("{name}: the content was rebuilt")));
        }
        match macos::audit(mtm) {
            Some(Ok(())) => {}
            Some(Err(reason)) => return Some(Err(format!("{name}: {reason}"))),
            None => return Some(Err(format!("{name}: the island is not on screen"))),
        }
        println!("  {name}: settled");
        self.running = None;
        None
    }
}
