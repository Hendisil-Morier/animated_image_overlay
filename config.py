import json
import math
import os
import tempfile
import time

DEBUG = bool(os.environ.get("OVERLAY_DEBUG"))
T0 = time.time()
CONFIG_PATH = os.path.expanduser("~/.config/apng-overlay.json")


def log(msg):
    """Print a timestamped diagnostic message when OVERLAY_DEBUG is set."""
    if DEBUG:
        print(f"[{time.time() - T0:7.2f}s] {msg}", flush=True)


def atomic_write_json(path, data):
    """Write JSON to a temporary file and atomically replace the target path."""
    directory = os.path.dirname(path) or "."
    os.makedirs(directory, exist_ok=True)
    fd, tmp_path = tempfile.mkstemp(prefix=".tmp-", suffix=".json", dir=directory)
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(data, f)
        os.replace(tmp_path, path)
    except Exception:
        if os.path.exists(tmp_path):
            os.unlink(tmp_path)
        raise


def load_saved_state(key):
    """Load and validate saved position and scale for an image path."""
    state = {"x": 100, "y": 100, "scale": 0.6}
    try:
        with open(CONFIG_PATH) as f:
            saved = json.load(f)
        if isinstance(saved, dict):
            entry = saved.get(key)
            if isinstance(entry, dict):
                for field in ("x", "y", "scale"):
                    val = entry.get(field)
                    if (
                        isinstance(val, (int, float))
                        and not isinstance(val, bool)
                        and math.isfinite(val)
                    ):
                        state[field] = val
    except (OSError, ValueError, AttributeError):
        pass
    return state


def save_state_entry(key, state):
    """Persist position and scale for a single image key inside CONFIG_PATH."""
    try:
        with open(CONFIG_PATH) as f:
            data = json.load(f)
    except (OSError, ValueError):
        data = {}
    if not isinstance(data, dict):
        data = {}
    data[key] = state
    atomic_write_json(CONFIG_PATH, data)
    log(f"saved: {state}")
