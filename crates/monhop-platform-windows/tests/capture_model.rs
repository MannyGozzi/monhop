use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use monhop_core::{FloorState, HidUsage, ModifierState, MouseButton, SharedFloor, TakeBackGate};
use monhop_platform_windows::{
    MONHOP_INJECTED_MARKER,
    capture::{
        CAPTURE_QUEUE_CAPACITY, CaptureEvent, CaptureStop, CapturedEvent, MAX_SUPPRESSION_TTL,
        StopReason, SuppressionLease, capture_channel,
    },
    capture_decode::{
        ClickCounter, DecodedInput, DoubleClickSettings, WINDOWS_DEFAULT_DOUBLE_CLICK,
        decode_keyboard, decode_mouse,
    },
    capture_physical::PhysicalCapture,
};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

#[test]
fn quick_nearby_presses_of_one_button_count_up_and_releases_repeat_the_count() {
    let mut clicks = ClickCounter::default();
    let left = MouseButton::Left;
    assert_eq!(
        clicks.count(left, false, ms(0), WINDOWS_DEFAULT_DOUBLE_CLICK),
        1
    );
    assert_eq!(
        clicks.count(left, true, ms(0), WINDOWS_DEFAULT_DOUBLE_CLICK),
        1
    );
    assert_eq!(
        clicks.count(left, false, ms(80), WINDOWS_DEFAULT_DOUBLE_CLICK),
        1
    );
    clicks.moved(2, -1);
    clicks.moved(-1, -1);
    assert_eq!(
        clicks.count(left, true, ms(500), WINDOWS_DEFAULT_DOUBLE_CLICK),
        2
    );
    assert_eq!(
        clicks.count(left, false, ms(560), WINDOWS_DEFAULT_DOUBLE_CLICK),
        2
    );
    assert_eq!(
        clicks.count(left, true, ms(700), WINDOWS_DEFAULT_DOUBLE_CLICK),
        3
    );
    assert_eq!(
        clicks.count(left, false, ms(750), WINDOWS_DEFAULT_DOUBLE_CLICK),
        3
    );
}

#[test]
fn a_late_far_or_different_press_starts_over() {
    let settings = WINDOWS_DEFAULT_DOUBLE_CLICK;
    let left = MouseButton::Left;
    let mut late = ClickCounter::default();
    late.count(left, true, ms(0), settings);
    assert_eq!(late.count(left, true, ms(501), settings), 1);

    for (dx, dy) in [(3, 0), (0, -3)] {
        let mut far = ClickCounter::default();
        far.count(left, true, ms(0), settings);
        far.moved(dx, dy);
        assert_eq!(far.count(left, true, ms(100), settings), 1);
    }

    let mut other = ClickCounter::default();
    other.count(left, true, ms(0), settings);
    assert_eq!(other.count(MouseButton::Right, true, ms(100), settings), 1);
    assert_eq!(
        other.count(left, true, ms(200), settings),
        1,
        "the other button broke the sequence"
    );

    let mut earlier = ClickCounter::default();
    earlier.count(left, true, ms(100), settings);
    assert_eq!(earlier.count(left, true, ms(50), settings), 1);
}

#[test]
fn the_users_own_settings_decide_the_window() {
    let generous = DoubleClickSettings {
        interval: Duration::from_millis(900),
        width: 20,
        height: 20,
    };
    let mut clicks = ClickCounter::default();
    clicks.count(MouseButton::Left, true, ms(0), generous);
    clicks.moved(10, -10);
    assert_eq!(clicks.count(MouseButton::Left, true, ms(900), generous), 2);
    for _ in 0..300 {
        clicks.count(MouseButton::Left, true, ms(900), generous);
    }
    assert_eq!(
        clicks.count(MouseButton::Left, true, ms(900), generous),
        u8::MAX
    );
}

#[test]
fn hook_buttons_decode_as_single_clicks_for_the_capture_thread_to_number() {
    assert!(matches!(
        decode_mouse(WM_LBUTTONDOWN, 0, 0, 0, 0, 0),
        DecodedInput::Event(CaptureEvent::Button {
            button: MouseButton::Left,
            pressed: true,
            click_count: 1,
        })
    ));
}

