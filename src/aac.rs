//! AAC bitstream parser + in-place `global_gain` rewriter for M4A/MP4 files.
//!
//! Ported from mp3rgain `src/aac.rs` (MIT). Stripped of the public
//! `AacAnalysis`, undo-tag plumbing, and the file-based entry points —
//! the Python-facing API. The bytes path runs on `&[u8]` / `&mut [u8]`; the
//! file path reads MP4 metadata and AAC samples incrementally.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::OnceLock;

use crate::aac_codebooks;
use crate::bits::{adjust_gain_value, read_bits_u8, write_bits_u8};
use crate::error::{Error, Result};
use crate::mp4;

// ---------------------------------------------------------------------------
// Public (crate-internal) entry points
// ---------------------------------------------------------------------------

/// Apply gain to an AAC/M4A byte buffer, in-place. Returns the number of
/// `global_gain` locations that were actually modified.
///
/// Callers must pass a non-zero `gain_steps`; the public API enforces this.
/// Locations whose current `global_gain` is 0 (silence) are skipped.
/// Saturating clamp at 0..=255.
pub(crate) fn apply_gain_to_bytes(data: &mut [u8], gain_steps: i32) -> Result<usize> {
    let locations = analyze_locations(data)?;
    Ok(apply_gain_to_data(data, &locations, gain_steps))
}

pub(crate) struct AacGainPlan {
    locations: Vec<AacGainLocation>,
}

pub(crate) fn analyze_file(src: &mut File) -> Result<AacGainPlan> {
    Ok(AacGainPlan {
        locations: analyze_locations_in_file(src)?,
    })
}

/// Apply gain to a source file while keeping memory bounded by metadata and
/// one patch chunk. The source is copied to `dst`, then only chunks containing
/// located `global_gain` bits are patched in `dst`.
pub(crate) fn apply_gain_plan_to_file(
    src: &mut File,
    dst: &mut File,
    plan: &AacGainPlan,
    gain_steps: i32,
) -> Result<usize> {
    src.seek(SeekFrom::Start(0))?;
    dst.seek(SeekFrom::Start(0))?;
    std::io::copy(src, dst)?;

    let modified = apply_gain_to_file_data(dst, &plan.locations, gain_steps)?;
    dst.flush()?;
    Ok(modified)
}

fn analyze_locations(data: &[u8]) -> Result<Vec<AacGainLocation>> {
    if !mp4::is_mp4(data) {
        return Err(Error::NotMp4);
    }

    let (sample_table, stsd_pos) = build_sample_table(data)?;
    let sample_rate = parse_audio_config(data, stsd_pos)?;

    let mut all_locations = Vec::new();
    let mut parse_warnings = 0u32;

    for (idx, entry) in sample_table.iter().enumerate() {
        let sample_start = entry.file_offset as usize;
        let sample_end = sample_start + entry.size as usize;

        if sample_end > data.len() {
            parse_warnings += 1;
            continue;
        }

        let sample_data = &data[sample_start..sample_end];
        let mut reader = BitReader::new(sample_data);

        match parse_raw_data_block(&mut reader, sample_rate) {
            Ok(locations) => {
                for mut loc in locations {
                    loc.sample_index = idx as u32;
                    loc.file_offset = entry.file_offset + loc.sample_byte_offset as u64;
                    all_locations.push(loc);
                }
            }
            Err(_) => {
                parse_warnings += 1;
            }
        }
    }

    if all_locations.is_empty() && parse_warnings > 0 {
        return Err(Error::AacParseFailure {
            warnings: parse_warnings,
        });
    }

    Ok(all_locations)
}

fn analyze_locations_in_file(file: &mut File) -> Result<Vec<AacGainLocation>> {
    if !is_mp4_file(file)? {
        return Err(Error::NotMp4);
    }

    let moov_data = read_top_level_box(file, mp4::MOOV)?.ok_or(Error::NoMoovBox)?;
    let (sample_table, stsd_pos) = build_sample_table(&moov_data)?;
    let sample_rate = parse_audio_config(&moov_data, stsd_pos)?;
    let file_len = file.seek(SeekFrom::End(0))?;

    let mut all_locations = Vec::new();
    let mut parse_warnings = 0u32;
    let mut sample_data = Vec::new();

    for (idx, entry) in sample_table.iter().enumerate() {
        let sample_end = match entry.file_offset.checked_add(entry.size as u64) {
            Some(end) => end,
            None => {
                parse_warnings += 1;
                continue;
            }
        };

        if sample_end > file_len {
            parse_warnings += 1;
            continue;
        }

        sample_data.resize(entry.size as usize, 0);
        file.seek(SeekFrom::Start(entry.file_offset))?;
        file.read_exact(&mut sample_data)?;

        let mut reader = BitReader::new(&sample_data);

        match parse_raw_data_block(&mut reader, sample_rate) {
            Ok(locations) => {
                for mut loc in locations {
                    loc.sample_index = idx as u32;
                    loc.file_offset = entry.file_offset + loc.sample_byte_offset as u64;
                    all_locations.push(loc);
                }
            }
            Err(_) => {
                parse_warnings += 1;
            }
        }
    }

    if all_locations.is_empty() && parse_warnings > 0 {
        return Err(Error::AacParseFailure {
            warnings: parse_warnings,
        });
    }

    Ok(all_locations)
}

