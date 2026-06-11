#!/usr/bin/env python3
"""check_replay_equivalence.py — mechanical port-equivalence gate.

Pins scripts/replay_cache.py --provider anthropic to the FROZEN →2032
verification artifact (haystack/scripts/replay_2032.py) byte-for-byte,
on BOTH output surfaces (text and --json).

Allowed delta: NONE on stdout. The provider key was dropped from
anthropic-mode JSON precisely so this check can assert true byte
identity. The single behavioral delta (not stdout) is documented in
the port: replay_cache.py exits 2 when an AC fails; the frozen script
always exits 0.

The transcript corpus is LIVE (sessions append while this runs), so
both harnesses are pinned to the same snapshot with an identical
--until in the recent past; extraction is deterministic given the
window.

Usage:
    python3 scripts/check_replay_equivalence.py [--frozen PATH] [--dir DIR]
Exit 0 = identical; exit 1 = divergence (first diff shown).
"""

import argparse
import difflib
import os
import subprocess
import sys
from datetime import datetime, timedelta, timezone

DEFAULT_FROZEN = os.path.expanduser("~/projects/haystack/scripts/replay_2032.py")
PORT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "replay_cache.py")


def run(cmd: list[str]) -> str:
    proc = subprocess.run(cmd, capture_output=True, text=True)
    # stderr (unknown-model rate warnings) is not part of the pinned
    # surface; AC-driven exit 2 from the port is the documented
    # behavioral delta, so accept 0 and 2 only.
    if proc.returncode not in (0, 2):
        sys.exit(f"FATAL: {' '.join(cmd)} exited {proc.returncode}:\n{proc.stderr}")
    return proc.stdout


def main() -> None:
    ap = argparse.ArgumentParser(description="Pin port-vs-frozen replay byte equivalence")
    ap.add_argument("--frozen", default=DEFAULT_FROZEN)
    ap.add_argument("--dir", default=None, help="transcript dir override (both harnesses)")
    args = ap.parse_args()

    if not os.path.exists(args.frozen):
        sys.exit(f"SKIP: frozen artifact not found at {args.frozen}")

    until = (datetime.now(timezone.utc) - timedelta(minutes=1)).strftime(
        "%Y-%m-%dT%H:%M:%S+00:00"
    )
    dir_args = ["--dir", args.dir] if args.dir else []

    failures = 0
    for label, extra in (("text", []), ("json", ["--json"])):
        frozen_out = run(
            [sys.executable, args.frozen, *dir_args, "--until", until, *extra]
        )
        port_out = run(
            [
                sys.executable,
                PORT,
                "--provider",
                "anthropic",
                *dir_args,
                "--until",
                until,
                *extra,
            ]
        )
        if frozen_out == port_out:
            print(f"{label}: IDENTICAL ({len(frozen_out)} bytes, until={until})")
        else:
            failures += 1
            print(f"{label}: DIVERGED")
            diff = difflib.unified_diff(
                frozen_out.splitlines(keepends=True),
                port_out.splitlines(keepends=True),
                fromfile="frozen",
                tofile="port",
            )
            sys.stdout.writelines(list(diff)[:40])

    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
