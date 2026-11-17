//! Streaming single-open APNG chunk splitter extracting sequential compressed frame packets.

// Buffered file I/O, chunk framing, and explicit static-PNG rewind seeks.
use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

// Contextual decoder/I/O error wrappers and overflow-safe canvas byte validation.
use crate::{
    decode::{
        codecs::{decode_err, io_err},
        pixel::checked_argb_byte_len,
    },
    error::CacheError,
};

// Standard 8-byte PNG file signature preceding all chunks.
pub(super) const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

// Maximum single PNG chunk payload accepted to reject malformed length prefixes cleanly.
const MAX_PNG_CHUNK_BYTES: usize = 64 * 1024 * 1024;

// Expected byte length of PNG `IHDR` header payload.
pub(super) const IHDR_DATA_LEN: usize = 13;

// Expected byte length of APNG `acTL` animation control payload.
const ACTL_DATA_LEN: usize = 8;

// Expected byte length of APNG `fcTL` frame control payload.
const FCTL_DATA_LEN: usize = 26;

// Minimum byte length of APNG `fdAT` payload (`4`-byte sequence prefix + compressed stream).
const FDAT_SEQ_PREFIX_LEN: usize = 4;

/// Parsed APNG `fcTL` frame geometry, fractional delay, and dispose/blend operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameControl
{
    /// Patch pixel width within `(canvas_w, canvas_h)`.
    pub width: u32,

    /// Patch pixel height within `(canvas_w, canvas_h)`.
    pub height: u32,

    /// Horizontal patch offset relative to the full canvas.
    pub x_offset: u32,

    /// Vertical patch offset relative to the full canvas.
    pub y_offset: u32,

    /// Frame delay numerator in seconds.
    pub delay_num: u16,

    /// Frame delay denominator in seconds (`0` implies `100` per APNG spec).
    pub delay_den: u16,

    /// Canvas disposal mode after displaying this frame (`0 = none`, `1 = bg`, `2 = prev`).
    pub dispose_op: u8,

    /// Pixel compositing mode onto the canvas (`0 = source`, `1 = over`).
    pub blend_op: u8,
}

/// Shared canvas header and palette/color chunks reused when building standalone frame PNGs.
#[derive(Debug, Clone)]
pub struct ApngHeader
{
    /// Full canvas pixel width from `IHDR`.
    pub canvas_w: u32,

    /// Full canvas pixel height from `IHDR`.
    pub canvas_h: u32,

    // Five `IHDR` trailing bytes (`bit_depth`, `color_type`, `compression`, `filter`, `interlace`).
    pub(super) ihdr_tail: [u8; 5],

    // Pre-encoded palette and color metadata chunks (`PLTE`, `tRNS`, `sRGB`, `gAMA`, `cHRM`, etc.).
    pub(super) shared_ancillary_chunks: Vec<u8>,
}

/// Compressed raw APNG frame chunks ready for parallel worker PNG inflation.
#[derive(Debug, Clone)]
pub struct RawApngPacket
{
    /// Zero-based animation frame index.
    pub seq: usize,

    /// Validated `fcTL` geometry, timing, and compositing operators.
    pub fctl: FrameControl,

    /// Raw compressed `IDAT` payloads (with any 4-byte `fdAT` sequence prefix already removed).
    pub compressed_idats: Vec<Vec<u8>>,
}

/// Single parsed PNG chunk `(tag, data, raw_crc)`.
struct RawChunk
{
    // Four-byte ASCII chunk identifier.
    tag: [u8; 4],

    // Unparsed chunk payload bytes.
    data: Vec<u8>,

    // Original big-endian CRC bytes preserved when copying shared header chunks verbatim.
    crc_bytes: [u8; 4],
}

/// Streaming single-open reader yielding compressed per-frame APNG packets sequentially.
pub struct ApngPacketReader
{
    // Source file path used for contextual error reporting.
    path: PathBuf,

    // Buffered source file handle positioned at the active chunk boundary.
    reader: BufReader<File>,

