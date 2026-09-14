//! Main-thread dispatch gate for marked controlled-trial input.

use std::{
    cell::RefCell,
    ptr::{NonNull, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use block2::RcBlock;
use monhop_core::{Point, RevocationSignal};
use objc2::{MainThreadMarker, rc::Retained, runtime::AnyObject};
use objc2_app_kit::{NSApplication, NSEvent, NSEventMask, NSEventType};
use objc2_core_graphics::{CGEvent, CGEventField};
use objc2_foundation::NSProcessInfo;
use serde::Serialize;

thread_local! {
    static DISPATCH: RefCell<Option<DispatchState>> = const { RefCell::new(None) };
}

struct DispatchState {
    _monitor: Retained<AnyObject>,
    active: Option<ActiveGuard>,
}

struct ActiveGuard {
    window_id: usize,
    focused: Arc<AtomicBool>,
    cancel: RevocationSignal,
    cutoff: f64,
    receiver_bounds: (Point, Point),
    counters: Arc<ArrivalCounters>,
}

#[derive(Clone, Default, Serialize)]
pub struct ArrivalCounts {
    pub keyboard: u64,
    pub button: u64,
    pub movement: u64,
    pub scroll: u64,
}

pub struct ArrivalCounters {
    keyboard: AtomicU64,
    button: AtomicU64,
    movement: AtomicU64,
    scroll: AtomicU64,
}

impl ArrivalCounters {
    pub fn snapshot(&self) -> ArrivalCounts {
        ArrivalCounts {
            keyboard: self.keyboard.load(Ordering::Relaxed),
            button: self.button.load(Ordering::Relaxed),
            movement: self.movement.load(Ordering::Relaxed),
            scroll: self.scroll.load(Ordering::Relaxed),
        }
    }

    fn record(&self, category: ArrivalCategory) {
        let count = match category {
            ArrivalCategory::Keyboard => &self.keyboard,
            ArrivalCategory::Button => &self.button,
            ArrivalCategory::Movement => &self.movement,
            ArrivalCategory::Scroll => &self.scroll,
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for ArrivalCounters {
    fn default() -> Self {
        Self {
            keyboard: AtomicU64::new(0),
            button: AtomicU64::new(0),
            movement: AtomicU64::new(0),
            scroll: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Copy)]
struct GuardFacts {
    window_id: usize,
    key_window_id: Option<usize>,
    focused: bool,
    revoked: bool,
    cutoff: f64,
    receiver_bounds: (Point, Point),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArrivalCategory {
    Keyboard,
    Button,
    Movement,
    Scroll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchAction {
    Pass,
    Consume(Option<ArrivalCategory>),
}

pub fn install(_marker: MainThreadMarker) -> Result<(), String> {
    DISPATCH.with(|slot| {
        let mut state = slot.borrow_mut();
        if state.is_some() {
            return Ok(());
        }
        let handler: RcBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent> =
            RcBlock::new(dispatch_event);
        // SAFETY: AppKit copies this local-monitor block and calls it on the main thread.
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(input_event_mask(), &handler)
        }
        .ok_or_else(|| "Could not install the controlled-test event gate.".to_owned())?;
        *state = Some(DispatchState {
            _monitor: monitor,
            active: None,
        });
        Ok(())
    })
}

pub fn arm(
    _marker: MainThreadMarker,
    window_id: usize,
    focused: Arc<AtomicBool>,
    cancel: RevocationSignal,
    receiver_bounds: (Point, Point),
) -> Result<Arc<ArrivalCounters>, String> {
    let cutoff = NSProcessInfo::processInfo().systemUptime();
    if !cutoff.is_finite() {
        return Err("Could not establish the controlled-test input epoch.".into());
    }
    if !receiver_bounds_are_valid(receiver_bounds) {
        return Err("Could not establish the controlled-test input bounds.".into());
    }
    let counters = Arc::new(ArrivalCounters::default());
    DISPATCH.with(|slot| {
        let mut state = slot.borrow_mut();
        let state = state.as_mut().ok_or_else(|| {
            "Open the test window before starting the controlled test.".to_owned()
        })?;
        state.active = Some(ActiveGuard {
            window_id,
            focused,
            cancel,
            cutoff,
            receiver_bounds,
            counters: Arc::clone(&counters),
        });
        Ok(counters)
    })
}

/// Removes the armed guard. The monitor stays installed and passes everything through.
pub fn disarm(_marker: MainThreadMarker) {
    DISPATCH.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.active = None;
        }
    });
}

fn dispatch_event(event: NonNull<NSEvent>) -> *mut NSEvent {
    let event_ptr = event.as_ptr();
    // SAFETY: AppKit provides this non-null event for the duration of the monitor callback.
    let event = unsafe { event.as_ref() };
    if !is_marked(event) {
        return event_ptr;
    }
    let event_window_number = event.windowNumber();
    let timestamp = event.timestamp();
    let category = arrival_category(event.r#type());
    let location = pointer_arrival(category)
        .then(|| event_location(event))
        .flatten();
    let key_window_id = current_key_window_id();
    DISPATCH.with(|slot| {
        let state = slot.borrow();
        let Some(active) = state.as_ref().and_then(|state| state.active.as_ref()) else {
            return monitor_result(monitor_action(true, false, false, category), event_ptr);
        };
        let facts = GuardFacts {
            window_id: active.window_id,
            key_window_id,
            focused: active.focused.load(Ordering::Acquire),
            revoked: active.cancel.is_revoked(),
            cutoff: active.cutoff,
            receiver_bounds: active.receiver_bounds,
        };
        let action = monitor_action(
            true,
            true,
            dispatch_allowed(
                true,
                event_window_number,
                category,
                timestamp,
                location,
                Some(facts),
            ),
            category,
        );
        record_action(&active.counters, action);
        monitor_result(action, event_ptr)
    })
}

fn monitor_result(action: DispatchAction, event: *mut NSEvent) -> *mut NSEvent {
    match action {
        DispatchAction::Pass => event,
        DispatchAction::Consume(_) => null_mut(),
    }
}

fn is_marked(event: &NSEvent) -> bool {
    event.CGEvent().is_some_and(|event| {
        CGEvent::integer_value_field(Some(&event), CGEventField::EventSourceUserData)
            == monhop_platform_macos::SYNTHETIC_EVENT_MARKER
    })
}

fn current_key_window_id() -> Option<usize> {
    let marker = MainThreadMarker::new()?;
    let window = NSApplication::sharedApplication(marker).keyWindow()?;
    window_id(window.windowNumber())
}

fn event_location(event: &NSEvent) -> Option<Point> {
    event.CGEvent().map(|event| {
        let location = CGEvent::location(Some(&event));
        Point::new(location.x, location.y)
    })
}

fn window_id(value: isize) -> Option<usize> {
    usize::try_from(value).ok()
}

fn dispatch_allowed(
    marked: bool,
    event_window_number: isize,
    category: Option<ArrivalCategory>,
    timestamp: f64,
    location: Option<Point>,
    facts: Option<GuardFacts>,
) -> bool {
    if !marked {
        return true;
    }
    let Some(facts) = facts else {
        return false;
    };
    let authorized = facts.focused
        && !facts.revoked
        && facts.key_window_id == Some(facts.window_id)
        && facts.cutoff.is_finite()
        && timestamp.is_finite()
        && timestamp >= facts.cutoff;
    let pointer_in_bounds = !pointer_arrival(category)
        || location.is_some_and(|point| point_in_bounds(point, facts.receiver_bounds));
    let window_matches = window_id(event_window_number) == Some(facts.window_id)
        || (event_window_number == 0 && pointer_arrival(category));
    authorized && pointer_in_bounds && window_matches
}

fn pointer_arrival(category: Option<ArrivalCategory>) -> bool {
    matches!(
        category,
        Some(ArrivalCategory::Button | ArrivalCategory::Movement | ArrivalCategory::Scroll)
    )
}

fn receiver_bounds_are_valid(bounds: (Point, Point)) -> bool {
    let (origin, size) = bounds;
    origin.is_finite()
        && size.is_finite()
        && size.x > 0.0
        && size.y > 0.0
        && (origin.x + size.x).is_finite()
        && (origin.y + size.y).is_finite()
}

fn point_in_bounds(point: Point, bounds: (Point, Point)) -> bool {
    if !receiver_bounds_are_valid(bounds) || !point.is_finite() {
        return false;
    }
    let (origin, size) = bounds;
    point.x >= origin.x
        && point.x < origin.x + size.x
        && point.y >= origin.y
        && point.y < origin.y + size.y
}

/// A disarmed gate never consumes: a later session's marked input is not this test's to eat.
fn monitor_action(
    marked: bool,
    armed: bool,
    accepted: bool,
    category: Option<ArrivalCategory>,
) -> DispatchAction {
    if !marked || !armed {
        DispatchAction::Pass
    } else if accepted {
        DispatchAction::Consume(category)
    } else {
        DispatchAction::Consume(None)
    }
}

fn record_action(counters: &ArrivalCounters, action: DispatchAction) {
    if let DispatchAction::Consume(Some(category)) = action {
        counters.record(category);
    }
}

fn arrival_category(event_type: NSEventType) -> Option<ArrivalCategory> {
    match event_type {
        NSEventType::KeyDown | NSEventType::KeyUp | NSEventType::FlagsChanged => {
            Some(ArrivalCategory::Keyboard)
        }
        NSEventType::LeftMouseDown
        | NSEventType::LeftMouseUp
        | NSEventType::RightMouseDown
        | NSEventType::RightMouseUp
        | NSEventType::OtherMouseDown
        | NSEventType::OtherMouseUp => Some(ArrivalCategory::Button),
        NSEventType::MouseMoved
        | NSEventType::LeftMouseDragged
        | NSEventType::RightMouseDragged
        | NSEventType::OtherMouseDragged => Some(ArrivalCategory::Movement),
        NSEventType::ScrollWheel => Some(ArrivalCategory::Scroll),
        _ => None,
    }
}

fn input_event_mask() -> NSEventMask {
    NSEventMask::KeyDown
        | NSEventMask::KeyUp
        | NSEventMask::FlagsChanged
        | NSEventMask::LeftMouseDown
        | NSEventMask::LeftMouseUp
        | NSEventMask::RightMouseDown
        | NSEventMask::RightMouseUp
        | NSEventMask::OtherMouseDown
        | NSEventMask::OtherMouseUp
        | NSEventMask::MouseMoved
        | NSEventMask::LeftMouseDragged
        | NSEventMask::RightMouseDragged
        | NSEventMask::OtherMouseDragged
        | NSEventMask::ScrollWheel
}

#[cfg(test)]
mod tests {
    use monhop_core::Point;

    use super::{
        ArrivalCategory, ArrivalCounters, DispatchAction, GuardFacts, arrival_category,
        dispatch_allowed, monitor_action, record_action,
    };
    use objc2_app_kit::NSEventType;

    fn active_guard() -> GuardFacts {
        GuardFacts {
            window_id: 41,
            key_window_id: Some(41),
            focused: true,
            revoked: false,
            cutoff: 100.0,
            receiver_bounds: (Point::new(10.0, 20.0), Point::new(4.0, 6.0)),
        }
    }

    fn marked_allowed(
        event_window_number: isize,
        category: Option<ArrivalCategory>,
        timestamp: f64,
        location: Option<Point>,
        facts: GuardFacts,
    ) -> bool {
        dispatch_allowed(
            true,
            event_window_number,
            category,
            timestamp,
            location,
            Some(facts),
        )
    }

    #[test]
    fn stale_marked_event_is_rejected_after_a_new_arm() {
        assert!(!marked_allowed(
            41,
            Some(ArrivalCategory::Keyboard),
            99.999,
            None,
            active_guard(),
        ));
    }

    #[test]
    fn marked_event_for_an_unknown_window_is_rejected() {
        assert!(!marked_allowed(
            99,
            Some(ArrivalCategory::Movement),
            100.0,
            Some(Point::new(11.0, 21.0)),
            active_guard(),
        ));
    }

    #[test]
    fn marked_event_without_a_window_is_rejected() {
        assert!(!marked_allowed(
            0,
            Some(ArrivalCategory::Keyboard),
            100.0,
            Some(Point::new(11.0, 21.0)),
            active_guard(),
        ));
    }

    #[test]
    fn revocation_rejects_marked_event() {
        let mut guard = active_guard();
        guard.revoked = true;
        assert!(!marked_allowed(
            41,
            Some(ArrivalCategory::Keyboard),
            100.0,
            None,
            guard,
        ));
    }

    #[test]
    fn nonfinite_marked_timestamp_is_rejected() {
        assert!(!marked_allowed(
            41,
            Some(ArrivalCategory::Keyboard),
            f64::NAN,
            None,
            active_guard(),
        ));
    }

    #[test]
    fn nonfinite_epoch_is_rejected() {
        let mut guard = active_guard();
        guard.cutoff = f64::INFINITY;
        assert!(!marked_allowed(
            41,
            Some(ArrivalCategory::Keyboard),
            100.0,
            None,
            guard,
        ));
    }

    #[test]
    fn physical_event_passes_without_a_guard() {
        assert!(dispatch_allowed(false, 0, None, f64::NAN, None, None,));
    }

    #[test]
    fn exact_current_window_marked_event_is_accepted_for_counting() {
        assert!(marked_allowed(
            41,
            Some(ArrivalCategory::Keyboard),
            100.0,
            None,
            active_guard(),
        ));
    }

    #[test]
    fn unassociated_pointer_at_pad_boundaries_is_accepted() {
        assert!(marked_allowed(
            0,
            Some(ArrivalCategory::Movement),
            100.0,
            Some(Point::new(10.0, 20.0)),
            active_guard(),
        ));
        assert!(marked_allowed(
            0,
            Some(ArrivalCategory::Scroll),
            100.0,
            Some(Point::new(13.999, 25.999)),
            active_guard(),
        ));
    }

    #[test]
    fn unassociated_pointer_outside_the_pad_is_rejected() {
        assert!(!marked_allowed(
            0,
            Some(ArrivalCategory::Button),
            100.0,
            Some(Point::new(14.0, 21.0)),
            active_guard(),
        ));
        assert!(!marked_allowed(
            0,
            Some(ArrivalCategory::Movement),
            100.0,
            Some(Point::new(11.0, 26.0)),
            active_guard(),
        ));
    }

    #[test]
    fn associated_pointer_outside_the_pad_is_rejected() {
        assert!(!marked_allowed(
            41,
            Some(ArrivalCategory::Movement),
            100.0,
            Some(Point::new(14.0, 21.0)),
            active_guard(),
        ));
    }

    #[test]
    fn unassociated_pointer_with_nonfinite_location_is_rejected() {
        assert!(!marked_allowed(
            0,
            Some(ArrivalCategory::Movement),
            100.0,
            Some(Point::new(f64::NAN, 21.0)),
            active_guard(),
        ));
    }

    #[test]
    fn stale_unassociated_pointer_is_rejected() {
        assert!(!marked_allowed(
            0,
            Some(ArrivalCategory::Scroll),
            99.999,
            Some(Point::new(11.0, 21.0)),
            active_guard(),
        ));
    }

    #[test]
    fn revocation_rejects_unassociated_pointer() {
        let mut guard = active_guard();
        guard.revoked = true;
        assert!(!marked_allowed(
            0,
            Some(ArrivalCategory::Button),
            100.0,
            Some(Point::new(11.0, 21.0)),
            guard,
        ));
    }

    #[test]
    fn accepted_marked_keyboard_arrival_is_counted_and_consumed() {
        let counters = ArrivalCounters::default();
        let action = monitor_action(true, true, true, arrival_category(NSEventType::KeyDown));
        record_action(&counters, action);
        assert_eq!(
            action,
            DispatchAction::Consume(Some(ArrivalCategory::Keyboard)),
        );
        assert_eq!(counters.snapshot().keyboard, 1);
    }

    #[test]
    fn blocked_marked_arrival_is_consumed_without_a_count() {
        let counters = ArrivalCounters::default();
        let action = monitor_action(true, true, false, arrival_category(NSEventType::MouseMoved));
        record_action(&counters, action);
        assert_eq!(action, DispatchAction::Consume(None),);
        assert_eq!(counters.snapshot().movement, 0);
    }

    #[test]
    fn physical_arrival_passes_untouched() {
        assert_eq!(
            monitor_action(false, true, false, Some(ArrivalCategory::Keyboard)),
            DispatchAction::Pass,
        );
    }

    #[test]
    fn a_disarmed_gate_passes_marked_events_through_without_counting_them() {
        let counters = ArrivalCounters::default();
        let action = monitor_action(true, false, false, Some(ArrivalCategory::Keyboard));
        record_action(&counters, action);
        assert_eq!(action, DispatchAction::Pass);
        assert_eq!(counters.snapshot().keyboard, 0);
        // Without an armed guard there are no facts to judge, so nothing is ever accepted.
        assert!(!dispatch_allowed(
            true,
            41,
            Some(ArrivalCategory::Keyboard),
            100.0,
            None,
            None,
        ));
    }

    #[test]
    fn event_types_have_only_the_four_arrival_categories() {
        assert_eq!(
            arrival_category(NSEventType::KeyUp),
            Some(ArrivalCategory::Keyboard),
        );
        assert_eq!(
            arrival_category(NSEventType::FlagsChanged),
            Some(ArrivalCategory::Keyboard),
        );
        assert_eq!(
            arrival_category(NSEventType::OtherMouseDown),
            Some(ArrivalCategory::Button),
        );
        assert_eq!(
            arrival_category(NSEventType::RightMouseUp),
            Some(ArrivalCategory::Button),
        );
        assert_eq!(
            arrival_category(NSEventType::LeftMouseDragged),
            Some(ArrivalCategory::Movement),
        );
        assert_eq!(
            arrival_category(NSEventType::ScrollWheel),
            Some(ArrivalCategory::Scroll),
        );
    }
}
