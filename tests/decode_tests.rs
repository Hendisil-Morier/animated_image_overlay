//! Integration tests verifying APNG, GIF, animated WebP, and static PNG decoding,
//! premultiplied ARGB8888 conversion, `--max-frames` limits, non-upscaling, and cancellation.

use std::{
    fs::{self, File},
    io::{BufReader, Seek},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use animated_image_widget::{
    decode::{
        DecodedFirstFrame, EncodedFrame, FrameDecoder, FrameStreamSink,
        apng::{ApngCompositor, ApngPacketReader, decode_apng_packet},
        resize_premultiplied_argb8888, rotate_argb8888_into,
        straight_rgba_to_premultiplied_argb8888,
    },
    error::CacheError,
};
use fast_image_resize::Resizer;
use image::{
    AnimationDecoder, Delay, Frame as ImgFrame, ImageEncoder, RgbaImage,
    codecs::{
        bmp::BmpEncoder,
        gif::GifEncoder,
        png::{PngDecoder, PngEncoder},
    },
};
use lz4_flex::decompress_size_prepended;
use webp_animation::Encoder as WebpEncoder;

struct TempDecodeDir
{
    path: PathBuf,
}

impl TempDecodeDir
{
    fn new(name: &str) -> Self
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

    fn path(&self) -> &Path
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

#[derive(Default)]
struct RecordedStream
{
    frames: Vec<Vec<u8>>,
    delays: Vec<u32>,
    delays_us: Vec<u32>,
    finished: bool,
    cancelled: bool,
    finish_err: Option<String>,
}

struct RecordingSink
{
    state: Arc<Mutex<RecordedStream>>,
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

fn build_synthetic_apng_2x2(frames_rgba: &[([u8; 4], u16)]) -> Vec<u8>
{
    let fractional: Vec<([u8; 4], u16, u16)> = frames_rgba
        .iter()
        .map(|&(rgba, delay_ms)| (rgba, delay_ms, 1000))
        .collect();
    build_synthetic_apng_2x2_fraction(&fractional)
}

fn build_synthetic_apng_2x2_fraction(frames_rgba: &[([u8; 4], u16, u16)]) -> Vec<u8>
{
    build_synthetic_apng_fraction(2, 2, frames_rgba)
}

fn build_synthetic_apng_fraction(
    width: u32,
    height: u32,
    frames_rgba: &[([u8; 4], u16, u16)],
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
        let nlen = !len;
        let mut out = vec![0x78, 0x01, 0x01];
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
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

    fn scanline_solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8>
    {
        let row_bytes = 1 + (width as usize) * 4;
        let mut raw = Vec::with_capacity(row_bytes * (height as usize));
        for _row in 0..height
        {
            raw.push(0); // filter byte 0
            for _col in 0..width
            {
                raw.extend_from_slice(&rgba);
            }
        }
        zlib_stored(&raw)
    }

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    // IHDR: width x height, 8-bit RGBA (color type 6)
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut out, b"IHDR", &ihdr);

    // acTL: num_frames, num_plays = 0
    let mut actl = Vec::new();
    actl.extend_from_slice(&(frames_rgba.len() as u32).to_be_bytes());
    actl.extend_from_slice(&0_u32.to_be_bytes());
    push_chunk(&mut out, b"acTL", &actl);

    let mut seq = 0_u32;
    for (idx, &(rgba, delay_num, delay_den)) in frames_rgba.iter().enumerate()
    {
        let mut fctl = Vec::with_capacity(26);
        fctl.extend_from_slice(&seq.to_be_bytes());
        seq += 1;
        fctl.extend_from_slice(&width.to_be_bytes()); // width
        fctl.extend_from_slice(&height.to_be_bytes()); // height
        fctl.extend_from_slice(&0_u32.to_be_bytes()); // x_offset
        fctl.extend_from_slice(&0_u32.to_be_bytes()); // y_offset
        fctl.extend_from_slice(&delay_num.to_be_bytes()); // delay_num
        fctl.extend_from_slice(&delay_den.to_be_bytes()); // delay_den
        fctl.push(0); // dispose_op = none
        fctl.push(0); // blend_op = source
        push_chunk(&mut out, b"fcTL", &fctl);

        let compressed = scanline_solid(width, height, rgba);
        if idx == 0
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

#[test]
fn decode_converts_straight_rgba_to_premultiplied_argb8888()
{
    let mut pixels = vec![
        200, 100, 50, 255, // opaque
        200, 100, 50, 0, // fully transparent clears RGB
        200, 100, 50, 128, // semi-transparent premultiplied
    ];
    straight_rgba_to_premultiplied_argb8888(&mut pixels);

    assert_eq!(&pixels[0..4], &[50, 100, 200, 255]);
    assert_eq!(&pixels[4..8], &[0, 0, 0, 0]);
    // ((50 * 128) + 127) / 255 = 25, ((100 * 128) + 127) / 255 = 50, ((200 * 128) + 127) / 255 = 100
    assert_eq!(&pixels[8..12], &[25, 50, 100, 128]);
}

#[test]
fn decode_static_png_never_upscales_and_downscales_wide_images()
{
    let temp = TempDecodeDir::new("static_png");
    let small_path = temp.path().join("small.png");
    let small_img = RgbaImage::from_pixel(120, 80, image::Rgba([10, 20, 30, 255]));
    PngEncoder::new(File::create(&small_path).unwrap())
        .write_image(&small_img, 120, 80, image::ExtendedColorType::Rgba8)
        .unwrap();

    let state = Arc::new(Mutex::new(RecordedStream::default()));
    let sink = Box::new(RecordingSink {
        state: Arc::clone(&state),
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let (first, handle) = FrameDecoder::open_stream(&small_path, 500, 0, cancel, sink)
        .expect("small static PNG should decode");

    assert!(handle.is_none());
    assert_eq!((first.width, first.height), (120, 80));
    assert_eq!(&first.argb8888[0..4], &[30, 20, 10, 255]);

    let wide_path = temp.path().join("wide.png");
    let wide_img = RgbaImage::from_pixel(1000, 500, image::Rgba([40, 80, 120, 255]));
    PngEncoder::new(File::create(&wide_path).unwrap())
        .write_image(&wide_img, 1000, 500, image::ExtendedColorType::Rgba8)
        .unwrap();

    let state2 = Arc::new(Mutex::new(RecordedStream::default()));
    let sink2 = Box::new(RecordingSink {
        state: Arc::clone(&state2),
    });
    let (first2, _) =
        FrameDecoder::open_stream(&wide_path, 250, 0, Arc::new(AtomicBool::new(false)), sink2)
            .expect("wide PNG should downscale");
    assert_eq!((first2.width, first2.height), (250, 125));
    assert_eq!(first2.argb8888.len(), 250 * 125 * 4);
}

#[test]
fn decode_streams_apng_gif_and_webp_respecting_max_frames()
{
    let temp = TempDecodeDir::new("animations");

    // 1. APNG (3 frames)
    let apng_path = temp.path().join("anim.apng");
    let apng_bytes = build_synthetic_apng_2x2(&[
        ([255, 0, 0, 255], 40),
        ([0, 255, 0, 255], 60),
        ([0, 0, 255, 255], 80),
    ]);
    fs::write(&apng_path, apng_bytes).unwrap();

    let apng_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (first_apng, worker) = FrameDecoder::open_stream(
        &apng_path,
        500,
        2,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&apng_rec),
        }),
    )
    .expect("APNG should stream");
    if let Some(h) = worker
    {
        h.join().unwrap();
    }
    let rec = apng_rec.lock().unwrap();
    assert!(rec.finished);
    assert!(rec.finish_err.is_none());
    assert_eq!(rec.frames.len(), 2); // capped by max_frames = 2
    assert_eq!((first_apng.width, first_apng.height), (2, 2));
    assert_eq!(rec.delays, vec![40, 60]);
    let f1_raw = decompress_size_prepended(&rec.frames[1]).unwrap();
    assert_eq!(&f1_raw[0..4], &[0, 255, 0, 255]); // green in BGRA

    // 2. GIF (3 frames)
    let gif_path = temp.path().join("anim.gif");
    let gif_frames = [
        ImgFrame::from_parts(
            RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255])),
            0,
            0,
            Delay::from_numer_denom_ms(50, 1),
        ),
        ImgFrame::from_parts(
            RgbaImage::from_pixel(4, 4, image::Rgba([40, 50, 60, 255])),
            0,
            0,
            Delay::from_numer_denom_ms(70, 1),
        ),
        ImgFrame::from_parts(
            RgbaImage::from_pixel(4, 4, image::Rgba([70, 80, 90, 255])),
            0,
            0,
            Delay::from_numer_denom_ms(90, 1),
        ),
    ];
    let mut encoder = GifEncoder::new(File::create(&gif_path).unwrap());
    encoder.encode_frames(gif_frames).unwrap();
    drop(encoder);

    let gif_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (_first_gif, gif_worker) = FrameDecoder::open_stream(
        &gif_path,
        500,
        0,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&gif_rec),
        }),
    )
    .expect("GIF should stream");
    if let Some(h) = gif_worker
    {
        h.join().unwrap();
    }
    let gif_done = gif_rec.lock().unwrap();
    assert_eq!(gif_done.frames.len(), 3);
    assert_eq!(gif_done.finish_err, None);

    // 3. Animated WebP (2 frames)
    let webp_path = temp.path().join("anim.webp");
    let mut webp_enc = WebpEncoder::new((8, 8)).unwrap();
    let f0_px = [255, 0, 0, 255].repeat(64);
    let f1_px = [0, 0, 255, 255].repeat(64);
    webp_enc.add_frame(&f0_px, 0).unwrap();
    webp_enc.add_frame(&f1_px, 40).unwrap();
    let webp_data = webp_enc.finalize(90).unwrap();
    fs::write(&webp_path, &*webp_data).unwrap();

    let webp_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (first_webp, webp_worker) = FrameDecoder::open_stream(
        &webp_path,
        500,
        0,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&webp_rec),
        }),
    )
    .expect("animated WebP should stream");
    assert_eq!((first_webp.width, first_webp.height), (8, 8));
    if let Some(h) = webp_worker
    {
        h.join().unwrap();
    }
    let webp_done = webp_rec.lock().unwrap();
    assert_eq!(webp_done.frames.len(), 2);
    assert_eq!(webp_done.delays, vec![40, 50]);
}