// Windows SDK `um/winuser.h` values for the hook records below.
const WM_KEYDOWN: u32 = 0x0100;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_MOUSEWHEEL: u32 = 0x020A;
const LLKHF_LOWER_IL_INJECTED: u32 = 0x02;
const LLKHF_INJECTED: u32 = 0x10;
const LLMHF_INJECTED: u32 = 0x01;
const LLMHF_LOWER_IL_INJECTED: u32 = 0x02;

fn motion(value: i32) -> CaptureEvent {
    CaptureEvent::AbsoluteMotion {
        x: value,
        y: -value,
    }
}

#[test]
fn queue_is_fixed_capacity_and_stops_before_any_payload_can_be_read() {
    let stop = CaptureStop::new();
    let (mut producer, mut consumer) = capture_channel(stop.clone());

    for value in 0..CAPTURE_QUEUE_CAPACITY as i32 {
        assert_eq!(producer.try_push(motion(value)), Ok(()));
    }

    assert_eq!(
        producer.try_push(motion(CAPTURE_QUEUE_CAPACITY as i32)),
        Err(StopReason::QueueFull)
    );
    assert_eq!(stop.reason(), Some(StopReason::QueueFull));
    assert_eq!(consumer.try_pop(), Err(StopReason::QueueFull));
}

#[test]
fn stopping_another_way_also_hides_queued_payloads() {
    let stop = CaptureStop::new();
    let (mut producer, mut consumer) = capture_channel(stop.clone());
    assert_eq!(producer.try_push(motion(1)), Ok(()));

    stop.stop(StopReason::Requested);

    assert_eq!(consumer.try_pop(), Err(StopReason::Requested));
}

#[test]
fn dropping_the_consumer_stops_all_producers() {
    let stop = CaptureStop::new();
    let (mut producer, consumer) = capture_channel(stop.clone());
    drop(consumer);

    assert_eq!(stop.reason(), Some(StopReason::ConsumerDropped));
    assert_eq!(
        producer.try_push(motion(1)),
        Err(StopReason::ConsumerDropped)
    );
}

#[test]
fn invalid_events_stop_with_the_first_reason() {
    let stop = CaptureStop::new();
    let (mut producer, _consumer) = capture_channel(stop.clone());
    let invalid = CaptureEvent::Key {
        usage: HidUsage(0),
        pressed: true,
        repeat: false,
        modifiers: ModifierState::default(),
    };

    assert_eq!(producer.try_push(invalid), Err(StopReason::InvalidInput));
    stop.stop(StopReason::Requested);
    assert_eq!(stop.reason(), Some(StopReason::InvalidInput));
}

#[test]
fn queue_preserves_signed_high_resolution_wheel_units() {
    let stop = CaptureStop::new();
    let (mut producer, mut consumer) = capture_channel(stop.clone());
    let event = CaptureEvent::Scroll {
        horizontal: 1,
        vertical: -7,
    };

    assert!(event.is_valid());
    assert_eq!(producer.try_push(event), Ok(()));
    assert_eq!(consumer.try_pop(), Ok(Some(event)));
    assert_eq!(stop.reason(), None);
}

