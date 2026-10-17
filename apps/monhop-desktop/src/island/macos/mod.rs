//! The island on screen: one non-activating panel around the camera notch, drawn with Core
//! Animation, and hidden while no display has a notch. Main thread only. Its state lives in one
//! thread-local that AppKit callbacks reach through `with_island`; the app's actions and hover
//! changes leave only after that borrow ends, so nothing the app does can re-enter it.

mod classes;
mod content;
mod draw;

use std::{
    cell::{Cell, RefCell},
    iter,
    ptr::NonNull,
    rc::Rc,
    time::{Duration, Instant},
};

use block2::RcBlock;
use objc2::{
    MainThreadMarker, Message,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    sel,
};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityGroupRole, NSApplicationDidChangeScreenParametersNotification,
    NSColor, NSEvent, NSScreen, NSStatusWindowLevel, NSWindowAnimationBehavior,
    NSWindowCollectionBehavior, NSWorkspace,
    NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification, NSWorkspaceDidWakeNotification,
    NSWorkspaceScreensDidSleepNotification, NSWorkspaceScreensDidWakeNotification,
    NSWorkspaceSessionDidBecomeActiveNotification, NSWorkspaceSessionDidResignActiveNotification,
    NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{
    NSDistributedNotificationCenter, NSNotification, NSNotificationCenter, NSNotificationName,
    NSNotificationSuspensionBehavior, NSObjectProtocol, NSOperationQueue, NSPoint, NSRect, NSSize,
    NSTimer, ns_string,
};
use objc2_quartz_core::{CALayer, CAShapeLayer};

pub use self::draw::every;
use self::{
    classes::{Assist, ControlElement, IslandPanel, IslandView, LockObserver},
    content::{CompactParts, ControlId, Look, OpenParts, Pending, Widget},
};
use super::{
    geometry::{self, DisplayInfo, Outline, Placement, Rect, Size},
    model::{Activity, ActivityId, Expanded, Presentation},
    tokens::{self, IslandTokens, Spring},
};

/// The shape counts as arrived within this many points.
const SETTLE_TOLERANCE: f64 = 0.5;
/// Screen changes arrive in bursts; the island re-places once they stop.
const SCREENS_QUIET: f64 = 0.1;
/// How long a control shows the user's change while the app has not reported it yet.
const PENDING: Duration = Duration::from_secs(3);
/// The window's own darkness slider moves by one level per step.
const LEVEL_STEP: u8 = 1;

/// What a control asks of the app; the driver carries it out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    OpenWindow,
    PreviewLevel(u8),
    SetComputer { fingerprint: String, enabled: bool },
    ToggleDimming,
    SetLevel(u8),
    SetClipboard(bool),
}

/// Whether Reduce Motion is honored; the island check always animates so it can measure motion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    FollowSystem,
    Always,
}

pub struct Context {
    /// False for the check: the pointer neither hovers nor clicks it.
    pub interactive: bool,
    pub motion: Motion,
    pub hover: Box<dyn Fn(bool) + Send>,
    pub act: Box<dyn Fn(Action) + Send>,
    /// Whether the island is on screen, so the feed rests while nothing shows it.
    pub showing: Box<dyn Fn(bool) + Send>,
}

