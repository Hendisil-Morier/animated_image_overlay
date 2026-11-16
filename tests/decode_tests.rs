//! Integration tests verifying APNG, GIF, animated WebP, and static decoding,
//! `--max-frames` limits, single-open extraction, parallel worker ordering, and cancellation.

#[path = "support/apng.rs"]
mod apng_support;
#[path = "support/mod.rs"]
mod support;

use std::{
    fs::{self, File},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use animated_image_widget::{
    decode::{DecodedFirstFrame, EncodedFrame, FrameDecoder, FrameStreamSink},
    error::CacheError,
};
use apng_support::encode_solid_apng_fraction;
use image::{
    Delay, Frame as ImgFrame, ImageEncoder, RgbaImage,
    codecs::{bmp::BmpEncoder, gif::GifEncoder, png::PngEncoder},
};
use lz4_flex::decompress_size_prepended;
use support::{RecordedStream, RecordingSink, TempDecodeDir};
use webp_animation::Encoder as WebpEncoder;

fn encode_apng_2x2_ms(frames_rgba: &[([u8; 4], u16)]) -> Vec<u8>
{
    let fractional: Vec<([u8; 4], u16, u16)> = frames_rgba
        .iter()
        .map(|&(rgba, delay_ms)| (rgba, delay_ms, 1000))
        .collect();
    encode_solid_apng_fraction(2, 2, &fractional)
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
    let apng_bytes = encode_apng_2x2_ms(&[
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
    assert!(!rec.cancelled);
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
    let apng_bytes = encode_apng_2x2_ms(&[([100, 150, 200, 255], 40), ([10, 20, 30, 255], 60)]);
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
fn decode_preserves_frame_order_across_parallel_workers()
{
    let temp = TempDecodeDir::new("parallel_order");
    let frame_spec: Vec<([u8; 4], u16, u16)> = (0..20_u8)
        .map(|i| ([i * 10, i * 5 + 20, 250 - i * 8, 255], 1, 120))
        .collect();
    let apng_path = temp.path().join("ordered_20_120fps.apng");
    fs::write(&apng_path, encode_solid_apng_fraction(64, 64, &frame_spec)).unwrap();

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
