//! Pure clipboard rules: text normalization, which flavor to share, MonHop's private source marker,
//! and the echo guard that keeps received or repeated content from going out again.

use std::borrow::Cow;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime};

use monhop_protocol::clipboard::{ClipboardTextError, MAX_CLIPBOARD_TEXT, validate_text};

use super::image;
use super::native::{Content, Origin, Skip, Snapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSkip {
    Empty,
    TooLarge,
}

/// Wire text: CRLF becomes LF, everything from the first NUL on is dropped, never empty, capped.
pub fn normalize_outgoing(raw: &str) -> Result<String, TextSkip> {
    let text = before_nul(raw).replace("\r\n", "\n");
    if text.is_empty() {
        return Err(TextSkip::Empty);
    }
    if text.len() > MAX_CLIPBOARD_TEXT as usize {
        return Err(TextSkip::TooLarge);
    }
    Ok(text)
}

/// Peer text must already follow the wire rules; nothing is repaired on this side.
pub fn accept_incoming(bytes: Vec<u8>) -> Result<String, ClipboardTextError> {
    validate_text(&bytes)?;
    String::from_utf8(bytes).map_err(|_| ClipboardTextError::InvalidUtf8)
}

/// Windows paste side: every LF without a CR before it gains one, so a second pass changes nothing.
pub fn windows_line_ends(text: &str) -> Cow<'_, str> {
    let lone_lf = text
        .split('\n')
        .rev()
        .skip(1)
        .any(|line| !line.ends_with('\r'));
    if !lone_lf {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push_str(if out.ends_with('\r') { "\n" } else { "\r\n" });
        }
        out.push_str(line);
    }
    Cow::Owned(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Text,
    Image,
    Nothing,
}

/// Text wins whenever there is any, except a lone http(s) or file URL beside an image: that is a
/// browser's "Copy image", and the image is what the user meant.
pub fn pick(text: Option<&str>, image_available: bool) -> Pick {
    let text = text.map(before_nul).filter(|text| !text.is_empty());
    match (text, image_available) {
        (Some(text), true) if is_lone_url(text) => Pick::Image,
        (Some(_), _) => Pick::Text,
        (None, true) => Pick::Image,
        (None, false) => Pick::Nothing,
    }
}

fn before_nul(text: &str) -> &str {
    text.find('\0').map_or(text, |end| &text[..end])
}

/// Drops every CR directly before an LF, whole runs of them where `normalize_outgoing` drops one,
/// so the result is the same however many times a text was normalized or given CRLF line ends.
fn lf_line_ends(text: &str) -> Cow<'_, str> {
    if !text.contains("\r\n") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for piece in text.split_inclusive('\n') {
        match piece.strip_suffix('\n') {
            Some(line) => {
                out.push_str(line.trim_end_matches('\r'));
                out.push('\n');
            }
            None => out.push_str(piece),
        }
    }
    Cow::Owned(out)
}

fn is_lone_url(text: &str) -> bool {
    let text = text.trim();
    if text.contains(char::is_whitespace) {
        return false;
    }
    let Some((scheme, rest)) = text.split_once("://") else {
        return false;
    };
    !rest.is_empty()
        && ["http", "https", "file"]
            .iter()
            .any(|known| scheme.eq_ignore_ascii_case(known))
}

/// Payload of MonHop's private clipboard format, written beside everything MonHop puts on the
/// clipboard: a per-process token and the write number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceMarker {
    pub token: u64,
    pub write: u64,
}

impl SourceMarker {
    pub const LEN: usize = 20;
    const MAGIC: [u8; 4] = *b"MHCS";

    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut bytes = [0; Self::LEN];
        bytes[..4].copy_from_slice(&Self::MAGIC);
        bytes[4..12].copy_from_slice(&self.token.to_le_bytes());
        bytes[12..].copy_from_slice(&self.write.to_le_bytes());
        bytes
    }

    /// Trailing bytes are ignored: Windows may round clipboard allocations up.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let bytes = bytes.get(..Self::LEN)?;
        if bytes[..4] != Self::MAGIC {
            return None;
        }
        Some(Self {
            token: u64::from_le_bytes(bytes[4..12].try_into().ok()?),
            write: u64::from_le_bytes(bytes[12..].try_into().ok()?),
        })
    }
}

