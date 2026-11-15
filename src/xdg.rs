//! Shared XDG Base Directory resolution with `$HOME` fallback.

// Standard OS string and path types for environment-driven base directory lookup.
use std::{ffi::OsStr, path::PathBuf};

/// Resolves an XDG directory override when non-empty, falling back to `$HOME/<fallback_subdir>`.
pub(crate) fn base_dir(
    xdg_value: Option<&OsStr>,
    home_value: Option<&OsStr>,
    fallback_subdir: &str,
) -> Option<PathBuf>
{
    if let Some(xdg) = xdg_value.filter(|path| !path.is_empty())
    {
        return Some(PathBuf::from(xdg));
    }

    home_value
        .filter(|path| !path.is_empty())
        .map(|home| PathBuf::from(home).join(fallback_subdir))
}
