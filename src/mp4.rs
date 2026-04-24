//! Minimal MP4 container walk — only the subset required to locate the
//! `stbl` of the AAC audio track and to sanity-check that a byte blob is an
//! MP4/M4A file.
//!
//! Ported from mp3rgain `src/mp4meta.rs` (MIT), then narrowed to the pieces
//! needed for AAC sample lookup plus one ffprobe-visible gain metadata tag.

use std::fs::File;
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
const UDTA: u32 = u32::from_be_bytes(*b"udta");
const META: u32 = u32::from_be_bytes(*b"meta");
const HDLR: u32 = u32::from_be_bytes(*b"hdlr");
const ILST: u32 = u32::from_be_bytes(*b"ilst");
const DATA: u32 = u32::from_be_bytes(*b"data");
const DESC: u32 = u32::from_be_bytes(*b"desc");

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

/// Record the gain operation in `moov/udta/meta/ilst/desc` so ffprobe exposes
/// it as `TAG:description`.
pub(crate) fn write_gain_metadata(file: &mut File, gain_steps: i32) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    let (moov_pos, moov_header) = find_box(&data, MOOV)
        .ok_or_else(|| invalid_data("cannot write gain metadata: no moov box"))?;
    if moov_header.header_size != 8 {
        return Err(invalid_data(
            "cannot write gain metadata: extended-size moov box not supported",
        ));
    }

    let moov_size = checked_usize(moov_header.size, "moov box too large")?;
    let moov_end = checked_add(moov_pos, moov_size, "moov box range overflow")?;
    if moov_end > data.len() {
        return Err(invalid_data("cannot write gain metadata: moov box truncated"));
    }

    let old_moov = &data[moov_pos..moov_end];
    let old_moov_end = moov_end as u64;
    let mut new_moov = with_gain_description(old_moov, gain_steps)?;
    let delta = new_moov.len() as i64 - old_moov.len() as i64;
    adjust_chunk_offsets_after(&mut new_moov, old_moov_end, delta)?;

    data.splice(moov_pos..moov_end, new_moov);

    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(&data)?;
    file.flush()
}

fn with_gain_description(moov: &[u8], gain_steps: i32) -> std::io::Result<Vec<u8>> {
    let payload = format!(
        "m4againpy version=1 gain_steps={gain_steps} gain_step_db={}",
        crate::GAIN_STEP_DB
    );
    let item = text_ilst_item(DESC, payload.as_bytes())?;

    let header = read_box_header(moov, 0)?;
    let content_start = header.header_size as usize;
    let mut out = moov.to_vec();

    if let Some((udta_pos, udta_header)) =
        find_box_in_container(moov, content_start, moov.len() - content_start, UDTA)
    {
        let udta_size = checked_usize(udta_header.size, "udta box too large")?;
        let udta_end = checked_add(udta_pos, udta_size, "udta box range overflow")?;
        let new_udta = with_metadata_in_udta(&moov[udta_pos..udta_end], &item)?;
        out.splice(udta_pos..udta_end, new_udta);
    } else {
        let udta = make_box(UDTA, &make_meta_box(&item)?)?;
        out.extend_from_slice(&udta);
    }

    rewrite_box_size(&mut out, 0)?;
    Ok(out)
}

fn with_metadata_in_udta(udta: &[u8], item: &[u8]) -> std::io::Result<Vec<u8>> {
    let header = read_box_header(udta, 0)?;
    let content_start = header.header_size as usize;
    let mut out = udta.to_vec();

    if let Some((meta_pos, meta_header)) =
        find_box_in_container(udta, content_start, udta.len() - content_start, META)
    {
        let meta_size = checked_usize(meta_header.size, "meta box too large")?;
        let meta_end = checked_add(meta_pos, meta_size, "meta box range overflow")?;
        let new_meta = with_metadata_in_meta(&udta[meta_pos..meta_end], item)?;
        out.splice(meta_pos..meta_end, new_meta);
    } else {
        out.extend_from_slice(&make_meta_box(item)?);
    }

    rewrite_box_size(&mut out, 0)?;
    Ok(out)
}

