//! Wayland output descriptor metadata and `--monitor` resolution by index or connector name.

// SCTK output metadata structure describing connected Wayland monitors.
use smithay_client_toolkit::output::OutputInfo;

// Typed monitor resolution error variants.
use crate::error::WaylandError;

/// Summary of an advertised Wayland output used by `--monitor` matching and layout setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorDescriptor
{
    /// Zero-based enumeration index matching compositor order.
    pub index: usize,

    /// Connector name (e.g. `"DP-1"`, `"eDP-1"`) if reported by the compositor.
    pub name: Option<String>,

    /// Logical width in compositor coordinates (minimum 1 px).
    pub width: i32,

    /// Logical height in compositor coordinates (minimum 1 px).
    pub height: i32,
}

impl MonitorDescriptor
{
    /// Builds a monitor descriptor from SCTK `OutputInfo` metadata.
    pub fn from_output_info(index: usize, info: &OutputInfo) -> Self
    {
        let (width, height) = info.logical_size.unwrap_or_else(|| {
            let active_mode = info
                .modes
                .iter()
                .find(|mode| mode.current)
                .or_else(|| info.modes.first());

            match active_mode
            {
                Some(mode) =>
                {
                    let scale = info.scale_factor.max(1);
                    (
                        (mode.dimensions.0 / scale).max(1),
                        (mode.dimensions.1 / scale).max(1),
                    )
                }
                None => (1920, 1080),
            }
        });

        Self {
            index,
            name: info.name.clone(),
            width: width.max(1),
            height: height.max(1),
        }
    }
}

/// Resolves `--monitor` by zero-based integer index or exact connector name.
pub fn resolve_monitor_index(
    monitors: &[MonitorDescriptor],
    selector: &str,
) -> Result<usize, WaylandError>
{
    if monitors.is_empty()
    {
        return Err(WaylandError::MonitorNotFound {
            selector: String::from("no monitors found"),
        });
    }

    if let Ok(index) = selector.parse::<isize>()
    {
        if index >= 0 && (index as usize) < monitors.len()
        {
            return Ok(index as usize);
        }

        let available = format_available_monitors(monitors);
        return Err(WaylandError::MonitorNotFound {
            selector: format!("{selector} (index out of range; available: {available})"),
        });
    }

    if let Some(matched) = monitors
        .iter()
        .find(|mon| mon.name.as_deref() == Some(selector))
    {
        return Ok(matched.index);
    }

    let available = format_available_monitors(monitors);
    Err(WaylandError::MonitorNotFound {
        selector: format!("{selector} (available: {available})"),
    })
}

/// Formats active monitor indices and connector names for clear diagnostic errors.
fn format_available_monitors(monitors: &[MonitorDescriptor]) -> String
{
    monitors
        .iter()
        .map(|mon| {
            let name = mon.name.as_deref().unwrap_or("unknown");
            format!("{}:{name}", mon.index)
        })
        .collect::<Vec<_>>()
        .join(", ")
}