    // Shared `IHDR` fields and palette/color chunks copied into per-frame synthetic PNGs.
    header: ApngHeader,

    // Expected frame count declared in `acTL` (`0` disables upper count clamp if unspecified).
    expected_frames: usize,

    // Count of animated frames emitted so far (`0..expected_frames`).
    emitted_frames: usize,

    // Initial `fcTL` + `IDAT` chunk pair when `IDAT` belongs to Frame 0 rather than a hidden fallback.
    first_frame_pending: Option<(FrameControl, Vec<u8>)>,

    // Next unconsumed chunk peeked while delimiting `IDAT`/`fdAT` runs.
    lookahead: Option<RawChunk>,

    // Set once `IEND` or EOF is reached.
    finished: bool,
}

impl ApngPacketReader
{
    /// Probes a PNG stream at offset `0`; returns `Ok(Ok(reader))` on valid APNGs (`acTL`), or
    /// rewinds and returns `Ok(Err(file))` when static PNG decoding should handle it instead.
    pub fn try_open(
        path: &Path,
        mut reader: BufReader<File>,
    ) -> Result<Result<Self, BufReader<File>>, CacheError>
    {
        if !read_exact_signature(&mut reader)
        {
            reader
                .seek(SeekFrom::Start(0))
                .map_err(|err| io_err(path, err))?;
            return Ok(Err(reader));
        }

        let Some(ihdr_chunk) = read_chunk_opt(path, &mut reader)?
        else
        {
            reader
                .seek(SeekFrom::Start(0))
                .map_err(|err| io_err(path, err))?;
            return Ok(Err(reader));
        };
        if &ihdr_chunk.tag != b"IHDR" || ihdr_chunk.data.len() != IHDR_DATA_LEN
        {
            reader
                .seek(SeekFrom::Start(0))
                .map_err(|err| io_err(path, err))?;
            return Ok(Err(reader));
        }

        let canvas_w = read_be_u32(&ihdr_chunk.data[0..4]);
        let canvas_h = read_be_u32(&ihdr_chunk.data[4..8]);
        let ihdr_tail = [
            ihdr_chunk.data[8],
            ihdr_chunk.data[9],
            ihdr_chunk.data[10],
            ihdr_chunk.data[11],
            ihdr_chunk.data[12],
        ];
        let _ = checked_argb_byte_len(canvas_w, canvas_h)?;

        // Reject 16-bit APNGs immediately to fail fast without scanning chunks.
        if ihdr_tail[0] == 16
        {
            return Err(decode_err(
                path,
                String::from("16-bit color depth is unsupported for animated APNG"),
            ));
        }

        let mut shared_ancillary = Vec::new();
        let mut saw_actl = false;
        let mut expected_frames = 0_usize;
        let mut pending_fctl: Option<FrameControl> = None;
        let mut first_idat: Option<Vec<u8>> = None;

        while let Some(chunk) = read_chunk_opt(path, &mut reader)?
        {
            match &chunk.tag
            {
                b"acTL" =>
                {
                    if chunk.data.len() != ACTL_DATA_LEN
                    {
                        return Err(decode_err(path, String::from("Invalid APNG acTL length")));
                    }
                    saw_actl = true;
                    expected_frames = read_be_u32(&chunk.data[0..4]) as usize;
                }
                b"fcTL" =>
                {
                    pending_fctl = Some(parse_fctl(path, &chunk.data, canvas_w, canvas_h)?);
                }
                b"IDAT" =>
                {
                    first_idat = Some(chunk.data);
                    break;
                }
                b"IEND" =>
                {
                    return rewind_as_static(path, reader);
                }
                tag if is_shared_ancillary_chunk(tag) =>
                {
                    append_raw_chunk(&mut shared_ancillary, &chunk);
                }
                _ =>
                {}
            }
        }

        if !saw_actl
        {
            return rewind_as_static(path, reader);
        }

        let first_frame_pending = match (pending_fctl, first_idat)
        {
            (Some(fctl), Some(idat)) => Some((fctl, idat)),
            _ => None,
        };

        Ok(Ok(Self {
            path: path.to_path_buf(),
            reader,
            header: ApngHeader {
                canvas_w,
                canvas_h,
                ihdr_tail,
                shared_ancillary_chunks: shared_ancillary,
            },
            expected_frames,
            emitted_frames: 0,
            first_frame_pending,
            lookahead: None,
            finished: false,
        }))
    }