fn with_metadata_in_meta(meta: &[u8], item: &[u8]) -> std::io::Result<Vec<u8>> {
    let header = read_box_header(meta, 0)?;
    let child_start = header.header_size as usize + 4;
    if child_start > meta.len() {
        return Err(invalid_data("meta box too short"));
    }

    let mut out = meta.to_vec();
    if let Some((ilst_pos, ilst_header)) =
        find_box_in_container(meta, child_start, meta.len() - child_start, ILST)
    {
        let ilst_size = checked_usize(ilst_header.size, "ilst box too large")?;
        let ilst_end = checked_add(ilst_pos, ilst_size, "ilst box range overflow")?;
        let new_ilst = with_item_in_ilst(&meta[ilst_pos..ilst_end], item)?;
        out.splice(ilst_pos..ilst_end, new_ilst);
    } else {
        out.extend_from_slice(&make_box(ILST, item)?);
    }

    rewrite_box_size(&mut out, 0)?;
    Ok(out)
}

fn with_item_in_ilst(ilst: &[u8], item: &[u8]) -> std::io::Result<Vec<u8>> {
    let header = read_box_header(ilst, 0)?;
    let content_start = header.header_size as usize;
    let mut out = ilst.to_vec();

    if let Some((desc_pos, desc_header)) =
        find_box_in_container(ilst, content_start, ilst.len() - content_start, DESC)
    {
        let desc_size = checked_usize(desc_header.size, "desc item too large")?;
        let desc_end = checked_add(desc_pos, desc_size, "desc item range overflow")?;
        out.splice(desc_pos..desc_end, item.iter().copied());
    } else {
        out.extend_from_slice(item);
    }

    rewrite_box_size(&mut out, 0)?;
    Ok(out)
}

fn make_meta_box(item: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&[0, 0, 0, 0]); // version/flags
    payload.extend_from_slice(&make_hdlr_box()?);
    payload.extend_from_slice(&make_box(ILST, item)?);
    make_box(META, &payload)
}

fn make_hdlr_box() -> std::io::Result<Vec<u8>> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&[0, 0, 0, 0]); // version/flags
    payload.extend_from_slice(&[0, 0, 0, 0]); // pre-defined
    payload.extend_from_slice(b"mdir");
    payload.extend_from_slice(&[0, 0, 0, 0]); // reserved
    payload.extend_from_slice(&[0, 0, 0, 0]); // reserved
    payload.extend_from_slice(&[0, 0, 0, 0]); // reserved
    payload.push(0); // empty name
    make_box(HDLR, &payload)
}

fn text_ilst_item(item_type: u32, text: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut data_payload = Vec::new();
    data_payload.extend_from_slice(&1u32.to_be_bytes()); // UTF-8
    data_payload.extend_from_slice(&0u32.to_be_bytes()); // locale
    data_payload.extend_from_slice(text);

    make_box(item_type, &make_box(DATA, &data_payload)?)
}

fn adjust_chunk_offsets_after(
    moov: &mut [u8],
    threshold: u64,
    delta: i64,
) -> std::io::Result<()> {
    if delta == 0 {
        return Ok(());
    }

    let header = read_box_header(moov, 0)?;
    let content_start = header.header_size as usize;
    adjust_chunk_offsets_in_container(moov, content_start, moov.len(), threshold, delta)
}

fn adjust_chunk_offsets_in_container(
    data: &mut [u8],
    start: usize,
    end: usize,
    threshold: u64,
    delta: i64,
) -> std::io::Result<()> {
    let mut pos = start;
    while pos + 8 <= end {
        let header = read_box_header(data, pos)?;
        let size = checked_usize(header.size, "MP4 box too large")?;
        let box_end = checked_add(pos, size, "MP4 box range overflow")?;
        if size < header.header_size as usize || box_end > end {
            return Err(invalid_data("invalid MP4 box size"));
        }

        match header.box_type {
            STCO => adjust_stco_offsets(data, pos, box_end, threshold, delta)?,
            CO64 => adjust_co64_offsets(data, pos, box_end, threshold, delta)?,
            _ if is_container_box(header.box_type) => {
                let child_start = pos + header.header_size as usize;
                let child_start = if header.box_type == META {
                    child_start + 4
                } else {
                    child_start
                };
                if child_start <= box_end {
                    adjust_chunk_offsets_in_container(data, child_start, box_end, threshold, delta)?;
                }
            }
            _ => {}
        }

        pos = box_end;
    }
    Ok(())
}