#[test]
fn queue_preserves_mac_logical_f64_payload_bits_without_rounding() {
    let stop = CaptureStop::new();
    let (mut producer, mut consumer) = capture_channel(stop.clone());
    let events = [
        CaptureEvent::LogicalAbsoluteMotion { x: -0.0, y: 12.25 },
        CaptureEvent::LogicalRelativeMotion {
            dx: 0.125,
            dy: -0.375,
        },
        CaptureEvent::LogicalScroll {
            horizontal: -0.015_625,
            vertical: 3.5,
        },
    ];

    for event in events {
        assert!(event.is_valid());
        assert_eq!(producer.try_push(event), Ok(()));
        let received = consumer.try_pop().expect("queue remains active").unwrap();
        match (event, received) {
            (
                CaptureEvent::LogicalAbsoluteMotion { x, y },
                CaptureEvent::LogicalAbsoluteMotion {
                    x: received_x,
                    y: received_y,
                },
            ) => {
                assert_eq!(received_x.to_bits(), x.to_bits());
                assert_eq!(received_y.to_bits(), y.to_bits());
            }
            (
                CaptureEvent::LogicalRelativeMotion { dx, dy },
                CaptureEvent::LogicalRelativeMotion {
                    dx: received_dx,
                    dy: received_dy,
                },
            ) => {
                assert_eq!(received_dx.to_bits(), dx.to_bits());
                assert_eq!(received_dy.to_bits(), dy.to_bits());
            }
            (
                CaptureEvent::LogicalScroll {
                    horizontal,
                    vertical,
                },
                CaptureEvent::LogicalScroll {
                    horizontal: received_horizontal,
                    vertical: received_vertical,
                },
            ) => {
                assert_eq!(received_horizontal.to_bits(), horizontal.to_bits());
                assert_eq!(received_vertical.to_bits(), vertical.to_bits());
            }
            _ => panic!("queue changed the logical event variant"),
        }
    }
    assert_eq!(stop.reason(), None);
}

#[test]
fn capture_event_debug_never_emits_input_values() {
    let event = CaptureEvent::Key {
        usage: HidUsage(0x04),
        pressed: true,
        repeat: true,
        modifiers: ModifierState(ModifierState::LEFT_CONTROL),
    };
    let debug = format!("{event:?}");

    assert_eq!(debug, "CaptureEvent::Key([redacted])");
    assert!(!debug.contains("0x04"));
    assert!(!debug.contains("true"));

    let route_debug = format!(
        "{:?}",
        CaptureEvent::RouteChanged {
            remote: true,
            revision: 42,
        }
    );
    assert_eq!(route_debug, "CaptureEvent::RouteChanged([redacted])");
    assert!(!route_debug.contains("42"));
}

#[test]
fn route_change_barrier_precedes_tagged_events_for_the_new_route() {
    let rejected_stop = CaptureStop::new();
    let (mut rejected_producer, _rejected_consumer) = capture_channel(rejected_stop.clone());
    assert_eq!(
        rejected_producer.try_push_tagged(CapturedEvent {
            event: CaptureEvent::RelativeMotion { dx: 3, dy: -2 },
            routing_revision: 1,
            remote: true,
            floor_generation: 0,
        }),
        Err(StopReason::InvalidInput)
    );
    assert_eq!(rejected_stop.reason(), Some(StopReason::InvalidInput));

    let stop = CaptureStop::new();
    let (mut producer, mut consumer) = capture_channel(stop);
    let barrier = CapturedEvent {
        event: CaptureEvent::RouteChanged {
            remote: true,
            revision: 1,
        },
        routing_revision: 1,
        remote: true,
        floor_generation: 0,
    };
    let event = CapturedEvent {
        event: CaptureEvent::RelativeMotion { dx: 3, dy: -2 },
        routing_revision: 1,
        remote: true,
        floor_generation: 0,
    };

    assert_eq!(producer.try_push_tagged(barrier), Ok(()));
    assert_eq!(producer.try_push_tagged(event), Ok(()));
    assert_eq!(consumer.try_pop_tagged(), Ok(Some(barrier)));
    assert_eq!(consumer.try_pop_tagged(), Ok(Some(event)));
}