fn process_token() -> u64 {
    static TOKEN: OnceLock<u64> = OnceLock::new();
    *TOKEN.get_or_init(|| {
        let mut hasher = RandomState::new().build_hasher();
        std::process::id().hash(&mut hasher);
        SystemTime::now().hash(&mut hasher);
        hasher.finish()
    })
}

/// Identifies content that crossed a link; keyed per guard and never logged.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ContentKey(u64);

#[derive(Clone, Copy)]
enum KeyKind {
    Text,
    Png,
    Dib,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    Unchanged,
    /// The change is MonHop's own last write.
    OwnWrite,
    /// Something on this computer changed the clipboard; read it.
    Changed,
}

/// Content cleared to go to peers; `Dib` still needs PNG encoding on the codec thread.
pub enum Outgoing {
    Text(String),
    Png(Vec<u8>),
    Dib(Vec<u8>),
}

impl fmt::Debug for Outgoing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => write!(formatter, "Text([redacted; {} bytes])", text.len()),
            Self::Png(png) => write!(formatter, "Png([redacted; {} bytes])", png.len()),
            Self::Dib(dib) => write!(formatter, "Dib([redacted; {} bytes])", dib.len()),
        }
    }
}

#[derive(Debug)]
pub enum Verdict {
    /// The clipboard changed while it was read; read again on the next tick.
    Retry,
    /// MonHop wrote it, so it came from a peer: never sent on.
    Received,
    /// The same content as the last item that crossed a link in either direction.
    Repeat,
    Empty,
    Skipped(Skip),
    Share(Outgoing),
}

/// Loop prevention for the clipboard thread, in three independent layers: the change marker of
/// MonHop's own writes, the private source marker, and a keyed hash of the last content that
/// crossed a link. Only locally originated changes pass, which keeps an N-computer mesh loop-free.
pub struct EchoGuard {
    token: u64,
    writes: u64,
    last_seen: u64,
    own_write: Option<u64>,
    local_change_at: Option<Instant>,
    last_content: Option<ContentKey>,
    keys: RandomState,
}

impl EchoGuard {
    /// `baseline` is the change marker when sharing starts, so nothing copied earlier is sent.
    pub fn new(baseline: u64) -> Self {
        Self {
            token: process_token(),
            writes: 0,
            last_seen: baseline,
            own_write: None,
            local_change_at: None,
            last_content: None,
            keys: RandomState::new(),
        }
    }

    pub fn observe(&mut self, marker: u64, now: Instant) -> Observation {
        if marker == self.last_seen {
            return Observation::Unchanged;
        }
        self.last_seen = marker;
        if self.own_write == Some(marker) {
            return Observation::OwnWrite;
        }
        self.local_change_at = Some(now);
        Observation::Changed
    }

    pub fn judge(&mut self, snapshot: Snapshot) -> Verdict {
        if snapshot.before != snapshot.after {
            return Verdict::Retry;
        }
        self.last_seen = snapshot.after;
        if let Origin::MonHop(_) = snapshot.origin {
            return Verdict::Received;
        }
        let (key, outgoing) = match snapshot.content {
            Content::Empty => return Verdict::Empty,
            Content::Skipped(skip) => return Verdict::Skipped(skip),
            Content::Text(raw) => match normalize_outgoing(&raw) {
                Ok(text) => (self.text_key(&text), Outgoing::Text(text)),
                Err(TextSkip::Empty) => return Verdict::Empty,
                Err(TextSkip::TooLarge) => return Verdict::Skipped(Skip::TooLarge),
            },
            Content::Png(png) => {
                if let Err(error) = image::check_outgoing_png(&png) {
                    return Verdict::Skipped(error.skip());
                }
                (self.png_key(&png), Outgoing::Png(png))
            }
            Content::Dib(dib) => (self.key(KeyKind::Dib, dib.as_slice()), Outgoing::Dib(dib)),
        };
        if self.last_content == Some(key) {
            return Verdict::Repeat;
        }
        self.last_content = Some(key);
        Verdict::Share(outgoing)
    }

    fn next_marker(&mut self) -> SourceMarker {
        self.writes += 1;
        SourceMarker {
            token: self.token,
            write: self.writes,
        }
    }

