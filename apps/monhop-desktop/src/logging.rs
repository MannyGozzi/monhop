//! A bounded event log on disk, for reading back why sharing stopped. Events only: lifecycle,
//! failures, counts and timings. Never key identities, typed text, or pointer positions.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    panic::Location,
    path::{Path, PathBuf},
    sync::{Mutex, Once, OnceLock, TryLockError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Three files of this size keep about a day of a reconnect loop and never more than 6 MiB.
const ROTATE_AT: u64 = 2 * 1024 * 1024;
const KEPT_FILES: usize = 3;
const FILE_NAME: &str = "monhop.log";
/// A panicking thread may already hold the sink, so the panic line waits at most this long.
const PANIC_LOCK_ATTEMPTS: u32 = 100;
const PANIC_LOCK_RETRY: Duration = Duration::from_millis(1);
/// Workspace crates compile with paths relative to the workspace root; libraries and std do not.
const OWN_SOURCE_ROOTS: [&str; 2] = ["crates", "apps"];

static LOGGER: OnceLock<FileLogger> = OnceLock::new();

struct FileLogger {
    path: PathBuf,
    sink: Mutex<Option<(File, u64)>>,
}

/// Opens `<dir>/monhop.log` (creating `dir`) and installs it as the process logger and panic log.
pub fn init(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("log folder: {error}"))?;
    let path = dir.join(FILE_NAME);
    let (file, size) = open(&path)?;
    let logger = LOGGER.get_or_init(|| FileLogger {
        path: path.clone(),
        sink: Mutex::new(None),
    });
    *lock(&logger.sink) = Some((file, size));
    // Second init in one process keeps the first logger; the file just switches.
    let _ = log::set_logger(logger);
    log::set_max_level(log::LevelFilter::Debug);
    install_panic_hook();
    Ok(path)
}

/// A panic that unwinds into a destructor aborts the process and a GUI app's stderr is unseen,
/// so each panic's location and message reach the log before the default hook runs.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(logger) = LOGGER.get() {
                logger.append_panic(&panic_line(
                    SystemTime::now(),
                    info.location(),
                    info.payload_as_str(),
                ));
            }
            previous(info);
        }));
    });
}

/// Our own panic messages can format runtime values that may hold input, and their location
/// already names the source line, so only library and std messages are written.
fn panic_line(now: SystemTime, location: Option<&Location<'_>>, message: Option<&str>) -> String {
    let message = match location {
        Some(location) if !is_own_source(location.file()) => message.unwrap_or("non-text payload"),
        _ => "message withheld",
    };
    format!(
        "{} ERROR monhop_desktop::panic panicked at {}: {message}\n",
        timestamp(now),
        location.map_or_else(|| "unknown".to_owned(), ToString::to_string),
    )
}

fn is_own_source(file: &str) -> bool {
    OWN_SOURCE_ROOTS.iter().any(|root| {
        file.strip_prefix(root)
            .is_some_and(|rest| rest.starts_with(['/', '\\']))
    })
}

/// The log file's location once `init` succeeded.
pub fn path() -> Option<PathBuf> {
    LOGGER
        .get()
        .filter(|logger| lock(&logger.sink).is_some())
        .map(|logger| logger.path.clone())
}

fn open(path: &Path) -> Result<(File, u64), String> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("log file: {error}"))?;
    let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    Ok((file, size))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FileLogger {
    fn append(&self, sink: &mut Option<(File, u64)>, line: &str) {
        let Some((file, size)) = sink.as_mut() else {
            return;
        };
        if file.write_all(line.as_bytes()).is_err() {
            return;
        }
        *size += line.len() as u64;
        if *size >= ROTATE_AT {
            rotate(&self.path);
            if let Ok(reopened) = open(&self.path) {
                *sink = Some(reopened);
            }
        }
    }

    /// Never blocks for good: the panicking thread itself may hold the sink.
    fn append_panic(&self, line: &str) {
        for _ in 0..PANIC_LOCK_ATTEMPTS {
            match self.sink.try_lock() {
                Ok(mut sink) => return self.append(&mut sink, line),
                Err(TryLockError::Poisoned(poisoned)) => {
                    return self.append(&mut poisoned.into_inner(), line);
                }
                Err(TryLockError::WouldBlock) => std::thread::sleep(PANIC_LOCK_RETRY),
            }
        }
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // Our crates log at Debug; libraries only when something is wrong.
        if metadata.target().starts_with("monhop") {
            metadata.level() <= log::Level::Debug
        } else {
            metadata.level() <= log::Level::Warn
        }
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} {} {}\n",
            timestamp(SystemTime::now()),
            record.level(),
            record.target(),
            record.args()
        );
        self.append(&mut lock(&self.sink), &line);
    }

    fn flush(&self) {
        if let Some((file, _)) = lock(&self.sink).as_mut() {
            let _ = file.flush();
        }
    }
}