fn is_mp4_file(file: &mut File) -> Result<bool> {
    file.seek(SeekFrom::Start(0))?;

    let mut prefix = Vec::with_capacity(128);
    {
        let mut limited = file.take(128);
        limited.read_to_end(&mut prefix)?;
    }

    file.seek(SeekFrom::Start(0))?;
    Ok(mp4::is_mp4(&prefix))
}

fn read_top_level_box(file: &mut File, box_type: u32) -> Result<Option<Vec<u8>>> {
    let file_len = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;

    while file.stream_position()? + 8 <= file_len {
        let pos = file.stream_position()?;
        let header = match mp4::BoxHeader::read(file)? {
            Some(header) => header,
            None => break,
        };
        let size = effective_box_size(&header, pos, file_len)?;

        if header.box_type == box_type {
            let size_usize = usize::try_from(size).map_err(|_| Error::AacParse {
                message: "MP4 box too large to read".into(),
            })?;
            file.seek(SeekFrom::Start(pos))?;
            let mut data = vec![0u8; size_usize];
            file.read_exact(&mut data)?;

            if header.size == 0 {
                let size_u32 = u32::try_from(size).map_err(|_| Error::AacParse {
                    message: "size-0 MP4 box too large to normalize".into(),
                })?;
                data[0..4].copy_from_slice(&size_u32.to_be_bytes());
            }

            return Ok(Some(data));
        }

        file.seek(SeekFrom::Start(pos + size))?;
    }

    Ok(None)
}