    /// Key of text whatever its line ends: CRLF, LF, or extra CRs before an LF all key alike, so
    /// received text a platform or clipboard manager re-wrote still matches.
    pub fn text_key(&self, text: &str) -> ContentKey {
        self.key(KeyKind::Text, lf_line_ends(text).as_bytes())
    }

    pub fn png_key(&self, png: &[u8]) -> ContentKey {
        self.key(KeyKind::Png, png)
    }

    fn key(&self, kind: KeyKind, bytes: &[u8]) -> ContentKey {
        ContentKey(self.keys.hash_one((kind as u8, bytes)))
    }

    /// Call right before writing received content; returns the source marker to write beside it.
    /// The content counts as crossed from here on, so any part of it that lands, even from a
    /// write that fails midway, is never sent back.
    pub fn begin_apply(&mut self, key: ContentKey) -> SourceMarker {
        self.last_content = Some(key);
        self.next_marker()
    }

    /// Records a finished write by the change marker right after it, so that change is skipped.
    pub fn applied(&mut self, marker_after: u64) {
        self.own_write = Some(marker_after);
    }

    /// A paste never overwrites what the user copied since the incoming transfer started, nor a
    /// change this guard has not observed yet. The adapter's write re-checks the marker while
    /// the clipboard is open, which closes the gap between this check and the write.
    pub fn may_apply(&self, started_at: Instant, current_marker: u64) -> bool {
        let unobserved = current_marker != self.last_seen && self.own_write != Some(current_marker);
        let copied_since = self.local_change_at.is_some_and(|at| at >= started_at);
        !unobserved && !copied_since
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use monhop_protocol::clipboard::MAX_CLIPBOARD_PNG;

    use super::*;
    use crate::clipboard::image::{Rgba, encode_png};

    fn snapshot(marker: u64, origin: Origin, content: Content) -> Snapshot {
        Snapshot {
            before: marker,
            after: marker,
            origin,
            content,
        }
    }

    fn local_text(marker: u64, text: &str) -> Snapshot {
        snapshot(marker, Origin::Local, Content::Text(text.to_owned()))
    }

    fn shared_text(verdict: Verdict) -> String {
        match verdict {
            Verdict::Share(Outgoing::Text(text)) => text,
            other => panic!("expected shared text, got {other:?}"),
        }
    }

    fn one_pixel_png() -> Vec<u8> {
        let image = Rgba::new(1, 1, vec![1, 2, 3, 4]).unwrap();
        encode_png(&image, MAX_CLIPBOARD_PNG as usize).unwrap()
    }

    #[test]
    fn outgoing_text_is_lf_only_stops_at_nul_and_is_never_empty() {
        assert_eq!(normalize_outgoing("a\r\nb\r\n").unwrap(), "a\nb\n");
        assert_eq!(normalize_outgoing("a\rb\n").unwrap(), "a\rb\n");
        assert_eq!(normalize_outgoing("abc\0def").unwrap(), "abc");
        assert_eq!(normalize_outgoing("\0abc"), Err(TextSkip::Empty));
        assert_eq!(normalize_outgoing(""), Err(TextSkip::Empty));
        assert_eq!(normalize_outgoing(" ").unwrap(), " ");
    }

    #[test]
    fn outgoing_text_is_capped_after_line_ends_shrink() {
        let cap = MAX_CLIPBOARD_TEXT as usize;
        assert_eq!(normalize_outgoing(&"x".repeat(cap)).unwrap().len(), cap);
        assert_eq!(
            normalize_outgoing(&"x".repeat(cap + 1)),
            Err(TextSkip::TooLarge)
        );
        let crlf = "\r\n".repeat(cap);
        assert_eq!(normalize_outgoing(&crlf).unwrap().len(), cap);
    }

    #[test]
    fn incoming_text_must_already_follow_the_wire_rules() {
        assert_eq!(accept_incoming(b"hi\nthere".to_vec()).unwrap(), "hi\nthere");
        assert_eq!(
            accept_incoming(b"a\0b".to_vec()),
            Err(ClipboardTextError::ContainsNul)
        );
        assert_eq!(
            accept_incoming(vec![0xFF, 0xFE]),
            Err(ClipboardTextError::InvalidUtf8)
        );
        assert_eq!(accept_incoming(Vec::new()), Err(ClipboardTextError::Empty));
    }

    #[test]
    fn windows_line_ends_are_added_once() {
        assert_eq!(windows_line_ends("a\nb"), "a\r\nb");
        assert_eq!(windows_line_ends("\n\n"), "\r\n\r\n");
        assert_eq!(windows_line_ends("a\r\n\nb\n"), "a\r\n\r\nb\r\n");
        assert_eq!(windows_line_ends("a\rb"), "a\rb");
        assert!(matches!(windows_line_ends("a\r\nb"), Cow::Borrowed(_)));
        for text in ["a\nb\r\nc\n", "\n", "x\r\r\n\n"] {
            let once = windows_line_ends(text).into_owned();
            assert_eq!(windows_line_ends(&once), once);
        }
    }

    #[test]
    fn text_wins_over_an_image_except_a_lone_url() {
        assert_eq!(pick(Some("hello"), false), Pick::Text);
        assert_eq!(pick(Some("hello"), true), Pick::Text);
        assert_eq!(pick(None, true), Pick::Image);
        assert_eq!(pick(Some(""), true), Pick::Image);
        assert_eq!(pick(Some("\0junk"), true), Pick::Image);
        assert_eq!(pick(None, false), Pick::Nothing);
        assert_eq!(pick(Some(""), false), Pick::Nothing);
        for url in [
            "https://example.com/a.png",
            "http://example.com/a.png",
            "HTTPS://EXAMPLE.COM/A.PNG",
            "file:///C:/Users/me/a.png",
            "  https://example.com/a.png\r\n",
        ] {
            assert_eq!(pick(Some(url), true), Pick::Image, "{url}");
            assert_eq!(pick(Some(url), false), Pick::Text, "{url}");
        }
        for text in [
            "https://example.com/a.png\nhttps://example.com/b.png",
            "see https://example.com/a.png",
            "ftp://example.com/a.png",
            "https://",
            "example.com/a.png",
        ] {
            assert_eq!(pick(Some(text), true), Pick::Text, "{text}");
        }
    }

    #[test]
    fn the_source_marker_round_trips_and_rejects_foreign_bytes() {
        let marker = SourceMarker {
            token: 0x0102_0304_0506_0708,
            write: 42,
        };
        let bytes = marker.to_bytes();
        assert_eq!(SourceMarker::parse(&bytes), Some(marker));
        let mut padded = bytes.to_vec();
        padded.extend_from_slice(&[0; 12]);
        assert_eq!(SourceMarker::parse(&padded), Some(marker));
        assert_eq!(SourceMarker::parse(&bytes[..19]), None);
        let mut foreign = bytes;
        foreign[0] = b'X';
        assert_eq!(SourceMarker::parse(&foreign), None);
    }

    #[test]
    fn markers_carry_one_process_token_and_count_writes() {
        let mut first = EchoGuard::new(0);
        let mut second = EchoGuard::new(0);
        let one = first.next_marker();
        let two = first.next_marker();
        assert_eq!((one.write, two.write), (1, 2));
        assert_eq!(one.token, two.token);
        assert_eq!(second.next_marker().token, one.token);
    }

    #[test]
    fn nothing_copied_before_sharing_started_is_read() {
        let mut guard = EchoGuard::new(7);
        assert_eq!(guard.observe(7, Instant::now()), Observation::Unchanged);
        assert_eq!(guard.observe(8, Instant::now()), Observation::Changed);
        assert_eq!(guard.observe(8, Instant::now()), Observation::Unchanged);
    }

    #[test]
    fn own_writes_are_skipped_by_their_change_marker() {
        let mut guard = EchoGuard::new(1);
        let _ = guard.begin_apply(guard.text_key("from peer"));
        guard.applied(3);
        assert_eq!(guard.observe(3, Instant::now()), Observation::OwnWrite);
        assert_eq!(guard.observe(3, Instant::now()), Observation::Unchanged);
        assert_eq!(guard.observe(4, Instant::now()), Observation::Changed);
    }

    #[test]
    fn anything_carrying_a_source_marker_is_never_sent() {
        let mut guard = EchoGuard::new(0);
        let marker = guard.next_marker();
        for origin in [
            Origin::MonHop(Some(marker)),
            Origin::MonHop(Some(SourceMarker { token: 9, write: 1 })),
            Origin::MonHop(None),
        ] {
            let verdict = guard.judge(snapshot(5, origin, Content::Text("x".into())));
            assert!(matches!(verdict, Verdict::Received), "{verdict:?}");
        }
    }

    #[test]
    fn received_content_reappearing_unmarked_is_not_sent_back() {
        let mut guard = EchoGuard::new(0);
        let _ = guard.begin_apply(guard.text_key("line one\nline two"));
        guard.applied(1);
        let verdict = guard.judge(local_text(2, "line one\r\nline two"));
        assert!(matches!(verdict, Verdict::Repeat), "{verdict:?}");

        let png = one_pixel_png();
        let _ = guard.begin_apply(guard.png_key(&png));
        guard.applied(3);
        let verdict = guard.judge(snapshot(4, Origin::Local, Content::Png(png)));
        assert!(matches!(verdict, Verdict::Repeat), "{verdict:?}");
    }

    #[test]
    fn an_applied_item_is_recognized_even_if_the_write_failed_midway() {
        let mut guard = EchoGuard::new(0);
        let _ = guard.begin_apply(guard.text_key("from a peer"));
        // The text landed, then the write failed before the source marker and the change marker
        // were recorded: to this guard it is an ordinary local change.
        assert_eq!(guard.observe(1, Instant::now()), Observation::Changed);
        let verdict = guard.judge(local_text(1, "from a peer"));
        assert!(matches!(verdict, Verdict::Repeat), "{verdict:?}");

        let png = one_pixel_png();
        let _ = guard.begin_apply(guard.png_key(&png));
        assert_eq!(guard.observe(2, Instant::now()), Observation::Changed);
        let verdict = guard.judge(snapshot(2, Origin::Local, Content::Png(png)));
        assert!(matches!(verdict, Verdict::Repeat), "{verdict:?}");
    }

    #[test]
    fn crlf_text_received_and_recopied_is_not_echoed() {
        let incoming = accept_incoming(b"one\r\ntwo\r\r\nthree\nfour".to_vec()).unwrap();
        // As Windows writes it, and as macOS leaves it.
        for recopied in [windows_line_ends(&incoming).into_owned(), incoming.clone()] {
            let mut guard = EchoGuard::new(0);
            let _ = guard.begin_apply(guard.text_key(&incoming));
            guard.applied(1);
            assert_eq!(guard.observe(1, Instant::now()), Observation::OwnWrite);
            // A clipboard manager copies it again, without MonHop's source marker.
            assert_eq!(guard.observe(2, Instant::now()), Observation::Changed);
            let verdict = guard.judge(local_text(2, &recopied));
            assert!(
                matches!(verdict, Verdict::Repeat),
                "{recopied:?}: {verdict:?}"
            );
        }
    }

    #[test]
    fn text_keys_ignore_how_line_ends_were_written() {
        let guard = EchoGuard::new(0);
        let key = guard.text_key("a\nb\n\nc");
        for same in ["a\r\nb\r\n\r\nc", "a\r\r\nb\n\r\r\r\nc", "a\nb\n\nc"] {
            assert!(guard.text_key(same) == key, "{same:?}");
        }
        for different in ["a\rb\n\nc", "a\nb\nc", "a\nb\n\nc\r", "a\nb\n\nc\n"] {
            assert!(guard.text_key(different) != key, "{different:?}");
        }
        assert_eq!(lf_line_ends("x\r\r\n"), "x\n");
        assert!(matches!(lf_line_ends("x\ry\n"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_repeat_of_the_last_shared_item_is_skipped_until_something_else_goes() {
        let mut guard = EchoGuard::new(0);
        assert_eq!(shared_text(guard.judge(local_text(1, "a\r\nb"))), "a\nb");
        assert!(matches!(
            guard.judge(local_text(2, "a\nb")),
            Verdict::Repeat
        ));
        assert_eq!(shared_text(guard.judge(local_text(3, "c"))), "c");
        assert_eq!(shared_text(guard.judge(local_text(4, "a\nb"))), "a\nb");
    }

    #[test]
    fn a_read_the_clipboard_moved_under_is_retried() {
        let mut guard = EchoGuard::new(0);
        assert_eq!(guard.observe(1, Instant::now()), Observation::Changed);
        let torn = Snapshot {
            before: 1,
            after: 2,
            origin: Origin::Local,
            content: Content::Text("partial".into()),
        };
        assert!(matches!(guard.judge(torn), Verdict::Retry));
        assert_eq!(guard.observe(2, Instant::now()), Observation::Changed);
        assert_eq!(shared_text(guard.judge(local_text(2, "whole"))), "whole");
        assert_eq!(guard.observe(2, Instant::now()), Observation::Unchanged);
    }

    #[test]
    fn skipped_empty_and_invalid_items_never_share() {
        let mut guard = EchoGuard::new(0);
        for skip in [
            Skip::Concealed,
            Skip::Files,
            Skip::Unsupported,
            Skip::TooLarge,
        ] {
            let verdict = guard.judge(snapshot(1, Origin::Local, Content::Skipped(skip)));
            assert!(matches!(verdict, Verdict::Skipped(found) if found == skip));
        }
        assert!(matches!(
            guard.judge(snapshot(2, Origin::Local, Content::Empty)),
            Verdict::Empty
        ));
        assert!(matches!(guard.judge(local_text(3, "\0")), Verdict::Empty));
        let long = "x".repeat(MAX_CLIPBOARD_TEXT as usize + 1);
        assert!(matches!(
            guard.judge(local_text(4, &long)),
            Verdict::Skipped(Skip::TooLarge)
        ));
        let not_png = Content::Png(b"definitely not a png, just text".to_vec());
        assert!(matches!(
            guard.judge(snapshot(5, Origin::Local, not_png)),
            Verdict::Skipped(Skip::Unsupported)
        ));
    }

    #[test]
    fn local_images_are_shared_as_found() {
        let mut guard = EchoGuard::new(0);
        let png = one_pixel_png();
        let verdict = guard.judge(snapshot(1, Origin::Local, Content::Png(png.clone())));
        assert!(matches!(verdict, Verdict::Share(Outgoing::Png(ref sent)) if *sent == png));
        let dib = vec![40, 0, 0, 0];
        let verdict = guard.judge(snapshot(2, Origin::Local, Content::Dib(dib.clone())));
        assert!(matches!(verdict, Verdict::Share(Outgoing::Dib(ref sent)) if *sent == dib));
    }

    #[test]
    fn a_local_copy_after_an_incoming_transfer_started_wins() {
        let start = Instant::now();
        let mut guard = EchoGuard::new(0);
        assert_eq!(guard.observe(1, start), Observation::Changed);
        let _ = guard.judge(local_text(1, "older local copy"));
        let incoming = start + Duration::from_millis(10);
        assert!(guard.may_apply(incoming, 1));

        assert!(!guard.may_apply(incoming, 2), "an unobserved change wins");

        assert_eq!(
            guard.observe(2, incoming + Duration::from_millis(5)),
            Observation::Changed
        );
        let _ = guard.judge(local_text(2, "copied during the transfer"));
        assert!(!guard.may_apply(incoming, 2));
        assert!(guard.may_apply(incoming + Duration::from_millis(6), 2));
    }

    #[test]
    fn a_paste_right_after_our_own_write_is_allowed() {
        let start = Instant::now();
        let mut guard = EchoGuard::new(0);
        let _ = guard.begin_apply(guard.text_key("first"));
        guard.applied(1);
        assert!(guard.may_apply(start, 1));
        assert_eq!(guard.observe(1, start), Observation::OwnWrite);
        assert!(guard.may_apply(start, 1));
    }

    #[test]
    fn debug_output_never_shows_content() {
        let secret = "hunter2";
        for rendered in [
            format!("{:?}", Content::Text(secret.into())),
            format!("{:?}", Outgoing::Text(secret.into())),
            format!("{:?}", Verdict::Share(Outgoing::Png(secret.into()))),
            format!("{:?}", local_text(1, secret)),
        ] {
            assert!(!rendered.contains(secret), "{rendered}");
            assert!(rendered.contains("redacted"), "{rendered}");
        }
    }
}
