//! Strict, bounded framing for already-authorized QUIC session streams.
//!
//! This module neither opens a socket nor establishes peer trust. Callers must use
//! [`crate::guarded_endpoint::GuardedEndpoint`] and obtain explicit sharing authorization before
//! passing a QUIC stream here.

use std::{cmp, error::Error, fmt, time::Duration};

use monhop_protocol::{
    DecodeError, EncodeError, Frame, HEADER_LEN, MAX_FRAME_LEN, Message, decode,
};

/// A write is abandoned when it cannot complete within this bounded interval.
pub const WRITE_DEADLINE: Duration = Duration::from_millis(60);

const ABORT_ERROR_CODE: u32 = 1;
const ABORT_REASON: &[u8] = b"session wire write aborted";

/// A category-only framing failure. It never contains peer bytes, keys, or input values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionWireError {
    Io,
    Closed,
    Truncated,
    InvalidFrame,
    /// The peer speaks another protocol version: every frame it sends is refused, so the
    /// handshake must name a build mismatch rather than retry a malformed frame.
    UnsupportedVersion,
    TooLarge,
    TimedOut,
    Terminal,
}

impl fmt::Display for SessionWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io => "session stream I/O failed",
            Self::Closed => "session stream closed before the next frame",
            Self::Truncated => "session stream ended during a frame",
            Self::InvalidFrame => "session stream contained an invalid frame",
            Self::UnsupportedVersion => "session stream carried another protocol version",
            Self::TooLarge => "session stream declared a frame above the fixed limit",
            Self::TimedOut => "session frame write timed out",
            Self::Terminal => "session framing is terminal",
        })
    }
}

impl Error for SessionWireError {}

/// The result of one pure [`FrameReader::feed`] operation.
///
/// `feed` consumes no bytes beyond the next complete frame. Pass the unconsumed input back to
/// `feed` to parse consecutive frames without allocating an intermediate buffer.
pub struct FeedResult {
    pub consumed: usize,
    pub frame: Option<Frame>,
}

impl fmt::Debug for FeedResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FeedResult")
            .field("consumed", &self.consumed)
            .field("frame", &self.frame.as_ref().map(|_| "Frame([redacted])"))
            .finish()
    }
}

/// A reusable bounded parser for one ordered reliable stream.
///
/// The fixed `MAX_FRAME_LEN` buffer is allocated once by [`Self::new`]. Quinn 0.11.11 documents
/// [`quinn::RecvStream::read`] as cancel-safe. After every completed read this type advances its
/// persistent offset before parsing, so a cancelled future cannot lose already-read bytes.
pub struct FrameReader {
    buffer: Vec<u8>,
    filled: usize,
    expected_len: Option<usize>,
    terminal: bool,
}

