//! Integration tests verifying CLI flag defaults, numeric bounds, and file validation.

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use animated_image_widget::{
    cli::{CliArgs, CliOutcome},
    error::OverlayError,
};

fn unique_temp_path(name: &str) -> PathBuf
{
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("aiw_cli_{name}_{}_{nanos}", std::process::id()))
}

#[test]
fn cli_applies_default_flags()
{
    let outcome = CliArgs::parse_flags_from(["overlay", "sample.apng"])
        .expect("valid default CLI flags should parse");

    let CliOutcome::Run(args) = outcome
    else
    {
        panic!("expected CliOutcome::Run");
    };

    assert_eq!(args.files, vec![PathBuf::from("sample.apng")]);
    assert_eq!(args.max_frames, 0);
    assert_eq!(args.base_width, 500);
    assert_eq!(args.monitor, "0");
    assert_eq!(args.scale, None);
    assert_eq!(args.ram_cache_mb, 128);
    assert_eq!(args.display_cache_mb, 64);
    assert_eq!(args.video_cache_mb, 512);
    assert_eq!(args.gpu, "auto");
    assert_eq!(args.audio, None);
    assert!(!args.mute);
    assert_eq!(args.volume, None);
    assert_eq!(args.max_fps, 240);
    assert_eq!(
        args.ram_cache_bytes().expect("default byte budget fits"),
        128 * 1024 * 1024
    );
    assert_eq!(
        args.display_cache_bytes()
            .expect("default display byte budget fits"),
        64 * 1024 * 1024
    );
    assert_eq!(
        args.video_cache_bytes()
            .expect("default video byte budget fits"),
        512 * 1024 * 1024
    );
    assert!(!args.edit);
    assert!(!args.stats);
    assert!(!args.debug);
}

#[test]
fn cli_parses_explicit_flags()
{
    let outcome = CliArgs::parse_flags_from([
        "overlay",
        "sprite.webp",
        "-n",
        "120",
        "--base-width",
        "320",
        "-m",
        "DP-2",
        "-s",
        "0.75",
        "-c",
        "ram=256",
        "--gpu",
        "cuda",
        "-f",
        "120",
        "--edit",
        "--stats",
        "--debug",
    ])
    .expect("explicit CLI flags should parse");

    let CliOutcome::Run(args) = outcome
    else
    {
        panic!("expected CliOutcome::Run");
    };

    assert_eq!(args.files, vec![PathBuf::from("sprite.webp")]);
    assert_eq!(args.max_frames, 120);
    assert_eq!(args.base_width, 320);
    assert_eq!(args.monitor, "DP-2");
    assert_eq!(args.scale, Some(0.75));
    assert_eq!(args.ram_cache_mb, 256);
    assert_eq!(args.gpu, "cuda");
    assert_eq!(args.max_fps, 120);
    assert_eq!(
        args.ram_cache_bytes().expect("explicit byte budget fits"),
        256 * 1024 * 1024
    );
    assert!(args.edit);
    assert!(args.stats);
    assert!(args.debug);

    let zero_outcome =
        CliArgs::parse_flags_from(["overlay", "sprite.webp", "-c", "ram=0", "--max-fps", "60"])
            .expect("-c ram=0 and --max-fps 60 should parse");
    assert!(matches!(
        zero_outcome,
        CliOutcome::Run(zero_args)
            if zero_args.ram_cache_bytes().ok() == Some(0) && zero_args.max_fps == 60
    ));
}

#[test]
fn cli_rejects_invalid_base_width_and_scales()
{
    for bad_args in [
        ["overlay", "img.png", "--base-width", "0"].as_slice(),
        ["overlay", "img.png", "--max-frames", "-1"].as_slice(),
        ["overlay", "img.png", "--max-fps", "0"].as_slice(),
        ["overlay", "img.png", "-f", "1001"].as_slice(),
        ["overlay", "img.png", "-f", "-1"].as_slice(),
        ["overlay", "img.png", "--scale", "0"].as_slice(),
        ["overlay", "img.png", "-s", "-0.5"].as_slice(),
        ["overlay", "img.png", "--scale", "nan"].as_slice(),
        ["overlay", "img.png", "--scale", "inf"].as_slice(),
        ["overlay", "img.png", "--scale", "-inf"].as_slice(),
        ["overlay", "img.png", "--monitor", "   "].as_slice(),
        ["overlay", "img.png", "--gpu", ""].as_slice(),
        ["overlay", "img.png", "--gpu", "   "].as_slice(),
        ["overlay", "img.png", "--gpu", "asccasdweenn"].as_slice(),
    ]
    {
        let err = CliArgs::parse_flags_from(bad_args.iter().copied())
            .expect_err("invalid CLI parameter should be rejected");
        assert!(
            matches!(err, OverlayError::CliValidation(_)),
            "expected CliValidation for {bad_args:?}, got {err:?}"
        );
    }
}

