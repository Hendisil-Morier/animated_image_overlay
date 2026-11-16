//! Integration tests verifying APNG chunk splitting, W3C compositing, fractional delays,
//! indexed palettes, hidden fallback frames, parallel APNG decoding, and abort handling.

#[path = "support/mod.rs"]
mod support;

use std::{
    fs::{self, File},
    io::{BufReader, Seek},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use animated_image_widget::{
    decode::{
        DecodedFirstFrame, EncodedFrame, FrameDecoder, FrameStreamSink,
        apng::{ApngCompositor, ApngPacketReader, decode_apng_packet},
        straight_rgba_to_premultiplied_argb8888,
    },
    error::CacheError,
};
use image::{
    AnimationDecoder, ImageEncoder, RgbaImage,
    codecs::png::{PngDecoder, PngEncoder},
};
use lz4_flex::decompress_size_prepended;
use support::{
    RecordedStream, RecordingSink, SyntheticPatchSpec, TempDecodeDir, encode_solid_apng_fraction,
    encode_synthetic_apng,
};

fn assert_apng_fraction_delay_us(
    dir: &Path,
    name: &str,
    frames: &[([u8; 4], u16, u16)],
    expected_us: u32,
) -> DecodedFirstFrame
{
    let apng_path = dir.join(name);
    fs::write(&apng_path, encode_solid_apng_fraction(2, 2, frames)).unwrap();

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
    assert!(!rec.cancelled);
    assert!(rec.finish_err.is_none());
    assert_eq!(rec.delays_us, vec![expected_us; frames.len()]);
    assert_eq!(rec.delays.len(), frames.len());
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
        encode_synthetic_apng(6, 6, 6, &[], None, &patches),
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
    let encoded = encode_synthetic_apng(
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
        encode_synthetic_apng(4, 4, 6, &[], None, &seq_patches),
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
    encode_synthetic_apng(8, 8, 6, &[], None, &patches)
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
