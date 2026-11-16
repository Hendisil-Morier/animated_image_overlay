//! Unique per-test temporary directories removed automatically on drop.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
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