impl fmt::Debug for FrameReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrameReader")
            .field("filled", &self.filled)
            .field("expected_len", &self.expected_len)
            .field("terminal", &self.terminal)
            .field("buffer", &"[redacted]")
            .finish()
    }
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    /// Allocates the fixed maximum parser buffer exactly once.
    pub fn new() -> Self {
        Self {
            buffer: vec![0; MAX_FRAME_LEN],
            filled: 0,
            expected_len: None,
            terminal: false,
        }
    }

    /// Reads exactly one framed protocol message from an already-open Quinn receive stream.
    pub async fn read_frame(
        &mut self,
        recv: &mut quinn::RecvStream,
    ) -> Result<Frame, SessionWireError> {
        self.ensure_active()?;

        loop {
            let target = self.target_len();
            let start = self.filled;
            let read = {
                let destination = &mut self.buffer[start..target];
                recv.read(destination).await
            };
            let read = match read {
                Ok(Some(0)) | Ok(None) => return Err(self.eof_error()),
                Ok(Some(read)) => read,
                Err(_) => return Err(self.fail(SessionWireError::Io)),
            };

            // `RecvStream::read` is cancel-safe and this runs without another await point.
            self.filled += read;
            if let Some(frame) = self.decode_if_complete()? {
                return Ok(frame);
            }
        }
    }

    /// Feeds copied bytes into the same parser state used by [`Self::read_frame`].
    ///
    /// This is a pure test seam. It consumes at most one frame and never allocates after
    /// [`Self::new`]. An empty input is valid and consumes no bytes.
    pub fn feed(&mut self, input: &[u8]) -> Result<FeedResult, SessionWireError> {
        self.ensure_active()?;
        let target = self.target_len();
        let consumed = cmp::min(input.len(), target - self.filled);
        if consumed != 0 {
            let end = self.filled + consumed;
            self.buffer[self.filled..end].copy_from_slice(&input[..consumed]);
            self.filled = end;
        }
        let frame = self.decode_if_complete()?;
        Ok(FeedResult { consumed, frame })
    }

    /// Marks the input source as ended. A clean end between frames is `Closed`; a partial frame
    /// is `Truncated`. Both leave this reader terminal.
    pub fn finish(&mut self) -> Result<(), SessionWireError> {
        self.ensure_active()?;
        Err(self.eof_error())
    }

    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn target_len(&self) -> usize {
        self.expected_len.unwrap_or(HEADER_LEN)
    }

    fn decode_if_complete(&mut self) -> Result<Option<Frame>, SessionWireError> {
        if self.expected_len.is_none() && self.filled == HEADER_LEN {
            let body_len = usize::from(u16::from_be_bytes([self.buffer[8], self.buffer[9]]));
            let total_len = HEADER_LEN
                .checked_add(body_len)
                .ok_or_else(|| self.fail(SessionWireError::TooLarge))?;
            if total_len > MAX_FRAME_LEN {
                return Err(self.fail(SessionWireError::TooLarge));
            }
            self.expected_len = Some(total_len);
        }

        let Some(expected_len) = self.expected_len else {
            return Ok(None);
        };
        if self.filled != expected_len {
            return Ok(None);
        }

        let frame = decode(&self.buffer[..expected_len]).map_err(|error| {
            self.fail(match error {
                DecodeError::FrameTooLarge => SessionWireError::TooLarge,
                DecodeError::UnsupportedVersion => SessionWireError::UnsupportedVersion,
                _ => SessionWireError::InvalidFrame,
            })
        })?;
        self.filled = 0;
        self.expected_len = None;
        Ok(Some(frame))
    }

    fn ensure_active(&self) -> Result<(), SessionWireError> {
        (!self.terminal)
            .then_some(())
            .ok_or(SessionWireError::Terminal)
    }

    fn eof_error(&mut self) -> SessionWireError {
        let error = if self.filled == 0 {
            SessionWireError::Closed
        } else {
            SessionWireError::Truncated
        };
        self.fail(error)
    }

    fn fail(&mut self, error: SessionWireError) -> SessionWireError {
        self.terminal = true;
        error
    }
}

/// Writes one complete protocol frame to an already-authorized Quinn stream.
///
/// Quinn documents `SendStream::write_all` as not cancellation-safe because a prefix can be
/// written. The guard therefore closes the entire connection on any failure, timeout, or future
/// cancellation after polling has started, preventing a partial frame from being retried.
pub async fn write_frame(
    connection: &quinn::Connection,
    send: &mut quinn::SendStream,
    frame: &Frame,
) -> Result<(), SessionWireError> {
    FrameWriter::default().write(connection, send, frame).await
}

/// Ping and Pong ride QUIC datagrams inside an input session: a lost one costs an interval, and
/// a retransmitting input stream cannot hold liveness hostage.
pub const fn is_heartbeat(message: &Message) -> bool {
    matches!(message, Message::Ping(_) | Message::Pong(_))
}

/// Decodes one whole datagram; only heartbeats are ever expected there.
pub fn decode_datagram(bytes: &[u8]) -> Result<Frame, SessionWireError> {
    let frame = decode(bytes).map_err(|error| match error {
        DecodeError::FrameTooLarge => SessionWireError::TooLarge,
        DecodeError::UnsupportedVersion => SessionWireError::UnsupportedVersion,
        _ => SessionWireError::InvalidFrame,
    })?;
    if !is_heartbeat(&frame.message) {
        return Err(SessionWireError::InvalidFrame);
    }
    Ok(frame)
}

