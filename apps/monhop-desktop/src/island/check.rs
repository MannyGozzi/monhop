//! `--check-island`: the island alone on the real screen, playing a fixed script of synthetic
//! snapshots through the real model, plus a few hand-made changes the model never makes, while
//! the drawn outline is sampled every frame. It starts nothing else MonHop runs, reads no saved
//! state and saves nothing.

use std::{
    cell::RefCell,
    collections::VecDeque,
    time::{Duration, Instant},
};

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

use super::{
    geometry::Outline,
    macos::{self, Context, Motion, Probe},
    model::{
        Activity, ActivityId, ClipboardInput, ComputerInput, DimmingInput, Direction, IslandInput,
        Model, Phase, Platform, Presentation, Row, Tone, Trailing, Transfer, TransferKind,
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
/// The interrupted step closes the island this long after it starts opening, about halfway.
const INTERRUPT_AFTER: f64 = 0.08;
/// The synthetic clock passes this between steps, so every notice has ended.
const BETWEEN: Duration = Duration::from_secs(10);

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
            interactive: false,
            motion: Motion::Always,
            hover: Box::new(|_| {}),
            act: Box::new(|_| {}),
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
    Show(Presentation),
    Expand(bool),
    /// Closes the island, which must still be moving toward open.
    Interrupt,
}

struct Step {
    name: &'static str,
    /// Seconds after the step starts, in order.
    orders: VecDeque<(f64, Order)>,
    expanded: bool,
    /// The content on show updates where it is: never rebuilt, and an open island never resized
    /// unless the step `resizes` it.
    in_place: bool,
    /// A row comes or goes, so the open island must change height in place.
    resizes: bool,
    /// The header's status dot keeps its place: never drawn away from it, and in the same place
    /// at the end.
    dot_stays: bool,
    /// That dot changes color through a transition, seen on some frame, never at once.
    dot_recolors: bool,
}

fn step(name: &'static str, order: Order, expanded: bool) -> Step {
    Step {
        name,
        orders: VecDeque::from([(0.0, order)]),
        expanded,
        in_place: false,
        resizes: false,
        dot_stays: false,
        dot_recolors: false,
    }
}

/// The model fed synthetic snapshots on a clock of its own.
struct Feed {
    model: Model,
    clock: Instant,
    input: IslandInput,
}

impl Feed {
    fn next(&mut self, after: Duration, change: impl FnOnce(&mut IslandInput)) -> Presentation {
        change(&mut self.input);
        self.clock += after;
        self.model.update(&self.input, self.clock)
    }
}

fn computer(name: &str, platform: Platform, tone: usize) -> ComputerInput {
    ComputerInput {
        fingerprint: name.to_ascii_lowercase(),
        name: name.to_owned(),
        platform,
        enabled: true,
        phase: Phase::Off,
        held: false,
        pointer_here: None,
        tone,
    }
}

fn studio(input: &mut IslandInput) -> &mut ComputerInput {
    &mut input.computers[0]
}

fn desk(input: &mut IslandInput) -> &mut ComputerInput {
    &mut input.computers[1]
}

fn shows(presentation: &Presentation, expected: Option<ActivityId>) -> Result<(), String> {
    let shown = presentation.compact.as_ref().map(|activity| activity.id);
    if shown == expected {
        Ok(())
    } else {
        Err(format!("expected {expected:?}, the model shows {shown:?}"))
    }
}

/// Idle, every compact activity (dimming and control of another computer also updating in place,
/// and that control restyled by hand), the open island with two computers, dimming and clipboard,
/// its header taking the widest status in place and then hand-made words as wide, a computer
/// joining and leaving it, closing, and an open interrupted mid-flight by a close.
fn script() -> Result<Vec<Step>, String> {
    let mut feed = Feed {
        model: Model::default(),
        clock: Instant::now(),
        input: IslandInput {
            computers: vec![
                computer("Studio", Platform::Mac, 0),
                computer("Desk PC", Platform::Windows, 1),
            ],
            paused: false,
            phase: Phase::Off,
            sharing_active: false,
            held: false,
            dimming: DimmingInput {
                dimmed: false,
                level: 40,
                min_level: 10,
                max_level: 99,
            },
            clipboard: Some(ClipboardInput {
                enabled: true,
                last: None,
            }),
            update_ready: None,
        },
    };
    let mut steps = Vec::new();
    let show = |name, presentation: Presentation, expected| -> Result<Step, String> {
        shows(&presentation, expected).map_err(|reason| format!("{name}: {reason}"))?;
        Ok(step(name, Order::Show(presentation), false))
    };

    steps.push(show("idle", feed.next(Duration::ZERO, |_| {}), None)?);
    let dimmed = feed.next(BETWEEN, |input| input.dimming.dimmed = true);
    steps.push(show("dimming", dimmed, Some(ActivityId::Dimming))?);
    let darker = feed.next(Duration::from_millis(500), |input| input.dimming.level = 70);
    steps.push(Step {
        in_place: true,
        ..show(
            "dimming, darker in place",
            darker,
            Some(ActivityId::Dimming),
        )?
    });
    let connected = feed.next(BETWEEN, |input| {
        input.phase = Phase::Sharing;
        input.sharing_active = true;
        let studio = studio(input);
        studio.phase = Phase::Sharing;
        studio.pointer_here = Some(true);
    });
    steps.push(show("connected", connected, Some(ActivityId::Connected))?);
    let copied = feed.next(BETWEEN, |input| {
        if let Some(clipboard) = &mut input.clipboard {
            clipboard.last = Some(Transfer {
                direction: Direction::Received,
                kind: TransferKind::Image,
                bytes: 1_468_006,
                peer: "studio".to_owned(),
                age_seconds: 0,
            });
        }
    });
    steps.push(show("clipboard", copied, Some(ActivityId::Clipboard))?);
    let update = feed.next(BETWEEN, |input| {
        input.update_ready = Some("9.9.9".to_owned())
    });
    steps.push(show("update", update, Some(ActivityId::Update))?);
    let dropped = feed.next(BETWEEN, |input| {
        input.phase = Phase::Reconnecting;
        input.sharing_active = false;
        let studio = studio(input);
        studio.phase = Phase::Reconnecting;
        studio.pointer_here = None;
    });
    steps.push(show("dropped", dropped, Some(ActivityId::Dropped))?);
    let reconnecting = feed.next(BETWEEN, |_| {});
    steps.push(show(
        "reconnecting",
        reconnecting,
        Some(ActivityId::Reconnecting),
    )?);
    feed.next(BETWEEN, |input| {
        input.phase = Phase::Sharing;
        input.sharing_active = true;
        input.held = true;
        let studio = studio(input);
        studio.phase = Phase::Sharing;
        studio.held = true;
        studio.pointer_here = Some(true);
    });
    let held = feed.next(BETWEEN, |_| {});
    steps.push(show("held", held, Some(ActivityId::Held))?);
    let controlling = feed.next(BETWEEN, |input| {
        input.held = false;
        let studio = studio(input);
        studio.held = false;
        studio.pointer_here = Some(false);
    });
    steps.push(show(
        "controlling",
        controlling,
        Some(ActivityId::Controlling),
    )?);
    feed.next(BETWEEN, |input| {
        let desk = desk(input);
        desk.phase = Phase::Sharing;
        desk.pointer_here = Some(true);
    });
    let other = feed.next(BETWEEN, |input| {
        studio(input).pointer_here = Some(true);
        desk(input).pointer_here = Some(false);
    });
    let labeled = other.clone();
    steps.push(Step {
        in_place: true,
        ..show(
            "controlling a computer of the other kind, in place",
            other,
            Some(ActivityId::Controlling),
        )?
    });
    // The model never restyles a compact activity in place; by hand, its label becomes words.
    let Some(Activity {
        trailing: Trailing::Labeled(_, name),
        ..
    }) = &labeled.compact
    else {
        return Err("controlling must label the computer with its kind".to_owned());
    };
    let mut worded = labeled.clone();
    if let Some(activity) = &mut worded.compact {
        activity.trailing = Trailing::Text(name.clone());
    }
    steps.push(Step {
        in_place: true,
        ..show(
            "controlling, its label turned to words in place",
            worded,
            Some(ActivityId::Controlling),
        )?
    });
    steps.push(Step {
        in_place: true,
        ..show(
            "controlling, its label back in place",
            labeled,
            Some(ActivityId::Controlling),
        )?
    });
    let back = feed.next(BETWEEN, |input| {
        studio(input).pointer_here = Some(true);
        desk(input).pointer_here = Some(true);
    });
    let back_rows = back.expanded.rows.clone();
    let rows = &back.expanded.rows;
    let computers = rows
        .iter()
        .filter(|row| matches!(row, Row::Computer(_)))
        .count();
    let dimming = rows.iter().any(|row| matches!(row, Row::Dimming(_)));
    let clipboard = rows.iter().any(|row| matches!(row, Row::Clipboard { .. }));
    if (computers, dimming, clipboard) != (2, true, true) {
        return Err("the open island needs two computers, dimming and clipboard".to_owned());
    }
    steps.push(show("pointer back here", back, None)?);

    steps.push(step("open", Order::Expand(true), true));
    let attention = feed.next(BETWEEN, |input| input.phase = Phase::Error);
    shows(&attention, None).map_err(|reason| format!("status in place: {reason}"))?;
    if attention.expanded.status != "Needs attention" || attention.expanded.rows != back_rows {
        return Err("the status step must change only the header's status".to_owned());
    }
    let open_in_place = |name, presentation| Step {
        in_place: true,
        ..step(name, Order::Show(presentation), true)
    };
    steps.push(open_in_place(
        "open, status changes in place",
        attention.clone(),
    ));
    // By hand: other words exactly as wide, so the dot stays put and only changes color.
    let mut reworded = attention.clone();
    reworded.expanded.status.push('\u{200B}');
    reworded.expanded.status_tone = Tone::Warning;
    steps.push(Step {
        dot_stays: true,
        dot_recolors: true,
        ..open_in_place("open, status reworded at the same width in place", reworded)
    });
    steps.push(Step {
        dot_stays: true,
        dot_recolors: true,
        ..open_in_place("open, status back in place", attention.clone())
    });
    let joined = feed.next(BETWEEN, |input| {
        input.computers.push(computer("Laptop", Platform::Mac, 2));
    });
    if joined.expanded.status != attention.expanded.status
        || joined.expanded.rows.len() != back_rows.len() + 1
    {
        return Err("the join step must only add a row".to_owned());
    }
    steps.push(Step {
        resizes: true,
        ..open_in_place("open, a computer joins in place", joined)
    });
    let left = feed.next(BETWEEN, |input| {
        input.computers.pop();
    });
    if left.expanded.rows != back_rows {
        return Err("the leave step must only take that row away".to_owned());
    }
    steps.push(Step {
        resizes: true,
        ..open_in_place("open, the computer leaves in place", left)
    });
    steps.push(step("close", Order::Expand(false), false));
    steps.push(Step {
        name: "open, closed mid-flight",
        orders: VecDeque::from([
            (0.0, Order::Expand(true)),
            (INTERRUPT_AFTER, Order::Interrupt),
        ]),
        expanded: false,
        in_place: false,
        resizes: false,
        dot_stays: false,
        dot_recolors: false,
    });
    Ok(steps)
}

struct Running {
    step: Step,
    began: Instant,
    /// When the last order went out.
    ordered: f64,
    rested_since: Option<Instant>,
    /// The content, target and status dot place on show before the first order.
    before: Option<(Option<u64>, Outline, Option<f64>)>,
    /// The status dot was seen changing color.
    recolored: bool,
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
        if let Some(running) = &mut self.running {
            running.recolored |= probe.status_recoloring;
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
        let Some(step) = self.running.as_ref().map(|running| &running.step) else {
            return Ok(());
        };
        if step.in_place && probe.bands_scaling {
            return Err("a band grew or shrank while the content updated in place".to_owned());
        }
        if step.dot_stays
            && let Some(drift) = probe
                .status_drift
                .filter(|drift| drift.abs() > REST_TOLERANCE)
        {
            return Err(format!(
                "the status dot was drawn {drift:.1} pt from its place"
            ));
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
                        recolored: false,
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
            running
                .before
                .get_or_insert((probe.content, probe.target, probe.status_place));
            match order {
                Order::Show(presentation) => macos::render(mtm, GENERATION, presentation),
                Order::Expand(expanded) => macos::set_expanded(mtm, expanded),
                Order::Interrupt => {
                    if !probe.moving || !probe.expanded {
                        return Some(Err(format!(
                            "{name}: the island was not opening when closed"
                        )));
                    }
                    macos::set_expanded(mtm, false);
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
        if probe.expanded != running.step.expanded {
            return Some(Err(format!(
                "{name}: the island is not {}",
                if running.step.expanded {
                    "open"
                } else {
                    "closed"
                }
            )));
        }
        let open_taller = probe.target.bounds().height > probe.idle.bounds().height;
        if running.step.expanded != open_taller {
            return Some(Err(format!(
                "{name}: the outline does not match the open state"
            )));
        }
        if running.step.expanded {
            match probe.status_drift {
                None => return Some(Err(format!("{name}: no open header is on show"))),
                Some(drift) if drift.abs() > REST_TOLERANCE => {
                    return Some(Err(format!(
                        "{name}: the status dot sits {drift:.1} pt from its word"
                    )));
                }
                Some(_) => {}
            }
        }
        if running.step.in_place
            && let Some((content, target, place)) = running.before
        {
            if probe.content != content {
                return Some(Err(format!("{name}: the content was rebuilt")));
            }
            let resized = probe.target != target;
            if running.step.expanded && resized != running.step.resizes {
                return Some(Err(format!(
                    "{name}: the open island {}",
                    if resized {
                        "changed size"
                    } else {
                        "kept its size"
                    }
                )));
            }
            if running.step.dot_stays && probe.status_place != place {
                return Some(Err(format!("{name}: the status dot moved")));
            }
            if running.step.dot_recolors && !running.recolored {
                return Some(Err(format!("{name}: the status dot changed color at once")));
            }
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
