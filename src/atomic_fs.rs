//! Crash-safe atomic file writer using sibling temporary files and RAII cleanup.
//!
//! Shared across configuration and frame cache storage so interrupted writes,
//! I/O failures, or task cancellations never leave partial artifacts or `.tmp-*`
//! leftovers in target directories.

// Standard filesystem, I/O buffering, paths, and atomic uniqueness counters.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

// Monotonic per-process counter ensuring unique sibling temp filenames without RNG overhead.
static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

// Maximum attempts to find an unused sibling temporary filename.
const MAX_TEMP_ATTEMPTS: usize = 64;

/// Holds an open sibling temporary file and removes it on drop unless committed.
pub struct AtomicTempFile
{
    // Destination path replaced on commit.
    target: PathBuf,

    // Sibling `.tmp-*` path removed automatically if dropped uncommitted.
    temp_path: PathBuf,

    // Open write handle to the temporary sibling file.
    file: Option<File>,
}

impl AtomicTempFile
{
    /// Creates parent directories if needed and opens a unique sibling temp file via `O_EXCL`.
    pub fn create(target: &Path, suffix: &str) -> io::Result<Self>
    {
        let parent = match target.parent()
        {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };

        fs::create_dir_all(parent)?;

        let pid = std::process::id();
        for _ in 0..MAX_TEMP_ATTEMPTS
        {
            let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
            let temp_name = format!(".tmp-{pid}-{seq}{suffix}");
            let temp_path = parent.join(temp_name);

            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
            {
                Ok(file) =>
                {
                    return Ok(Self {
                        target: target.to_path_buf(),
                        temp_path,
                        file: Some(file),
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Exhausted attempts creating unique temporary sibling file",
        ))
    }

    /// Returns mutable access to the underlying temporary file handle for streaming writes.
    pub fn file_mut(&mut self) -> io::Result<&mut File>
    {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("Temporary file handle already consumed"))
    }

    /// Flushes buffered OS writes and atomically replaces the target path via `rename`.
    pub fn commit(mut self) -> io::Result<()>
    {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other("Temporary file handle already committed"))?;

        file.flush()?;
        drop(file);

        fs::rename(&self.temp_path, &self.target)?;
        Ok(())
    }
}

impl Drop for AtomicTempFile
{
    /// Deletes an uncommitted temporary sibling file if writing failed or was cancelled.
    fn drop(&mut self)
    {
        drop(self.file.take());
        if self.temp_path.exists()
        {
            let _ = fs::remove_file(&self.temp_path);
        }
    }
}

/// Atomically writes `bytes` to `target` via a sibling temporary file with the given `suffix`.
pub fn write_bytes(target: &Path, suffix: &str, bytes: &[u8]) -> io::Result<()>
{
    let mut temp = AtomicTempFile::create(target, suffix)?;
    temp.file_mut()?.write_all(bytes)?;
    temp.commit()
}
