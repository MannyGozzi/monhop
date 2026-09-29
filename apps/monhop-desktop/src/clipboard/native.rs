//! The platform clipboard as the hub sees it. Each adapter lives on the one thread that makes every
//! platform clipboard call and is only created while sharing is on and a peer is attached.

use std::error::Error;
use std::fmt;
use std::task::Waker;
use std::time::Duration;

use monhop_protocol::clipboard::{MAX_CLIPBOARD_PNG, MAX_CLIPBOARD_TEXT};
use serde::Serialize;

use super::content::SourceMarker;
use super::image::{MAX_DIB_BYTES, Rgba};

/// Whether this process may read the clipboard: macOS paste privacy; Windows always allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Allowed,
    Ask,
    Denied,
    Unknown,
}

/// Size bounds an adapter checks against what the platform reports before copying any bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadLimits {
    /// Raw platform text (UTF-16 on Windows, UTF-8 on macOS) before normalization.
    pub text_bytes: usize,
    pub png_bytes: usize,
    pub dib_bytes: usize,
}

impl ReadLimits {
    /// UTF-16 with CRLF line ends takes up to four bytes per byte of normalized UTF-8 text.
    pub const STANDARD: Self = Self {
        text_bytes: 4 * (MAX_CLIPBOARD_TEXT as usize + 1),
        png_bytes: MAX_CLIPBOARD_PNG as usize,
        dib_bytes: MAX_DIB_BYTES,
    };
}

/// Why an item was not read past the marker and size checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Password managers and other apps asked monitors to skip it (or its marker was unreadable).
    Concealed,
    Files,
    Unsupported,
    TooLarge,
}

/// Who put the current item on the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Local,
    /// MonHop's private source format is present, so a peer sent it; `None` when unreadable.
    MonHop(Option<SourceMarker>),
}

/// The item chosen by the adapter (see `content::pick`); adapters read nothing for `MonHop` items.
pub enum Content {
    /// Platform text as found; `content::normalize_outgoing` turns it into wire text.
    Text(String),
    /// The platform's own PNG flavor, unmodified.
    Png(Vec<u8>),
    /// A packed DIB: BITMAPINFOHEADER or a later header, then optional masks and pixels.
    Dib(Vec<u8>),
    Skipped(Skip),
    Empty,
}

impl fmt::Debug for Content {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => write!(formatter, "Text([redacted; {} bytes])", text.len()),
            Self::Png(png) => write!(formatter, "Png([redacted; {} bytes])", png.len()),
            Self::Dib(dib) => write!(formatter, "Dib([redacted; {} bytes])", dib.len()),
            Self::Skipped(skip) => write!(formatter, "Skipped({skip:?})"),
            Self::Empty => formatter.write_str("Empty"),
        }
    }
}

/// One read, bracketed by change markers; `before != after` means the clipboard moved mid-read.
#[derive(Debug)]
pub struct Snapshot {
    pub before: u64,
    pub after: u64,
    pub origin: Origin,
    pub content: Content,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeError {
    /// Another process held the clipboard through every retry.
    Busy,
    Failed,
}

impl fmt::Display for NativeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Busy => "the clipboard is busy",
            Self::Failed => "the clipboard operation failed",
        })
    }
}

impl Error for NativeError {}

pub trait NativeClipboard {
    /// A counter every clipboard change moves (Windows sequence number, macOS changeCount).
    fn change_marker(&self) -> u64;

    /// Sleeps up to `timeout` while keeping the owning thread responsive to the platform (Windows
    /// pumps its message-only window); returns early when the waker fires.
    fn wait(&mut self, timeout: Duration);

    /// Interrupts `wait` from any thread.
    fn waker(&self) -> Waker;

    fn access(&self) -> Access;

    /// Checks markers first and never reads past a concealed, files or MonHop item.
    fn read(&mut self, limits: &ReadLimits) -> Result<Snapshot, NativeError>;