fn effective_box_size(header: &mp4::BoxHeader, pos: u64, file_len: u64) -> Result<u64> {
    let size = if header.size == 0 {
        file_len.saturating_sub(pos)
    } else {
        header.size
    };

    if size < header.header_size as u64 {
        return Err(Error::AacParse {
            message: "invalid MP4 box size".into(),
        });
    }

    let end = pos.checked_add(size).ok_or_else(|| Error::AacParse {
        message: "MP4 box size overflow".into(),
    })?;
    if end > file_len {
        return Err(Error::AacParse {
            message: "MP4 box extends past end of file".into(),
        });
    }

    Ok(size)
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AacGainLocation {
    sample_index: u32,
    file_offset: u64,
    sample_byte_offset: u32,
    bit_offset: u8,
    #[allow(dead_code)]
    channel: u8,
    original_gain: u8,
}

impl AacGainLocation {
    fn new(
        sample_index: u32,
        file_offset: u64,
        sample_byte_offset: u32,
        bit_offset: u8,
        channel: u8,
        original_gain: u8,
    ) -> Self {
        Self {
            sample_index,
            file_offset,
            sample_byte_offset,
            bit_offset,
            channel,
            original_gain,
        }
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ID_SCE: u32 = 0;
const ID_CPE: u32 = 1;
const ID_CCE: u32 = 2;
const ID_LFE: u32 = 3;
const ID_DSE: u32 = 4;
const ID_PCE: u32 = 5;
const ID_FIL: u32 = 6;
const ID_END: u32 = 7;

const ZERO_HCB: u8 = 0;
const NOISE_HCB: u8 = 13;
const INTENSITY_HCB2: u8 = 14;
const INTENSITY_HCB: u8 = 15;
const ESC_HCB: u8 = 11;

const EIGHT_SHORT_SEQUENCE: u8 = 2;

const MAX_SFBS: usize = 64;
const MAX_WINDOWS: usize = 8;

// MP4 box types used locally by the AAC walk (not in crate::mp4 because they
// aren't needed elsewhere).
const STSC: u32 = u32::from_be_bytes(*b"stsc");
const STSZ: u32 = u32::from_be_bytes(*b"stsz");
const ESDS: u32 = u32::from_be_bytes(*b"esds");

// ---------------------------------------------------------------------------
// BitReader
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    fn position(&self) -> (usize, u8) {
        (self.byte_pos, self.bit_pos)
    }

    fn bits_remaining(&self) -> usize {
        let remaining_bytes = self.data.len().saturating_sub(self.byte_pos);
        let total_bits = remaining_bytes.saturating_mul(8);
        total_bits.saturating_sub(self.bit_pos as usize)
    }

    fn read_bits(&mut self, n: u8) -> Result<u32> {
        let bits_to_read = n as usize;
        if bits_to_read == 0 {
            return Ok(0);
        }

        let value = self.peek_bits(n)?;
        self.advance_bits(bits_to_read);

        Ok(value)
    }

    fn peek_bits(&self, n: u8) -> Result<u32> {
        let bits_to_read = n as usize;
        if bits_to_read == 0 {
            return Ok(0);
        }

        if self.bits_remaining() < bits_to_read {
            return Err(Error::AacParse {
                message: "unexpected end of bitstream".into(),
            });
        }

        let bytes_needed = (self.bit_pos as usize + bits_to_read).div_ceil(8);
        let mut window = 0u64;
        for byte in &self.data[self.byte_pos..self.byte_pos + bytes_needed] {
            window = (window << 8) | u64::from(*byte);
        }

        let window_bits = bytes_needed * 8;
        let shift = window_bits - self.bit_pos as usize - bits_to_read;
        let mask = if n == 32 {
            u64::from(u32::MAX)
        } else {
            (1u64 << bits_to_read) - 1
        };
        Ok(((window >> shift) & mask) as u32)
    }

    fn advance_bits(&mut self, bits_to_advance: usize) {
        let next_bit = self.byte_pos * 8 + self.bit_pos as usize + bits_to_advance;
        self.byte_pos = next_bit / 8;
        self.bit_pos = (next_bit % 8) as u8;
    }

    fn read_bit(&mut self) -> Result<bool> {
        if self.byte_pos >= self.data.len() {
            return Err(Error::AacParse {
                message: "unexpected end of bitstream".into(),
            });
        }

        let bit = ((self.data[self.byte_pos] >> (7 - self.bit_pos)) & 1) != 0;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Ok(bit)
    }

    fn skip_bits(&mut self, n: usize) -> Result<()> {
        let total_bits = self.byte_pos * 8 + self.bit_pos as usize + n;
        self.byte_pos = total_bits / 8;
        self.bit_pos = (total_bits % 8) as u8;
        if self.byte_pos > self.data.len() || (self.byte_pos == self.data.len() && self.bit_pos > 0)
        {
            return Err(Error::AacParse {
                message: "unexpected end of bitstream".into(),
            });
        }
        Ok(())
    }

    fn byte_align(&mut self) {
        if self.bit_pos > 0 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Huffman decoder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct HuffmanEntry {
    symbol: u16,
    len: u8,
}

struct HuffmanTable {
    lens: &'static [u8],
    codes: &'static [u32],
    max_len: u8,
    entries: Vec<HuffmanEntry>,
}

impl HuffmanTable {
    fn new(lens: &'static [u8], codes: &'static [u32], max_len: u8) -> Self {
        let size = 1usize << max_len;
        let mut entries = vec![HuffmanEntry { symbol: 0, len: 0 }; size];

        for (symbol, (&len, &code)) in lens.iter().zip(codes.iter()).enumerate() {
            if len == 0 {
                continue;
            }

            let prefix = (code as usize) << (max_len - len);
            let fill = 1usize << (max_len - len);
            for slot in entries.iter_mut().skip(prefix).take(fill) {
                *slot = HuffmanEntry {
                    symbol: symbol as u16,
                    len,
                };
            }
        }

        Self {
            lens,
            codes,
            max_len,
            entries,
        }
    }
}

static SCF_HUFFMAN_TABLE: OnceLock<HuffmanTable> = OnceLock::new();
static SPECTRUM_HUFFMAN_TABLES: OnceLock<Vec<HuffmanTable>> = OnceLock::new();

fn scf_huffman_table() -> &'static HuffmanTable {
    SCF_HUFFMAN_TABLE.get_or_init(|| {
        HuffmanTable::new(
            &aac_codebooks::SCF_CB_LENS,
            &aac_codebooks::SCF_CB_CODES,
            aac_codebooks::SCF_CB_MAX_LEN,
        )
    })
}

fn spectrum_huffman_tables() -> &'static [HuffmanTable] {
    SPECTRUM_HUFFMAN_TABLES.get_or_init(|| {
        aac_codebooks::SPECTRUM_CODEBOOKS
            .iter()
            .map(|codebook| HuffmanTable::new(codebook.lens, codebook.codes, codebook.max_len))
            .collect()
    })
}

fn decode_huffman(reader: &mut BitReader, table: &HuffmanTable) -> Result<usize> {
    if reader.bits_remaining() >= table.max_len as usize {
        let bits = reader.peek_bits(table.max_len)? as usize;
        let entry = table.entries[bits];
        if entry.len != 0 {
            reader.advance_bits(entry.len as usize);
            return Ok(entry.symbol as usize);
        }
    }

    decode_huffman_slow(reader, table.lens, table.codes, table.max_len)
}

fn decode_huffman_slow(
    reader: &mut BitReader,
    lens: &[u8],
    codes: &[u32],
    max_len: u8,
) -> Result<usize> {
    let mut code: u32 = 0;
    let mut bits_read: u8 = 0;

    for _ in 0..max_len {
        code = (code << 1) | u32::from(reader.read_bit()?);
        bits_read += 1;

        for (i, (&len, &cw)) in lens.iter().zip(codes.iter()).enumerate() {
            if len == bits_read && cw == code {
                return Ok(i);
            }
        }
    }

    Err(Error::AacParse {
        message: "invalid Huffman code".into(),
    })
}

// ---------------------------------------------------------------------------
// MP4 sample table parser
// ---------------------------------------------------------------------------

struct SampleEntry {
    file_offset: u64,
    size: u32,
}

fn read_u32_be(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn read_u64_be(data: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ])
}

fn build_sample_table(data: &[u8]) -> Result<(Vec<SampleEntry>, usize)> {
    let (moov_pos, moov_header) = mp4::find_box(data, mp4::MOOV).ok_or(Error::NoMoovBox)?;
    let moov_start = moov_pos + moov_header.header_size as usize;
    let moov_size = moov_header.content_size() as usize;

    let (stbl_start, stbl_size, stsd_pos) = find_audio_stbl(data, moov_start, moov_size)?;

    // STSZ
    let (stsz_pos, stsz_header) = mp4::find_box_in_container(data, stbl_start, stbl_size, STSZ)
        .ok_or_else(|| Error::AacParse {
            message: "no stsz box".into(),
        })?;
    let stsz_content = stsz_pos + stsz_header.header_size as usize;
    let _version = read_u32_be(data, stsz_content);
    let default_size = read_u32_be(data, stsz_content + 4);
    let sample_count = read_u32_be(data, stsz_content + 8) as usize;

    let mut sample_sizes = Vec::with_capacity(sample_count);
    if default_size != 0 {
        sample_sizes.resize(sample_count, default_size);
    } else {
        let sizes_start = stsz_content + 12;
        for i in 0..sample_count {
            sample_sizes.push(read_u32_be(data, sizes_start + i * 4));
        }
    }

    // STSC
    let (stsc_pos, stsc_header) = mp4::find_box_in_container(data, stbl_start, stbl_size, STSC)
        .ok_or_else(|| Error::AacParse {
            message: "no stsc box".into(),
        })?;
    let stsc_content = stsc_pos + stsc_header.header_size as usize;
    let stsc_count = read_u32_be(data, stsc_content + 4) as usize;
    let stsc_entries_start = stsc_content + 8;

    struct StscEntry {
        first_chunk: u32,
        samples_per_chunk: u32,
    }
    let mut stsc_entries = Vec::with_capacity(stsc_count);
    for i in 0..stsc_count {
        let off = stsc_entries_start + i * 12;
        stsc_entries.push(StscEntry {
            first_chunk: read_u32_be(data, off),
            samples_per_chunk: read_u32_be(data, off + 4),
        });
    }

    // STCO / CO64
    let chunk_offsets = parse_chunk_offsets(data, stbl_start, stbl_size)?;

    let mut entries = Vec::with_capacity(sample_count);
    let mut sample_idx = 0usize;

    for (chunk_idx, &chunk_offset) in chunk_offsets.iter().enumerate() {
        let chunk_num = chunk_idx + 1;

        let samples_in_chunk = {
            let mut spc = stsc_entries[0].samples_per_chunk;
            for entry in &stsc_entries {
                if entry.first_chunk as usize <= chunk_num {
                    spc = entry.samples_per_chunk;
                } else {
                    break;
                }
            }
            spc as usize
        };

        let mut offset_in_chunk = 0u64;
        for _ in 0..samples_in_chunk {
            if sample_idx >= sample_count {
                break;
            }
            entries.push(SampleEntry {
                file_offset: chunk_offset + offset_in_chunk,
                size: sample_sizes[sample_idx],
            });
            offset_in_chunk += sample_sizes[sample_idx] as u64;
            sample_idx += 1;
        }
    }

    Ok((entries, stsd_pos))
}

fn find_audio_stbl(
    data: &[u8],
    moov_start: usize,
    moov_size: usize,
) -> Result<(usize, usize, usize)> {
    let mut search_pos = moov_start;
    let moov_end = moov_start + moov_size;

    while search_pos < moov_end {
        let (trak_pos, trak_header) =
            match mp4::find_box_in_container(data, search_pos, moov_end - search_pos, mp4::TRAK) {
                Some(x) => x,
                None => break,
            };

        if let Some(result) = find_aac_stbl_in_trak(data, &trak_header, trak_pos) {
            return Ok(result);
        }

        search_pos = trak_pos + trak_header.size as usize;
    }

    Err(Error::NoAacTrack)
}

fn find_aac_stbl_in_trak(
    data: &[u8],
    trak_header: &mp4::BoxHeader,
    trak_pos: usize,
) -> Option<(usize, usize, usize)> {
    let trak_start = trak_pos + trak_header.header_size as usize;
    let trak_size = trak_header.content_size() as usize;

    let (mdia_pos, mdia_h) = mp4::find_box_in_container(data, trak_start, trak_size, mp4::MDIA)?;
    let (minf_pos, minf_h) = mp4::find_box_in_container(
        data,
        mdia_pos + mdia_h.header_size as usize,
        mdia_h.content_size() as usize,
        mp4::MINF,
    )?;
    let (stbl_pos, stbl_h) = mp4::find_box_in_container(
        data,
        minf_pos + minf_h.header_size as usize,
        minf_h.content_size() as usize,
        mp4::STBL,
    )?;

    let stbl_start = stbl_pos + stbl_h.header_size as usize;
    let stbl_size = stbl_h.content_size() as usize;

    let (stsd_pos, stsd_h) = mp4::find_box_in_container(data, stbl_start, stbl_size, mp4::STSD)?;

    let entries_start = stsd_pos + stsd_h.header_size as usize + 8;
    if entries_start + 8 > data.len() {
        return None;
    }

    let entry_type = read_u32_be(data, entries_start + 4);
    if entry_type == mp4::MP4A {
        Some((stbl_start, stbl_size, stsd_pos))
    } else {
        None
    }
}

fn parse_chunk_offsets(data: &[u8], stbl_start: usize, stbl_size: usize) -> Result<Vec<u64>> {
    if let Some((stco_pos, stco_h)) =
        mp4::find_box_in_container(data, stbl_start, stbl_size, mp4::STCO)
    {
        let content = stco_pos + stco_h.header_size as usize;
        let count = read_u32_be(data, content + 4) as usize;
        let mut offsets = Vec::with_capacity(count);
        for i in 0..count {
            offsets.push(read_u32_be(data, content + 8 + i * 4) as u64);
        }
        return Ok(offsets);
    }

    if let Some((co64_pos, co64_h)) =
        mp4::find_box_in_container(data, stbl_start, stbl_size, mp4::CO64)
    {
        let content = co64_pos + co64_h.header_size as usize;
        let count = read_u32_be(data, content + 4) as usize;
        let mut offsets = Vec::with_capacity(count);
        for i in 0..count {
            offsets.push(read_u64_be(data, content + 8 + i * 8));
        }
        return Ok(offsets);
    }

    Err(Error::AacParse {
        message: "no stco or co64 box found".into(),
    })
}

// ---------------------------------------------------------------------------
// AudioSpecificConfig parser
// ---------------------------------------------------------------------------

const SAMPLE_RATE_TABLE: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

fn parse_audio_config(data: &[u8], stsd_pos: usize) -> Result<u32> {
    let stsd_header_end = stsd_pos + 8;
    let entries_start = stsd_header_end + 8;

    if entries_start + 4 > data.len() {
        return Err(Error::AacParse {
            message: "stsd too short".into(),
        });
    }
    let mp4a_size = read_u32_be(data, entries_start) as usize;
    let mp4a_start = entries_start;
    let mp4a_end = mp4a_start + mp4a_size;

    let sr_offset = mp4a_start + 32;
    if sr_offset + 4 > data.len() {
        return Err(Error::AacParse {
            message: "mp4a too short for sample rate".into(),
        });
    }
    let sr_fixed = read_u32_be(data, sr_offset);
    let sample_rate = sr_fixed >> 16;

    let esds_search_start = mp4a_start + 36;
    if esds_search_start < mp4a_end {
        if let Some(asc_sr) = parse_esds_sample_rate(data, esds_search_start, mp4a_end) {
            return Ok(asc_sr);
        }
    }

    Ok(sample_rate)
}

fn parse_esds_sample_rate(data: &[u8], start: usize, end: usize) -> Option<u32> {
    let (esds_pos, esds_h) = mp4::find_box_in_container(data, start, end - start, ESDS)?;
    let esds_content = esds_pos + esds_h.header_size as usize + 4;
    let esds_end = esds_pos + esds_h.size as usize;

    let asc_data = find_audio_specific_config(data, esds_content, esds_end)?;

    if asc_data.len() < 2 {
        return None;
    }

    let _aot = (asc_data[0] >> 3) & 0x1F;
    let sr_idx = ((asc_data[0] & 0x07) << 1) | (asc_data[1] >> 7);

    if sr_idx == 0x0F && asc_data.len() >= 5 {
        let freq = ((asc_data[1] as u32 & 0x7F) << 17)
            | ((asc_data[2] as u32) << 9)
            | ((asc_data[3] as u32) << 1)
            | ((asc_data[4] as u32) >> 7);
        return Some(freq);
    }

    if (sr_idx as usize) < SAMPLE_RATE_TABLE.len() {
        Some(SAMPLE_RATE_TABLE[sr_idx as usize])
    } else {
        None
    }
}

fn find_audio_specific_config(data: &[u8], start: usize, end: usize) -> Option<&[u8]> {
    let mut pos = start;

    if pos >= end || data[pos] != 3 {
        return None;
    }
    pos += 1;
    let (_len, consumed) = read_desc_length(data, pos, end)?;
    pos += consumed;
    pos += 3;

    if pos >= end || data[pos] != 4 {
        return None;
    }
    pos += 1;
    let (_len, consumed) = read_desc_length(data, pos, end)?;
    pos += consumed;
    pos += 13;

    if pos >= end || data[pos] != 5 {
        return None;
    }
    pos += 1;
    let (len, consumed) = read_desc_length(data, pos, end)?;
    pos += consumed;

    let asc_end = (pos + len).min(end);
    Some(&data[pos..asc_end])
}

fn read_desc_length(data: &[u8], start: usize, end: usize) -> Option<(usize, usize)> {
    let mut len = 0usize;
    let mut consumed = 0usize;
    let mut pos = start;

    loop {
        if pos >= end {
            return None;
        }
        let b = data[pos];
        pos += 1;
        consumed += 1;
        len = (len << 7) | (b & 0x7F) as usize;
        if b & 0x80 == 0 {
            break;
        }
        if consumed >= 4 {
            break;
        }
    }

    Some((len, consumed))
}

// ---------------------------------------------------------------------------
// AAC bitstream parsers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct IcsInfo {
    window_sequence: u8,
    max_sfb: usize,
    long_win: bool,
    window_groups: usize,
    window_group_len: [usize; MAX_WINDOWS],
}