#[test]
fn spsc_ring_preserves_100k_fifo_events_and_metadata_across_wraparound() {
    const EVENT_COUNT: usize = 100_000;
    const MAX_AHEAD: usize = 128;

    let stop = CaptureStop::new();
    let (mut producer, mut consumer) = capture_channel(stop.clone());
    let consumed = Arc::new(AtomicUsize::new(0));
    let producer_consumed = Arc::clone(&consumed);

    thread::scope(|scope| {
        scope.spawn(move || {
            let barrier = CapturedEvent {
                event: CaptureEvent::RouteChanged {
                    remote: true,
                    revision: 1,
                },
                routing_revision: 1,
                remote: true,
                floor_generation: 0,
            };
            assert_eq!(producer.try_push_tagged(barrier), Ok(()));
            for value in 0..EVENT_COUNT {
                let admitted = value + 1;
                while admitted >= producer_consumed.load(Ordering::Acquire) + MAX_AHEAD {
                    thread::yield_now();
                }
                let value = i32::try_from(value).expect("test value fits i32");
                let record = CapturedEvent {
                    event: CaptureEvent::RelativeMotion {
                        dx: value + 1,
                        dy: -(value + 1),
                    },
                    routing_revision: 1,
                    remote: true,
                    floor_generation: 0,
                };
                assert_eq!(producer.try_push_tagged(record), Ok(()));
            }
        });

        for expected in 0..=EVENT_COUNT {
            let record = loop {
                match consumer.try_pop_tagged().expect("queue remains active") {
                    Some(record) => break record,
                    None => thread::yield_now(),
                }
            };
            if expected == 0 {
                assert_eq!(
                    record,
                    CapturedEvent {
                        event: CaptureEvent::RouteChanged {
                            remote: true,
                            revision: 1,
                        },
                        routing_revision: 1,
                        remote: true,
                        floor_generation: 0,
                    }
                );
            } else {
                let value = i32::try_from(expected).expect("test value fits i32");
                assert_eq!(
                    record,
                    CapturedEvent {
                        event: CaptureEvent::RelativeMotion {
                            dx: value,
                            dy: -value,
                        },
                        routing_revision: 1,
                        remote: true,
                        floor_generation: 0,
                    }
                );
            }
            consumed.fetch_add(1, Ordering::Release);
        }
    });

    assert_eq!(consumer.try_pop_tagged(), Ok(None));
    assert_eq!(stop.reason(), None);
}

#[test]
fn lease_rejects_stale_generations_and_zero_ttls() {
    let stale_stop = CaptureStop::new();
    let mut stale = SuppressionLease::new(3, Duration::ZERO, stale_stop.clone());
    assert_eq!(
        stale.renew(4, Duration::ZERO, Duration::from_millis(1)),
        Err(StopReason::InvalidInput)
    );
    assert_eq!(stale_stop.reason(), Some(StopReason::InvalidInput));

    let zero_stop = CaptureStop::new();
    let mut zero = SuppressionLease::new(3, Duration::ZERO, zero_stop.clone());
    assert_eq!(
        zero.renew(3, Duration::ZERO, Duration::ZERO),
        Err(StopReason::InvalidInput)
    );
    assert_eq!(zero_stop.reason(), Some(StopReason::InvalidInput));
}

#[test]
fn lease_enforces_maximum_ttl_and_exact_expiry() {
    let max_stop = CaptureStop::new();
    let mut max = SuppressionLease::new(3, Duration::ZERO, max_stop.clone());
    assert_eq!(max.renew(3, Duration::ZERO, MAX_SUPPRESSION_TTL), Ok(()));
    assert!(max.is_suppressing(MAX_SUPPRESSION_TTL - Duration::from_millis(1)));
    assert!(!max.is_suppressing(MAX_SUPPRESSION_TTL));
    assert_eq!(max_stop.reason(), Some(StopReason::LeaseExpired));

    let too_long_stop = CaptureStop::new();
    let mut too_long = SuppressionLease::new(3, Duration::ZERO, too_long_stop.clone());
    assert_eq!(
        too_long.renew(
            3,
            Duration::ZERO,
            MAX_SUPPRESSION_TTL + Duration::from_millis(1)
        ),
        Err(StopReason::InvalidInput)
    );
    assert_eq!(too_long_stop.reason(), Some(StopReason::InvalidInput));
}

#[test]
fn missed_expiry_is_detected_by_renewal_without_a_timer_callback() {
    let stop = CaptureStop::new();
    let mut lease = SuppressionLease::new(9, Duration::ZERO, stop.clone());
    assert_eq!(
        lease.renew(9, Duration::ZERO, Duration::from_millis(10)),
        Ok(())
    );

    assert_eq!(
        lease.renew(9, Duration::from_millis(11), Duration::from_millis(10)),
        Err(StopReason::LeaseExpired)
    );
    assert_eq!(stop.reason(), Some(StopReason::LeaseExpired));
}

