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

    assert_eq!(args.file, PathBuf::from("sample.apng"));
    assert_eq!(args.max_frames, 0);
    assert_eq!(args.base_width, 500);
    assert_eq!(args.monitor, "0");
    assert_eq!(args.scale, None);
    assert_eq!(args.ram_cache_mb, 128);
    assert_eq!(
        args.ram_cache_bytes().expect("default byte budget fits"),
        128 * 1024 * 1024
    );
    assert!(!args.edit);
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
        "-r",
        "256",
        "--edit",
        "--debug",
    ])
    .expect("explicit CLI flags should parse");

    let CliOutcome::Run(args) = outcome
    else
    {
        panic!("expected CliOutcome::Run");
    };

    assert_eq!(args.file, PathBuf::from("sprite.webp"));
    assert_eq!(args.max_frames, 120);
    assert_eq!(args.base_width, 320);
    assert_eq!(args.monitor, "DP-2");
    assert_eq!(args.scale, Some(0.75));
    assert_eq!(args.ram_cache_mb, 256);
    assert_eq!(
        args.ram_cache_bytes().expect("explicit byte budget fits"),
        256 * 1024 * 1024
    );
    assert!(args.edit);
    assert!(args.debug);

    let zero_outcome = CliArgs::parse_flags_from(["overlay", "sprite.webp", "--ram-cache-mb", "0"])
        .expect("--ram-cache-mb 0 should disable RAM LRU cache");
    assert!(matches!(
        zero_outcome,
        CliOutcome::Run(zero_args) if zero_args.ram_cache_bytes().ok() == Some(0)
    ));
}

#[test]
fn cli_rejects_invalid_base_width_and_scales()
{
    let max_mb = usize::MAX.to_string();
    for bad_args in [
        ["overlay", "img.png", "--base-width", "0"].as_slice(),
        ["overlay", "img.png", "--max-frames", "-1"].as_slice(),
        ["overlay", "img.png", "--ram-cache-mb", "-1"].as_slice(),
        ["overlay", "img.png", "--ram-cache-mb", max_mb.as_str()].as_slice(),
        ["overlay", "img.png", "--scale", "0"].as_slice(),
        ["overlay", "img.png", "-s", "-0.5"].as_slice(),
        ["overlay", "img.png", "--scale", "nan"].as_slice(),
        ["overlay", "img.png", "--scale", "inf"].as_slice(),
        ["overlay", "img.png", "--scale", "-inf"].as_slice(),
        ["overlay", "img.png", "--monitor", "   "].as_slice(),
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
    assert!(matches!(parsed, CliOutcome::Run(args) if args.file == existing));

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
        "--ram-cache-mb",
        "--edit",
        "--debug",
        "Triple-click",
        "Esc",
        "hjkl",
        "+ / -",
        "r / R",
        "Rotate 90 degrees",
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