    /// Returns a cloned copy of the shared APNG header for parallel decode workers.
    pub fn header(&self) -> ApngHeader
    {
        self.header.clone()
    }

    /// Reads the next sequential compressed frame packet, or returns `None` at end of animation.
    pub fn next_packet(&mut self) -> Result<Option<RawApngPacket>, CacheError>
    {
        if self.finished
            || (self.expected_frames > 0 && self.emitted_frames >= self.expected_frames)
        {
            return Ok(None);
        }

        if let Some((fctl, first_idat)) = self.first_frame_pending.take()
        {
            let mut idats = vec![first_idat];
            self.collect_consecutive_payloads(b"IDAT", false, &mut idats)?;
            let seq = self.emitted_frames;
            self.emitted_frames += 1;
            return Ok(Some(RawApngPacket {
                seq,
                fctl,
                compressed_idats: idats,
            }));
        }

        let fctl = loop
        {
            let Some(chunk) = self.take_or_read_chunk()?
            else
            {
                self.finished = true;
                return Ok(None);
            };
            match &chunk.tag
            {
                b"fcTL" =>
                {
                    break parse_fctl(
                        &self.path,
                        &chunk.data,
                        self.header.canvas_w,
                        self.header.canvas_h,
                    )?;
                }
                b"IEND" =>
                {
                    self.finished = true;
                    return Ok(None);
                }
                _ =>
                {}
            }
        };

        let mut idats = Vec::new();
        self.collect_consecutive_payloads(b"fdAT", true, &mut idats)?;
        if idats.is_empty()
        {
            return Err(decode_err(
                &self.path,
                String::from("APNG fcTL frame missing fdAT payload chunks"),
            ));
        }

        let seq = self.emitted_frames;
        self.emitted_frames += 1;
        Ok(Some(RawApngPacket {
            seq,
            fctl,
            compressed_idats: idats,
        }))
    }

    /// Collects consecutive `IDAT` or `fdAT` chunks until the next `fcTL`, `IEND`, or EOF.
    fn collect_consecutive_payloads(
        &mut self,
        expected_tag: &[u8; 4],
        strip_seq_prefix: bool,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), CacheError>
    {
        while let Some(chunk) = self.take_or_read_chunk()?
        {
            if &chunk.tag == expected_tag
            {
                if strip_seq_prefix
                {
                    if chunk.data.len() <= FDAT_SEQ_PREFIX_LEN
                    {
                        return Err(decode_err(&self.path, String::from("Truncated APNG fdAT")));
                    }
                    out.push(chunk.data[FDAT_SEQ_PREFIX_LEN..].to_vec());
                }
                else
                {
                    out.push(chunk.data);
                }
                continue;
            }

            if &chunk.tag == b"fcTL"
            {
                self.lookahead = Some(chunk);
                break;
            }
            if &chunk.tag == b"IEND"
            {
                self.finished = true;
                break;
            }
        }
        Ok(())
    }

    /// Returns any stashed lookahead chunk first, otherwise reads the next chunk from `reader`.
    fn take_or_read_chunk(&mut self) -> Result<Option<RawChunk>, CacheError>
    {
        if let Some(chunk) = self.lookahead.take()
        {
            return Ok(Some(chunk));
        }
        read_chunk_opt(&self.path, &mut self.reader)
    }
}

/// Rewinds `reader` back to offset `0` so non-APNG static PNG decoding proceeds smoothly.
fn rewind_as_static(
    path: &Path,
    mut reader: BufReader<File>,
) -> Result<Result<ApngPacketReader, BufReader<File>>, CacheError>
{
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|err| io_err(path, err))?;
    Ok(Err(reader))
}