#[test]
fn decode_respects_atomic_cancellation()
{
    let temp = TempDecodeDir::new("cancel");
    let gif_path = temp.path().join("many.gif");
    let frames: Vec<_> = (0..25_u8)
        .map(|idx| {
            ImgFrame::from_parts(
                RgbaImage::from_pixel(16, 16, image::Rgba([idx * 10, 0, 0, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(30, 1),
            )
        })
        .collect();
    let mut enc = GifEncoder::new(File::create(&gif_path).unwrap());
    enc.encode_frames(frames).unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(RecordedStream::default()));

    struct CancelAfterFirst
    {
        inner: RecordingSink,
        cancel: Arc<AtomicBool>,
    }

    impl FrameStreamSink for CancelAfterFirst
    {
        fn on_first_frame(&mut self, frame: &DecodedFirstFrame) -> Result<(), CacheError>
        {
            self.inner.on_first_frame(frame)?;
            self.cancel.store(true, Ordering::Release);
            Ok(())
        }

        fn on_frame(&mut self, frame: EncodedFrame) -> Result<(), CacheError>
        {
            self.inner.on_frame(frame)
        }

        fn on_finish(self: Box<Self>, cancelled: bool, result: Result<(), CacheError>)
        {
            Box::new(self.inner).on_finish(cancelled, result);
        }
    }

    let (_first, worker) = FrameDecoder::open_stream(
        &gif_path,
        500,
        0,
        Arc::clone(&cancel),
        Box::new(CancelAfterFirst {
            inner: RecordingSink {
                state: Arc::clone(&state),
            },
            cancel,
        }),
    )
    .expect("cancellation after first frame should still succeed opening");

    if let Some(h) = worker
    {
        h.join().unwrap();
    }

    let rec = state.lock().unwrap();
    assert!(rec.finished);
    assert!(rec.cancelled);
    assert!(rec.frames.len() <= 2);
}

#[test]
fn decode_single_frame_sync_and_static_formats_work_via_single_open()
{
    let temp = TempDecodeDir::new("single_open");

    // 1. Magic-byte static PNG without `.png` extension exercises `OpenedSource::StaticPng`.
    let no_ext_png = temp.path().join("static_image_no_ext");
    let png_img = RgbaImage::from_pixel(12, 6, image::Rgba([20, 40, 80, 255]));
    PngEncoder::new(File::create(&no_ext_png).unwrap())
        .write_image(&png_img, 12, 6, image::ExtendedColorType::Rgba8)
        .unwrap();

    let png_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (first_png, png_worker) = FrameDecoder::open_stream(
        &no_ext_png,
        500,
        0,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&png_rec),
        }),
    )
    .expect("extensionless static PNG should decode");
    assert!(png_worker.is_none());
    assert_eq!((first_png.width, first_png.height), (12, 6));
    assert_eq!(&first_png.argb8888[0..4], &[80, 40, 20, 255]);

    // 2. Non-PNG still container (`.bmp`) exercises `OpenedSource::Static`.
    let bmp_path = temp.path().join("still.bmp");
    let bmp_img = RgbaImage::from_pixel(10, 8, image::Rgba([15, 30, 45, 255]));
    BmpEncoder::new(&mut File::create(&bmp_path).unwrap())
        .write_image(&bmp_img, 10, 8, image::ExtendedColorType::Rgba8)
        .unwrap();

    let bmp_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (first_bmp, bmp_worker) = FrameDecoder::open_stream(
        &bmp_path,
        500,
        0,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&bmp_rec),
        }),
    )
    .expect("static BMP should decode via single opened reader");
    assert!(bmp_worker.is_none());
    assert_eq!((first_bmp.width, first_bmp.height), (10, 8));
    assert_eq!(&first_bmp.argb8888[0..4], &[45, 30, 15, 255]);

    // 3. Animated APNG with `max_frames = 1` exercises synchronous APNG extraction without spawning.
    let apng_path = temp.path().join("single_frame.png");
    let apng_bytes =
        build_synthetic_apng_2x2(&[([100, 150, 200, 255], 40), ([10, 20, 30, 255], 60)]);
    fs::write(&apng_path, apng_bytes).unwrap();

    let apng_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (first_apng, apng_worker) = FrameDecoder::open_stream(
        &apng_path,
        500,
        1,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&apng_rec),
        }),
    )
    .expect("max_frames = 1 APNG should decode synchronously");
    assert!(apng_worker.is_none());
    assert_eq!((first_apng.width, first_apng.height), (2, 2));
    assert_eq!(&first_apng.argb8888[0..4], &[200, 150, 100, 255]);
    assert!(apng_rec.lock().unwrap().finished);
    assert_eq!(apng_rec.lock().unwrap().frames.len(), 1);

    // 4. GIF with `max_frames = 1` exercises synchronous GIF extraction from single opened handle.
    let gif_path = temp.path().join("single_frame.gif");
    let gif_frames = [
        ImgFrame::from_parts(
            RgbaImage::from_pixel(4, 4, image::Rgba([25, 50, 75, 255])),
            0,
            0,
            Delay::from_numer_denom_ms(50, 1),
        ),
        ImgFrame::from_parts(
            RgbaImage::from_pixel(4, 4, image::Rgba([100, 100, 100, 255])),
            0,
            0,
            Delay::from_numer_denom_ms(50, 1),
        ),
    ];
    GifEncoder::new(File::create(&gif_path).unwrap())
        .encode_frames(gif_frames)
        .unwrap();

    let gif_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (first_gif, gif_worker) = FrameDecoder::open_stream(
        &gif_path,
        500,
        1,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&gif_rec),
        }),
    )
    .expect("max_frames = 1 GIF should decode synchronously");
    assert!(gif_worker.is_none());
    assert_eq!(&first_gif.argb8888[0..4], &[75, 50, 25, 255]);
    assert_eq!(gif_rec.lock().unwrap().frames.len(), 1);

    // 5. WebP with `max_frames = 1` exercises synchronous WebP extraction from single opened handle.
    let webp_path = temp.path().join("single_frame.webp");
    let mut webp_enc = WebpEncoder::new((8, 8)).unwrap();
    webp_enc
        .add_frame(&[12, 34, 56, 255].repeat(64), 0)
        .unwrap();
    webp_enc
        .add_frame(&[90, 90, 90, 255].repeat(64), 40)
        .unwrap();
    fs::write(&webp_path, &*webp_enc.finalize(90).unwrap()).unwrap();

    let webp_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (_first_webp, webp_worker) = FrameDecoder::open_stream(
        &webp_path,
        500,
        1,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&webp_rec),
        }),
    )
    .expect("max_frames = 1 WebP should decode synchronously");
    assert!(webp_worker.is_none());
    assert!(webp_rec.lock().unwrap().finished);
    assert_eq!(webp_rec.lock().unwrap().frames.len(), 1);
}

