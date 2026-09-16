//! Length-prefixed framing: `[u32 BE total length][JSON envelope][optional
//! binary payload]`. Identical to the JARVIS router v1 codec, so a v1 peer and
//! a v2 peer share one stream format.

use std::fmt;
use std::io::{Read, Write};

use crate::envelope::{Envelope, EnvelopeError, MAX_FRAME};

#[derive(Debug)]
pub enum FrameError {
    TooLarge { total: u32 },
    NoJson,
    JsonSizeMismatch { declared: usize, actual: usize },
    Envelope(EnvelopeError),
    Io(std::io::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { total } => write!(f, "frame too large: {total} bytes"),
            Self::NoJson => write!(f, "malformed frame: no JSON object found"),
            Self::JsonSizeMismatch { declared, actual } => write!(
                f,
                "payload size mismatch: declared {declared}, actual {actual}"
            ),
            Self::Envelope(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "frame io error: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<EnvelopeError> for FrameError {
    fn from(e: EnvelopeError) -> Self {
        Self::Envelope(e)
    }
}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Read one frame from a stream. Returns the envelope and its binary payload
/// (empty for control frames).
pub fn read_frame(reader: &mut impl Read) -> Result<(Envelope, Vec<u8>), FrameError> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let total = u32::from_be_bytes(len_bytes);
    if total == 0 || total > MAX_FRAME {
        return Err(FrameError::TooLarge { total });
    }

    let mut buf = vec![0u8; total as usize];
    reader.read_exact(&mut buf)?;

    decode_frame(&buf)
}

/// Decode the frame body after its four-byte stream length prefix. QUIC
/// datagrams use this same body so envelope semantics have one codec.
pub fn decode_frame(buf: &[u8]) -> Result<(Envelope, Vec<u8>), FrameError> {
    if buf.is_empty() || buf.len() > MAX_FRAME as usize {
        return Err(FrameError::TooLarge {
            total: u32::try_from(buf.len()).unwrap_or(u32::MAX),
        });
    }
    let json_end = find_json_end(buf).ok_or(FrameError::NoJson)?;
    let envelope: Envelope = serde_json::from_slice(&buf[..json_end])
        .map_err(|e| FrameError::Envelope(EnvelopeError::Malformed(e.to_string())))?;
    let actual = buf.len() - json_end;
    if actual != envelope.payload_bytes {
        return Err(FrameError::JsonSizeMismatch {
            declared: envelope.payload_bytes,
            actual,
        });
    }
    Ok((envelope, buf[json_end..].to_vec()))
}

/// Read one frame and validate freshness/identity with the injected clock.
pub fn read_validated_frame(
    reader: &mut impl Read,
    installation_id: &str,
    now_unix_ms: u64,
) -> Result<(Envelope, Vec<u8>), FrameError> {
    let (envelope, payload) = read_frame(reader)?;
    envelope.validate(installation_id, now_unix_ms)?;
    Ok((envelope, payload))
}

/// Write one frame. The envelope's `payload_bytes` must match `payload.len()`.
pub fn write_frame(
    writer: &mut impl Write,
    envelope: &Envelope,
    payload: &[u8],
) -> Result<(), FrameError> {
    let frame = encode_frame(envelope, payload)?;
    writer.write_all(&(frame.len() as u32).to_be_bytes())?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

/// Encode a frame body without the TCP length prefix. Used directly for QUIC
/// datagrams and inside a QUIC stream whose FIN supplies the boundary.
pub fn encode_frame(envelope: &Envelope, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() != envelope.payload_bytes {
        return Err(FrameError::JsonSizeMismatch {
            declared: envelope.payload_bytes,
            actual: payload.len(),
        });
    }
    let header = serde_json::to_vec(envelope).map_err(|e| FrameError::Io(e.into()))?;
    let total = header.len() + payload.len();
    if total > MAX_FRAME as usize {
        return Err(FrameError::TooLarge {
            total: total as u32,
        });
    }
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&header);
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Locate the end of the JSON object that starts the frame, so the binary
/// payload boundary is exact without buffering twice.
fn find_json_end(buf: &[u8]) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut found_start = false;
    for (i, byte) in buf.iter().enumerate() {
        if !found_start && *byte != b'{' {
            continue;
        }
        found_start = true;
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
        } else if *byte == b'"' {
            in_string = true;
        } else if *byte == b'{' {
            depth += 1;
        } else if *byte == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Message;
    use crate::qos::Qos;
    use std::io::Cursor;

    fn stream_envelope(payload_len: usize) -> Envelope {
        let mut envelope = Envelope::new(
            "home",
            "mac-studio-brain",
            Some("home-pi-audio"),
            "tok",
            Message::new(
                "audio_playback_chunk",
                serde_json::json!({
                    "format": "s16_le",
                    "sample_rate_hz": 48000u32,
                    "channels": 2u16,
                    "timestamp_ms": 123456u64,
                    "payload_bytes": payload_len,
                }),
            ),
        );
        envelope.payload_bytes = payload_len;
        envelope
    }

    #[test]
    fn control_frame_round_trips() {
        let envelope = Envelope::new(
            "home",
            "brain",
            None,
            "",
            Message::new("heartbeat", serde_json::Value::Null),
        );
        let mut buf = Vec::new();
        write_frame(&mut buf, &envelope, &[]).unwrap();
        let mut cursor = Cursor::new(&buf);
        let (read, payload) = read_frame(&mut cursor).unwrap();
        assert!(payload.is_empty());
        assert_eq!(read, envelope);
        assert_eq!(Qos::of_kind(&read.message.kind), Qos::Command);
    }

    #[test]
    fn binary_payload_round_trips() {
        let pcm: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let envelope = stream_envelope(pcm.len());
        let mut buf = Vec::new();
        write_frame(&mut buf, &envelope, &pcm).unwrap();
        let mut cursor = Cursor::new(&buf);
        let (read, payload) = read_frame(&mut cursor).unwrap();
        assert_eq!(payload, pcm);
        assert_eq!(read, envelope);
        assert_eq!(Qos::of_kind(&read.message.kind), Qos::Stream);
    }

    #[test]
    fn write_rejects_payload_size_mismatch() {
        let envelope = stream_envelope(16);
        let mut buf = Vec::new();
        assert!(matches!(
            write_frame(&mut buf, &envelope, &[0u8; 8]),
            Err(FrameError::JsonSizeMismatch { .. })
        ));
    }

    #[test]
    fn read_rejects_oversized_length() {
        let mut cursor = Cursor::new((MAX_FRAME + 1).to_be_bytes().to_vec());
        assert!(matches!(
            read_frame(&mut cursor),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn validated_frame_rejects_stale() {
        let envelope = Envelope::new(
            "home",
            "brain",
            None,
            "",
            Message::new("heartbeat", serde_json::Value::Null),
        );
        let mut buf = Vec::new();
        write_frame(&mut buf, &envelope, &[]).unwrap();
        let mut cursor = Cursor::new(&buf);
        let future = envelope.issued_at_unix_ms + 60_000;
        assert!(matches!(
            read_validated_frame(&mut cursor, "home", future),
            Err(FrameError::Envelope(EnvelopeError::Stale { .. }))
        ));
    }
}