#[test]
fn cli_validates_missing_versus_existing_input_file()
{
    let missing = unique_temp_path("missing.apng");
    let err = CliArgs::parse_and_validate_from(["overlay", missing.to_str().unwrap()])
        .expect_err("missing input file must fail validation");
    assert!(matches!(err, OverlayError::MissingInputFile(p) if p == missing));

    let existing = unique_temp_path("existing.apng");
    fs::write(&existing, b"stub").expect("temp test file should write");

    let parsed = CliArgs::parse_and_validate_from(["overlay", existing.to_str().unwrap()])
        .expect("existing regular file should pass validation");
    assert!(matches!(parsed, CliOutcome::Run(args) if args.files == vec![existing.clone()]));

    let _ = fs::remove_file(&existing);
}

#[test]
fn cli_handles_help_flag_as_early_exit()
{
    let outcome = CliArgs::parse_and_validate_from(["overlay", "--help"])
        .expect("--help should return EarlyExit rather than error");
    let CliOutcome::EarlyExit(text) = outcome
    else
    {
        panic!("expected CliOutcome::EarlyExit");
    };

    for expected in [
        "--base-width",
        "--cap",
        "--gpu",
        "--max-fps",
        "--edit",
        "--stats",
        "--debug",
        "on corner handle",
        "Triple-click",
        "Esc",
        "hjkl",
        "(12 px)",
        "+ / -",
        "repeatable",
        "r / R",
        "Rotate 90 degrees",
        "[ / ]",
        "Rotate 5 degrees",
        "Reset rotation",
        "f / F",
        "Flip horizontally",
        "Space",
        "q",
    ]
    {
        assert!(
            text.contains(expected),
            "--help output missing {expected:?}:\n{text}"
        );
    }
}

#[test]
fn cli_parses_and_validates_cap_suboptions()
{
    let CliOutcome::Run(ram_only) =
        CliArgs::parse_flags_from(["overlay", "a.apng", "-c", "ram=10"]).unwrap()
    else
    {
        panic!("expected Run");
    };
    assert_eq!(ram_only.ram_cache_mb, 10);
    assert_eq!(ram_only.display_cache_mb, 64);

    let CliOutcome::Run(display_only) =
        CliArgs::parse_flags_from(["overlay", "a.apng", "--cap", "display=32"]).unwrap()
    else
    {
        panic!("expected Run");
    };
    assert_eq!(display_only.ram_cache_mb, 128);
    assert_eq!(display_only.display_cache_mb, 32);

    let CliOutcome::Run(both) = CliArgs::parse_flags_from([
        "overlay",
        "a.apng",
        "-c",
        "ram=256",
        "-c",
        "ram=0,display=128",
    ])
    .unwrap()
    else
    {
        panic!("expected Run");
    };
    assert_eq!(both.ram_cache_bytes().unwrap(), 0);
    assert_eq!(both.display_cache_bytes().unwrap(), 128 * 1024 * 1024);

    let CliOutcome::Run(repeated) =
        CliArgs::parse_flags_from(["overlay", "a.apng", "-c", "ram=16", "-c", "display=0"])
            .unwrap()
    else
    {
        panic!("expected Run");
    };
    assert_eq!(repeated.ram_cache_mb, 16);
    assert_eq!(repeated.display_cache_bytes().unwrap(), 0);

    let max_mb = usize::MAX.to_string();
    let bad_overflow = format!("display={max_mb}");
    let bad_ram_overflow = format!("ram={max_mb}");
    for bad in [
        ["overlay", "a.apng", "-c", ""].as_slice(),
        ["overlay", "a.apng", "-c", "ram"].as_slice(),
        ["overlay", "a.apng", "-c", "ram=-1"].as_slice(),
        ["overlay", "a.apng", "-c", "ram=10,"].as_slice(),
        ["overlay", "a.apng", "-c", "unknown=10"].as_slice(),
        ["overlay", "a.apng", "-c", "display=abc"].as_slice(),
        ["overlay", "a.apng", "-c", "display=-1"].as_slice(),
        ["overlay", "a.apng", "-c", "video=-1"].as_slice(),
        ["overlay", "a.apng", "-c", bad_overflow.as_str()].as_slice(),
        ["overlay", "a.apng", "-c", bad_ram_overflow.as_str()].as_slice(),
    ]
    {
        assert!(
            matches!(
                CliArgs::parse_flags_from(bad.iter().copied()),
                Err(OverlayError::CliValidation(_))
            ),
            "expected CliValidation for {bad:?}"
        );
    }

    let CliOutcome::Run(audio_flags) = CliArgs::parse_flags_from([
        "overlay",
        "clip.mp4",
        "--audio",
        "track.mp3",
        "--mute",
        "--volume",
        "0.35",
        "-c",
        "video=0",
    ])
    .expect("audio and video cache flags should parse")
    else
    {
        panic!("expected Run");
    };
    assert_eq!(audio_flags.audio, Some(PathBuf::from("track.mp3")));
    assert!(audio_flags.mute);
    assert_eq!(audio_flags.volume, Some(0.35));
    assert_eq!(audio_flags.video_cache_mb, 0);

    for bad_vol in ["-0.1", "1.1", "nan", "inf"]
    {
        assert!(matches!(
            CliArgs::parse_flags_from(["overlay", "clip.mp4", "--volume", bad_vol]),
            Err(OverlayError::CliValidation(_))
        ));
    }

    let missing_img = unique_temp_path("video_present.mp4");
    fs::write(&missing_img, b"stub").expect("temp media file written");
    let missing_aud = unique_temp_path("absent_audio.wav");
    let audio_err = CliArgs::parse_and_validate_from([
        "overlay",
        missing_img.to_str().unwrap(),
        "--audio",
        missing_aud.to_str().unwrap(),
    ])
    .expect_err("missing --audio file must fail validation");
    assert!(matches!(audio_err, OverlayError::MissingAudioFile(p) if p == missing_aud));
    let _ = fs::remove_file(&missing_img);
}