/// What the island check reads each frame.
pub struct Probe {
    /// The outline as it is drawn now, and where it is headed.
    pub shown: Outline,
    pub target: Outline,
    /// The resting outline on this display.
    pub idle: Outline,
    pub moving: bool,
    /// No shape, window or content motion is left.
    pub at_rest: bool,
    /// The drawn outline lies inside the window.
    pub fits: bool,
    pub expanded: bool,
    /// The fastest any point of the current motion travels, in points per second.
    pub peak_speed: f64,
    /// The content tree on show; a rebuild or a new entrance changes it.
    pub content: Option<u64>,
    /// How far the open header's status dot is drawn from its place beside the word.
    pub status_drift: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
enum Pointer {
    Entered,
    Exited,
    Moved(NSPoint),
    Down(NSPoint),
    Dragged(NSPoint),
    Up(NSPoint),
}

thread_local! {
    static ISLAND: RefCell<Option<Island>> = const { RefCell::new(None) };
}

/// Shows the island; a running one is replaced.
pub fn start(mtm: MainThreadMarker, generation: u64, context: Context) {
    stop(mtm);
    let Ok(tokens) = tokens::tokens() else {
        return;
    };
    let island = Island::new(mtm, generation, tokens, context);
    ISLAND.with(|slot| *slot.borrow_mut() = Some(island));
    with_island(Island::begin);
}

/// Removes the island and everything it observes.
pub fn stop(mtm: MainThreadMarker) {
    let _ = mtm;
    let taken = ISLAND.with(|slot| slot.try_borrow_mut().map(|mut slot| slot.take()));
    match taken {
        Ok(Some(island)) => island.end(),
        Ok(None) => {}
        Err(_) => draw::later(|| {
            if let Some(mtm) = MainThreadMarker::new() {
                stop(mtm);
            }
        }),
    }
}

/// Shows a presentation from the feed that started this island.
pub fn render(mtm: MainThreadMarker, generation: u64, presentation: Presentation) {
    let _ = mtm;
    with_island_soon(move |island| island.show(generation, presentation));
}

/// Opens or closes the island as hover or a click would.
pub fn set_expanded(mtm: MainThreadMarker, expanded: bool) {
    let _ = mtm;
    with_island_soon(move |island| island.set_expanded(expanded));
}

pub fn probe(mtm: MainThreadMarker) -> Option<Probe> {
    let _ = mtm;
    with_island(|island| island.probe())
}

/// Some connected display has a camera notch, the only place the island shows.
pub fn has_notch() -> bool {
    MainThreadMarker::new().and_then(notch_placement).is_some()
}

fn display_info(screen: &NSScreen) -> DisplayInfo {
    let area =
        |rect: NSRect| (rect.size.width > 0.0 && rect.size.height > 0.0).then(|| to_rect(rect));
    DisplayInfo {
        frame: to_rect(screen.frame()),
        safe_top: screen.safeAreaInsets().top,
        aux_left: area(screen.auxiliaryTopLeftArea()),
        aux_right: area(screen.auxiliaryTopRightArea()),
        scale: screen.backingScaleFactor(),
    }
}

fn to_rect(rect: NSRect) -> Rect {
    Rect::new(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

/// Around the camera of the first display that has one; only a built-in panel does.
fn notch_placement(mtm: MainThreadMarker) -> Option<Placement> {
    NSScreen::screens(mtm)
        .iter()
        .find_map(|screen| geometry::placement(&display_info(&screen)))
}

fn reduce_motion(motion: Motion) -> bool {
    motion == Motion::FollowSystem
        && NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

fn size_of(outline: &Outline) -> Size {
    let bounds = outline.bounds();
    Size::new(bounds.width, bounds.height)
}

fn contains(rect: Rect, point: NSPoint) -> bool {
    point.x >= rect.x && point.x <= rect.max_x() && point.y >= rect.y && point.y <= rect.max_y()
}

fn cancel(slot: &mut Option<Retained<NSTimer>>) {
    if let Some(timer) = slot.take() {
        timer.invalidate();
    }
}

/// Runs `work` on the island with implicit animations off, then delivers what it queued.
fn with_island<R>(work: impl FnOnce(&mut Island) -> R) -> Option<R> {
    let (result, context, outbox) = ISLAND.with(|slot| {
        let mut slot = slot.try_borrow_mut().ok()?;
        let island = slot.as_mut()?;
        let result = draw::quietly(|| work(island));
        Some((
            result,
            Rc::clone(&island.context),
            std::mem::take(&mut island.outbox),
        ))
    })?;
    if let Some(showing) = outbox.showing {
        (context.showing)(showing);
    }
    if let Some(hovering) = outbox.hover {
        (context.hover)(hovering);
    }
    for action in outbox.actions {
        (context.act)(action);
    }
    Some(result)
}

/// Like `with_island`, deferred past the current borrow when an AppKit callback lands inside one.
fn with_island_soon(work: impl FnOnce(&mut Island) + 'static) {
    if ISLAND.with(|slot| slot.try_borrow_mut().is_err()) {
        draw::later(move || {
            with_island(work);
        });
    } else {
        with_island(work);
    }
}

fn pointer(event: Pointer) {
    with_island_soon(move |island| island.on_pointer(event));
}

fn assist(element: &ControlElement, kind: Assist) -> bool {
    with_island(|island| island.assist(element, kind)).unwrap_or(false)
}

/// The distributed center may call on any thread; the change always lands on the main one.
fn screen_locked(generation: u64, locked: bool) {
    let apply = move || {
        with_island_soon(move |island| {
            if island.generation == generation {
                island.set_locked(locked);
            }
        });
    };
    if MainThreadMarker::new().is_some() {
        apply();
    } else {
        let block = RcBlock::new(apply);
        // SAFETY: the block holds only plain values, and the main queue runs it on the main thread.
        unsafe { NSOperationQueue::mainQueue().addOperationWithBlock(&block) };
    }
}

/// What leaves the island once its borrow ends.
#[derive(Default)]
struct Outbox {
    hover: Option<bool>,
    showing: Option<bool>,
    actions: Vec<Action>,
}

enum Content {
    /// At rest the island is the notch's own black, with nothing inside.
    Idle,
    Compact(ActivityId, CompactParts),
    Open(OpenParts),
}

impl Content {
    /// Leaves and comes back as one.
    fn group(&self) -> Option<&CALayer> {
        match self {
            Self::Idle => None,
            Self::Compact(_, parts) => Some(&parts.group),
            Self::Open(parts) => Some(&parts.group),
        }
    }

    /// Arrive one after another, top to bottom.
    fn bands(&self) -> Vec<&CALayer> {
        match self {
            Self::Idle => Vec::new(),
            Self::Compact(_, parts) => parts.bands(),
            Self::Open(parts) => parts.bands(),
        }
    }
}

/// One content tree; each reveal or dismissal gives it a new id, so a stale timer finds nothing.
struct Piece {
    id: u64,
    content: Content,
}

/// The running shape motion, kept so a retarget carries its speed on.
struct Swing {
    id: u64,
    from: Outline,
    to: Outline,
    start: f64,
    spring: Spring,
    velocity: f64,
    duration: f64,
    peak: f64,
}

struct Press {
    index: usize,
    over: bool,
}

#[derive(Default)]
struct Hidden {
    asleep: bool,
    screens_asleep: bool,
    locked: bool,
    session_away: bool,
    no_display: bool,
}

impl Hidden {
    fn any(&self) -> bool {
        self.asleep || self.screens_asleep || self.locked || self.session_away || self.no_display
    }
}

#[derive(Default)]
struct Timers {
    open: Option<Retained<NSTimer>>,
    close: Option<Retained<NSTimer>>,
    settle: Option<Retained<NSTimer>>,
    screens: Option<Retained<NSTimer>>,
}

enum Want {
    Idle,
    Compact(Activity),
    Open(Expanded),
}

type Observer = (
    Retained<NSNotificationCenter>,
    Retained<ProtocolObject<dyn NSObjectProtocol>>,
);

struct Island {
    mtm: MainThreadMarker,
    generation: u64,
    context: Rc<Context>,
    outbox: Outbox,
    tokens: &'static IslandTokens,
    panel: Retained<IslandPanel>,
    view: Retained<IslandView>,
    root: Retained<CALayer>,
    stage: Retained<CALayer>,
    fill: Retained<CAShapeLayer>,
    content: Retained<CALayer>,
    mask: Retained<CAShapeLayer>,
    look: Look,
    presentation: Option<Presentation>,
    expanded: bool,
    shown: Option<Piece>,
    leaving: Vec<Piece>,
    target: Outline,
    swing: Option<Swing>,
    envelope: Size,
    inside: bool,
    hovered: Option<usize>,
    press: Option<Press>,
    hidden: Hidden,
    next_id: u64,
    timers: Timers,
    observers: Vec<Observer>,
    /// Hears screen locks even while MonHop is in the background.
    lock: Option<Retained<LockObserver>>,
}

impl Island {
    fn new(
        mtm: MainThreadMarker,
        generation: u64,
        tokens: &'static IslandTokens,
        context: Context,
    ) -> Self {
        let found = notch_placement(mtm);
        let place = found.unwrap_or(Placement::NOWHERE);
        let look = Look::new(tokens, place, reduce_motion(context.motion));
        let target = place.outline(place.idle);
        let envelope = place.envelope(place.idle, place.idle, 0.0);

        let root = CALayer::new();
        let stage = CALayer::new();
        let fill = CAShapeLayer::new();
        let content = CALayer::new();
        let mask = CAShapeLayer::new();
        let path = draw::path(&target);
        for shape in [&fill, &mask] {
            shape.setFillColor(Some(&draw::cg(tokens.island_fill)));
            shape.setContentsScale(place.scale);
            shape.setPath(Some(&path));
        }
        // SAFETY: the mask is a shape layer only this content layer uses.
        unsafe { content.setMask(Some(&mask)) };
        stage.addSublayer(&fill);
        stage.addSublayer(&content);
        root.addSublayer(&stage);

        let size = NSSize::new(envelope.width, envelope.height);
        let view = IslandView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), size), fill.clone());
        view.setLayer(Some(&root));
        view.setWantsLayer(true);
        view.setLayerUsesCoreImageFilters(true);
        // SAFETY: AppKit's exported role constant.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityGroupRole }));
        view.setAccessibilityLabel(Some(ns_string!("MonHop")));

        let panel = IslandPanel::new(mtm, draw::rect(place.frame(envelope)));
        // SAFETY: the Retained handle owns the panel; AppKit must not release it on close.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setLevel(NSStatusWindowLevel + 1);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setHidesOnDeactivate(false);
        panel.setCanHide(false);
        panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
        panel.setIgnoresMouseEvents(!context.interactive);
        panel.setContentView(Some(&view));

        Self {
            mtm,
            generation,
            context: Rc::new(context),
            outbox: Outbox::default(),
            tokens,
            panel,
            view,
            root,
            stage,
            fill,
            content,
            mask,
            look,
            presentation: None,
            expanded: false,
            shown: None,
            leaving: Vec::new(),
            target,
            swing: None,
            envelope,
            inside: false,
            hovered: None,
            press: None,
            hidden: Hidden {
                no_display: found.is_none(),
                ..Hidden::default()
            },
            next_id: 0,
            timers: Timers::default(),
            observers: Vec::new(),
            lock: None,
        }
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn begin(&mut self) {
        self.observe();
        self.lay_out_stage();
        self.apply(false);
        self.update_tracking();
        self.update_visibility();
        if !self.look.reduce {
            draw::add(
                &self.stage,
                "opacity",
                &draw::tween(
                    "opacity",
                    &draw::number(0.0),
                    &draw::number(1.0),
                    self.tokens.base,
                    self.tokens.ease_out,
                    0.0,
                ),
            );
        }
    }

    /// Stops observing at once; the panel fades out and leaves, letting every click through.
    fn end(mut self) {
        self.panel.setIgnoresMouseEvents(true);
        self.view.detach();
        for (center, observer) in self.observers.drain(..) {
            let observer: &AnyObject = observer.as_ref();
            // SAFETY: the observer was registered with this center by `watch`.
            unsafe { center.removeObserver(observer) };
        }
        if let Some(lock) = self.lock.take() {
            let observer: &AnyObject = &lock;
            // SAFETY: the observer was registered with this center by `observe`.
            unsafe { NSDistributedNotificationCenter::defaultCenter().removeObserver(observer) };
        }
        for slot in [
            &mut self.timers.open,
            &mut self.timers.close,
            &mut self.timers.settle,
            &mut self.timers.screens,
        ] {
            cancel(slot);
        }
        let panel = self.panel.clone();
        let leave = move || {
            panel.orderOut(None);
            panel.close();
        };
        if self.look.reduce || !self.panel.isVisible() {
            leave();
            return;
        }
        let tokens = self.tokens;
        draw::quietly(|| draw::fade_to(&self.stage, 0.0, tokens.fast, tokens.ease_out, 0.0));
        let once = Cell::new(Some(leave));
        let _ = draw::after(tokens.fast, move || {
            if let Some(leave) = once.take() {
                leave();
            }
        });
    }

    /// Calls `work` after `seconds` if this island is still the one shown.
    fn later(&self, seconds: f64, work: impl FnOnce(&mut Island) + 'static) -> Retained<NSTimer> {
        let generation = self.generation;
        let once = Cell::new(Some(work));
        draw::after(seconds, move || {
            if let Some(work) = once.take() {
                with_island(|island| {
                    if island.generation == generation {
                        work(island);
                    }
                });
            }
        })
    }

    fn watch(
        &mut self,
        center: &NSNotificationCenter,
        name: &NSNotificationName,
        work: fn(&mut Island),
    ) {
        let generation = self.generation;
        let block = RcBlock::new(move |_: NonNull<NSNotification>| {
            with_island_soon(move |island| {
                if island.generation == generation {
                    work(island);
                }
            });
        });
        // SAFETY: the observer runs on the main queue, touches only main-thread state, and is
        // removed when the island ends.
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(name),
                None,
                Some(&NSOperationQueue::mainQueue()),
                &block,
            )
        };
        self.observers.push((center.retain(), observer));
    }

