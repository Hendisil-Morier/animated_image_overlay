//! Shared temporary directories, recording sinks, and synthetic APNG builders for decode integration tests.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use animated_image_widget::{
    decode::{DecodedFirstFrame, EncodedFrame, FrameStreamSink},
    error::CacheError,
};

/// Temporary directory cleaned up automatically when dropped.
pub struct TempDecodeDir
{
    path: PathBuf,
}

impl TempDecodeDir
{
    pub fn new(name: &str) -> Self
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aiw_dec_{name}_{}_{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("temp decode dir created");
        Self { path }
    }

    pub fn path(&self) -> &Path
    {
        &self.path
    }
}

impl Drop for TempDecodeDir
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Captured frames, delays, and terminal stream status observed by `RecordingSink`.
#[derive(Default)]
pub struct RecordedStream
{
    pub frames: Vec<Vec<u8>>,
    pub delays: Vec<u32>,
    pub delays_us: Vec<u32>,
    pub finished: bool,
    pub cancelled: bool,
    pub finish_err: Option<String>,
}

/// Thread-safe test `FrameStreamSink` appending emitted frames into `RecordedStream`.
pub struct RecordingSink
{
    pub state: Arc<Mutex<RecordedStream>>,
}

impl FrameStreamSink for RecordingSink
{
    fn on_first_frame(&mut self, frame: &DecodedFirstFrame) -> Result<(), CacheError>
    {
        let mut guard = self.state.lock().unwrap();
        guard.frames.push(frame.lz4_payload.clone());
        guard.delays.push(frame.delay_ms);
        guard.delays_us.push(frame.delay_us);
        Ok(())
    }

    fn on_frame(&mut self, frame: EncodedFrame) -> Result<(), CacheError>
    {
        let mut guard = self.state.lock().unwrap();
        guard.frames.push(frame.lz4_payload);
        guard.delays.push(frame.delay_ms);
        guard.delays_us.push(frame.delay_us);
        Ok(())
    }

    fn on_finish(self: Box<Self>, cancelled: bool, result: Result<(), CacheError>)
    {
        let mut guard = self.state.lock().unwrap();
        guard.cancelled = cancelled;
        guard.finished = true;
        guard.finish_err = result.err().map(|e| e.to_string());
    }
}

/// Frame control and solid patch descriptor for building synthetic APNG files cleanly.
#[derive(Clone, Copy)]
pub struct SyntheticPatchSpec
{
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub rgba: [u8; 4],
    pub delay_num: u16,
    pub delay_den: u16,
    pub dispose_op: u8,
    pub blend_op: u8,
}

/// Builds a synthetic full-canvas RGBA APNG from `(rgba, delay_num, delay_den)` descriptors.
pub fn encode_solid_apng_fraction(
    width: u32,
    height: u32,
    frames_rgba: &[([u8; 4], u16, u16)],
) -> Vec<u8>
{
    let patches: Vec<SyntheticPatchSpec> = frames_rgba
        .iter()
        .map(|&(rgba, delay_num, delay_den)| SyntheticPatchSpec {
            x: 0,
            y: 0,
            w: width,
            h: height,
            rgba,
            delay_num,
            delay_den,
            dispose_op: 0,
            blend_op: 0,
        })
        .collect();
    encode_synthetic_apng(width, height, 6, &[], None, &patches)
}

/// Encodes an uncompressed zlib-stored APNG stream with optional ancillary chunks and hidden `IDAT`.
pub fn encode_synthetic_apng(
    canvas_w: u32,
    canvas_h: u32,
    color_type: u8,
    extra_chunks: &[(&[u8; 4], &[u8])],
    hidden_thumbnail: Option<&[u8]>,
    patches: &[SyntheticPatchSpec],
) -> Vec<u8>
{
    fn crc32(bytes: &[u8]) -> u32
    {
        let mut crc = 0xFFFF_FFFF_u32;
        for &b in bytes
        {
            crc ^= u32::from(b);
            for _ in 0..8
            {
                let mask = (crc & 1).wrapping_neg() & 0xEDB8_8320;
                crc = (crc >> 1) ^ mask;
            }
        }
        !crc
    }

    fn adler32(bytes: &[u8]) -> u32
    {
        let mut a = 1_u32;
        let mut b = 0_u32;
        for &byte in bytes
        {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    fn zlib_stored(raw: &[u8]) -> Vec<u8>
    {
        let len = raw.len() as u16;
        let mut out = vec![0x78, 0x01, 0x01];
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(raw);
        out.extend_from_slice(&adler32(raw).to_be_bytes());
        out
    }

    fn push_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8])
    {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        let mut crc_input = Vec::with_capacity(4 + data.len());
        crc_input.extend_from_slice(tag);
        crc_input.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    }

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&canvas_w.to_be_bytes());
    ihdr.extend_from_slice(&canvas_h.to_be_bytes());
    ihdr.extend_from_slice(&[8, color_type, 0, 0, 0]);
    push_chunk(&mut out, b"IHDR", &ihdr);

    for (tag, data) in extra_chunks
    {
        push_chunk(&mut out, tag, data);
    }

    let mut actl = Vec::new();
    actl.extend_from_slice(&(patches.len() as u32).to_be_bytes());
    actl.extend_from_slice(&0_u32.to_be_bytes());
    push_chunk(&mut out, b"acTL", &actl);

    if let Some(thumb_scanlines) = hidden_thumbnail
    {
        push_chunk(&mut out, b"IDAT", &zlib_stored(thumb_scanlines));
    }

    let mut seq = 0_u32;
    for (idx, p) in patches.iter().enumerate()
    {
        let mut fctl = Vec::with_capacity(26);
        fctl.extend_from_slice(&seq.to_be_bytes());
        seq += 1;
        fctl.extend_from_slice(&p.w.to_be_bytes());
        fctl.extend_from_slice(&p.h.to_be_bytes());
        fctl.extend_from_slice(&p.x.to_be_bytes());
        fctl.extend_from_slice(&p.y.to_be_bytes());
        fctl.extend_from_slice(&p.delay_num.to_be_bytes());
        fctl.extend_from_slice(&p.delay_den.to_be_bytes());
        fctl.push(p.dispose_op);
        fctl.push(p.blend_op);
        push_chunk(&mut out, b"fcTL", &fctl);

        let mut raw = Vec::new();
        for _row in 0..p.h
        {
            raw.push(0);
            for _col in 0..p.w
            {
                if color_type == 3
                {
                    raw.push(p.rgba[0]);
                }
                else
                {
                    raw.extend_from_slice(&p.rgba);
                }
            }
        }
        let compressed = zlib_stored(&raw);
        if idx == 0 && hidden_thumbnail.is_none()
        {
            push_chunk(&mut out, b"IDAT", &compressed);
        }
        else
        {
            let mut fdat = Vec::with_capacity(4 + compressed.len());
            fdat.extend_from_slice(&seq.to_be_bytes());
            seq += 1;
            fdat.extend_from_slice(&compressed);
            push_chunk(&mut out, b"fdAT", &fdat);
        }
    }

    push_chunk(&mut out, b"IEND", &[]);
    out
}