/// Reuses one bounded encoding buffer for the high-frequency ordered input stream.
pub struct FrameWriter {
    encoded: Vec<u8>,
}
impl Default for FrameWriter {
    fn default() -> Self {
        Self {
            encoded: Vec::with_capacity(MAX_FRAME_LEN),
        }
    }
}
impl FrameWriter {
    pub async fn write(
        &mut self,
        connection: &quinn::Connection,
        send: &mut quinn::SendStream,
        frame: &Frame,
    ) -> Result<(), SessionWireError> {
        let mut close_guard = CloseConnectionOnDrop::new(connection);
        self.encoded.clear();
        frame
            .encode_into(&mut self.encoded)
            .map_err(|error| match error {
                EncodeError::FrameTooLarge => SessionWireError::TooLarge,
                _ => SessionWireError::InvalidFrame,
            })?;

        match tokio::time::timeout(WRITE_DEADLINE, send.write_all(&self.encoded)).await {
            Ok(Ok(())) => {
                close_guard.disarm();
                Ok(())
            }
            Ok(Err(_)) => Err(SessionWireError::Io),
            Err(_) => Err(SessionWireError::TimedOut),
        }
    }

    /// Sends a heartbeat as a datagram; a full send buffer drops the oldest queued heartbeat.
    pub fn send_datagram(
        &mut self,
        connection: &quinn::Connection,
        frame: &Frame,
    ) -> Result<(), SessionWireError> {
        if !is_heartbeat(&frame.message) {
            return Err(SessionWireError::InvalidFrame);
        }
        self.encoded.clear();
        frame
            .encode_into(&mut self.encoded)
            .map_err(|_| SessionWireError::InvalidFrame)?;
        connection
            .send_datagram(bytes::Bytes::copy_from_slice(&self.encoded))
            .map_err(|error| match error {
                quinn::SendDatagramError::TooLarge => SessionWireError::TooLarge,
                quinn::SendDatagramError::ConnectionLost(_) => SessionWireError::Io,
                quinn::SendDatagramError::UnsupportedByPeer
                | quinn::SendDatagramError::Disabled => SessionWireError::Terminal,
            })
    }
}

struct CloseConnectionOnDrop<'a> {
    connection: &'a quinn::Connection,
    armed: bool,
}