fn parse_ics_info(reader: &mut BitReader) -> Result<IcsInfo> {
    let _reserved = reader.read_bits(1)?;
    let window_sequence = reader.read_bits(2)? as u8;
    let _window_shape = reader.read_bits(1)?;
    let long_win = window_sequence != EIGHT_SHORT_SEQUENCE;

    let (max_sfb, window_groups, window_group_len) = if !long_win {
        let max_sfb = reader.read_bits(4)? as usize;
        let scale_factor_grouping = reader.read_bits(7)? as u8;

        let mut groups = 1usize;
        let mut group_len = [0usize; MAX_WINDOWS];
        group_len[0] = 1;
        for i in 0..7 {
            if (scale_factor_grouping >> (6 - i)) & 1 == 0 {
                groups += 1;
                group_len[groups - 1] = 1;
            } else {
                group_len[groups - 1] += 1;
            }
        }
        (max_sfb, groups, group_len)
    } else {
        let max_sfb = reader.read_bits(6)? as usize;
        let predictor_data_present = reader.read_bit()?;
        if predictor_data_present {
            return Err(Error::AacParse {
                message: "predictor data not supported for AAC-LC".into(),
            });
        }
        let mut group_len = [0usize; MAX_WINDOWS];
        group_len[0] = 1;
        (max_sfb, 1, group_len)
    };

    Ok(IcsInfo {
        window_sequence,
        max_sfb,
        long_win,
        window_groups,
        window_group_len,
    })
}