    fn observe(&mut self) {
        let app = NSNotificationCenter::defaultCenter();
        let workspace = NSWorkspace::sharedWorkspace().notificationCenter();
        // SAFETY: AppKit's exported notification names.
        let names = unsafe {
            [
                NSApplicationDidChangeScreenParametersNotification,
                NSWorkspaceWillSleepNotification,
                NSWorkspaceDidWakeNotification,
                NSWorkspaceScreensDidSleepNotification,
                NSWorkspaceScreensDidWakeNotification,
                NSWorkspaceSessionDidResignActiveNotification,
                NSWorkspaceSessionDidBecomeActiveNotification,
                NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification,
            ]
        };
        let [
            screens,
            will_sleep,
            did_wake,
            screens_sleep,
            screens_wake,
            resign,
            resumed,
            motion,
        ] = names;
        self.watch(&app, screens, Island::screens_changed);
        self.watch(&workspace, will_sleep, |island| {
            island.hidden.asleep = true;
            island.update_visibility();
        });
        self.watch(&workspace, did_wake, |island| {
            island.hidden.asleep = false;
            island.replace();
        });
        self.watch(&workspace, screens_sleep, |island| {
            island.hidden.screens_asleep = true;
            island.update_visibility();
        });
        self.watch(&workspace, screens_wake, |island| {
            island.hidden.screens_asleep = false;
            island.replace();
        });
        self.watch(&workspace, resign, |island| {
            island.hidden.session_away = true;
            island.update_visibility();
        });
        self.watch(&workspace, resumed, |island| {
            island.hidden.session_away = false;
            island.replace();
        });
        self.watch(&workspace, motion, Island::motion_changed);
        // AppKit holds distributed notifications back while the app is inactive, as an accessory
        // app nearly always is, unless the observer asks for them at once.
        let lock = LockObserver::new(self.generation);
        let distributed = NSDistributedNotificationCenter::defaultCenter();
        for (selector, name) in [
            (sel!(screenLocked:), ns_string!("com.apple.screenIsLocked")),
            (
                sel!(screenUnlocked:),
                ns_string!("com.apple.screenIsUnlocked"),
            ),
        ] {
            // SAFETY: the observer implements the selector, taking the notification, and is
            // removed when the island ends.
            unsafe {
                distributed.addObserver_selector_name_object_suspensionBehavior(
                    &lock,
                    selector,
                    Some(name),
                    None,
                    NSNotificationSuspensionBehavior::DeliverImmediately,
                );
            }
        }
        self.lock = Some(lock);
    }

