//! The island on screen: one non-activating panel around the camera notch that every pointer
//! event passes through, drawn with Core Animation, and hidden while no display has a notch. Main
//! thread only. Its state lives in one thread-local that AppKit callbacks reach through
//! `with_island`; what it tells the feed leaves only after that borrow ends.

mod classes;
mod content;
mod draw;

use std::{
    cell::{Cell, RefCell},
    iter,
    ptr::NonNull,
    rc::Rc,
};

use block2::RcBlock;
use objc2::{
    MainThreadMarker, MainThreadOnly, Message,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    sel,
};
use objc2_app_kit::{
    NSApplicationDidChangeScreenParametersNotification, NSColor, NSScreen, NSStatusWindowLevel,
    NSView, NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWorkspace,
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
    classes::{IslandPanel, LockObserver},
    content::{CompactParts, Look},
};
use super::{
    geometry::{self, DisplayInfo, Outline, Placement, Rect, Size},
    model::Activity,
    tokens::{self, IslandTokens, Spring},
};

/// The shape counts as arrived within this many points.
const SETTLE_TOLERANCE: f64 = 0.5;
/// Screen changes arrive in bursts; the island re-places once they stop.
const SCREENS_QUIET: f64 = 0.1;

/// Whether Reduce Motion is honored; the island check always animates so it can measure motion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    FollowSystem,
    Always,
}