#[test]
fn rotate_argb8888_handles_chirality_and_transparent_apron_antialiasing()
{
    // 2x2 source pixels in [B, G, R, A]:
    // (0,0)=[1,0,0,255], (1,0)=[2,0,0,255]
    // (0,1)=[3,0,0,255], (1,1)=[4,0,0,255]
    let grid: [u8; 16] = [
        1, 0, 0, 255, 2, 0, 0, 255, // row 0
        3, 0, 0, 255, 4, 0, 0, 255, // row 1
    ];
    let mut dst_small = [0_u8; 16];

    // 90 deg CW unflipped -> [3, 1; 4, 2]
    rotate_argb8888_into(&grid, 2, 2, 90.0, false, 2, 2, &mut dst_small).unwrap();
    assert_eq!(
        dst_small,
        [
            3, 0, 0, 255, 1, 0, 0, 255, // row 0
            4, 0, 0, 255, 2, 0, 0, 255, // row 1
        ]
    );

    // 90 deg CW flipped horizontally -> [1, 3; 2, 4]
    rotate_argb8888_into(&grid, 2, 2, 90.0, true, 2, 2, &mut dst_small).unwrap();
    assert_eq!(
        dst_small,
        [
            1, 0, 0, 255, 3, 0, 0, 255, // row 0
            2, 0, 0, 255, 4, 0, 0, 255, // row 1
        ]
    );

    // 20x10 solid opaque colored block rotated 45 deg preserves center alpha=255 and valid premul edge alpha.
    let block = [25, 100, 200, 255].repeat(20 * 10);
    let mut dst_full = vec![0_u8; 22 * 22 * 4];
    rotate_argb8888_into(&block, 20, 10, 45.0, false, 22, 22, &mut dst_full).unwrap();

    // Exterior corner lies strictly outside source box.
    assert_eq!(&dst_full[0..4], &[0, 0, 0, 0]);

    // Center pixel remains solid opaque with exact source channels.
    let mid_off = (11 * 22 + 11) * 4;
    assert_eq!(&dst_full[mid_off..mid_off + 4], &[25, 100, 200, 255]);

    // Partial apron alpha exists along boundaries and every output pixel stays premultiplied-valid.
    let mut has_semitransparent = false;
    for px in dst_full.as_chunks::<4>().0
    {
        let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
        assert!(b <= a && g <= a && r <= a, "invalid premul pixel {px:?}");
        if a > 0 && a < 255
        {
            has_semitransparent = true;
        }
    }
    assert!(has_semitransparent);

    // Scaled physical target (11x11) preserves centered foreground pixels and premultiplied edges.
    let mut dst_half = vec![0_u8; 11 * 11 * 4];
    rotate_argb8888_into(&block, 20, 10, 45.0, false, 11, 11, &mut dst_half).unwrap();
    assert_eq!(&dst_half[0..4], &[0, 0, 0, 0]);
    let half_mid = (5 * 11 + 5) * 4;
    assert_eq!(&dst_half[half_mid..half_mid + 4], &[25, 100, 200, 255]);
    for px in dst_half.as_chunks::<4>().0
    {
        let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
        assert!(b <= a && g <= a && r <= a, "invalid premul pixel {px:?}");
    }

    // Degenerate 1x1 source and mirrored fractional sweep verify interior/apron boundary transitions.
    let single = [40_u8, 80, 120, 200];
    let mut dst_1x1 = vec![0_u8; 4 * 4 * 4];
    rotate_argb8888_into(&single, 1, 1, 33.0, false, 4, 4, &mut dst_1x1).unwrap();
    for px in dst_1x1.as_chunks::<4>().0
    {
        assert!(px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3]);
    }

    let mut gradient = vec![0_u8; 16 * 12 * 4];
    for (idx, px) in gradient.as_chunks_mut::<4>().0.iter_mut().enumerate()
    {
        let b = ((idx * 17) % 256) as u8;
        let g = ((idx * 31) % 256) as u8;
        let r = ((idx * 53) % 256) as u8;
        *px = [b, g, r, 255];
    }
    let mut dst_grad = vec![0_u8; 24 * 24 * 4];
    for angle in [37.5, 143.0, 225.0, 310.0]
    {
        rotate_argb8888_into(&gradient, 16, 12, angle, true, 24, 24, &mut dst_grad).unwrap();
        let center_off = (12 * 24 + 12) * 4;
        assert_eq!(dst_grad[center_off + 3], 255);
    }
}

