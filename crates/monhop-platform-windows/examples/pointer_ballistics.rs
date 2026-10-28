//! Ground truth for MonHop's model of Windows pointer ballistics.
//!
//! Does nothing unless invoked with exactly `--run`, on the interactive desktop with the mouse
//! left alone for a few seconds. It moves this computer's cursor only by relative `SendInput`
//! moves, never a button or key, compares how far Windows moves it with the model, prints a table
//! and PASS or FAIL, and puts the cursor back where it was.

use std::{ffi::OsString, process::ExitCode};

fn explicit_run() -> bool {
    std::env::args_os().skip(1).eq([OsString::from("--run")])
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    let _ = explicit_run();
    eprintln!("pointer_ballistics status=UNSUPPORTED platform=non_windows");
    ExitCode::from(2)
}

#[cfg(windows)]
fn main() -> ExitCode {
    if !explicit_run() {
        eprintln!("pointer_ballistics status=NOT_STARTED required=--run");
        return ExitCode::from(2);
    }
    windows::run()
}

#[cfg(windows)]
mod windows {
    use std::{
        hint,
        mem::size_of,
        process::ExitCode,
        time::{Duration, Instant},
    };

    use monhop_core::pointer_ballistics::Ballistics;
    use monhop_platform_windows::pointer_settings;
    use windows_sys::Win32::{
        Foundation::{POINT, RECT},
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
            Input::KeyboardAndMouse::{
                INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
            },
            WindowsAndMessaging::{GetPhysicalCursorPos, SetPhysicalCursorPos},
        },
    };

    /// Counts in one move, each sent as one report.
    const SIZES: [i32; 12] = [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64];
    const MIN_MOVES: usize = 60;
    /// Moves one way before turning back; long runs make a small gain error show.
    const LONGEST_RUN: usize = 30;
    /// A run reaches at most this share of the monitor's width from its centre.
    const RUN_REACH: f64 = 1.0 / 3.0;
    /// How long one move may take to show before the next is sent.
    const MOVE_SHOWS: Duration = Duration::from_millis(2);
    /// How long the cursor must keep still before a run's end is read, and the longest wait.
    const RUN_SETTLES: Duration = Duration::from_millis(8);
    const SETTLE_LIMIT: Duration = Duration::from_secs(1);
    const DEADLINE: Duration = Duration::from_secs(14);
    /// A run's total differs from the model's truncate-and-carry total only by the sub-pixel
    /// carry Windows brings in and the two leave behind, each under a pixel.
    const TOLERANCE: i32 = 2;

    struct Row {
        moves: usize,
        runs: usize,
        model: i64,
        actual: i64,
        worst: i32,
    }

    pub fn run() -> ExitCode {
        // SAFETY: no window exists yet; from here every coordinate is a physical pixel.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let Ok(saved) = cursor() else {
            return failed("cursor_unreadable");
        };
        let outcome = measure(saved);
        // SAFETY: plain coordinates the cursor held before the run.
        unsafe { SetPhysicalCursorPos(saved.x, saved.y) };
        outcome
    }

    fn measure(saved: POINT) -> ExitCode {
        let started = Instant::now();
        let mut ballistics = pointer_settings::read();
        let primary = pointer_settings::pixels_per_dip_at(0, 0);
        let here = pointer_settings::pixels_per_dip_at(saved.x, saved.y);
        let Some(bounds) = monitor_of(saved) else {
            return failed("monitor_unreadable");
        };
        println!(
            "pointer_ballistics settings={ballistics:?} primary_scale={primary} \
             cursor_monitor_scale={here} tolerance_px_per_run={TOLERANCE}"
        );
        println!(
            "{:>5} {:>6} {:>5} {:>9} {:>9} {:>7} {:>9}",
            "size", "moves", "runs", "model_px", "actual_px", "ratio", "worst_run"
        );
        let mut pass = true;
        // Windows never clears its sub-pixel remainder, so the model carries one through every run.
        let mut carry = 0.0;
        for size in SIZES {
            if started.elapsed() > DEADLINE {
                return failed("deadline");
            }
            let row = match measure_size(&mut ballistics, &mut carry, primary, size, bounds) {
                Ok(row) => row,
                Err(reason) => return failed(reason),
            };
            let ratio = row.actual as f64 / (row.model as f64).max(1.0);
            println!(
                "{size:>5} {:>6} {:>5} {:>9} {:>9} {ratio:>7.3} {:>9}",
                row.moves, row.runs, row.model, row.actual, row.worst
            );
            pass &= row.worst <= TOLERANCE;
        }
        let status = if pass { "PASS" } else { "FAIL" };
        println!(
            "pointer_ballistics status={status} elapsed_ms={}",
            started.elapsed().as_millis()
        );
        if pass {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }

    /// Runs of `size`-count moves out from the monitor's centre and back, each compared with the
    /// model's whole pixels, which carry their fraction to the next move as Windows does.
    fn measure_size(
        ballistics: &mut Ballistics,
        carry: &mut f64,
        pixels_per_dip: f64,
        size: i32,
        bounds: RECT,
    ) -> Result<Row, &'static str> {
        let per_move = {
            let mut probe = *ballistics;
            probe.apply(f64::from(size), 0.0, 1);
            probe.apply(f64::from(size), 0.0, 1).0 * pixels_per_dip
        };
        let reach = f64::from(bounds.right - bounds.left) * RUN_REACH;
        let length = ((reach / per_move.max(1.0)) as usize).clamp(1, LONGEST_RUN);
        let runs = MIN_MOVES.div_ceil(length).next_multiple_of(2);
        let centre = POINT {
            x: bounds.left + (bounds.right - bounds.left) / 2,
            y: bounds.top + (bounds.bottom - bounds.top) / 2,
        };
        // SAFETY: plain coordinates inside the monitor the cursor started on.
        if unsafe { SetPhysicalCursorPos(centre.x, centre.y) } == 0 {
            return Err("cursor_unplaced");
        }
        let mut at = settled()?;
        let mut row = Row {
            moves: runs * length,
            runs,
            model: 0,
            actual: 0,
            worst: 0,
        };
        for run in 0..runs {
            let direction = if run % 2 == 0 { 1 } else { -1 };
            let from = at;
            let mut model = 0;
            for _ in 0..length {
                nudge(direction * size)?;
                at = shown(at)?;
                if at.y != from.y {
                    return Err("cursor_moved_by_hand");
                }
                if !(bounds.left..bounds.right).contains(&at.x) {
                    return Err("cursor_left_monitor");
                }
                *carry += ballistics.apply(f64::from(direction * size), 0.0, 1).0 * pixels_per_dip;
                let whole = carry.trunc();
                *carry -= whole;
                model += whole as i32;
            }
            at = settled()?;
            let actual = at.x - from.x;
            row.model += i64::from(model.abs());
            row.actual += i64::from(actual.abs());
            row.worst = row.worst.max((actual - model).abs());
        }
        if row.actual == 0 && row.model > 0 {
            return Err("cursor_never_moved");
        }
        Ok(row)
    }

    fn nudge(dx: i32) -> Result<(), &'static str> {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: one initialized relative-move record with no button flag.
        if unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) } == 1 {
            Ok(())
        } else {
            Err("sendinput_refused")
        }
    }

    fn cursor() -> Result<POINT, &'static str> {
        let mut point = POINT::default();
        // SAFETY: the writable POINT outlives this read-only call.
        if unsafe { GetPhysicalCursorPos(&mut point) } == 0 {
            return Err("cursor_unreadable");
        }
        Ok(point)
    }

    /// The cursor once a move has shown, or after [`MOVE_SHOWS`] for one too small to.
    fn shown(before: POINT) -> Result<POINT, &'static str> {
        let since = Instant::now();
        loop {
            let now = cursor()?;
            if now.x != before.x || now.y != before.y || since.elapsed() >= MOVE_SHOWS {
                return Ok(now);
            }
            hint::spin_loop();
        }
    }

    /// The cursor once it has kept still for [`RUN_SETTLES`].
    fn settled() -> Result<POINT, &'static str> {
        let started = Instant::now();
        let mut last = cursor()?;
        let mut since = Instant::now();
        while since.elapsed() < RUN_SETTLES {
            if started.elapsed() > SETTLE_LIMIT {
                return Err("cursor_kept_moving");
            }
            hint::spin_loop();
            let now = cursor()?;
            if now.x != last.x || now.y != last.y {
                last = now;
                since = Instant::now();
            }
        }
        Ok(last)
    }

    fn monitor_of(point: POINT) -> Option<RECT> {
        // SAFETY: a by-value point and flag; the monitor handle needs no release.
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..MONITORINFO::default()
        };
        // SAFETY: `monitor` came from MonitorFromPoint and info is sized for its version.
        (!monitor.is_null() && unsafe { GetMonitorInfoW(monitor, &mut info) } != 0)
            .then_some(info.rcMonitor)
    }

    fn failed(reason: &str) -> ExitCode {
        println!("pointer_ballistics status=FAIL reason={reason}");
        ExitCode::FAILURE
    }
}