fn adjust_stco_offsets(
    data: &mut [u8],
    pos: usize,
    end: usize,
    threshold: u64,
    delta: i64,
) -> std::io::Result<()> {
    let count_pos = pos + 12;
    if count_pos + 4 > end {
        return Err(invalid_data("stco box too short"));
    }
    let count = read_u32_at(data, count_pos)? as usize;
    let entries_start = count_pos + 4;
    if entries_start + count * 4 > end {
        return Err(invalid_data("stco entries truncated"));
    }

    for idx in 0..count {
        let off = entries_start + idx * 4;
        let value = read_u32_at(data, off)? as u64;
        if value >= threshold {
            let adjusted = checked_adjust_offset(value, delta)?;
            let adjusted_u32 = u32::try_from(adjusted)
                .map_err(|_| invalid_data("adjusted stco offset does not fit in u32"))?;
            data[off..off + 4].copy_from_slice(&adjusted_u32.to_be_bytes());
        }
    }
    Ok(())
}

fn adjust_co64_offsets(
    data: &mut [u8],
    pos: usize,
    end: usize,
    threshold: u64,
    delta: i64,
) -> std::io::Result<()> {
    let count_pos = pos + 12;
    if count_pos + 4 > end {
        return Err(invalid_data("co64 box too short"));
    }
    let count = read_u32_at(data, count_pos)? as usize;
    let entries_start = count_pos + 4;
    if entries_start + count * 8 > end {
        return Err(invalid_data("co64 entries truncated"));
    }

    for idx in 0..count {
        let off = entries_start + idx * 8;
        let value = read_u64_at(data, off)?;
        if value >= threshold {
            let adjusted = checked_adjust_offset(value, delta)?;
            data[off..off + 8].copy_from_slice(&adjusted.to_be_bytes());
        }
    }
    Ok(())
}

fn is_container_box(box_type: u32) -> bool {
    matches!(box_type, MOOV | TRAK | MDIA | MINF | STBL | UDTA | META)
}

fn make_box(box_type: u32, payload: &[u8]) -> std::io::Result<Vec<u8>> {
    let size = 8usize
        .checked_add(payload.len())
        .ok_or_else(|| invalid_data("MP4 box size overflow"))?;
    let size_u32 = u32::try_from(size).map_err(|_| invalid_data("MP4 box too large"))?;

    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&size_u32.to_be_bytes());
    out.extend_from_slice(&box_type.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

fn rewrite_box_size(data: &mut [u8], pos: usize) -> std::io::Result<()> {
    let size = data
        .len()
        .checked_sub(pos)
        .ok_or_else(|| invalid_data("MP4 box size underflow"))?;
    let size_u32 = u32::try_from(size).map_err(|_| invalid_data("MP4 box too large"))?;
    data[pos..pos + 4].copy_from_slice(&size_u32.to_be_bytes());
    Ok(())
}

fn read_box_header(data: &[u8], pos: usize) -> std::io::Result<BoxHeader> {
    let mut cursor = Cursor::new(&data[pos..]);
    BoxHeader::read(&mut cursor)?.ok_or_else(|| invalid_data("missing MP4 box header"))
}

fn read_u32_at(data: &[u8], pos: usize) -> std::io::Result<u32> {
    if pos + 4 > data.len() {
        return Err(invalid_data("u32 read past end of data"));
    }
    Ok(u32::from_be_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

fn read_u64_at(data: &[u8], pos: usize) -> std::io::Result<u64> {
    if pos + 8 > data.len() {
        return Err(invalid_data("u64 read past end of data"));
    }
    Ok(u64::from_be_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
        data[pos + 4],
        data[pos + 5],
        data[pos + 6],
        data[pos + 7],
    ]))
}

fn checked_adjust_offset(value: u64, delta: i64) -> std::io::Result<u64> {
    if delta >= 0 {
        value
            .checked_add(delta as u64)
            .ok_or_else(|| invalid_data("chunk offset overflow"))
    } else {
        value
            .checked_sub(delta.unsigned_abs())
            .ok_or_else(|| invalid_data("chunk offset underflow"))
    }
}

fn checked_add(a: usize, b: usize, message: &'static str) -> std::io::Result<usize> {
    a.checked_add(b).ok_or_else(|| invalid_data(message))
}

fn checked_usize(value: u64, message: &'static str) -> std::io::Result<usize> {
    usize::try_from(value).map_err(|_| invalid_data(message))
}

fn invalid_data(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}