impl<'a> CloseConnectionOnDrop<'a> {
    const fn new(connection: &'a quinn::Connection) -> Self {
        Self {
            connection,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CloseConnectionOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.connection.close(ABORT_ERROR_CODE.into(), ABORT_REASON);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monhop_protocol::{Message, PROTOCOL_VERSION, SessionEpoch};

    #[test]
    fn v9_datagrams_preserve_scope_and_reject_unknown_scope_bytes() {
        use monhop_protocol::FrameScope;
        for scope in [
            FrameScope::Connection,
            FrameScope::LowerControlsHigher,
            FrameScope::HigherControlsLower,
        ] {
            let frame =
                Frame::new(SessionEpoch::new(1).unwrap(), 0, Message::Ping(1)).with_scope(scope);
            let mut bytes = Vec::new();
            frame.encode_into(&mut bytes).unwrap();
            assert_eq!(decode_datagram(&bytes).unwrap(), frame);
            for unknown in 3..=255 {
                bytes[7] = unknown;
                assert!(decode_datagram(&bytes).is_err());
            }
        }
    }

    fn frame(sequence: u64) -> Frame {
        Frame::new(
            SessionEpoch::new(1).expect("nonzero test epoch"),
            sequence,
            Message::Ping(sequence),
        )
    }

    fn encoded(frame: &Frame) -> Vec<u8> {
        let mut output = Vec::with_capacity(MAX_FRAME_LEN);
        frame.encode_into(&mut output).expect("valid test frame");
        output
    }
    #[test]
    fn a_foreign_protocol_version_is_named_rather_than_treated_as_malformed() {
        let mut bytes = encoded(&frame(1));
        // Bytes 4..6 carry the protocol version; any change there is another build's frame.
        bytes[5] ^= 0x01;
        let mut reader = FrameReader::new();
        let header = reader.feed(&bytes).expect("the header alone parses");
        assert_eq!(header.consumed, HEADER_LEN);
        assert!(matches!(
            reader.feed(&bytes[HEADER_LEN..]),
            Err(SessionWireError::UnsupportedVersion)
        ));
        assert!(matches!(
            decode_datagram(&bytes),
            Err(SessionWireError::UnsupportedVersion)
        ));
    }

    fn drain(reader: &mut FrameReader, input: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < input.len() {
            let result = reader.feed(&input[offset..]).expect("valid frame bytes");
            assert!(result.consumed > 0, "parser must consume pending bytes");
            offset += result.consumed;
            if let Some(frame) = result.frame {
                frames.push(frame);
            }
        }
        frames
    }

    #[test]
    fn every_split_point_uses_the_same_incremental_parser_state() {
        let source = frame(7);
        let encoded = encoded(&source);

        for split in 0..=encoded.len() {
            let mut reader = FrameReader::new();
            let mut frames = Vec::new();
            if split != 0 {
                frames.extend(drain(&mut reader, &encoded[..split]));
            }
            frames.extend(drain(&mut reader, &encoded[split..]));
            assert_eq!(
                frames.as_slice(),
                std::slice::from_ref(&source),
                "split point {split}"
            );
        }
    }

    #[test]
    fn one_reader_parses_consecutive_frames_without_a_length_prefix() {
        let first = frame(1);
        let second = frame(2);
        let mut bytes = encoded(&first);
        bytes.extend_from_slice(&encoded(&second));

        let frames = drain(&mut FrameReader::new(), &bytes);

        assert_eq!(frames, [first, second]);
    }

    #[test]
    fn oversized_body_length_is_terminal_as_soon_as_the_header_is_complete() {
        let mut header = [0_u8; HEADER_LEN];
        header[8..10].copy_from_slice(&u16::MAX.to_be_bytes());
        let mut reader = FrameReader::new();

        assert!(matches!(
            reader.feed(&header),
            Err(SessionWireError::TooLarge)
        ));
        assert!(reader.is_terminal());
        assert!(matches!(reader.feed(&[]), Err(SessionWireError::Terminal)));
    }

    #[test]
    fn eof_is_closed_or_truncated_and_then_terminal() {
        let mut empty = FrameReader::new();
        assert_eq!(empty.finish(), Err(SessionWireError::Closed));
        assert_eq!(empty.finish(), Err(SessionWireError::Terminal));

        let source = encoded(&frame(3));
        let mut partial = FrameReader::new();
        assert_eq!(
            partial
                .feed(&source[..HEADER_LEN - 1])
                .expect("partial header")
                .frame,
            None
        );
        assert_eq!(partial.finish(), Err(SessionWireError::Truncated));
        assert!(matches!(partial.feed(&[]), Err(SessionWireError::Terminal)));
    }

    #[test]
    fn unsupported_protocol_version_is_named_and_terminal() {
        let mut invalid = encoded(&frame(4));
        let unsupported_version: u16 = if PROTOCOL_VERSION == 0 { 1 } else { 0 };
        invalid[4..6].copy_from_slice(&unsupported_version.to_be_bytes());
        let mut reader = FrameReader::new();

        let header = reader.feed(&invalid).expect("complete header");
        assert_eq!(header.consumed, HEADER_LEN);
        assert_eq!(header.frame, None);
        let result = reader.feed(&invalid[HEADER_LEN..]);
        assert!(
            matches!(result, Err(SessionWireError::UnsupportedVersion)),
            "unexpected parser result: {result:?}"
        );
        assert_eq!(reader.finish(), Err(SessionWireError::Terminal));
    }
}