    fn set_locked(&mut self, locked: bool) {
        self.hidden.locked = locked;
        if locked {
            self.update_visibility();
        } else {
            self.replace();
        }
    }

    fn screens_changed(&mut self) {
        cancel(&mut self.timers.screens);
        self.timers.screens = Some(self.later(SCREENS_QUIET, |island| {
            island.timers.screens = None;
            island.replace();
        }));
    }

    /// Finds the notch again; a new place is laid out at once, never animated across.
    fn replace(&mut self) {
        let found = notch_placement(self.mtm);
        self.hidden.no_display = found.is_none();
        if let Some(place) = found
            && place != self.look.place
        {
            self.relayout(place);
        }
        self.update_visibility();
    }

    fn motion_changed(&mut self) {
        let reduce = reduce_motion(self.context.motion);
        if reduce != self.look.reduce {
            self.look.reduce = reduce;
            self.relayout(self.look.place);
        }
    }

    fn relayout(&mut self, place: Placement) {
        self.look = Look::new(self.tokens, place, self.look.reduce);
        for piece in self.leaving.drain(..).chain(self.shown.take()) {
            if let Some(group) = piece.content.group() {
                group.removeFromSuperlayer();
            }
        }
        self.press = None;
        self.hovered = None;
        self.swing = None;
        cancel(&mut self.timers.settle);
        let target = place.outline(place.idle);
        let path = draw::path(&target);
        for shape in [&self.fill, &self.mask] {
            draw::remove(shape, "path");
            shape.setContentsScale(place.scale);
            shape.setPath(Some(&path));
        }
        self.target = target;
        self.envelope = Size::new(0.0, 0.0);
        self.set_envelope(place.envelope(place.idle, place.idle, 0.0));
        self.apply(false);
        self.update_tracking();
    }

    fn update_visibility(&mut self) {
        self.outbox.showing = Some(!self.hidden.any());
        if self.hidden.any() {
            if self.expanded {
                self.end_press();
                self.expanded = false;
                self.apply(false);
            }
            if self.inside {
                self.set_inside(false);
            }
            self.panel.orderOut(None);
        } else if !self.panel.isVisible() {
            self.panel.orderFrontRegardless();
            self.update_tracking();
        }
    }

    fn input_away(&self) -> bool {
        self.presentation
            .as_ref()
            .is_some_and(|presentation| presentation.input_away)
    }

    fn show(&mut self, generation: u64, presentation: Presentation) {
        if generation != self.generation || self.presentation.as_ref() == Some(&presentation) {
            return;
        }
        let away = presentation.input_away;
        self.presentation = Some(presentation);
        if away {
            cancel(&mut self.timers.open);
            cancel(&mut self.timers.close);
            if self.expanded {
                self.end_press();
                self.expanded = false;
            }
        }
        self.apply(true);
    }

    fn set_expanded(&mut self, expanded: bool) {
        cancel(&mut self.timers.open);
        cancel(&mut self.timers.close);
        if self.expanded == expanded || (expanded && (self.input_away() || self.hidden.any())) {
            return;
        }
        if !expanded {
            self.end_press();
        }
        self.expanded = expanded;
        self.apply(true);
    }