#[test]
fn resize_premultiplied_argb8888_downscales_and_preserves_identity()
{
    let mut resizer = Resizer::new();
    let solid: Vec<u8> = [30_u8, 90, 180, 255]
        .into_iter()
        .cycle()
        .take(20 * 10 * 4)
        .collect();

    // Matching source and destination dimensions copy bytes identically without Lanczos filtering.
    let same = resize_premultiplied_argb8888(&solid, 20, 10, 20, 10, &mut resizer).unwrap();
    assert_eq!(same, solid);

    // Downscaling halves dimensions while preserving premultiplied channel ordering (`<= A`).
    let small = resize_premultiplied_argb8888(&solid, 20, 10, 10, 5, &mut resizer).unwrap();
    assert_eq!(small.len(), 10 * 5 * 4);
    assert_eq!(&small[0..4], &[30, 90, 180, 255]);

    let semi: Vec<u8> = [25_u8, 60, 110, 128]
        .into_iter()
        .cycle()
        .take(20 * 10 * 4)
        .collect();
    let small_semi = resize_premultiplied_argb8888(&semi, 20, 10, 10, 5, &mut resizer).unwrap();
    assert_eq!(&small_semi[0..4], &[25, 60, 110, 128]);

    // Mismatched source byte counts return an error rather than resizing invalid memory.
    assert!(resize_premultiplied_argb8888(&solid[..16], 20, 10, 10, 5, &mut resizer).is_err());
}

fn assert_apng_fraction_delay_us(
    dir: &Path,
    name: &str,
    frames: &[([u8; 4], u16, u16)],
    expected_us: u32,
) -> DecodedFirstFrame
{
    let apng_path = dir.join(name);
    fs::write(&apng_path, build_synthetic_apng_2x2_fraction(frames)).unwrap();

    let rec_state = Arc::new(Mutex::new(RecordedStream::default()));
    let (first, worker) = FrameDecoder::open_stream(
        &apng_path,
        500,
        0,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&rec_state),
        }),
    )
    .expect("APNG should stream");
    if let Some(handle) = worker
    {
        handle.join().unwrap();
    }

    assert_eq!(first.delay_us, expected_us);
    let rec = rec_state.lock().unwrap();
    assert!(rec.finished);
    assert!(rec.finish_err.is_none());
    assert_eq!(rec.delays_us, vec![expected_us; frames.len()]);
    first
}

#[test]
fn decode_preserves_60fps_apng_microsecond_delays()
{
    let temp = TempDecodeDir::new("apng_60fps");
    let first = assert_apng_fraction_delay_us(
        temp.path(),
        "sixty_fps.apng",
        &[
            ([255, 0, 0, 255], 1, 60),
            ([0, 255, 0, 255], 1, 60),
            ([0, 0, 255, 255], 1, 60),
        ],
        16_667,
    );
    assert_eq!(first.delay_ms, 17);
}