    /// `text` is wire text (LF line ends); returns the change marker right after the write.
    fn write_text(&mut self, text: &str, marker: SourceMarker) -> Result<u64, NativeError>;

    /// `png` is MonHop's own re-encode; Windows also writes a CF_DIBV5 built from `rgba`.
    fn write_png(
        &mut self,
        png: &[u8],
        rgba: Option<&Rgba>,
        marker: SourceMarker,
    ) -> Result<u64, NativeError>;
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::clipboard::content::{
        EchoGuard, Observation, Outgoing, Verdict, accept_incoming, windows_line_ends,
    };
    use crate::clipboard::image::{encode_png, reencode_png};

    enum Item {
        Text(String),
        Png(Vec<u8>),
        Dib(Vec<u8>),
        Concealed,
    }

    #[derive(Default)]
    struct FakeClipboard {
        marker: u64,
        item: Option<Item>,
        source: Option<SourceMarker>,
        dib_beside_png: bool,
        busy: bool,
        waits: Vec<Duration>,
    }

    impl FakeClipboard {
        fn copy(&mut self, item: Item) {
            self.marker += 1;
            self.item = Some(item);
            self.source = None;
        }

        fn write(&mut self, item: Item, marker: SourceMarker) -> u64 {
            // A single write can move the platform counter more than once.
            self.marker += 2;
            self.item = Some(item);
            self.source = Some(marker);
            self.marker
        }
    }

    impl NativeClipboard for FakeClipboard {
        fn change_marker(&self) -> u64 {
            self.marker
        }

        fn wait(&mut self, timeout: Duration) {
            self.waits.push(timeout);
        }

        fn waker(&self) -> Waker {
            Waker::noop().clone()
        }

        fn access(&self) -> Access {
            Access::Allowed
        }

        fn read(&mut self, limits: &ReadLimits) -> Result<Snapshot, NativeError> {
            if self.busy {
                return Err(NativeError::Busy);
            }
            let origin = self
                .source
                .map_or(Origin::Local, |marker| Origin::MonHop(Some(marker)));
            let content = match (&self.item, origin) {
                (_, Origin::MonHop(_)) | (None, _) => Content::Empty,
                (Some(Item::Concealed), _) => Content::Skipped(Skip::Concealed),
                (Some(Item::Text(text)), _) if text.len() > limits.text_bytes => {
                    Content::Skipped(Skip::TooLarge)
                }
                (Some(Item::Text(text)), _) => Content::Text(text.clone()),
                (Some(Item::Png(png)), _) if png.len() > limits.png_bytes => {
                    Content::Skipped(Skip::TooLarge)
                }
                (Some(Item::Png(png)), _) => Content::Png(png.clone()),
                (Some(Item::Dib(dib)), _) if dib.len() > limits.dib_bytes => {
                    Content::Skipped(Skip::TooLarge)
                }
                (Some(Item::Dib(dib)), _) => Content::Dib(dib.clone()),
            };
            Ok(Snapshot {
                before: self.marker,
                after: self.marker,
                origin,
                content,
            })
        }

        fn write_text(&mut self, text: &str, marker: SourceMarker) -> Result<u64, NativeError> {
            Ok(self.write(Item::Text(windows_line_ends(text).into_owned()), marker))
        }

        fn write_png(
            &mut self,
            png: &[u8],
            rgba: Option<&Rgba>,
            marker: SourceMarker,
        ) -> Result<u64, NativeError> {
            self.dib_beside_png = rgba.is_some();
            Ok(self.write(Item::Png(png.to_vec()), marker))
        }
    }

    fn tick(
        clipboard: &mut dyn NativeClipboard,
        guard: &mut EchoGuard,
        limits: &ReadLimits,
    ) -> Option<Verdict> {
        let observed_at = Instant::now();
        match guard.observe(clipboard.change_marker(), observed_at) {
            Observation::Changed => Some(guard.judge(clipboard.read(limits).unwrap())),
            Observation::Unchanged | Observation::OwnWrite => None,
        }
    }