    fn want(&self) -> Want {
        let Some(presentation) = &self.presentation else {
            return Want::Idle;
        };
        if self.expanded {
            return Want::Open(presentation.expanded.clone());
        }
        presentation
            .compact
            .clone()
            .map_or(Want::Idle, Want::Compact)
    }

    fn apply(&mut self, animate: bool) {
        let animate = animate && !self.look.reduce;
        match self.want() {
            Want::Open(expanded) => self.show_open(&expanded, animate),
            Want::Compact(activity) => self.show_compact(&activity, animate),
            Want::Idle => self.show_idle(animate),
        }
        self.place_accessibility();
    }

    fn show_open(&mut self, expanded: &Expanded, animate: bool) {
        let now = Instant::now();
        let dragging = self.dragging();
        let held = self.held();
        let mtm = self.mtm;
        let updated = match &mut self.shown {
            Some(Piece {
                content: Content::Open(parts),
                ..
            }) => content::update_open(&self.look, parts, expanded, dragging, now, mtm)
                .then_some(parts.size),
            _ => None,
        };
        if let Some(size) = updated {
            self.hold(held);
            self.set_shape(self.look.place.outline(size), animate);
            return;
        }
        let mut revived = None;
        for (index, piece) in self.leaving.iter_mut().enumerate() {
            if let Content::Open(parts) = &mut piece.content
                && content::update_open(&self.look, parts, expanded, false, now, mtm)
            {
                revived = Some(index);
                break;
            }
        }
        self.end_press();
        self.dismiss_shown(animate);
        let piece = match revived {
            Some(index) => {
                let mut piece = self.leaving.remove(index);
                self.revive(&mut piece, animate);
                piece
            }
            None => {
                let parts = content::build_open(&self.look, expanded, self.mtm);
                self.content.addSublayer(&parts.group);
                let mut piece = Piece {
                    id: 0,
                    content: Content::Open(parts),
                };
                self.reveal(&mut piece, animate);
                piece
            }
        };
        self.shown = Some(piece);
        let place = self.look.place;
        self.set_shape(
            place.outline(content::open_size(&self.look, expanded)),
            animate,
        );
    }

    fn show_compact(&mut self, activity: &Activity, animate: bool) {
        let place = self.look.place;
        let wing = content::compact_wing(&self.look, activity);
        let target = place.outline(place.compact(wing));
        if let Some(Piece {
            content: Content::Compact(id, parts),
            ..
        }) = &mut self.shown
            && *id == activity.id
        {
            content::update_compact(&self.look, parts, activity, wing);
            self.set_shape(target, animate);
            return;
        }
        let revived = self.leaving.iter().position(
            |piece| matches!(&piece.content, Content::Compact(id, _) if *id == activity.id),
        );
        self.end_press();
        self.dismiss_shown(animate);
        let piece = match revived {
            Some(index) => {
                let mut piece = self.leaving.remove(index);
                if let Content::Compact(_, parts) = &mut piece.content {
                    content::update_compact(&self.look, parts, activity, wing);
                }
                self.revive(&mut piece, animate);
                piece
            }
            None => {
                let parts = content::build_compact(&self.look, activity, wing);
                self.content.addSublayer(&parts.group);
                let mut piece = Piece {
                    id: 0,
                    content: Content::Compact(activity.id, parts),
                };
                self.reveal(&mut piece, animate);
                piece
            }
        };
        self.shown = Some(piece);
        self.set_shape(target, animate);
    }

    fn show_idle(&mut self, animate: bool) {
        let place = self.look.place;
        if !matches!(
            &self.shown,
            Some(Piece {
                content: Content::Idle,
                ..
            })
        ) {
            self.end_press();
            self.dismiss_shown(animate);
            self.shown = Some(Piece {
                id: self.next(),
                content: Content::Idle,
            });
        }
        self.set_shape(place.outline(place.idle), animate);
    }

    /// Materializes new content band by band, top to bottom, each a stagger after the last and
    /// the first a stagger after the shape starts.
    fn reveal(&mut self, piece: &mut Piece, animate: bool) {
        piece.id = self.next();
        if !animate {
            return;
        }
        let stagger = self.tokens.stagger;
        for (index, band) in piece.content.bands().into_iter().enumerate() {
            content::materialize(&self.look, band, stagger * (index as f64 + 1.0));
        }
    }

    /// Brings leaving content back from wherever its exit had taken it.
    fn revive(&mut self, piece: &mut Piece, animate: bool) {
        piece.id = self.next();
        let Some(group) = piece.content.group() else {
            return;
        };
        if animate {
            content::refocus(&self.look, group);
        } else {
            for layer in iter::once(group).chain(piece.content.bands()) {
                content::rest(layer);
            }
        }
    }

    /// Blurs, fades and sinks the shown content at once, ahead of the shape closing over it.
    fn dismiss_shown(&mut self, animate: bool) {
        let Some(mut piece) = self.shown.take() else {
            return;
        };
        let Some(group) = piece.content.group().map(CALayer::retain) else {
            return;
        };
        if !animate {
            group.removeFromSuperlayer();
            return;
        }
        let tokens = self.tokens;
        content::dissolve(&self.look, &group);
        piece.id = self.next();
        let id = piece.id;
        self.leaving.push(piece);
        let _ = self.later(tokens.fast, move |island| {
            if let Some(index) = island.leaving.iter().position(|piece| piece.id == id) {
                let piece = island.leaving.remove(index);
                if let Some(group) = piece.content.group() {
                    group.removeFromSuperlayer();
                }
            }
        });
    }

    fn shown_outline(&self) -> Outline {
        // SAFETY: the presentation layer is a read-only copy of the fill layer.
        unsafe { self.fill.presentationLayer() }
            .and_then(|layer| layer.downcast::<CAShapeLayer>().ok())
            .and_then(|layer| layer.path())
            .and_then(|path| draw::read_outline(&path))
            .unwrap_or(self.target)
    }