#[test]
fn decode_preserves_120fps_and_240fps_apng_microsecond_delays()
{
    let temp = TempDecodeDir::new("apng_high_fps");
    for (name, num, den, expected_us) in [
        ("fps120.apng", 1_u16, 120_u16, 8_333_u32),
        ("fps240.apng", 1_u16, 240_u16, 4_167_u32),
    ]
    {
        let _ = assert_apng_fraction_delay_us(
            temp.path(),
            name,
            &[([255, 0, 0, 255], num, den), ([0, 255, 0, 255], num, den)],
            expected_us,
        );
    }
}

#[test]
fn decode_preserves_frame_order_across_parallel_workers()
{
    let temp = TempDecodeDir::new("parallel_order");
    let frame_spec: Vec<([u8; 4], u16, u16)> = (0..20_u8)
        .map(|i| ([i * 10, i * 5 + 20, 250 - i * 8, 255], 1, 120))
        .collect();
    let apng_path = temp.path().join("ordered_20_120fps.apng");
    fs::write(
        &apng_path,
        build_synthetic_apng_fraction(64, 64, &frame_spec),
    )
    .unwrap();

    let rec_state = Arc::new(Mutex::new(RecordedStream::default()));
    let (first, worker) = FrameDecoder::open_stream(
        &apng_path,
        32,
        0,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&rec_state),
        }),
    )
    .expect("20-frame downscaled APNG should decode cleanly");
    if let Some(handle) = worker
    {
        handle.join().unwrap();
    }

    assert_eq!((first.width, first.height), (32, 32));
    let rec = rec_state.lock().unwrap();
    assert!(rec.finished);
    assert!(!rec.cancelled);
    assert!(rec.finish_err.is_none());
    assert_eq!(rec.frames.len(), 20);
    assert_eq!(rec.delays_us, vec![8_333; 20]);

    for (idx, payload) in rec.frames.iter().enumerate()
    {
        let raw = decompress_size_prepended(payload).unwrap();
        assert_eq!(raw.len(), 32 * 32 * 4);
        let [r, g, b, a] = frame_spec[idx].0;
        assert_eq!(&raw[0..4], &[b, g, r, a], "frame {idx} out of order");
    }
}