struct SectionData {
    sfb_cb: [[u8; MAX_SFBS]; MAX_WINDOWS],
}

fn parse_section_data(reader: &mut BitReader, info: &IcsInfo) -> Result<SectionData> {
    let sect_bits = if info.long_win { 5u8 } else { 3u8 };
    let sect_esc_val = (1u32 << sect_bits) - 1;
    let mut sfb_cb = [[0u8; MAX_SFBS]; MAX_WINDOWS];

    for group_cb in sfb_cb.iter_mut().take(info.window_groups) {
        let mut k = 0usize;
        while k < info.max_sfb {
            let cb = reader.read_bits(4)? as u8;
            if cb == 12 {
                return Err(Error::AacParse {
                    message: "reserved codebook 12".into(),
                });
            }

            let mut sect_len = 0usize;
            loop {
                let incr = reader.read_bits(sect_bits)? as usize;
                sect_len += incr;
                if incr < sect_esc_val as usize {
                    break;
                }
            }

            if sect_len == 0 || k + sect_len > info.max_sfb {
                return Err(Error::AacParse {
                    message: "invalid AAC section length".into(),
                });
            }

            for slot in group_cb.iter_mut().skip(k).take(sect_len) {
                *slot = cb;
            }
            k += sect_len;
        }
    }

    Ok(SectionData { sfb_cb })
}