    /// Springs the fill and the content mask from what shows now to `target`; the same target
    /// again changes nothing.
    fn set_shape(&mut self, target: Outline, animate: bool) {
        if target == self.target {
            return;
        }
        let place = self.look.place;
        let now = draw::now();
        let shown = self.shown_outline();
        let carried = self.swing.as_ref().map_or(0.0, |swing| {
            let elapsed = now - swing.start;
            if elapsed >= swing.duration {
                return 0.0;
            }
            swing.spring.velocity(elapsed, swing.velocity)
                * shown.share_along(&swing.from, &swing.to, &target)
        });
        self.target = target;
        let path = draw::path(&target);
        for shape in [&self.fill, &self.mask] {
            draw::remove(shape, "path");
            shape.setPath(Some(&path));
        }
        let travel = shown.travel(&target);
        cancel(&mut self.timers.settle);
        if !animate || travel <= SETTLE_TOLERANCE {
            self.swing = None;
            let size = size_of(&target);
            self.set_envelope(place.envelope(size, size, 0.0));
            self.update_tracking();
            return;
        }
        let tokens = self.tokens;
        let growing = size_of(&target).area() > size_of(&shown).area();
        let spring = if growing {
            tokens.grow_spring()
        } else {
            tokens.shrink_spring()
        };
        // Closing from rest, the shape trails the content leaving inside it; a moving shape
        // retargets at once so it never stalls.
        let resting = self
            .swing
            .as_ref()
            .is_none_or(|swing| now - swing.start >= swing.duration);
        let lead = if !growing && resting && !self.leaving.is_empty() {
            tokens.stagger
        } else {
            0.0
        };
        let duration = spring.settle_time(travel, SETTLE_TOLERANCE, carried);
        let (low, high) = spring.excursion(carried, duration);
        let swing_share = (high - 1.0).max(-low).max(0.0);
        let needed = place.envelope(size_of(&shown), size_of(&target), swing_share);
        self.set_envelope(Size::new(
            needed.width.max(self.envelope.width),
            needed.height.max(self.envelope.height),
        ));
        let from = draw::path(&shown);
        let animation = draw::spring(
            "path",
            draw::path_value(&from),
            draw::path_value(&path),
            spring,
            carried,
            duration,
        );
        draw::delayed(&animation, lead);
        for shape in [&self.fill, &self.mask] {
            draw::add(shape, "path", &animation);
        }
        let id = self.next();
        self.swing = Some(Swing {
            id,
            from: shown,
            to: target,
            start: now + lead,
            spring,
            velocity: carried,
            duration,
            peak: spring.peak_speed(carried) * travel,
        });
        self.timers.settle = Some(self.later(lead + duration + 1.0 / 60.0, move |island| {
            island.settle(id)
        }));
        self.update_tracking();
    }

    /// The motion ended: the window shrinks to the shape at rest.
    fn settle(&mut self, id: u64) {
        if self.swing.as_ref().is_none_or(|swing| swing.id != id) {
            return;
        }
        self.swing = None;
        self.timers.settle = None;
        let size = size_of(&self.target);
        self.set_envelope(self.look.place.envelope(size, size, 0.0));
        self.update_tracking();
    }

    /// The window and every layer in it change together, so the island never moves on screen.
    fn set_envelope(&mut self, envelope: Size) {
        if envelope == self.envelope {
            return;
        }
        self.envelope = envelope;
        let frame = self.look.place.frame(envelope);
        self.panel.setFrame_display(draw::rect(frame), true);
        self.lay_out_stage();
        self.place_accessibility();
    }