#[test]
fn decode_stops_pipeline_cleanly_on_midstream_cancel_and_sink_error()
{
    let temp = TempDecodeDir::new("midstream_stop");
    let gif_path = temp.path().join("many.gif");
    let gif_frames: Vec<_> = (0..25_u8)
        .map(|idx| {
            ImgFrame::from_parts(
                RgbaImage::from_pixel(32, 32, image::Rgba([idx * 10, 40, 80, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(20, 1),
            )
        })
        .collect();
    GifEncoder::new(File::create(&gif_path).unwrap())
        .encode_frames(gif_frames)
        .unwrap();

    // 1. Mid-stream cancellation after frame 3 finishes cleanly without stalling.
    struct CancelOnThirdFrame
    {
        inner: RecordingSink,
        cancel: Arc<AtomicBool>,
        seen: usize,
    }

    impl FrameStreamSink for CancelOnThirdFrame
    {
        fn on_first_frame(&mut self, frame: &DecodedFirstFrame) -> Result<(), CacheError>
        {
            self.seen = 1;
            self.inner.on_first_frame(frame)
        }

        fn on_frame(&mut self, frame: EncodedFrame) -> Result<(), CacheError>
        {
            self.inner.on_frame(frame)?;
            self.seen += 1;
            if self.seen >= 3
            {
                self.cancel.store(true, Ordering::Release);
            }
            Ok(())
        }

        fn on_finish(self: Box<Self>, cancelled: bool, result: Result<(), CacheError>)
        {
            Box::new(self.inner).on_finish(cancelled, result);
        }
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (_first, cancel_worker) = FrameDecoder::open_stream(
        &gif_path,
        16,
        0,
        Arc::clone(&cancel),
        Box::new(CancelOnThirdFrame {
            inner: RecordingSink {
                state: Arc::clone(&cancel_rec),
            },
            cancel,
            seen: 0,
        }),
    )
    .unwrap();
    if let Some(handle) = cancel_worker
    {
        handle.join().unwrap();
    }
    let cancel_guard = cancel_rec.lock().unwrap();
    assert!(cancel_guard.finished);
    assert!(cancel_guard.cancelled);
    assert!(cancel_guard.finish_err.is_none());
    assert!(cancel_guard.frames.len() < 25);

    // 2. Mid-stream sink write failure aborts reader/workers cleanly and surfaces the error.
    struct ErrorOnThirdFrame
    {
        inner: RecordingSink,
        seen: usize,
    }

    impl FrameStreamSink for ErrorOnThirdFrame
    {
        fn on_first_frame(&mut self, frame: &DecodedFirstFrame) -> Result<(), CacheError>
        {
            self.seen = 1;
            self.inner.on_first_frame(frame)
        }

        fn on_frame(&mut self, frame: EncodedFrame) -> Result<(), CacheError>
        {
            self.seen += 1;
            if self.seen == 3
            {
                return Err(CacheError::Codec {
                    path: PathBuf::from("synthetic.gif"),
                    frame_index: 2,
                    reason: String::from("Injected sink failure"),
                });
            }
            self.inner.on_frame(frame)
        }

        fn on_finish(self: Box<Self>, cancelled: bool, result: Result<(), CacheError>)
        {
            Box::new(self.inner).on_finish(cancelled, result);
        }
    }

    let err_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (_first2, err_worker) = FrameDecoder::open_stream(
        &gif_path,
        16,
        0,
        Arc::new(AtomicBool::new(false)),
        Box::new(ErrorOnThirdFrame {
            inner: RecordingSink {
                state: Arc::clone(&err_rec),
            },
            seen: 0,
        }),
    )
    .unwrap();
    if let Some(handle) = err_worker
    {
        handle.join().unwrap();
    }
    let err_guard = err_rec.lock().unwrap();
    assert!(err_guard.finished);
    assert!(!err_guard.cancelled);
    assert!(
        err_guard
            .finish_err
            .as_deref()
            .is_some_and(|msg| msg.contains("Injected sink failure"))
    );
}

#[derive(Clone, Copy)]
struct SyntheticPatchSpec
{
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    rgba: [u8; 4],
    delay_num: u16,
    delay_den: u16,
    dispose_op: u8,
    blend_op: u8,
}

fn png_test_helpers_encode(
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

#[test]
fn apng_packet_reader_and_compositor_match_image_crate_across_blend_and_dispose_modes()
{
    let temp = TempDecodeDir::new("apng_parity");
    let apng_path = temp.path().join("complex.apng");
    let patches = [
        SyntheticPatchSpec {
            x: 0,
            y: 0,
            w: 6,
            h: 6,
            rgba: [20, 40, 80, 255],
            delay_num: 1,
            delay_den: 60,
            dispose_op: 0, // None
            blend_op: 0,   // Source
        },
        SyntheticPatchSpec {
            x: 1,
            y: 1,
            w: 4,
            h: 4,
            rgba: [200, 10, 30, 128],
            delay_num: 1,
            delay_den: 120,
            dispose_op: 2, // Previous (reverts back to Frame 0 before Frame 2)
            blend_op: 1,   // Over
        },
        SyntheticPatchSpec {
            x: 2,
            y: 2,
            w: 3,
            h: 3,
            rgba: [10, 220, 40, 200],
            delay_num: 2,
            delay_den: 0,  // 0 => 100 (20 ms)
            dispose_op: 1, // Background (clears (2,2,3,3) before Frame 3)
            blend_op: 1,   // Over
        },
        SyntheticPatchSpec {
            x: 0,
            y: 0,
            w: 2,
            h: 2,
            rgba: [255, 255, 255, 180],
            delay_num: 1,
            delay_den: 240,
            dispose_op: 0,
            blend_op: 0, // Source
        },
    ];
    fs::write(
        &apng_path,
        png_test_helpers_encode(6, 6, 6, &[], None, &patches),
    )
    .unwrap();

    let ref_frames: Vec<_> = PngDecoder::new(BufReader::new(File::open(&apng_path).unwrap()))
        .unwrap()
        .apng()
        .unwrap()
        .into_frames()
        .map(|f| f.unwrap().into_buffer().into_raw())
        .collect();

    let mut reader =
        ApngPacketReader::try_open(&apng_path, BufReader::new(File::open(&apng_path).unwrap()))
            .unwrap()
            .unwrap_or_else(|_| panic!("valid APNG must open cleanly"));
    let header = reader.header();
    let mut compositor = ApngCompositor::new(header.canvas_w, header.canvas_h).unwrap();

    let mut actual_frames = Vec::new();
    let mut actual_delays = Vec::new();
    while let Some(packet) = reader.next_packet().unwrap()
    {
        let patch = decode_apng_packet(&apng_path, &header, packet).unwrap();
        let (delay_us, rgba) = compositor.apply_patch_rgba(patch).unwrap();
        actual_delays.push(delay_us);
        actual_frames.push(rgba);
    }

    assert_eq!(actual_frames.len(), ref_frames.len());
    assert_eq!(actual_delays, vec![16_667, 8_333, 20_000, 4_167]);
    for (idx, (actual, expected)) in actual_frames.iter().zip(ref_frames.iter()).enumerate()
    {
        assert_eq!(actual, expected, "canvas mismatch at frame {idx}");
    }
}

#[test]
fn apng_packet_reader_handles_indexed_palette_hidden_thumbnail_and_static_rewind()
{
    let temp = TempDecodeDir::new("apng_palette_thumb");
    let apng_path = temp.path().join("indexed_hidden.apng");

    // 2 palette colors: idx 0 = [255, 0, 0] with alpha 128, idx 1 = [0, 200, 50] with alpha 255.
    let plte = [255_u8, 0, 0, 0, 200, 50];
    let trns = [128_u8, 255];
    // Hidden 4x4 fallback IDAT thumbnail preceding frame 0's fcTL.
    let thumb_raw = [0_u8, 1, 1, 1, 1].repeat(4);
    let patches = [
        SyntheticPatchSpec {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
            rgba: [0, 0, 0, 0], // palette idx 0
            delay_num: 1,
            delay_den: 60,
            dispose_op: 0,
            blend_op: 0,
        },
        SyntheticPatchSpec {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
            rgba: [1, 0, 0, 0], // palette idx 1
            delay_num: 1,
            delay_den: 60,
            dispose_op: 0,
            blend_op: 1,
        },
    ];
    let encoded = png_test_helpers_encode(
        4,
        4,
        3, // indexed
        &[(b"PLTE", &plte), (b"tRNS", &trns)],
        Some(&thumb_raw),
        &patches,
    );
    fs::write(&apng_path, encoded).unwrap();

    let ref_frames: Vec<_> = PngDecoder::new(BufReader::new(File::open(&apng_path).unwrap()))
        .unwrap()
        .apng()
        .unwrap()
        .into_frames()
        .map(|f| f.unwrap().into_buffer().into_raw())
        .collect();

    let mut reader =
        ApngPacketReader::try_open(&apng_path, BufReader::new(File::open(&apng_path).unwrap()))
            .unwrap()
            .unwrap_or_else(|_| panic!("indexed APNG should open"));
    let header = reader.header();
    let mut compositor = ApngCompositor::new(header.canvas_w, header.canvas_h).unwrap();
    let mut actual_frames = Vec::new();
    while let Some(packet) = reader.next_packet().unwrap()
    {
        let patch = decode_apng_packet(&apng_path, &header, packet).unwrap();
        let (_delay, rgba) = compositor.apply_patch_rgba(patch).unwrap();
        actual_frames.push(rgba);
    }
    assert_eq!(actual_frames, ref_frames);

    // Non-animated PNG rewinds cleanly back to byte offset 0.
    let static_path = temp.path().join("static.png");
    let static_img = RgbaImage::from_pixel(8, 8, image::Rgba([9, 80, 200, 255]));
    PngEncoder::new(File::create(&static_path).unwrap())
        .write_image(&static_img, 8, 8, image::ExtendedColorType::Rgba8)
        .unwrap();

    let fallback = ApngPacketReader::try_open(
        &static_path,
        BufReader::new(File::open(&static_path).unwrap()),
    )
    .unwrap();
    let mut rewound = match fallback
    {
        Ok(_) => panic!("static PNG must not be identified as APNG"),
        Err(buf) => buf,
    };
    assert_eq!(rewound.stream_position().unwrap(), 0);
    assert!(PngDecoder::new(rewound).is_ok());

    // W3C APNG spec: Background disposal clears region before a subsequent Previous snapshot.
    let bg_then_prev = temp.path().join("bg_then_prev.apng");
    let seq_patches = [
        SyntheticPatchSpec {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
            rgba: [10, 20, 30, 255],
            delay_num: 1,
            delay_den: 60,
            dispose_op: 0, // None
            blend_op: 0,
        },
        SyntheticPatchSpec {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
            rgba: [200, 50, 60, 255],
            delay_num: 1,
            delay_den: 60,
            dispose_op: 1, // Background: clears (1,1,2,2) to [0,0,0,0] before frame 2
            blend_op: 0,
        },
        SyntheticPatchSpec {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
            rgba: [0, 200, 0, 255],
            delay_num: 1,
            delay_den: 60,
            dispose_op: 2, // Previous: must restore [0,0,0,0] before frame 3, not frame 1
            blend_op: 0,
        },
        SyntheticPatchSpec {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            rgba: [255, 255, 255, 255],
            delay_num: 1,
            delay_den: 60,
            dispose_op: 0,
            blend_op: 0,
        },
    ];
    fs::write(
        &bg_then_prev,
        png_test_helpers_encode(4, 4, 6, &[], None, &seq_patches),
    )
    .unwrap();
    let mut spec_reader = ApngPacketReader::try_open(
        &bg_then_prev,
        BufReader::new(File::open(&bg_then_prev).unwrap()),
    )
    .unwrap()
    .unwrap_or_else(|_| panic!("spec APNG should open"));
    let spec_header = spec_reader.header();
    let mut spec_comp = ApngCompositor::new(spec_header.canvas_w, spec_header.canvas_h).unwrap();
    let mut frames_out = Vec::new();
    while let Some(pkt) = spec_reader.next_packet().unwrap()
    {
        let decoded = decode_apng_packet(&bg_then_prev, &spec_header, pkt).unwrap();
        frames_out.push(spec_comp.apply_patch_rgba(decoded).unwrap().1);
    }
    let center_offset = (4 + 1) * 4;
    assert_eq!(
        &frames_out[3][center_offset..center_offset + 4],
        &[0, 0, 0, 0]
    );
}

fn build_multi_frame_subrect_apng(frame_count: usize) -> Vec<u8>
{
    let mut patches = Vec::with_capacity(frame_count);
    for idx in 0..frame_count
    {
        let i = u8::try_from(idx).unwrap_or(255);
        patches.push(
            if idx == 0
            {
                SyntheticPatchSpec {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 8,
                    rgba: [10, 20, 30, 255],
                    delay_num: 1,
                    delay_den: 60,
                    dispose_op: 0,
                    blend_op: 0,
                }
            }
            else
            {
                SyntheticPatchSpec {
                    x: u32::from(i % 5),
                    y: u32::from((i / 2) % 5),
                    w: 3,
                    h: 3,
                    rgba: [
                        20_u8.wrapping_add(i.wrapping_mul(9)),
                        40_u8.wrapping_add(i.wrapping_mul(13)),
                        60_u8.wrapping_add(i.wrapping_mul(17)),
                        200,
                    ],
                    delay_num: 1,
                    delay_den: 120,
                    dispose_op: i % 3,
                    blend_op: i % 2,
                }
            },
        );
    }
    png_test_helpers_encode(8, 8, 6, &[], None, &patches)
}

#[test]
fn apng_stream_decodes_frames_in_parallel_preserving_order_compositing_and_max_frames()
{
    let temp = TempDecodeDir::new("apng_parallel_stream");
    let apng_path = temp.path().join("multi.apng");
    let total_frames = 24_usize;
    fs::write(&apng_path, build_multi_frame_subrect_apng(total_frames)).unwrap();

    // Compute reference premultiplied ARGB8888 frames sequentially.
    let mut seq_reader =
        ApngPacketReader::try_open(&apng_path, BufReader::new(File::open(&apng_path).unwrap()))
            .unwrap()
            .unwrap_or_else(|_| panic!("synthetic APNG should open"));
    let header = seq_reader.header();
    let mut compositor = ApngCompositor::new(header.canvas_w, header.canvas_h).unwrap();
    let mut reference_argb = Vec::with_capacity(total_frames);
    let mut reference_delays = Vec::with_capacity(total_frames);
    while let Some(packet) = seq_reader.next_packet().unwrap()
    {
        let patch = decode_apng_packet(&apng_path, &header, packet).unwrap();
        let (delay_us, mut straight_rgba) = compositor.apply_patch_rgba(patch).unwrap();
        straight_rgba_to_premultiplied_argb8888(&mut straight_rgba);
        reference_argb.push(straight_rgba);
        reference_delays.push(delay_us);
    }
    assert_eq!(reference_argb.len(), total_frames);

    // 1. Full stream via `FrameDecoder::open_stream` matches sequential compositor frame-for-frame.
    let rec_state = Arc::new(Mutex::new(RecordedStream::default()));
    let (first, worker) = FrameDecoder::open_stream(
        &apng_path,
        8,
        0,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&rec_state),
        }),
    )
    .expect("parallel APNG stream should start");
    assert_eq!(
        decompress_size_prepended(&first.lz4_payload).unwrap(),
        reference_argb[0]
    );
    worker
        .expect("multi-frame APNG should spawn worker")
        .join()
        .unwrap();

    let rec = rec_state.lock().unwrap();
    assert!(rec.finished);
    assert!(!rec.cancelled);
    assert!(rec.finish_err.is_none());
    assert_eq!(rec.delays_us, reference_delays);
    assert_eq!(rec.frames.len(), total_frames);
    for (idx, lz4_bytes) in rec.frames.iter().enumerate()
    {
        let actual_argb = decompress_size_prepended(lz4_bytes).unwrap();
        assert_eq!(
            actual_argb, reference_argb[idx],
            "parallel stream frame {idx} drifted from sequential compositor"
        );
    }
    drop(rec);

    // 2. `--max-frames` limits streamed APNG count cleanly without decoding extras.
    let capped_state = Arc::new(Mutex::new(RecordedStream::default()));
    let (_capped_first, capped_worker) = FrameDecoder::open_stream(
        &apng_path,
        8,
        7,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&capped_state),
        }),
    )
    .expect("capped APNG stream should start");
    capped_worker.unwrap().join().unwrap();
    let capped = capped_state.lock().unwrap();
    assert!(capped.finished);
    assert!(capped.finish_err.is_none());
    assert_eq!(capped.frames.len(), 7);
    for idx in 0..7
    {
        assert_eq!(
            decompress_size_prepended(&capped.frames[idx]).unwrap(),
            reference_argb[idx]
        );
    }
}

