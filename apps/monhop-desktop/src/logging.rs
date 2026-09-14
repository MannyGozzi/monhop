//! A bounded event log on disk, for reading back why sharing stopped. Events only: lifecycle,
//! failures, counts and timings. Never key identities, typed text, or pointer positions.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

/// Three files of this size keep about a day of a reconnect loop and never more than 6 MiB.
const ROTATE_AT: u64 = 2 * 1024 * 1024;
const KEPT_FILES: usize = 3;
const FILE_NAME: &str = "monhop.log";

static LOGGER: OnceLock<FileLogger> = OnceLock::new();

struct FileLogger {
    path: PathBuf,
    sink: Mutex<Option<(File, u64)>>,
}

/// Opens `<dir>/monhop.log` (creating `dir`) and installs it as the process logger.
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
    Ok(path)
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
        let mut guard = lock(&self.sink);
        let Some((file, size)) = guard.as_mut() else {
            return;
        };
        if file.write_all(line.as_bytes()).is_err() {
            return;
        }
        *size += line.len() as u64;
        if *size >= ROTATE_AT {
            rotate(&self.path);
            if let Ok(reopened) = open(&self.path) {
                *guard = Some(reopened);
            }
        }
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
}