    /// Island coordinates (origin at the top center, y up) for every layer under the stage.
    fn lay_out_stage(&self) {
        let envelope = self.envelope;
        self.root.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(envelope.width, envelope.height),
        ));
        let area = Rect::new(
            -envelope.width / 2.0,
            -envelope.height,
            envelope.width,
            envelope.height,
        );
        self.stage.setBounds(draw::rect(area));
        self.stage
            .setPosition(NSPoint::new(envelope.width / 2.0, envelope.height / 2.0));
        draw::share_space(&self.fill, area);
        draw::share_space(&self.content, area);
        draw::share_space(&self.mask, area);
    }

    /// Hover follows the target outline; a pointer already inside a new area counts as inside.
    fn update_tracking(&mut self) {
        let bounds = self.target.bounds();
        let origin = self.view.origin();
        let rect = NSRect::new(
            NSPoint::new(origin.x + bounds.x, origin.y + bounds.y),
            NSSize::new(bounds.width, bounds.height),
        );
        self.view.set_hover_rect(rect);
        if !self.context.interactive || self.hidden.any() {
            return;
        }
        let mouse = NSEvent::mouseLocation();
        let frame = self.panel.frame();
        let local = Rect::new(
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
        );
        let inside = contains(
            local,
            NSPoint::new(mouse.x - frame.origin.x, mouse.y - frame.origin.y),
        );
        self.set_inside(inside);
    }

    fn open_parts(&self) -> Option<&OpenParts> {
        match &self.shown {
            Some(Piece {
                content: Content::Open(parts),
                ..
            }) if self.expanded => Some(parts),
            _ => None,
        }
    }

    fn dragging(&self) -> bool {
        self.press.as_ref().is_some_and(|press| {
            self.open_parts()
                .and_then(|parts| parts.controls.get(press.index))
                .is_some_and(|control| control.id == ControlId::Level)
        })
    }

    fn control_at(&self, point: NSPoint) -> Option<usize> {
        self.open_parts()?
            .controls
            .iter()
            .position(|control| contains(control.hit, point))
    }

    fn place_accessibility(&self) {
        let children = match self.open_parts() {
            Some(parts) => {
                let view: &AnyObject = &self.view;
                content::place_elements(parts, self.view.origin(), view);
                parts
                    .controls
                    .iter()
                    .filter_map(|control| control.element.clone())
                    .collect()
            }
            None => Vec::new(),
        };
        self.view.set_accessibility_children(children);
    }

    fn on_pointer(&mut self, event: Pointer) {
        if !self.context.interactive {
            return;
        }
        match event {
            Pointer::Entered => self.set_inside(true),
            Pointer::Exited => self.set_inside(false),
            Pointer::Moved(point) => self.hover_at(Some(point)),
            Pointer::Down(point) => self.press_at(point),
            Pointer::Dragged(point) => self.drag_to(point),
            Pointer::Up(point) => self.release_at(point),
        }
    }

    /// Dwelling opens the island; leaving it open closes it after a grace period.
    fn set_inside(&mut self, inside: bool) {
        if self.inside == inside {
            return;
        }
        self.inside = inside;
        self.outbox.hover = Some(inside);
        cancel(&mut self.timers.open);
        cancel(&mut self.timers.close);
        if inside {
            if !self.expanded && !self.input_away() {
                self.timers.open = Some(self.later(self.tokens.hover_delay, |island| {
                    island.timers.open = None;
                    if island.inside {
                        island.set_expanded(true);
                    }
                }));
            }
        } else {
            self.hover_at(None);
            self.close_soon();
        }
    }

    fn close_soon(&mut self) {
        if !self.expanded || self.inside || self.press.is_some() {
            return;
        }
        cancel(&mut self.timers.close);
        self.timers.close = Some(self.later(self.tokens.close_delay, |island| {
            island.timers.close = None;
            if !island.inside && island.press.is_none() {
                island.set_expanded(false);
            }
        }));
    }

    /// Buttons light up and the slider's thumb grows under the pointer, as in the window.
    fn hover_at(&mut self, point: Option<NSPoint>) {
        if self.press.is_some() {
            return;
        }
        let index = point
            .and_then(|point| self.control_at(point))
            .filter(|index| {
                self.open_parts().is_some_and(|parts| {
                    !matches!(parts.controls[*index].widget, Widget::Switch { .. })
                })
            });
        if index == self.hovered {
            return;
        }
        let previous = std::mem::replace(&mut self.hovered, index);
        for (index, on) in [(previous, false), (index, true)] {
            if let Some(index) = index {
                self.light(index, on);
            }
        }
    }

    fn light(&self, index: usize, on: bool) {
        let Some(control) = self
            .open_parts()
            .and_then(|parts| parts.controls.get(index))
        else {
            return;
        };
        let tokens = self.tokens;
        match &control.widget {
            Widget::Button { .. } => content::set_button_hover(&self.look, &control.widget, on),
            Widget::Slider { .. } => {
                let scale = if on { tokens.scale_thumb_hover } else { 1.0 };
                content::set_thumb_scale(&self.look, &control.widget, scale);
            }
            Widget::Switch { .. } => {}
        }
    }

    fn press_at(&mut self, point: NSPoint) {
        if !self.expanded {
            if !self.input_away() {
                self.set_expanded(true);
            }
            return;
        }
        cancel(&mut self.timers.close);
        let Some(index) = self.control_at(point) else {
            return;
        };
        self.press = Some(Press { index, over: true });
        let tokens = self.tokens;
        let Some(control) = self
            .open_parts()
            .and_then(|parts| parts.controls.get(index))
        else {
            return;
        };
        if matches!(control.widget, Widget::Slider { .. }) {
            content::set_thumb_scale(&self.look, &control.widget, tokens.scale_thumb_active);
            self.slide(index, point.x);
        } else {
            content::scale_to(
                &self.look,
                &control.root,
                tokens.scale_press,
                tokens.instant,
            );
        }
    }

    /// Moves the slider under the pointer and previews that darkness, as the window's slider does.
    fn slide(&mut self, index: usize, x: f64) {
        let look = &self.look;
        let Some(Piece {
            content: Content::Open(parts),
            ..
        }) = &mut self.shown
        else {
            return;
        };
        let Some(control) = parts.controls.get_mut(index) else {
            return;
        };
        let Some(level) = content::slider_level(look, &control.widget, x - control.hit.x) else {
            return;
        };
        if control.value() == level {
            return;
        }
        content::set_slider(look, &mut control.widget, level, true);
        control.pending = Some(Pending {
            value: level,
            until: Instant::now() + PENDING,
        });
        if let Some(element) = &control.element {
            content::refresh_element_value(control, element);
        }
        self.outbox.actions.push(Action::PreviewLevel(level));
    }

    fn drag_to(&mut self, point: NSPoint) {
        let Some(press) = &self.press else {
            return;
        };
        let index = press.index;
        let was_over = press.over;
        let Some(control) = self
            .open_parts()
            .and_then(|parts| parts.controls.get(index))
        else {
            return;
        };
        if matches!(control.widget, Widget::Slider { .. }) {
            self.slide(index, point.x);
            return;
        }
        let over = contains(control.hit, point);
        if over != was_over {
            let tokens = self.tokens;
            let scale = if over { tokens.scale_press } else { 1.0 };
            content::scale_to(&self.look, &control.root, scale, tokens.instant);
            if let Some(press) = &mut self.press {
                press.over = over;
            }
        }
    }

    fn release_at(&mut self, point: NSPoint) {
        let Some(press) = self.press.take() else {
            return;
        };
        let tokens = self.tokens;
        let Some(control) = self
            .open_parts()
            .and_then(|parts| parts.controls.get(press.index))
        else {
            return;
        };
        let over = contains(control.hit, point);
        if matches!(control.widget, Widget::Slider { .. }) {
            let scale = if over { tokens.scale_thumb_hover } else { 1.0 };
            content::set_thumb_scale(&self.look, &control.widget, scale);
            let level = control.value();
            self.hovered = over.then_some(press.index);
            self.outbox.actions.push(Action::SetLevel(level));
        } else {
            content::scale_to(&self.look, &control.root, 1.0, tokens.instant);
            if over {
                self.activate(press.index);
            }
        }
        self.close_soon();
    }

    /// The pressed and the hovered control, by what they change, to find again once rows move.
    fn held(&self) -> (Option<ControlId>, Option<ControlId>) {
        let id = |index: usize| {
            self.open_parts()
                .and_then(|parts| parts.controls.get(index))
                .map(|control| control.id.clone())
        };
        (
            self.press.as_ref().and_then(|press| id(press.index)),
            self.hovered.and_then(id),
        )
    }

    fn hold(&mut self, (pressed, hovered): (Option<ControlId>, Option<ControlId>)) {
        let find = |id: Option<ControlId>| {
            let id = id?;
            self.open_parts()?
                .controls
                .iter()
                .position(|control| control.id == id)
        };
        let (pressed, hovered) = (find(pressed), find(hovered));
        match (&mut self.press, pressed) {
            (Some(press), Some(index)) => press.index = index,
            (press, _) => *press = None,
        }
        self.hovered = hovered;
    }

    /// A drag that the island stops tracking (it closed or rebuilt) still saves its level.
    fn end_press(&mut self) {
        let dragged = self
            .press
            .as_ref()
            .filter(|_| self.dragging())
            .and_then(|press| self.open_parts()?.controls.get(press.index))
            .map(|control| control.value());
        self.outbox.actions.extend(dragged.map(Action::SetLevel));
        self.press = None;
        self.hovered = None;
    }

    /// A switch flips at once and holds the new value until the app reports it; buttons act.
    fn activate(&mut self, index: usize) {
        let look = &self.look;
        let Some(Piece {
            content: Content::Open(parts),
            ..
        }) = &mut self.shown
        else {
            return;
        };
        let Some(control) = parts.controls.get_mut(index) else {
            return;
        };
        let turning_on = control.value() == 0;
        let action = match &control.id {
            ControlId::OpenWindow | ControlId::SetUp => Some(Action::OpenWindow),
            ControlId::Computer(fingerprint) => Some(Action::SetComputer {
                fingerprint: fingerprint.clone(),
                enabled: turning_on,
            }),
            ControlId::Dimming => Some(Action::ToggleDimming),
            ControlId::Clipboard => Some(Action::SetClipboard(turning_on)),
            ControlId::Level => None,
        };
        if matches!(control.widget, Widget::Switch { .. }) {
            content::set_switch(look, &mut control.widget, turning_on);
            control.pending = Some(Pending {
                value: u8::from(turning_on),
                until: Instant::now() + PENDING,
            });
            if let Some(element) = &control.element {
                content::refresh_element_value(control, element);
            }
        }
        self.outbox.actions.extend(action);
    }

    /// Only the element the shown controls own acts; one an assistive app kept from rebuilt or
    /// ended content does nothing.
    fn assist(&mut self, element: &ControlElement, kind: Assist) -> bool {
        let index = element.control();
        let Some(control) = self
            .open_parts()
            .and_then(|parts| parts.controls.get(index))
            .filter(|control| {
                control
                    .element
                    .as_deref()
                    .is_some_and(|own| std::ptr::eq(own, element))
            })
        else {
            return false;
        };
        let slider = match &control.widget {
            Widget::Slider {
                min, max, level, ..
            } => Some((*min, *max, *level)),
            _ => None,
        };
        match (slider, kind) {
            (None, Assist::Press) => {
                self.activate(index);
                true
            }
            (Some((min, max, level)), Assist::Increment | Assist::Decrement) => {
                let next = if kind == Assist::Increment {
                    level.saturating_add(LEVEL_STEP).min(max)
                } else {
                    level.saturating_sub(LEVEL_STEP).max(min)
                };
                let look = &self.look;
                if let Some(Piece {
                    content: Content::Open(parts),
                    ..
                }) = &mut self.shown
                    && let Some(control) = parts.controls.get_mut(index)
                {
                    content::set_slider(look, &mut control.widget, next, false);
                    control.pending = Some(Pending {
                        value: next,
                        until: Instant::now() + PENDING,
                    });
                    if let Some(element) = &control.element {
                        content::refresh_element_value(control, element);
                    }
                }
                self.outbox.actions.push(Action::SetLevel(next));
                true
            }
            _ => false,
        }
    }

    fn probe(&self) -> Probe {
        let shown = self.shown_outline();
        let moving = self
            .fill
            .animationKeys()
            .is_some_and(|keys| keys.count() > 0);
        let bounds = shown.bounds();
        let envelope = self.envelope;
        let slack = 0.01;
        let fits = bounds.x >= -envelope.width / 2.0 - slack
            && bounds.max_x() <= envelope.width / 2.0 + slack
            && bounds.y >= -envelope.height - slack
            && bounds.max_y() <= slack;
        let size = size_of(&self.target);
        let place = self.look.place;
        let still = |layer: &CALayer| layer.animationKeys().is_none_or(|keys| keys.count() == 0);
        let content_rest = self.leaving.is_empty()
            && self.shown.as_ref().is_none_or(|piece| {
                piece
                    .content
                    .group()
                    .is_none_or(|group| group.opacity() == 1.0 && still(group))
                    && piece.content.bands().into_iter().all(still)
            });
        Probe {
            shown,
            target: self.target,
            idle: place.outline(place.idle),
            moving,
            at_rest: !moving
                && self.swing.is_none()
                && self.envelope == place.envelope(size, size, 0.0)
                && content_rest,
            fits,
            expanded: self.expanded,
            peak_speed: self.swing.as_ref().map_or(0.0, |swing| swing.peak),
            content: self.shown.as_ref().map(|piece| piece.id),
            status_drift: match &self.shown {
                Some(Piece {
                    content: Content::Open(parts),
                    ..
                }) => Some(content::status_drift(&self.look, parts)),
                _ => None,
            },
        }
    }
}
