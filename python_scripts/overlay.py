#!/usr/bin/env python3
import argparse
import math
import os
import sys


def parse_args(argv=None):
    """Parse and validate CLI flags for the Wayland image overlay."""
    parser = argparse.ArgumentParser(
        description="Display an animated image as a click-through Wayland overlay."
    )
    parser.add_argument("file", help="Path to the image or animation file.")
    parser.add_argument(
        "-n", "--max-frames",
        type=int,
        default=0,
        help="Maximum number of frames to decode (0 = all frames).",
    )
    parser.add_argument(
        "-w", "--base-width",
        type=int,
        default=500,
        help="Stored frame width in pixels (default: 500).",
    )
    parser.add_argument(
        "-m", "--monitor",
        default="0",
        help="Target monitor index or connector name (default: 0).",
    )
    parser.add_argument(
        "-s", "--scale",
        type=float,
        default=None,
        help="Initial display scale (overrides saved scale).",
    )
    args = parser.parse_args(argv)
    if args.max_frames < 0:
        parser.error("--max-frames must be >= 0")
    if args.base_width < 1:
        parser.error("--base-width must be >= 1")
    if args.scale is not None and (not math.isfinite(args.scale) or args.scale <= 0):
        parser.error("--scale must be a positive finite number")
    return args


def main():
    args = parse_args()
    if not os.path.isfile(args.file):
        sys.exit(f"error: file not found: {args.file}")

    # Import window only after CLI validation so preload_layer_shell runs before GTK.
    from window import OverlayApp

    app = OverlayApp(args)
    sys.exit(app.run())


if __name__ == "__main__":
    main()
