//! Minimal MP4 container walk — only the subset required to locate the
//! `stbl` of the AAC audio track and to sanity-check that a byte blob is an
//! MP4/M4A file.
//!
//! Ported from mp3rgain `src/mp4meta.rs` (MIT). Everything related to
//! iTunes/APE metadata, ReplayGain, undo tags, and track-atom rewriting has
//! been stripped.

use std::io::{Cursor, Read, Seek, SeekFrom, Write};

// Four-cc constants used by the AAC walk.
pub(crate) const MOOV: u32 = u32::from_be_bytes(*b"moov");
pub(crate) const TRAK: u32 = u32::from_be_bytes(*b"trak");
pub(crate) const MDIA: u32 = u32::from_be_bytes(*b"mdia");
pub(crate) const MINF: u32 = u32::from_be_bytes(*b"minf");
pub(crate) const STBL: u32 = u32::from_be_bytes(*b"stbl");
pub(crate) const STSD: u32 = u32::from_be_bytes(*b"stsd");
pub(crate) const STCO: u32 = u32::from_be_bytes(*b"stco");
pub(crate) const CO64: u32 = u32::from_be_bytes(*b"co64");
pub(crate) const MP4A: u32 = u32::from_be_bytes(*b"mp4a");
const UUID: u32 = u32::from_be_bytes(*b"uuid");
const MP4GAINPY_GAIN_UUID: [u8; 16] = [
    0x95, 0xa5, 0x87, 0x70, 0x4b, 0xa7, 0x42, 0xee, 0x9e, 0x88, 0x34, 0x0e, 0x58, 0xbf, 0x35, 0x80,
];

#[derive(Debug, Clone)]
pub(crate) struct BoxHeader {
    pub(crate) size: u64,
    pub(crate) box_type: u32,
    pub(crate) header_size: u8, // 8 for normal, 16 for extended size
}

impl BoxHeader {
    pub(crate) fn read<R: Read>(reader: &mut R) -> std::io::Result<Option<Self>> {
        let mut buf = [0u8; 8];
        match reader.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }

        let size = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let box_type = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

        let (size, header_size) = if size == 1 {
            let mut ext_buf = [0u8; 8];
            reader.read_exact(&mut ext_buf)?;
            (u64::from_be_bytes(ext_buf), 16)
        } else if size == 0 {
            (0, 8) // extends to EOF
        } else {
            (size as u64, 8)
        };

        Ok(Some(BoxHeader {
            size,
            box_type,
            header_size,
        }))
    }

    pub(crate) fn content_size(&self) -> u64 {
        if self.size == 0 {
            0
        } else {
            self.size - self.header_size as u64
        }
    }
}

/// Find the first top-level box of a given type.
pub(crate) fn find_box(data: &[u8], box_type: u32) -> Option<(usize, BoxHeader)> {
    let mut cursor = Cursor::new(data);

    while let Ok(Some(header)) = BoxHeader::read(&mut cursor) {
        let pos = cursor.position() as usize - header.header_size as usize;

        if header.box_type == box_type {
            return Some((pos, header));
        }

        if header.size == 0 {
            break;
        }

        let next_pos = pos as u64 + header.size;
        if next_pos >= data.len() as u64 {
            break;
        }
        cursor.set_position(next_pos);
    }

    None
}

/// Find a box within a specific container range.
pub(crate) fn find_box_in_container(
    data: &[u8],
    container_start: usize,
    container_size: usize,
    box_type: u32,
) -> Option<(usize, BoxHeader)> {
    let container_end = container_start + container_size;
    let mut pos = container_start;

    while pos + 8 <= container_end {
        let mut cursor = Cursor::new(&data[pos..]);
        if let Ok(Some(header)) = BoxHeader::read(&mut cursor) {
            if header.box_type == box_type {
                return Some((pos, header));
            }

            if header.size == 0 {
                break;
            }

            pos += header.size as usize;
        } else {
            break;
        }
    }

    None
}

fn is_accepted_brand(brand: &[u8]) -> bool {
    matches!(
        brand,
        b"M4A " | b"M4B " | b"M4V " | b"mp41" | b"mp42" | b"isom" | b"iso2"
    )
}

/// Check whether a byte blob looks like an MP4/M4A file by inspecting its
/// `ftyp` box (major brand + compatible brands). Only reads the first 128
/// bytes — enough for a typical ftyp.
pub(crate) fn is_mp4(data: &[u8]) -> bool {
    let bytes_read = data.len().min(128);
    if bytes_read < 12 {
        return false;
    }
    let buf = &data[..bytes_read];
    let size = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if &buf[4..8] != b"ftyp" || size < 12 {
        return false;
    }
    let check_end = size.min(bytes_read);
    let mut offset = 8;
    while offset + 4 <= check_end {
        if is_accepted_brand(&buf[offset..offset + 4]) {
            return true;
        }
        offset = if offset == 8 { 16 } else { offset + 4 };
    }
    false
}

/// Append a small top-level `uuid` box recording the gain operation.
///
/// This deliberately avoids editing `moov/ilst`, because growing `moov` can
/// require shifting media data and rewriting chunk offsets on some files.
pub(crate) fn append_gain_metadata<W: Seek + Write>(
    writer: &mut W,
    gain_steps: i32,
) -> std::io::Result<()> {
    let payload = format!(
        "mp4gainpy\nversion=1\ngain_steps={gain_steps}\ngain_step_db={}\n",
        crate::GAIN_STEP_DB
    );
    let box_size = 8usize + MP4GAINPY_GAIN_UUID.len() + payload.len();

    writer.seek(SeekFrom::End(0))?;
    writer.write_all(&(box_size as u32).to_be_bytes())?;
    writer.write_all(&UUID.to_be_bytes())?;
    writer.write_all(&MP4GAINPY_GAIN_UUID)?;
    writer.write_all(payload.as_bytes())?;
    writer.flush()
}
