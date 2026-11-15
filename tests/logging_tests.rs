//! Integration tests verifying dual-output logging setup, error mapping, and panic flushing.

use std::{
    fs,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use animated_image_widget::{
    error::OverlayError,
    logging::{build_subscriber_in_dir, init_logging_in_dir, parse_debug_flag},
};

struct TempTestDir
{
    path: PathBuf,
}

impl TempTestDir
{
    fn new(name: &str) -> Self
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aiw_log_test_{name}_{}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp test dir should be creatable");
        Self { path }
    }

    fn path(&self) -> &Path
    {
        &self.path
    }
}

impl Drop for TempTestDir
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn read_all_logs(dir: &Path) -> String
{
    let mut combined = String::new();
    for entry in fs::read_dir(dir).expect("log directory should be readable")
    {
        let entry = entry.expect("dir entry should be valid");
        if entry.path().is_file()
        {
            let content = fs::read_to_string(entry.path()).expect("log file should be UTF-8");
            combined.push_str(&content);
        }
    }
    combined
}

#[test]
fn debug_flag_parsing_handles_empty_falsey_and_truthy_values()
{
    assert!(!parse_debug_flag(None));
    assert!(!parse_debug_flag(Some("")));
    assert!(!parse_debug_flag(Some("   ")));
    assert!(!parse_debug_flag(Some("0")));
    assert!(!parse_debug_flag(Some("false")));
    assert!(!parse_debug_flag(Some("FALSE")));
    assert!(!parse_debug_flag(Some("no")));

    assert!(parse_debug_flag(Some("1")));
    assert!(parse_debug_flag(Some("true")));
    assert!(parse_debug_flag(Some("yes")));
}

#[test]
fn build_subscriber_writes_and_flushes_disk_logs()
{
    let temp = TempTestDir::new("disk_flush");
    let logs_dir = temp.path().join("logs");

    let (subscriber, guard) =
        build_subscriber_in_dir(&logs_dir, false).expect("subscriber build should succeed");

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(frame_count = 42, "decoded animated overlay source");
    });

    guard.flush();

    let logs = read_all_logs(&logs_dir);
    assert!(logs.contains("decoded animated overlay source"));
    assert!(logs.contains("frame_count=42"));
}

#[test]
fn build_subscriber_maps_unwritable_path_to_overlay_error()
{
    let temp = TempTestDir::new("invalid_dir");
    let blocking_file = temp.path().join("not_a_directory");
    fs::write(&blocking_file, b"occupied").expect("blocking file should write");

    let result = build_subscriber_in_dir(&blocking_file, false);
    assert!(matches!(result, Err(OverlayError::LoggingInit(_))));
}

#[test]
fn global_logging_panic_hook_flushes_diagnostics_on_panic()
{
    let temp = TempTestDir::new("panic_flush");
    let logs_dir = temp.path().join("logs");

    let _guard = init_logging_in_dir(&logs_dir, false)
        .expect("global logging initialization should succeed once");

    tracing::info!("pre-panic diagnostic line");

    let panic_result = panic::catch_unwind(AssertUnwindSafe(|| {
        panic!("simulated fatal renderer panic");
    }));
    assert!(panic_result.is_err());

    let logs = read_all_logs(&logs_dir);
    assert!(logs.contains("pre-panic diagnostic line"));
    assert!(logs.contains("Unhandled panic occurred"));
    assert!(logs.contains("simulated fatal renderer panic"));
}
