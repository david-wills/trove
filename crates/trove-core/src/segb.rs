//! SEGB v2 segment reader — the on-disk format of Apple's Biome event
//! streams (`~/Library/Biome/streams/…`). Deliberately OS-free: callers hand
//! it bytes, it hands back records, so it unit-tests against fixtures (the
//! `Watcher`/`Scrobbler` testability pattern).
//!
//! Format (verified against real App.InFocus segments on macOS 26, and
//! matching the `ccl_segb` forensic reference):
//!
//! ```text
//! offset 0   header, 32 bytes: "SEGB" magic, i32 LE entry count,
//!            f64 LE creation time (Apple epoch), 16 unknown bytes
//! offset 32  data area: entries packed in offset order, each
//!            [u32 LE crc32-of-data][i32 LE unknown][data…], padded to a
//!            4-byte boundary
//! EOF-16×n   trailer, one 16-byte record per entry: i32 LE end offset
//!            (relative to the end of the header), i32 LE state,
//!            f64 LE write time (Apple epoch)
//! ```
//!
//! Entry length is not stored inline — it falls out of walking the trailer
//! offsets in order. State 1 is a live record; state 3 is deleted (its data
//! is zeroed, so the CRC no longer matches); state 4 is an empty slot that
//! occupies no data-area bytes. Only state-1 records with a matching CRC are
//! returned; the rest are counted (`deleted` for normal churn, `anomalies`
//! for torn/unreadable records) so collectors can log drift loudly instead
//! of silently dropping data.
//!
//! Biome record payloads are protobuf messages; [`proto_fields`] is the
//! minimal wire-format walker the per-stream decoders (App.InFocus,
//! Media.NowPlaying) build on. Unknown fields are carried through untouched,
//! so a new field in a macOS update can never break decoding.

use anyhow::{bail, Result};

/// Seconds between the Unix epoch (1970) and the Apple/Core Foundation epoch
/// (2001-01-01) that every Biome timestamp counts from.
pub const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

const MAGIC: &[u8; 4] = b"SEGB";
const HEADER_LEN: usize = 32;
const TRAILER_ENTRY_LEN: usize = 16;
const ENTRY_HEADER_LEN: usize = 8;

/// One live record from a segment.
#[derive(Debug, Clone)]
pub struct SegbEntry {
    /// When the record was written, seconds since the Apple epoch (from the
    /// trailer — per-stream payloads usually carry their own event time).
    pub timestamp: f64,
    /// The record payload (protobuf for Biome streams).
    pub data: Vec<u8>,
}

/// A parsed segment.
#[derive(Debug, Clone)]
pub struct SegbSegment {
    /// Segment creation time, seconds since the Apple epoch.
    pub created: f64,
    /// Live records, in data-area (chronological) order.
    pub entries: Vec<SegbEntry>,
    /// Deleted records and empty slots (states 3/4) — entirely normal;
    /// segments past Biome's retention window consist of nothing else.
    pub deleted: usize,
    /// Records that *should* have been live but weren't readable: CRC
    /// mismatches, out-of-bounds trailer offsets. Nonzero on the file being
    /// appended right now is a torn tail (retried next pass); a surge
    /// suggests format drift — worth a loud log line.
    pub anomalies: usize,
}

fn u32_le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b.try_into().unwrap())
}

fn i32_le(b: &[u8]) -> i32 {
    i32::from_le_bytes(b.try_into().unwrap())
}

fn f64_le(b: &[u8]) -> f64 {
    f64::from_le_bytes(b.try_into().unwrap())
}

/// Parse a SEGB v2 segment. Errors only on a structurally unusable file
/// (bad magic, impossible counts); individual bad entries are skipped and
/// counted, never fatal — segments are appended live by `biomed` and the
/// tail may be torn mid-write.
pub fn read_segb(bytes: &[u8]) -> Result<SegbSegment> {
    if bytes.len() < HEADER_LEN {
        bail!("segment too short for header: {} bytes", bytes.len());
    }
    if &bytes[..4] != MAGIC {
        bail!("bad magic: {:02x?} (not SEGB v2)", &bytes[..4]);
    }
    let count = i32_le(&bytes[4..8]);
    let created = f64_le(&bytes[8..16]);
    if count < 0 {
        bail!("negative entry count: {count}");
    }
    let count = count as usize;
    let trailer_len = count
        .checked_mul(TRAILER_ENTRY_LEN)
        .filter(|t| HEADER_LEN + t <= bytes.len());
    let Some(trailer_len) = trailer_len else {
        bail!("entry count {count} exceeds file size {}", bytes.len());
    };
    let trailer_start = bytes.len() - trailer_len;

    // Trailer entries are written in insertion order but the data area is
    // walked by ascending end offset, mirroring the reference parser.
    let mut trailer: Vec<(i32, i32, f64)> = (0..count)
        .map(|i| {
            let t = &bytes[trailer_start + i * TRAILER_ENTRY_LEN..];
            (i32_le(&t[..4]), i32_le(&t[4..8]), f64_le(&t[8..16]))
        })
        .collect();
    trailer.sort_by_key(|&(end_offset, _, _)| end_offset);

    let mut entries = Vec::new();
    let mut deleted = 0usize;
    let mut anomalies = 0usize;
    let mut pos = HEADER_LEN;
    for (end_offset, state, timestamp) in trailer {
        // State 4 is an empty slot: it occupies no bytes in the data area.
        if state == 4 {
            deleted += 1;
            continue;
        }
        let Some(end_abs) = usize::try_from(end_offset)
            .ok()
            .and_then(|o| o.checked_add(HEADER_LEN))
            .filter(|&e| e >= pos + ENTRY_HEADER_LEN && e <= trailer_start)
        else {
            // Inconsistent trailer offset (torn tail): skip without moving.
            anomalies += 1;
            continue;
        };
        let data = &bytes[pos + ENTRY_HEADER_LEN..end_abs];
        let crc_stored = u32_le(&bytes[pos..pos + 4]);
        if state != 1 {
            // Deleted record (state 3): data is zeroed, nothing to read.
            deleted += 1;
        } else if crc32(data) == crc_stored {
            entries.push(SegbEntry {
                timestamp,
                data: data.to_vec(),
            });
        } else {
            anomalies += 1; // torn write
        }
        pos = end_abs + (end_abs.wrapping_neg() & 3); // round up to 4
    }
    Ok(SegbSegment {
        created,
        entries,
        deleted,
        anomalies,
    })
}