/// Returns `true` when `tag` carries global palette or color profile metadata needed per frame.
fn is_shared_ancillary_chunk(tag: &[u8; 4]) -> bool
{
    matches!(
        tag,
        b"PLTE" | b"tRNS" | b"gAMA" | b"cHRM" | b"sRGB" | b"iCCP" | b"cICP"
    )
}

/// Appends an untouched pre-`IDAT` PNG chunk cleanly using its original CRC bytes.
fn append_raw_chunk(out: &mut Vec<u8>, chunk: &RawChunk)
{
    out.extend_from_slice(&(chunk.data.len() as u32).to_be_bytes());
    out.extend_from_slice(&chunk.tag);
    out.extend_from_slice(&chunk.data);
    out.extend_from_slice(&chunk.crc_bytes);
}

/// Parses big-endian `u32` from a 4-byte slice.
fn read_be_u32(bytes: &[u8]) -> u32
{
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Parses and validates an APNG `fcTL` chunk against `(canvas_w, canvas_h)`.
fn parse_fctl(
    path: &Path,
    data: &[u8],
    canvas_w: u32,
    canvas_h: u32,
) -> Result<FrameControl, CacheError>
{
    if data.len() != FCTL_DATA_LEN
    {
        return Err(decode_err(
            path,
            format!("Invalid APNG fcTL chunk length {}", data.len()),
        ));
    }

    let width = read_be_u32(&data[4..8]);
    let height = read_be_u32(&data[8..12]);
    let x_offset = read_be_u32(&data[12..16]);
    let y_offset = read_be_u32(&data[16..20]);
    let delay_num = u16::from_be_bytes([data[20], data[21]]);
    let delay_den = u16::from_be_bytes([data[22], data[23]]);
    let dispose_op = data[24];
    let blend_op = data[25];

    let x_end = x_offset.checked_add(width);
    let y_end = y_offset.checked_add(height);
    if width == 0
        || height == 0
        || x_end.is_none_or(|end| end > canvas_w)
        || y_end.is_none_or(|end| end > canvas_h)
    {
        return Err(decode_err(
            path,
            format!(
                "APNG fcTL patch ({x_offset},{y_offset} {width}x{height}) exceeds canvas ({canvas_w}x{canvas_h})"
            ),
        ));
    }

    Ok(FrameControl {
        width,
        height,
        x_offset,
        y_offset,
        delay_num,
        delay_den,
        dispose_op,
        blend_op,
    })
}

/// Checks whether the stream starts with the standard 8-byte PNG signature.
fn read_exact_signature(reader: &mut BufReader<File>) -> bool
{
    let mut sig = [0_u8; 8];
    reader.read_exact(&mut sig).is_ok() && sig == PNG_SIGNATURE
}

/// Reads the next PNG chunk cleanly, returning `Ok(None)` at clean EOF before a length prefix.
fn read_chunk_opt(path: &Path, reader: &mut BufReader<File>)
-> Result<Option<RawChunk>, CacheError>
{
    let mut len_buf = [0_u8; 4];
    match reader.read_exact(&mut len_buf)
    {
        Ok(()) =>
        {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(io_err(path, err)),
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_PNG_CHUNK_BYTES
    {
        return Err(decode_err(
            path,
            format!("APNG chunk length {len} exceeds maximum bound"),
        ));
    }

    let mut tag = [0_u8; 4];
    reader
        .read_exact(&mut tag)
        .map_err(|err| io_err(path, err))?;

    let mut data = vec![0_u8; len];
    reader
        .read_exact(&mut data)
        .map_err(|err| io_err(path, err))?;

    let mut crc_bytes = [0_u8; 4];
    reader
        .read_exact(&mut crc_bytes)
        .map_err(|err| io_err(path, err))?;

    Ok(Some(RawChunk {
        tag,
        data,
        crc_bytes,
    }))
}