#[test]
fn lease_clock_regression_is_terminal_and_release_can_be_followed_by_renewal() {
    let stop = CaptureStop::new();
    let mut lease = SuppressionLease::new(5, Duration::from_millis(10), stop.clone());
    assert_eq!(
        lease.renew(5, Duration::from_millis(10), Duration::from_millis(20)),
        Ok(())
    );
    assert!(!lease.is_suppressing(Duration::from_millis(9)));
    assert_eq!(stop.reason(), Some(StopReason::ClockRegression));

    let release_stop = CaptureStop::new();
    let mut released = SuppressionLease::new(5, Duration::ZERO, release_stop.clone());
    assert_eq!(
        released.renew(5, Duration::ZERO, Duration::from_millis(10)),
        Ok(())
    );
    assert_eq!(released.release(5, Duration::from_millis(1)), Ok(()));
    assert!(!released.is_suppressing(Duration::from_millis(1)));
    assert_eq!(
        released.renew(5, Duration::from_millis(2), Duration::from_millis(10)),
        Ok(())
    );
    assert!(released.is_suppressing(Duration::from_millis(2)));
    assert_eq!(release_stop.reason(), None);
}

#[test]
fn injected_events_never_take_back() {
    let floor = SharedFloor::new();
    let receiving = floor
        .transition(floor.snapshot(), FloorState::Receiving)
        .unwrap();
    let gate = TakeBackGate::new(floor);
    gate.open_injection(receiving.generation);
    let stop = CaptureStop::new();
    let (mut producer, mut consumer) = capture_channel(stop.clone());
    let mut capture = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
    let mut admit = |decoded| match decoded {
        DecodedInput::Event(event) => {
            capture.process(event, false, Duration::ZERO, &mut producer, &stop);
            true
        }
        _ => false,
    };

    for injected in [
        decode_keyboard(WM_KEYDOWN, 0x41, 0x1e, LLKHF_INJECTED, 0),
        decode_keyboard(WM_KEYDOWN, 0x41, 0x1e, LLKHF_LOWER_IL_INJECTED, 0),
        decode_keyboard(WM_KEYDOWN, 0x41, 0x1e, 0, MONHOP_INJECTED_MARKER),
        decode_mouse(WM_LBUTTONDOWN, LLMHF_INJECTED, 0, 0, 0, 0),
        decode_mouse(WM_LBUTTONDOWN, LLMHF_LOWER_IL_INJECTED, 0, 0, 0, 0),
        decode_mouse(WM_MOUSEWHEEL, 0, MONHOP_INJECTED_MARKER, 120 << 16, 0, 0),
    ] {
        assert!(!admit(injected), "injected input never reaches process");
    }
    assert_eq!(gate.floor().snapshot(), receiving);
    assert!(gate.admits_injection());
    assert_eq!(gate.take_triggered(), None);

    assert!(admit(decode_keyboard(WM_KEYDOWN, 0x41, 0x1e, 0, 0)));
    assert!(
        gate.take_triggered().is_some(),
        "the same key pressed physically takes back"
    );
    assert!(matches!(
        consumer.try_pop(),
        Ok(Some(CaptureEvent::Key { pressed: true, .. }))
    ));
}

#[cfg(windows)]
#[test]
fn session_start_requires_capture_permit() {
    use monhop_core::{NativeSessionClaim, RevocationSignal};
    use monhop_platform_windows::native_capture::{NativeCapture, NativeCaptureError};

    let (permit, injection) = NativeSessionClaim::claim()
        .expect("no other test in this binary claims native input")
        .split();
    drop(injection);
    assert!(
        NativeSessionClaim::claim().is_none(),
        "a live capture permit keeps the session claimed"
    );
    assert!(matches!(
        NativeCapture::start_diagnostic(Duration::from_secs(1)),
        Err(NativeCaptureError::AlreadyActive)
    ));

    let revocation = RevocationSignal::default();
    revocation.request_stop();
    assert!(matches!(
        NativeCapture::start_for_session(
            1,
            revocation,
            permit,
            TakeBackGate::new(SharedFloor::new())
        ),
        Err(NativeCaptureError::Stopped(StopReason::InvalidInput))
    ));
    assert!(
        !NativeSessionClaim::is_claimed(),
        "a refused start releases its permit"
    );
}
