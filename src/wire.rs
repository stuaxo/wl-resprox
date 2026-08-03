//! Manual Wayland wire-protocol framing.
//!
//! Every Wayland message starts with an 8-byte header: a 32-bit sender
//! object ID, followed by a 32-bit word packing a 16-bit opcode (low bits)
//! and a 16-bit total message length in bytes -- header included (high
//! bits). This framing is true regardless of which interface the sender
//! object belongs to: unlike decoding individual arguments, finding where
//! one message ends and the next begins needs no protocol/interface
//! knowledge at all. That's what makes it possible to intercept and rewrite
//! `sender_id` (and, later, object-id-typed arguments) without first
//! building out full per-interface codegen.
//!
//! Byte order is native-endian, not little-endian: this matches the actual
//! Wayland wire protocol (and libwayland's own implementation), which
//! assumes both ends of a *local* socket share the host's endianness.
//! sommelier-rs's raw parser (reference/sommelier-rs/sommelier/src/proxy.rs)
//! does the same. Waypipe uses explicit little-endian instead because it's
//! a network transport that may bridge different architectures -- a
//! constraint that doesn't apply to us (see docs/architecture-context.md
//! section 4).

/// Size of the fixed Wayland message header, in bytes.
pub const HEADER_LEN: usize = 8;

/// A parsed Wayland message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageHeader {
    pub sender_id: u32,
    pub opcode: u16,
    /// Total message length in bytes, header included.
    pub length: u16,
}

impl MessageHeader {
    /// Parses the header from the start of `buf`. Returns `None` if `buf`
    /// is shorter than a full header -- this is a normal, expected
    /// condition (messages routinely arrive split across multiple socket
    /// reads), not an error: callers should just wait for more bytes.
    pub fn parse(buf: &[u8]) -> Option<MessageHeader> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        let sender_id = u32::from_ne_bytes(buf[0..4].try_into().unwrap());
        let word2 = u32::from_ne_bytes(buf[4..8].try_into().unwrap());
        Some(MessageHeader {
            sender_id,
            opcode: (word2 & 0xffff) as u16,
            length: (word2 >> 16) as u16,
        })
    }

    /// Total length of the message this header describes, header included.
    pub fn message_len(&self) -> usize {
        self.length as usize
    }
}

/// Reads just the sender_id from a raw message buffer, without parsing the
/// full header. Returns `None` if `msg` is too short to contain one.
// Not called from main.rs yet: this milestone is "simple ID reflection"
// (client id == host id always), not real translation. The real Shadow
// Table (Phase 4) is what will actually call this.
#[allow(dead_code)]
pub fn read_sender_id(msg: &[u8]) -> Option<u32> {
    Some(u32::from_ne_bytes(msg.get(0..4)?.try_into().unwrap()))
}

/// Overwrites the sender_id in place in a raw message buffer. This is the
/// actual ID-rewriting primitive the Shadow Table (Phase 4) will use: read
/// the original id via `read_sender_id`, look up its translation, then
/// mutate the bytes in place before forwarding -- no need to fully decode
/// or re-encode the rest of the message just to change which object it's
/// addressed to.
///
/// Returns `Err` if `msg` is too short to contain a sender_id field.
#[allow(dead_code)] // see read_sender_id above -- same reason
pub fn write_sender_id(msg: &mut [u8], new_id: u32) -> Result<(), &'static str> {
    let slot = msg
        .get_mut(0..4)
        .ok_or("message shorter than a sender_id field")?;
    slot.copy_from_slice(&new_id.to_ne_bytes());
    Ok(())
}

/// Attempts to split one complete message off the front of `buf`, returning
/// the message slice (header included) and its length. Returns `None` if
/// `buf` doesn't yet contain a complete message -- either the header itself
/// hasn't fully arrived, or the header's declared length extends past what
/// we currently have buffered. Either way, the caller should keep reading
/// from the socket and try again once more data arrives.
pub fn take_message(buf: &[u8]) -> Option<(&[u8], usize)> {
    let header = MessageHeader::parse(buf)?;
    let len = header.message_len();
    if len < HEADER_LEN || buf.len() < len {
        return None;
    }
    Some((&buf[..len], len))
}