pub struct Context {
    pub motion: Motion,
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
    /// The fastest any point of the current motion travels, in points per second.
    pub peak_speed: f64,
    /// The content tree on show; a rebuild or a new entrance changes it.
    pub content: Option<u64>,
    /// The band of the content on show is growing or shrinking.
    pub band_scaling: bool,
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

/// Shows an activity, or the island at rest, from the feed that started this island.
pub fn render(mtm: MainThreadMarker, generation: u64, activity: Option<Activity>) {
    let _ = mtm;
    with_island_soon(move |island| island.show(generation, activity));
}

pub fn probe(mtm: MainThreadMarker) -> Option<Probe> {
    let _ = mtm;
    with_island(|island| island.probe())
}

/// The content at rest holds exactly the layers a fresh build of what it shows would, and no
/// filters; for the island check.
pub fn audit(mtm: MainThreadMarker) -> Option<Result<(), String>> {
    let _ = mtm;
    with_island(|island| island.audit())
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

fn cancel(slot: &mut Option<Retained<NSTimer>>) {
    if let Some(timer) = slot.take() {
        timer.invalidate();
    }
}

/// Runs `work` on the island with implicit animations off, then tells the feed whether it shows.
fn with_island<R>(work: impl FnOnce(&mut Island) -> R) -> Option<R> {
    let (result, context, showing) = ISLAND.with(|slot| {
        let mut slot = slot.try_borrow_mut().ok()?;
        let island = slot.as_mut()?;
        let result = draw::quietly(|| work(island));
        Some((result, Rc::clone(&island.context), island.outbox.take()))
    })?;
    if let Some(showing) = showing {
        (context.showing)(showing);
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

enum Content {
    /// At rest the island is the notch's own black, with nothing inside.
    Idle,
    Compact(CompactParts),
}

impl Content {
    /// Leaves and comes back as one.
    fn group(&self) -> Option<&CALayer> {
        match self {
            Self::Idle => None,
            Self::Compact(parts) => Some(&parts.group),
        }
    }

    /// What arrives inside the group.
    fn band(&self) -> Option<&CALayer> {
        match self {
            Self::Idle => None,
            Self::Compact(parts) => Some(&parts.band),
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
    settle: Option<Retained<NSTimer>>,
    screens: Option<Retained<NSTimer>>,
}

type Observer = (
    Retained<NSNotificationCenter>,
    Retained<ProtocolObject<dyn NSObjectProtocol>>,
);

struct Island {
    mtm: MainThreadMarker,
    generation: u64,
    context: Rc<Context>,
    /// Whether it is on screen, once that changed, for the feed after the borrow ends.
    outbox: Option<bool>,
    tokens: &'static IslandTokens,
    panel: Retained<IslandPanel>,
    root: Retained<CALayer>,
    stage: Retained<CALayer>,
    fill: Retained<CAShapeLayer>,
    content: Retained<CALayer>,
    mask: Retained<CAShapeLayer>,
    look: Look,
    activity: Option<Activity>,
    shown: Option<Piece>,
    leaving: Vec<Piece>,
    target: Outline,
    swing: Option<Swing>,
    envelope: Size,
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
        let view = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), size),
        );
        view.setLayer(Some(&root));
        view.setWantsLayer(true);
        view.setLayerUsesCoreImageFilters(true);

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
        // Nothing in it takes the pointer: clicks reach the menu bar and the apps below.
        panel.setIgnoresMouseEvents(true);
        panel.setContentView(Some(&view));

        Self {
            mtm,
            generation,
            context: Rc::new(context),
            outbox: None,
            tokens,
            panel,
            root,
            stage,
            fill,
            content,
            mask,
            look,
            activity: None,
            shown: None,
            leaving: Vec::new(),
            target,
            swing: None,
            envelope,
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

    /// Stops observing at once; the panel fades out and leaves.
    fn end(mut self) {
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
        cancel(&mut self.timers.settle);
        cancel(&mut self.timers.screens);
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
    }

    fn update_visibility(&mut self) {
        self.outbox = Some(!self.hidden.any());
        if self.hidden.any() {
            self.panel.orderOut(None);
        } else if !self.panel.isVisible() {
            self.panel.orderFrontRegardless();
        }
    }

    fn show(&mut self, generation: u64, activity: Option<Activity>) {
        if generation != self.generation || self.activity == activity {
            return;
        }
        self.activity = activity;
        self.apply(true);
    }

    fn apply(&mut self, animate: bool) {
        let animate = animate && !self.look.reduce;
        match self.activity.clone() {
            Some(activity) => self.show_compact(&activity, animate),
            None => self.show_idle(animate),
        }
    }

    fn show_compact(&mut self, activity: &Activity, animate: bool) {
        let place = self.look.place;
        let wing = content::compact_wing(&self.look, activity);
        let target = place.outline(place.compact(wing));
        if let Some(Piece {
            content: Content::Compact(parts),
            ..
        }) = &mut self.shown
            && parts.continues(activity)
        {
            content::update_compact(&self.look, parts, activity, wing);
            self.set_shape(target, animate);
            return;
        }
        let revived = self.leaving.iter().position(
            |piece| matches!(&piece.content, Content::Compact(parts) if parts.continues(activity)),
        );
        self.dismiss_shown(animate);
        let piece = match revived {
            Some(index) => {
                let mut piece = self.leaving.remove(index);
                if let Content::Compact(parts) = &mut piece.content {
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
                    content: Content::Compact(parts),
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
            self.dismiss_shown(animate);
            self.shown = Some(Piece {
                id: self.next(),
                content: Content::Idle,
            });
        }
        self.set_shape(place.outline(place.idle), animate);
    }

    /// Materializes new content a stagger after the shape starts.
    fn reveal(&mut self, piece: &mut Piece, animate: bool) {
        piece.id = self.next();
        if let Some(band) = piece.content.band().filter(|_| animate) {
            content::materialize(&self.look, band, self.tokens.stagger);
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
            for layer in iter::once(group).chain(piece.content.band()) {
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
                    && piece.content.band().is_none_or(still)
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
            peak_speed: self.swing.as_ref().map_or(0.0, |swing| swing.peak),
            content: self.shown.as_ref().map(|piece| piece.id),
            band_scaling: self
                .shown
                .as_ref()
                .and_then(|piece| piece.content.band())
                .is_some_and(content::scaling),
        }
    }

    fn audit(&self) -> Result<(), String> {
        let groups = usize::from(
            self.shown
                .as_ref()
                .is_some_and(|piece| piece.content.group().is_some()),
        );
        // SAFETY: an array of the layer's sublayers, counted at once.
        let held = unsafe { self.content.sublayers() }.map_or(0, |layers| layers.count());
        if held != groups {
            return Err(format!(
                "the island holds {held} content groups, not {groups}"
            ));
        }
        match (
            self.shown.as_ref().map(|piece| &piece.content),
            &self.activity,
        ) {
            (Some(Content::Compact(parts)), Some(activity)) => {
                content::audit_compact(&self.look, parts, activity)
            }
            (Some(Content::Idle), None) => Ok(()),
            _ => Err("the content on show is not what the island wants".to_owned()),
        }
    }
}