    #[test]
    fn local_copies_go_out_once_and_received_writes_never_do() {
        let limits = ReadLimits::STANDARD;
        let mut clipboard = FakeClipboard::default();
        clipboard.copy(Item::Text("copied before sharing".into()));
        let mut guard = EchoGuard::new(clipboard.change_marker());
        assert!(tick(&mut clipboard, &mut guard, &limits).is_none());

        clipboard.copy(Item::Text("hello\r\n".into()));
        let verdict = tick(&mut clipboard, &mut guard, &limits);
        assert!(
            matches!(verdict, Some(Verdict::Share(Outgoing::Text(ref text))) if text == "hello\n")
        );
        assert!(tick(&mut clipboard, &mut guard, &limits).is_none());

        let incoming = accept_incoming(b"from a peer\n".to_vec()).unwrap();
        let started_at = Instant::now() + Duration::from_secs(1);
        assert!(guard.may_apply(started_at, clipboard.change_marker()));
        let marker = guard.next_marker();
        let after = clipboard.write_text(&incoming, marker).unwrap();
        guard.applied(guard.text_key(&incoming), after);
        assert!(tick(&mut clipboard, &mut guard, &limits).is_none());

        clipboard.marker += 1;
        let verdict = tick(&mut clipboard, &mut guard, &limits);
        assert!(matches!(verdict, Some(Verdict::Received)), "{verdict:?}");

        clipboard.copy(Item::Text("from a peer\r\n".into()));
        let verdict = tick(&mut clipboard, &mut guard, &limits);
        assert!(matches!(verdict, Some(Verdict::Repeat)), "{verdict:?}");

        let image = Rgba::new(1, 1, vec![9, 8, 7, 255]).unwrap();
        let peer_png = encode_png(&image, MAX_CLIPBOARD_PNG as usize).unwrap();
        let (decoded, png) = reencode_png(&peer_png).unwrap();
        let marker = guard.next_marker();
        let after = clipboard.write_png(&png, Some(&decoded), marker).unwrap();
        guard.applied(guard.png_key(&png), after);
        assert!(clipboard.dib_beside_png);
        assert!(tick(&mut clipboard, &mut guard, &limits).is_none());

        clipboard.copy(Item::Concealed);
        let verdict = tick(&mut clipboard, &mut guard, &limits);
        assert!(matches!(verdict, Some(Verdict::Skipped(Skip::Concealed))));

        let small = ReadLimits {
            dib_bytes: 8,
            ..ReadLimits::STANDARD
        };
        clipboard.copy(Item::Dib(vec![0; 9]));
        let verdict = tick(&mut clipboard, &mut guard, &small);
        assert!(matches!(verdict, Some(Verdict::Skipped(Skip::TooLarge))));
    }

    #[test]
    fn the_adapter_contract_reports_busy_and_wakes() {
        let mut clipboard = FakeClipboard {
            busy: true,
            ..FakeClipboard::default()
        };
        assert_eq!(
            clipboard.read(&ReadLimits::STANDARD).unwrap_err(),
            NativeError::Busy
        );
        clipboard.waker().wake_by_ref();
        clipboard.wait(Duration::from_millis(250));
        assert_eq!(clipboard.waits, [Duration::from_millis(250)]);
        assert_eq!(clipboard.access(), Access::Allowed);
        assert_eq!(
            NativeError::Failed.to_string(),
            "the clipboard operation failed"
        );
    }

    #[test]
    fn standard_limits_admit_capped_content_in_its_raw_platform_form() {
        let limits = ReadLimits::STANDARD;
        assert!(limits.text_bytes >= 4 * MAX_CLIPBOARD_TEXT as usize);
        assert_eq!(limits.png_bytes, MAX_CLIPBOARD_PNG as usize);
        assert!(limits.dib_bytes > 124 + 4 * 64_000_000);
    }
}