/// Builds one complete wire message (header + payload) from scratch.
/// Together with `put_u32`/`put_str`/`read_u32`/`read_str` below, these are
/// the primitives for code that needs to speak Wayland as a synthetic
/// client -- currently `run_connection`'s reconnect state-recovery path
/// (see recreation.rs) and, standalone, `examples/probe_bind.rs` and
/// `tests/integration.rs`'s hand-crafted-bytes tests. Consolidated here
/// rather than left duplicated across those three, which is how this
/// function used to exist (three near-identical copies).
pub fn build_message(sender_id: u32, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let length = (HEADER_LEN + payload.len()) as u16;
    let mut msg = Vec::with_capacity(length as usize);
    put_u32(&mut msg, sender_id);
    put_u32(&mut msg, ((length as u32) << 16) | opcode as u32);
    msg.extend_from_slice(payload);
    msg
}

pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_ne_bytes());
}

/// Appends a wire-encoded string: a u32 length (NUL terminator included),
/// the bytes themselves, a NUL, then padding out to the next 4-byte
/// boundary.
pub fn put_str(buf: &mut Vec<u8>, s: &str) {
    let with_nul_len = s.len() + 1;
    put_u32(buf, with_nul_len as u32);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
    let padded = with_nul_len.next_multiple_of(4);
    for _ in with_nul_len..padded {
        buf.push(0);
    }
}

pub fn read_u32(payload: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(payload.get(offset..offset + 4)?.try_into().ok()?))
}

/// Reads a wire string starting at `offset` (see `put_str` for the wire
/// layout). Returns the decoded string (NUL trimmed) and the offset just
/// past it (including padding).
pub fn read_str(payload: &[u8], offset: usize) -> Option<(String, usize)> {
    let len = read_u32(payload, offset)? as usize;
    let start = offset + 4;
    let bytes = payload.get(start..start + len)?;
    let s = std::str::from_utf8(&bytes[..len.saturating_sub(1)]).ok()?.to_string();
    let padded = len.next_multiple_of(4);
    Some((s, start + padded))
}

/// Hex-encodes bytes for diagnostic output -- shared so a live `warn!` log
/// line and `recorder::Recorder::record`'s on-disk format use the exact
/// same representation of the same bytes.
pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_message(sender_id: u32, opcode: u16, body: &[u8]) -> Vec<u8> {
        let length = (HEADER_LEN + body.len()) as u16;
        let mut msg = Vec::with_capacity(length as usize);
        msg.extend_from_slice(&sender_id.to_ne_bytes());
        msg.extend_from_slice(&(((length as u32) << 16) | opcode as u32).to_ne_bytes());
        msg.extend_from_slice(body);
        msg
    }

    #[test]
    fn parses_header_fields() {
        let msg = build_message(42, 3, &[1, 2, 3, 4]);
        let header = MessageHeader::parse(&msg).expect("should parse");
        assert_eq!(header.sender_id, 42);
        assert_eq!(header.opcode, 3);
        assert_eq!(header.message_len(), HEADER_LEN + 4);
    }

    #[test]
    fn parse_returns_none_on_short_buffer() {
        assert!(MessageHeader::parse(&[0u8; 7]).is_none());
        assert!(MessageHeader::parse(&[]).is_none());
    }

    #[test]
    fn read_and_write_sender_id_round_trip() {
        let mut msg = build_message(1, 0, &[]);
        assert_eq!(read_sender_id(&msg), Some(1));
        write_sender_id(&mut msg, 999).expect("should write");
        assert_eq!(read_sender_id(&msg), Some(999));
        // opcode/length untouched by rewriting sender_id
        let header = MessageHeader::parse(&msg).unwrap();
        assert_eq!(header.opcode, 0);
        assert_eq!(header.message_len(), HEADER_LEN);
    }

    #[test]
    fn write_sender_id_rejects_too_short_buffer() {
        let mut buf = [0u8; 3];
        assert!(write_sender_id(&mut buf, 5).is_err());
    }

    #[test]
    fn take_message_splits_exactly_one_message_from_a_longer_buffer() {
        let mut buf = build_message(1, 0, &[9, 9]);
        buf.extend(build_message(2, 1, &[]));

        let (first, consumed) = take_message(&buf).expect("first message should parse");
        assert_eq!(consumed, HEADER_LEN + 2);
        assert_eq!(read_sender_id(first), Some(1));

        let (second, consumed2) = take_message(&buf[consumed..]).expect("second message");
        assert_eq!(consumed2, HEADER_LEN);
        assert_eq!(read_sender_id(second), Some(2));
    }

    #[test]
    fn take_message_returns_none_when_incomplete() {
        // Header claims a body that hasn't fully arrived yet.
        let full = build_message(1, 0, &[1, 2, 3, 4]);
        assert!(take_message(&full[..HEADER_LEN + 2]).is_none());
        // Not even a full header yet.
        assert!(take_message(&full[..5]).is_none());
    }
}