/// IEEE CRC-32 (the zlib polynomial), as stored in entry headers. Local
/// implementation — payloads are tiny, no dependency is worth it.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

/// One decoded protobuf field value. `Fixed64` is returned as raw bits —
/// Biome stores `f64` timestamps there, use [`f64::from_bits`].
#[derive(Debug, Clone, PartialEq)]
pub enum ProtoValue<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// Walk a protobuf wire-format message into `(field number, value)` pairs.
/// `None` means the buffer is not valid protobuf (truncated, zeroed by a
/// deletion, or not protobuf at all) — callers log-and-skip the record.
pub fn proto_fields(buf: &[u8]) -> Option<Vec<(u64, ProtoValue<'_>)>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let (tag, n) = varint(&buf[i..])?;
        i += n;
        let field = tag >> 3;
        if field == 0 {
            return None; // field 0 is invalid protobuf
        }
        let value = match tag & 7 {
            0 => {
                let (v, n) = varint(&buf[i..])?;
                i += n;
                ProtoValue::Varint(v)
            }
            1 => {
                let b = buf.get(i..i + 8)?;
                i += 8;
                ProtoValue::Fixed64(u64::from_le_bytes(b.try_into().unwrap()))
            }
            2 => {
                let (len, n) = varint(&buf[i..])?;
                i += n;
                let len = usize::try_from(len).ok()?;
                let b = buf.get(i..i + len)?;
                i += len;
                ProtoValue::Bytes(b)
            }
            5 => {
                let b = buf.get(i..i + 4)?;
                i += 4;
                ProtoValue::Fixed32(u32::from_le_bytes(b.try_into().unwrap()))
            }
            _ => return None, // group wire types: not used by Biome
        };
        out.push((field, value));
    }
    Some(out)
}

/// Decode one LEB128 varint; returns (value, bytes consumed).
fn varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    let mut shift = 0u32;
    for (n, &b) in buf.iter().enumerate() {
        if shift >= 64 {
            return None;
        }
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some((v, n + 1));
        }
        shift += 7;
    }
    None
}