fn parse_scale_factor_data(
    reader: &mut BitReader,
    info: &IcsInfo,
    section: &SectionData,
) -> Result<()> {
    let mut noise_pcm_flag = true;
    let scf_table = scf_huffman_table();

    for g in 0..info.window_groups {
        for sfb in 0..info.max_sfb {
            let cb = section.sfb_cb[g][sfb];
            if cb == ZERO_HCB {
                continue;
            }
            if cb == NOISE_HCB && noise_pcm_flag {
                reader.read_bits(9)?;
                noise_pcm_flag = false;
                continue;
            }
            decode_huffman(reader, scf_table)?;
        }
    }
    Ok(())
}

fn parse_spectral_data(
    reader: &mut BitReader,
    info: &IcsInfo,
    section: &SectionData,
    bands: &[usize],
) -> Result<()> {
    let huffman_tables = spectrum_huffman_tables();

    for g in 0..info.window_groups {
        // Short-window spectral data is ordered by scalefactor band, then by
        // each window in the group; reversing that order desynchronizes CPEs.
        for sfb in 0..info.max_sfb {
            let cb_idx = section.sfb_cb[g][sfb];
            if matches!(
                cb_idx,
                ZERO_HCB | NOISE_HCB | INTENSITY_HCB | INTENSITY_HCB2
            ) {
                continue;
            }

            let start = bands[sfb];
            let end = bands[sfb + 1];
            let width = end - start;

            let cb_info = &aac_codebooks::SPECTRUM_CODEBOOKS[cb_idx as usize - 1];
            let dim = cb_info.dimension as usize;
            let num_codewords = width / dim;
            let huffman_table = &huffman_tables[cb_idx as usize - 1];

            for _w in 0..info.window_group_len[g] {
                for _ in 0..num_codewords {
                    let symbol = decode_huffman(reader, huffman_table)?;

                    if cb_info.is_unsigned {
                        if cb_info.dimension == 4 {
                            let (a, b, c, d) = aac_codebooks::AAC_QUADS[symbol];
                            if a != 0 {
                                reader.read_bits(1)?;
                            }
                            if b != 0 {
                                reader.read_bits(1)?;
                            }
                            if c != 0 {
                                reader.read_bits(1)?;
                            }
                            if d != 0 {
                                reader.read_bits(1)?;
                            }
                        } else {
                            let mod_val = cb_info.mod_value as usize;
                            let x = symbol / mod_val;
                            let y = symbol % mod_val;
                            if x != 0 {
                                reader.read_bits(1)?;
                            }
                            if y != 0 {
                                reader.read_bits(1)?;
                            }

                            if cb_idx == ESC_HCB {
                                if x == 16 {
                                    read_escape(reader)?;
                                }
                                if y == 16 {
                                    read_escape(reader)?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn read_escape(reader: &mut BitReader) -> Result<()> {
    let mut n = 0u8;
    while reader.read_bit()? {
        n += 1;
        if n >= 9 {
            return Err(Error::AacParse {
                message: "escape sequence too long".into(),
            });
        }
    }
    reader.skip_bits((n as usize) + 4)?;
    Ok(())
}

fn parse_pulse_data(reader: &mut BitReader) -> Result<()> {
    let number_pulse = reader.read_bits(2)? as usize;
    let _pulse_start_sfb = reader.read_bits(6)?;
    for _ in 0..number_pulse + 1 {
        let _pulse_offset = reader.read_bits(5)?;
        let _pulse_amp = reader.read_bits(4)?;
    }
    Ok(())
}

fn parse_tns_data(reader: &mut BitReader, info: &IcsInfo) -> Result<()> {
    let n_filt_bits = if info.long_win { 2u8 } else { 1u8 };
    let length_bits = if info.long_win { 6u8 } else { 4u8 };
    let order_bits = if info.long_win { 5u8 } else { 3u8 };

    let num_windows = if info.long_win { 1 } else { 8 };

    for _ in 0..num_windows {
        let mut remaining_bands = info.max_sfb;
        let n_filt = reader.read_bits(n_filt_bits)? as usize;
        if n_filt > 0 {
            let coef_res = reader.read_bits(1)?;
            for _ in 0..n_filt {
                let length = reader.read_bits(length_bits)? as usize;
                if length > remaining_bands {
                    return Err(Error::AacParse {
                        message: "invalid TNS filter length".into(),
                    });
                }
                remaining_bands -= length;

                let order = reader.read_bits(order_bits)? as usize;
                if order > 0 {
                    let _direction = reader.read_bits(1)?;
                    let coef_compress = reader.read_bits(1)?;
                    let coef_bits = (coef_res + 3 - coef_compress) as u8;
                    for _ in 0..order {
                        reader.read_bits(coef_bits)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn parse_ics(
    reader: &mut BitReader,
    channel: u8,
    common_window: bool,
    shared_info: Option<&IcsInfo>,
    sample_rate: u32,
) -> Result<(AacGainLocation, IcsInfo)> {
    let (byte_off, bit_off) = reader.position();
    let global_gain = reader.read_bits(8)? as u8;

    let gain_loc = AacGainLocation::new(0, 0, byte_off as u32, bit_off, channel, global_gain);

    let info = if common_window {
        shared_info.unwrap().clone()
    } else {
        parse_ics_info(reader)?
    };

    let (long_bands, short_bands) = aac_codebooks::swb_offsets(sample_rate);
    let bands = if info.long_win {
        long_bands
    } else {
        short_bands
    };

    if info.max_sfb >= bands.len() {
        return Err(Error::AacParse {
            message: format!(
                "max_sfb {} exceeds available bands {}",
                info.max_sfb,
                bands.len() - 1
            ),
        });
    }

    let section = parse_section_data(reader, &info)?;
    parse_scale_factor_data(reader, &info, &section)?;

    if reader.read_bit()? {
        if !info.long_win {
            return Err(Error::AacParse {
                message: "pulse data in short window".into(),
            });
        }
        parse_pulse_data(reader)?;
    }

    if reader.read_bit()? {
        parse_tns_data(reader, &info)?;
    }

    if reader.read_bit()? {
        return Err(Error::AacParse {
            message: "gain control data not supported for AAC-LC".into(),
        });
    }

    parse_spectral_data(reader, &info, &section, bands)?;

    Ok((gain_loc, info))
}

fn parse_sce(reader: &mut BitReader, sample_rate: u32) -> Result<Vec<AacGainLocation>> {
    let _tag = reader.read_bits(4)?;
    let (loc, _) = parse_ics(reader, 0, false, None, sample_rate)?;
    Ok(vec![loc])
}

fn parse_cpe(reader: &mut BitReader, sample_rate: u32) -> Result<Vec<AacGainLocation>> {
    let _tag = reader.read_bits(4)?;
    let common_window = reader.read_bit()?;

    let shared_info = if common_window {
        let info = parse_ics_info(reader)?;
        let ms_mask_present = reader.read_bits(2)?;
        if ms_mask_present == 1 {
            for _g in 0..info.window_groups {
                for _sfb in 0..info.max_sfb {
                    reader.read_bits(1)?;
                }
            }
        }
        Some(info)
    } else {
        None
    };

    let (loc1, _) = parse_ics(reader, 0, common_window, shared_info.as_ref(), sample_rate)?;
    let (loc2, _) = parse_ics(reader, 1, common_window, shared_info.as_ref(), sample_rate)?;

    Ok(vec![loc1, loc2])
}

fn skip_dse(reader: &mut BitReader) -> Result<()> {
    let _tag = reader.read_bits(4)?;
    let align = reader.read_bit()?;
    let mut count = reader.read_bits(8)? as usize;
    if count == 255 {
        count += reader.read_bits(8)? as usize;
    }
    if align {
        reader.byte_align();
    }
    reader.skip_bits(count * 8)?;
    Ok(())
}

fn skip_fil(reader: &mut BitReader) -> Result<()> {
    let mut count = reader.read_bits(4)? as usize;
    if count == 15 {
        count += reader.read_bits(8)? as usize - 1;
    }
    reader.skip_bits(count * 8)?;
    Ok(())
}

fn skip_pce(reader: &mut BitReader) -> Result<()> {
    let _element_instance_tag = reader.read_bits(4)?;
    let _object_type = reader.read_bits(2)?;
    let _sampling_frequency_index = reader.read_bits(4)?;
    let num_front = reader.read_bits(4)? as usize;
    let num_side = reader.read_bits(4)? as usize;
    let num_back = reader.read_bits(4)? as usize;
    let num_lfe = reader.read_bits(2)? as usize;
    let num_assoc_data = reader.read_bits(3)? as usize;
    let num_valid_cc = reader.read_bits(4)? as usize;
    let mono_mixdown_present = reader.read_bit()?;
    if mono_mixdown_present {
        reader.read_bits(4)?;
    }
    let stereo_mixdown_present = reader.read_bit()?;
    if stereo_mixdown_present {
        reader.read_bits(4)?;
    }
    let matrix_mixdown_idx_present = reader.read_bit()?;
    if matrix_mixdown_idx_present {
        reader.read_bits(3)?;
    }
    for _ in 0..num_front {
        reader.read_bits(5)?;
    }
    for _ in 0..num_side {
        reader.read_bits(5)?;
    }
    for _ in 0..num_back {
        reader.read_bits(5)?;
    }
    for _ in 0..num_lfe {
        reader.read_bits(4)?;
    }
    for _ in 0..num_assoc_data {
        reader.read_bits(4)?;
    }
    for _ in 0..num_valid_cc {
        reader.read_bits(5)?;
    }
    reader.byte_align();
    let comment_len = reader.read_bits(8)? as usize;
    reader.skip_bits(comment_len * 8)?;
    Ok(())
}

fn parse_raw_data_block(reader: &mut BitReader, sample_rate: u32) -> Result<Vec<AacGainLocation>> {
    let mut locations = Vec::new();

    loop {
        if reader.bits_remaining() < 3 {
            break;
        }
        let id = reader.read_bits(3)?;

        match id {
            ID_SCE | ID_LFE => {
                let locs = parse_sce(reader, sample_rate)?;
                locations.extend(locs);
            }
            ID_CPE => {
                let locs = parse_cpe(reader, sample_rate)?;
                locations.extend(locs);
            }
            ID_CCE => {
                return Err(Error::AacParse {
                    message: "CCE element found - sample skipped".into(),
                });
            }
            ID_DSE => skip_dse(reader)?,
            ID_PCE => skip_pce(reader)?,
            ID_FIL => skip_fil(reader)?,
            ID_END => break,
            _ => {
                return Err(Error::AacParse {
                    message: format!("unsupported element type {}", id),
                });
            }
        }
    }

    Ok(locations)
}

// ---------------------------------------------------------------------------
// Gain read / write
// ---------------------------------------------------------------------------

fn read_aac_gain_at(data: &[u8], loc: &AacGainLocation) -> u8 {
    read_bits_u8(data, loc.file_offset as usize, loc.bit_offset)
}

fn write_aac_gain_at(data: &mut [u8], loc: &AacGainLocation, value: u8) {
    write_bits_u8(data, loc.file_offset as usize, loc.bit_offset, value)
}

fn apply_gain_to_data(data: &mut [u8], locations: &[AacGainLocation], gain_steps: i32) -> usize {
    let mut modified = 0usize;
    for loc in locations {
        let current = read_aac_gain_at(data, loc);
        if current == 0 {
            continue;
        }
        let new_value = adjust_gain_value(current, gain_steps);
        if new_value != current {
            write_aac_gain_at(data, loc, new_value);
            modified += 1;
        }
    }
    modified
}

fn apply_gain_to_file_data(
    file: &mut File,
    locations: &[AacGainLocation],
    gain_steps: i32,
) -> Result<usize> {
    const PATCH_CHUNK_SIZE: u64 = 1024 * 1024;

    // The chunking loop assumes sorted locations: idx selects chunk_start,
    // PATCH_CHUNK_SIZE only extends forward, and write_bits_u8 uses
    // loc.file_offset - chunk_start after adjust_gain_value decides to patch.
    assert!(locations
        .windows(2)
        .all(|pair| pair[0].file_offset <= pair[1].file_offset));

    let mut modified = 0usize;
    let mut idx = 0usize;
    let mut buf = Vec::new();

    while idx < locations.len() {
        let Some(first_idx) = locations[idx..].iter().position(|loc| {
            loc.original_gain != 0
                && adjust_gain_value(loc.original_gain, gain_steps) != loc.original_gain
        }) else {
            break;
        };
        idx += first_idx;

        let chunk_start = locations[idx].file_offset;
        let chunk_limit = chunk_start.saturating_add(PATCH_CHUNK_SIZE);
        let mut chunk_end = chunk_start;
        let mut end_idx = idx;

        while end_idx < locations.len() {
            let loc = &locations[end_idx];
            let new_value = adjust_gain_value(loc.original_gain, gain_steps);
            let will_modify = loc.original_gain != 0 && new_value != loc.original_gain;
            let loc_len = if loc.bit_offset == 0 { 1 } else { 2 };
            let loc_end = loc.file_offset + loc_len;

            if loc_end > chunk_limit && chunk_end > chunk_start {
                break;
            }

            if will_modify {
                chunk_end = chunk_end.max(loc_end);
            }
            end_idx += 1;
        }

        buf.resize((chunk_end - chunk_start) as usize, 0);
        file.seek(SeekFrom::Start(chunk_start))?;
        file.read_exact(&mut buf)?;

        for loc in &locations[idx..end_idx] {
            if loc.original_gain == 0 {
                continue;
            }

            let new_value = adjust_gain_value(loc.original_gain, gain_steps);
            if new_value == loc.original_gain {
                continue;
            }

            let offset = (loc.file_offset - chunk_start) as usize;
            write_bits_u8(&mut buf[offset..], 0, loc.bit_offset, new_value);
            modified += 1;
        }

        file.seek(SeekFrom::Start(chunk_start))?;
        file.write_all(&buf)?;
        idx = end_idx;
    }

    Ok(modified)
}