#[test]
fn cli_parses_gpu_options()
{
    let CliOutcome::Run(none_args) =
        CliArgs::parse_flags_from(["overlay", "video.mp4", "--gpu", "none"]).unwrap()
    else
    {
        panic!("expected Run");
    };
    assert_eq!(none_args.gpu, "none");

    let CliOutcome::Run(vaapi_args) =
        CliArgs::parse_flags_from(["overlay", "video.mp4", "--gpu", "vaapi"]).unwrap()
    else
    {
        panic!("expected Run");
    };
    assert_eq!(vaapi_args.gpu, "vaapi");

    let err = CliArgs::parse_flags_from(["overlay", "video.mp4", "--gpu", "asccasdweenn"])
        .expect_err("unknown gpu backend should be rejected");
    match err
    {
        OverlayError::CliValidation(msg) =>
        {
            assert!(msg.contains("Unknown --gpu mode 'asccasdweenn'"));
            assert!(msg.contains("Recognized options:"));
        }
        other => panic!("expected CliValidation, got {other:?}"),
    }
}

#[test]
fn cli_parses_multiple_files_and_rejects_audio_with_multi_file()
{
    let outcome = CliArgs::parse_flags_from(["overlay", "first.png", "second.gif", "third.mp4"])
        .expect("multiple positional files should parse");
    let CliOutcome::Run(args) = outcome
    else
    {
        panic!("expected Run");
    };
    assert_eq!(
        args.files,
        vec![
            PathBuf::from("first.png"),
            PathBuf::from("second.gif"),
            PathBuf::from("third.mp4"),
        ]
    );

    let f1 = unique_temp_path("multi_1.png");
    let f2 = unique_temp_path("multi_2.png");
    fs::write(&f1, b"stub").unwrap();
    fs::write(&f2, b"stub").unwrap();

    let valid_multi =
        CliArgs::parse_and_validate_from(["overlay", f1.to_str().unwrap(), f2.to_str().unwrap()])
            .expect("multiple existing files should pass validation");
    let CliOutcome::Run(multi_args) = valid_multi
    else
    {
        panic!("expected Run");
    };
    assert_eq!(multi_args.files.len(), 2);

    let audio_path = unique_temp_path("test_audio.mp3");
    fs::write(&audio_path, b"stub").unwrap();

    let err = CliArgs::parse_and_validate_from([
        "overlay",
        f1.to_str().unwrap(),
        f2.to_str().unwrap(),
        "--audio",
        audio_path.to_str().unwrap(),
    ])
    .expect_err("audio flag with multiple files must fail validation");

    assert!(matches!(
        err,
        OverlayError::CliValidation(msg) if msg.contains("--audio cannot be used with multiple files")
    ));

    let missing = unique_temp_path("multi_missing.png");
    let missing_err = CliArgs::parse_and_validate_from([
        "overlay",
        f1.to_str().unwrap(),
        missing.to_str().unwrap(),
    ])
    .expect_err("missing second file must fail validation");
    assert!(matches!(missing_err, OverlayError::MissingInputFile(p) if p == missing));

    let _ = fs::remove_file(&f1);
    let _ = fs::remove_file(&f2);
    let _ = fs::remove_file(&audio_path);
}