/// Build a syntactically valid SEGB v2 segment from (state, payload) pairs —
/// the inverse of [`read_segb`], for tests here and in the per-stream
/// collectors (screen time).
#[cfg(test)]
pub(crate) fn build_segment(records: &[(i32, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(records.len() as i32).to_le_bytes());
    out.extend_from_slice(&123_456_789.0f64.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    let mut trailer = Vec::new();
    for (i, (state, data)) in records.iter().enumerate() {
        if *state == 4 {
            // Empty slot: trailer-only, no data-area bytes.
            trailer.push((out.len() - HEADER_LEN, *state, i as f64));
            continue;
        }
        out.extend_from_slice(&crc32(data).to_le_bytes());
        out.extend_from_slice(&10i32.to_le_bytes());
        out.extend_from_slice(data);
        trailer.push((out.len() - HEADER_LEN, *state, i as f64));
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    for (end_offset, state, ts) in trailer {
        out.extend_from_slice(&(end_offset as i32).to_le_bytes());
        out.extend_from_slice(&state.to_le_bytes());
        out.extend_from_slice(&ts.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        // zlib.crc32(b"123456789") — the standard check value.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn parses_records_in_order_with_padding() {
        // Lengths 5 and 6 force padding between records.
        let seg = build_segment(&[(1, b"hello"), (1, b"world!"), (1, b"x")]);
        let parsed = read_segb(&seg).unwrap();
        assert_eq!(parsed.created, 123_456_789.0);
        assert_eq!(parsed.deleted + parsed.anomalies, 0);
        let datas: Vec<&[u8]> = parsed.entries.iter().map(|e| e.data.as_slice()).collect();
        assert_eq!(datas, vec![&b"hello"[..], b"world!", b"x"]);
        assert_eq!(parsed.entries[1].timestamp, 1.0);
    }

    #[test]
    fn skips_deleted_and_empty_entries() {
        let seg = build_segment(&[(1, b"live"), (3, b"deleted"), (4, b""), (1, b"alive")]);
        let parsed = read_segb(&seg).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[1].data, b"alive");
        assert_eq!(parsed.deleted, 2);
        assert_eq!(parsed.anomalies, 0);
    }

    #[test]
    fn skips_crc_mismatch() {
        let mut seg = build_segment(&[(1, b"good"), (1, b"flip")]);
        // Corrupt the last byte of the second record's data ("flip" sits
        // right against the trailer — 4 bytes, no padding).
        let n = seg.len() - 2 * TRAILER_ENTRY_LEN - 1;
        seg[n] ^= 0xff;
        let parsed = read_segb(&seg).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].data, b"good");
        assert_eq!(parsed.anomalies, 1);
    }

    #[test]
    fn rejects_bad_magic_and_impossible_counts() {
        assert!(read_segb(b"shrt").is_err());
        let mut seg = build_segment(&[(1, b"data")]);
        seg[0] = b'X';
        assert!(read_segb(&seg).is_err());
        // Entry count larger than the file can hold.
        let mut seg = build_segment(&[(1, b"data")]);
        seg[4..8].copy_from_slice(&1_000_000i32.to_le_bytes());
        assert!(read_segb(&seg).is_err());
    }

    #[test]
    fn survives_inconsistent_trailer_offsets() {
        let mut seg = build_segment(&[(1, b"ok"), (1, b"second")]);
        // Point the second trailer entry before the first — impossible.
        let t = seg.len() - TRAILER_ENTRY_LEN;
        seg[t..t + 4].copy_from_slice(&1i32.to_le_bytes());
        let parsed = read_segb(&seg).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.anomalies, 1);
    }

    #[test]
    fn proto_walks_all_wire_types_and_unknown_fields() {
        // field 1: varint 5; field 2: fixed64 (f64 2.5); field 3: "hi";
        // field 200: varint 1 (two-byte tag); field 5: fixed32.
        let mut buf = vec![0x08, 0x05];
        buf.extend_from_slice(&[0x11]);
        buf.extend_from_slice(&2.5f64.to_le_bytes());
        buf.extend_from_slice(&[0x1a, 0x02, b'h', b'i']);
        buf.extend_from_slice(&[0xc0, 0x0c, 0x01]); // (200<<3)|0 = 1600 varint
        buf.extend_from_slice(&[0x2d, 1, 0, 0, 0]);
        let fields = proto_fields(&buf).unwrap();
        assert_eq!(fields[0], (1, ProtoValue::Varint(5)));
        assert_eq!(fields[1], (2, ProtoValue::Fixed64(2.5f64.to_bits())));
        assert_eq!(fields[2], (3, ProtoValue::Bytes(b"hi")));
        assert_eq!(fields[3], (200, ProtoValue::Varint(1)));
        assert_eq!(fields[4], (5, ProtoValue::Fixed32(1)));
    }

    #[test]
    fn proto_rejects_garbage() {
        assert!(proto_fields(&[0x00, 0x01]).is_none()); // field 0
        assert!(proto_fields(&[0x0a, 0x10, b'x']).is_none()); // truncated bytes
        assert!(proto_fields(&[0x0c]).is_none()); // wire type 4 (group)
        assert!(proto_fields(&[0x08]).is_none()); // truncated varint
        assert!(proto_fields(&[0x08, 0xff]).is_none()); // unterminated varint
    }

    /// The committed fixture is a real iPhone App.InFocus segment captured
    /// 2026-06-11 (13 days of data, macOS 26.5.1 host), sanitized in place:
    /// every live record's field-1 scene string was overwritten with 'x's of
    /// the same length and its CRC recomputed, so framing, offsets, counts,
    /// timestamps, and bundle ids are all untouched real data. The counts
    /// pin the format assumptions — if a macOS update changes SEGB framing,
    /// this fails loudly instead of the collector silently dropping data.
    #[test]
    fn parses_real_iphone_segment_fixture() {
        // The fixture is derived from a real device and is not shipped in the
        // public repository; the test is a no-op without it.
        let Ok(bytes) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/infocus-iphone.segb"
        )) else {
            eprintln!("skipping: tests/fixtures/infocus-iphone.segb not present");
            return;
        };
        let bytes: &[u8] = &bytes;
        let parsed = read_segb(bytes).unwrap();
        // 5353 trailer entries: 5330 live, 22 deleted (state 3), 1 empty
        // (state 4). The spike's regex scan found 4,484 of these — the
        // framing walk recovers all 5,330 (the regex missed timestamps
        // containing newline bytes).
        assert_eq!(parsed.entries.len(), 5330);
        assert_eq!(parsed.deleted, 23, "22 deleted + 1 empty slot");
        assert_eq!(parsed.anomalies, 0);
        // Every live record is valid protobuf.
        assert!(parsed.entries.iter().all(|e| proto_fields(&e.data).is_some()));
    }
}