/// monhop.log -> monhop.1.log -> monhop.2.log; the oldest is deleted.
fn rotate(path: &Path) {
    let numbered = |n: usize| path.with_file_name(format!("monhop.{n}.log"));
    let _ = std::fs::remove_file(numbered(KEPT_FILES - 1));
    for n in (1..KEPT_FILES - 1).rev() {
        let _ = std::fs::rename(numbered(n), numbered(n + 1));
    }
    let _ = std::fs::rename(path, numbered(1));
}

/// UTC, millisecond precision, without a date crate.
fn timestamp(now: SystemTime) -> String {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        since.subsec_millis()
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps_are_utc_with_milliseconds() {
        let at = UNIX_EPOCH + Duration::from_millis(1_789_300_000_123);
        assert_eq!(timestamp(at), "2026-09-13T11:46:40.123Z");
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn rotation_keeps_three_bounded_files() {
        let dir = std::env::temp_dir().join(format!("monhop-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        for n in 1..=4 {
            std::fs::write(&path, format!("gen {n}")).unwrap();
            rotate(&path);
        }
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("monhop.1.log")).unwrap(),
            "gen 4"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("monhop.2.log")).unwrap(),
            "gen 3"
        );
        assert!(!dir.join("monhop.3.log").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn library_noise_stays_out_and_own_events_get_in() {
        let logger = FileLogger {
            path: PathBuf::new(),
            sink: Mutex::new(None),
        };
        let own = log::Metadata::builder()
            .level(log::Level::Debug)
            .target("monhop_transport::session_link")
            .build();
        let quiet = log::Metadata::builder()
            .level(log::Level::Info)
            .target("tauri")
            .build();
        let loud = log::Metadata::builder()
            .level(log::Level::Warn)
            .target("tauri")
            .build();
        assert!(log::Log::enabled(&logger, &own));
        assert!(!log::Log::enabled(&logger, &quiet));
        assert!(log::Log::enabled(&logger, &loud));
    }

    #[test]
    fn own_panics_keep_their_location_and_withhold_their_message() {
        let at = UNIX_EPOCH + Duration::from_millis(1_789_300_000_123);
        let location = Location::caller();
        assert!(location.file().starts_with("apps"));
        assert_eq!(
            panic_line(at, Some(location), Some("key 0x04 held")),
            format!(
                "2026-09-13T11:46:40.123Z ERROR monhop_desktop::panic panicked at {location}: message withheld\n"
            )
        );
        assert_eq!(
            panic_line(at, None, Some("key 0x04 held")),
            "2026-09-13T11:46:40.123Z ERROR monhop_desktop::panic panicked at unknown: message withheld\n"
        );
    }

    #[test]
    fn a_panic_on_any_thread_reaches_the_log_through_the_installed_hook() {
        let dir =
            std::env::temp_dir().join(format!("monhop-panic-hook-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = init(&dir).unwrap();
        assert!(std::thread::spawn(|| panic!("from a test")).join().is_err());
        let logged = std::fs::read_to_string(&path).unwrap();
        // The logger is process-wide, so close its file before removing the folder (Windows).
        *lock(&LOGGER.get().unwrap().sink) = None;
        std::fs::remove_dir_all(&dir).unwrap();
        let expected = format!("ERROR monhop_desktop::panic panicked at {}:", file!());
        assert!(logged.contains(&expected), "{logged}");
        assert!(logged.contains(": message withheld\n"), "{logged}");
    }

    #[test]
    fn only_workspace_paths_count_as_own_source() {
        assert!(is_own_source("crates/monhop-transport/src/session.rs"));
        assert!(is_own_source("apps\\monhop-desktop\\src\\main.rs"));
        assert!(!is_own_source(
            "/Users/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/quinn-0.11.11/src/mutex.rs"
        ));
        assert!(!is_own_source(
            "/rustc/48a229cea/library/core/src/option.rs"
        ));
        assert!(!is_own_source("vendor/tao/src/lib.rs"));
        assert!(!is_own_source("cratesio/src/lib.rs"));
    }

    #[test]
    fn a_panic_on_the_thread_holding_the_sink_is_dropped_instead_of_deadlocking() {
        let dir =
            std::env::temp_dir().join(format!("monhop-panic-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let logger = FileLogger {
            path: path.clone(),
            sink: Mutex::new(Some(open(&path).unwrap())),
        };
        {
            let _held = lock(&logger.sink);
            logger.append_panic("dropped\n");
        }
        logger.append_panic("written\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "written\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