#[test]
fn apng_stream_stops_cleanly_on_early_cancel_corrupt_fdat_and_sink_error()
{
    let temp = TempDecodeDir::new("apng_parallel_abort");
    let apng_path = temp.path().join("many.apng");
    fs::write(&apng_path, build_multi_frame_subrect_apng(24)).unwrap();

    struct CancelOnThirdApngFrame
    {
        inner: RecordingSink,
        cancel: Arc<AtomicBool>,
        seen: usize,
    }

    impl FrameStreamSink for CancelOnThirdApngFrame
    {
        fn on_first_frame(&mut self, frame: &DecodedFirstFrame) -> Result<(), CacheError>
        {
            self.seen = 1;
            self.inner.on_first_frame(frame)
        }

        fn on_frame(&mut self, frame: EncodedFrame) -> Result<(), CacheError>
        {
            self.inner.on_frame(frame)?;
            self.seen += 1;
            if self.seen >= 3
            {
                self.cancel.store(true, Ordering::Release);
            }
            Ok(())
        }

        fn on_finish(self: Box<Self>, cancelled: bool, result: Result<(), CacheError>)
        {
            Box::new(self.inner).on_finish(cancelled, result);
        }
    }

    // 1. Cooperative cancel stops reader, packet workers, compositor, and sink cleanly.
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (_first, cancel_worker) = FrameDecoder::open_stream(
        &apng_path,
        8,
        0,
        Arc::clone(&cancel),
        Box::new(CancelOnThirdApngFrame {
            inner: RecordingSink {
                state: Arc::clone(&cancel_rec),
            },
            cancel,
            seen: 0,
        }),
    )
    .unwrap();
    cancel_worker.unwrap().join().unwrap();
    let cancel_guard = cancel_rec.lock().unwrap();
    assert!(cancel_guard.finished);
    assert!(cancel_guard.cancelled);
    assert!(cancel_guard.finish_err.is_none());
    assert!(cancel_guard.frames.len() < 24);
    drop(cancel_guard);

    // 2. Sink error aborts parallel APNG workers and surfaces the error to `on_finish`.
    struct ErrorOnFourthApngFrame
    {
        inner: RecordingSink,
        seen: usize,
    }

    impl FrameStreamSink for ErrorOnFourthApngFrame
    {
        fn on_first_frame(&mut self, frame: &DecodedFirstFrame) -> Result<(), CacheError>
        {
            self.seen = 1;
            self.inner.on_first_frame(frame)
        }

        fn on_frame(&mut self, frame: EncodedFrame) -> Result<(), CacheError>
        {
            self.seen += 1;
            if self.seen == 4
            {
                return Err(CacheError::Codec {
                    path: PathBuf::from("synthetic.apng"),
                    frame_index: 3,
                    reason: String::from("Injected APNG sink error"),
                });
            }
            self.inner.on_frame(frame)
        }

        fn on_finish(self: Box<Self>, cancelled: bool, result: Result<(), CacheError>)
        {
            Box::new(self.inner).on_finish(cancelled, result);
        }
    }

    let err_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (_first2, err_worker) = FrameDecoder::open_stream(
        &apng_path,
        8,
        0,
        Arc::new(AtomicBool::new(false)),
        Box::new(ErrorOnFourthApngFrame {
            inner: RecordingSink {
                state: Arc::clone(&err_rec),
            },
            seen: 0,
        }),
    )
    .unwrap();
    err_worker.unwrap().join().unwrap();
    let err_guard = err_rec.lock().unwrap();
    assert!(err_guard.finished);
    assert!(!err_guard.cancelled);
    assert!(
        err_guard
            .finish_err
            .as_deref()
            .is_some_and(|msg| msg.contains("Injected APNG sink error"))
    );
    drop(err_guard);

    // 3. Corrupt `fdAT` payload in frame 3 surfaces worker decode failure without hanging.
    let mut corrupt_bytes = build_multi_frame_subrect_apng(10);
    let mut pos = 8_usize;
    let mut fdat_seen = 0_usize;
    while pos + 12 <= corrupt_bytes.len()
    {
        let len = u32::from_be_bytes([
            corrupt_bytes[pos],
            corrupt_bytes[pos + 1],
            corrupt_bytes[pos + 2],
            corrupt_bytes[pos + 3],
        ]) as usize;
        let tag = &corrupt_bytes[pos + 4..pos + 8];
        if tag == b"fdAT"
        {
            fdat_seen += 1;
            if fdat_seen == 3 && len > 6
            {
                // Flip the zlib CMF byte immediately after the 4-byte `fdAT` sequence prefix.
                corrupt_bytes[pos + 12] ^= 0xFF;
                break;
            }
        }
        pos += 12 + len;
    }
    let corrupt_path = temp.path().join("corrupt_fdat.apng");
    fs::write(&corrupt_path, &corrupt_bytes).unwrap();

    let corrupt_rec = Arc::new(Mutex::new(RecordedStream::default()));
    let (_first3, corrupt_worker) = FrameDecoder::open_stream(
        &corrupt_path,
        8,
        0,
        Arc::new(AtomicBool::new(false)),
        Box::new(RecordingSink {
            state: Arc::clone(&corrupt_rec),
        }),
    )
    .expect("frame 0 is valid so startup succeeds before worker fails on frame 3");
    corrupt_worker.unwrap().join().unwrap();
    let corrupt_guard = corrupt_rec.lock().unwrap();
    assert!(corrupt_guard.finished);
    assert!(!corrupt_guard.cancelled);
    assert!(corrupt_guard.finish_err.is_some());
}
