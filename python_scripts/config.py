import fcntl     # Advisory file locking to serialize concurrent config saves
import json      # Serialization and parsing for persisted overlay state
import math      # Finite-number validation for saved coordinates and scale
import os        # Path handling, directory creation, and atomic file replacement
import tempfile  # Temporary sibling file creation prior to os.replace
import time      # Elapsed timestamps for debug diagnostics

# Enables timestamped diagnostic output when set in the environment.
DEBUG = bool(os.environ.get("OVERLAY_DEBUG"))

# Startup epoch used to compute relative seconds in log prefixes.
T0 = time.time()

# Path to the shared JSON file storing per-image position and scale state.
CONFIG_PATH = os.path.expanduser("~/.config/apng-overlay.json")


def log(msg):
    """Print a timestamped diagnostic message when OVERLAY_DEBUG is set."""
    if DEBUG:
        print(f"[{time.time() - T0:7.2f}s] {msg}", flush=True)


def _atomic_write(path, suffix, mode, writer):
    """Write via a sibling temp file, atomically replace path, and unlink on error."""
    directory = os.path.dirname(path) or "."
    os.makedirs(directory, exist_ok=True)
    fd, tmp_path = tempfile.mkstemp(prefix=".tmp-", suffix=suffix, dir=directory)
    try:
        with os.fdopen(fd, mode) as f:
            writer(f)
        os.replace(tmp_path, path)
    except Exception:
        if os.path.exists(tmp_path):
            os.unlink(tmp_path)
        raise


def atomic_write_json(path, data):
    """Write JSON to a temporary file and atomically replace the target path."""
    _atomic_write(
        path,
        ".json",
        "w",
        lambda f: json.dump(data, f),
    )


def atomic_write_bytes(path, data):
    """Write raw bytes to a temporary file and atomically replace the target path."""
    suffix = os.path.splitext(path)[1] or ".tmp"
    _atomic_write(
        path,
        suffix,
        "wb",
        lambda f: f.write(data),
    )


def _read_config_dict():
    """Read CONFIG_PATH as a dictionary, returning an empty dict if missing or invalid."""
    try:
        with open(CONFIG_PATH) as f:
            data = json.load(f)
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _is_valid_number(val):
    """Return True when val is a finite int or float other than a boolean."""
    return (
        isinstance(val, (int, float))
        and not isinstance(val, bool)
        and math.isfinite(val)
    )


def load_saved_state(key):
    """Load and validate saved position and scale for an image path."""
    state = {"x": 100, "y": 100, "scale": 0.6}
    entry = _read_config_dict().get(key)
    if not isinstance(entry, dict):
        return state
    for field in ("x", "y", "scale"):
        val = entry.get(field)
        if _is_valid_number(val):
            state[field] = val
    return state


def save_state_entry(key, state):
    """Persist position and scale for one key in CONFIG_PATH under an exclusive flock."""
    directory = os.path.dirname(CONFIG_PATH) or "."
    os.makedirs(directory, exist_ok=True)
    lock_path = f"{CONFIG_PATH}.lock"
    with open(lock_path, "a+") as lock_file:
        fcntl.flock(lock_file, fcntl.LOCK_EX)
        try:
            data = _read_config_dict()
            data[key] = state
            atomic_write_json(CONFIG_PATH, data)
        finally:
            fcntl.flock(lock_file, fcntl.LOCK_UN)
    log(f"saved: {state}")
