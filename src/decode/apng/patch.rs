//! Parallel-safe single-frame PNG synthesis and straight-RGBA patch inflation for APNG packets.

// In-memory cursor and path types for standalone PNG inflation.
use std::{io::Cursor, path::Path};

// Standard PNG frame decoder handling bit-depth unpacking, palettes, tRNS, and unfiltering.
use image::{DynamicImage, codecs::png::PngDecoder};

// Shared APNG packet metadata, codec error helpers, dimension bounds, and delay clamping.
use super::reader::{ApngHeader, FrameControl, IHDR_DATA_LEN, PNG_SIGNATURE, RawApngPacket};
use crate::{
    decode::{codecs::decode_err, pixel::checked_argb_byte_len},
    error::CacheError,
    playback::PlaybackClock,
};

// Precomputed IEEE CRC-32 lookup table (`0xEDB88320`) used when framing standalone PNG chunks.
const CRC32_TABLE: [u32; 256] = build_crc32_table();

/// Independently inflated straight-RGBA patch awaiting ordered canvas compositing.
#[derive(Debug, Clone)]
pub struct DecodedApngPatch
{
    /// Zero-based animation frame index.
    pub seq: usize,

    /// Frame control metadata controlling canvas placement and blending.
    pub fctl: FrameControl,

    /// Normalized microsecond display duration (`>= CACHE_MIN_FRAME_DELAY_US`).
    pub delay_us: u32,

    /// Straight `[R, G, B, A]` pixels sized `fctl.width * fctl.height * 4`.
    pub rgba: Vec<u8>,
}

/// Decodes one compressed APNG packet independently via `PngDecoder` into straight-RGBA pixels.
pub fn decode_apng_packet(
    path: &Path,
    header: &ApngHeader,
    packet: RawApngPacket,
) -> Result<DecodedApngPatch, CacheError>
{
    let total_payload_bytes: usize = packet.compressed_idats.iter().map(Vec::len).sum();
    let est_capacity = PNG_SIGNATURE.len()
        + 12
        + IHDR_DATA_LEN
        + header.shared_ancillary_chunks.len()
        + packet.compressed_idats.len() * 12
        + total_payload_bytes
        + 12;

    let mut sub_png = Vec::with_capacity(est_capacity);
    sub_png.extend_from_slice(&PNG_SIGNATURE);

    let mut ihdr = [0_u8; IHDR_DATA_LEN];
    ihdr[0..4].copy_from_slice(&packet.fctl.width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&packet.fctl.height.to_be_bytes());
    ihdr[8..13].copy_from_slice(&header.ihdr_tail);
    append_png_chunk(&mut sub_png, b"IHDR", &ihdr);

    sub_png.extend_from_slice(&header.shared_ancillary_chunks);

    for payload in &packet.compressed_idats
    {
        append_png_chunk(&mut sub_png, b"IDAT", payload);
    }
    append_png_chunk(&mut sub_png, b"IEND", &[]);

    let decoder =
        PngDecoder::new(Cursor::new(&sub_png)).map_err(|err| decode_err(path, err.to_string()))?;
    let rgba = DynamicImage::from_decoder(decoder)
        .map_err(|err| decode_err(path, err.to_string()))?
        .into_rgba8();

    let expected_len = checked_argb_byte_len(packet.fctl.width, packet.fctl.height)?;
    let raw_bytes = rgba.into_raw();
    if raw_bytes.len() != expected_len
    {
        return Err(decode_err(
            path,
            format!(
                "Decoded APNG patch byte count {} did not match expected {}",
                raw_bytes.len(),
                expected_len
            ),
        ));
    }

    let delay_us = delay_us_from_fraction(packet.fctl.delay_num, packet.fctl.delay_den);
    Ok(DecodedApngPatch {
        seq: packet.seq,
        fctl: packet.fctl,
        delay_us,
        rgba: raw_bytes,
    })
}

/// Generates the 256-entry IEEE CRC-32 table at compile time cleanly in safe Rust.
const fn build_crc32_table() -> [u32; 256]
{
    let mut table = [0_u32; 256];
    let mut idx = 0_usize;
    while idx < 256
    {
        let mut crc = idx as u32;
        let mut bit = 0;
        while bit < 8
        {
            if (crc & 1) != 0
            {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            }
            else
            {
                crc >>= 1;
            }
            bit += 1;
        }
        table[idx] = crc;
        idx += 1;
    }
    table
}

/// Computes IEEE CRC-32 sequentially over PNG chunk `tag` and `data` bytes.
fn chunk_crc32(tag: &[u8; 4], data: &[u8]) -> u32
{
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in tag.iter().chain(data.iter())
    {
        let lookup_idx = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[lookup_idx];
    }
    !crc
}

/// Appends one complete length-prefixed and CRC-tagged PNG chunk onto `out`.
fn append_png_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8])
{
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    out.extend_from_slice(&chunk_crc32(tag, data).to_be_bytes());
}

/// Converts APNG `(delay_num, delay_den)` fractions directly into clamped microsecond delays.
fn delay_us_from_fraction(delay_num: u16, delay_den: u16) -> u32
{
    let denom = if delay_den == 0 { 100 } else { delay_den };
    let delay_us = (f64::from(delay_num) * 1_000_000.0) / f64::from(denom);
    PlaybackClock::normalize_cache_delay_us(Some(delay_us))
}
