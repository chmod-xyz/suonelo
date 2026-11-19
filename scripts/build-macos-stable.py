#!/usr/bin/env python3
"""Build locally with private build paths remapped throughout Cargo dependencies.
Run in a fresh source checkout. The app uses an ad-hoc signature.
The Intel CI workflow verifies the app, privacy scan and unsigned DMG.
"""
import os
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parent.parent
home = Path.home()
sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
pairs = [(str(home), "/build/home"), (str(sysroot), "/build/rust"),
         (str(root), "/build/suonelo"), (str(root.resolve()), "/build/suonelo")]
env = os.environ.copy()
flags = ["--remap-path-prefix=" + source + "=" + target for source, target in pairs]
# Encoded flags keep each mapping a single argument even with spaces.
env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
# Native dependencies are built in this fresh, space-free temporary checkout.
# Map the home prefix for compiler metadata as well.
env["CFLAGS"] = "-ffile-prefix-map=" + str(home) + "=/build/home"
env["CXXFLAGS"] = env["CFLAGS"]
env["NEXT_TELEMETRY_DISABLED"] = "1"
subprocess.run(["bash", "scripts/build-macos.sh", "--app"], cwd=root, env=env, check=True)
