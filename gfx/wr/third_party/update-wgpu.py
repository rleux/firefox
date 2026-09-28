#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path

import tomllib


def main():
    parser = argparse.ArgumentParser(
        description="Package Firefox's pinned wgpu API wrapper."
    )
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    directory = Path(__file__).resolve().parent
    root = directory.parents[2]
    bindings = tomllib.loads((root / "gfx/wgpu_bindings/Cargo.toml").read_text())
    dependency = bindings["dependencies"]["wgc"]
    revision = dependency["rev"]
    destination = directory / "wgpu"

    if destination.exists() and not args.check:
        status = subprocess.check_output(
            [
                "git",
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--",
                str(destination),
            ],
            cwd=root,
        )
        if status:
            parser.error(
                "Commit or save local changes to the wrapper before updating it."
            )

    with tempfile.TemporaryDirectory(prefix="webrender-wgpu-") as temporary_path:
        temporary = Path(temporary_path)
        with tarfile.open(args.archive) as archive:
            archive.extractall(temporary, filter="data")
        source = temporary / f"wgpu-{revision}"
        manifest = source / "wgpu/Cargo.toml"
        if not manifest.is_file():
            parser.error(f"Archive must contain wgpu revision {revision}.")
        env = os.environ.copy()
        env.setdefault("RUSTUP_TOOLCHAIN", "stable")
        subprocess.run(
            [
                "cargo",
                "package",
                "--offline",
                "--no-verify",
                "--allow-dirty",
                "--exclude-lockfile",
                "-p",
                "wgpu",
                "--no-default-features",
                "--features",
                "std,vulkan,wgsl",
                "--target-dir",
                str(temporary / "target"),
            ],
            cwd=source,
            env=env,
            check=True,
        )
        packages = list((temporary / "target/package").glob("wgpu-*.crate"))
        if len(packages) != 1:
            raise RuntimeError("Expected exactly one packaged wgpu crate.")
        package = packages[0]
        unpacked = temporary / "package"
        with tarfile.open(package) as archive:
            archive.extractall(unpacked, filter="data")
        wrapper = unpacked / package.name.removesuffix(".crate")
        files = sorted(
            path.relative_to(wrapper) for path in wrapper.rglob("*") if path.is_file()
        )
        if args.check:
            actual = sorted(
                path.relative_to(destination)
                for path in destination.rglob("*")
                if path.is_file()
            )
            if actual != files or any(
                (wrapper / path).read_bytes() != (destination / path).read_bytes()
                for path in files
            ):
                raise RuntimeError(
                    "Vendored wrapper differs from the packaged upstream source."
                )
            print(f"Verified {len(files)} upstream files at {revision}.")
            return

        if destination.exists():
            shutil.rmtree(destination)
        shutil.copytree(wrapper, destination)
        metadata = {
            "repository": dependency["git"],
            "revision": revision,
            "source_directory": "wgpu",
            "archive_url": f"https://codeload.github.com/gfx-rs/wgpu/tar.gz/{revision}",
            "archive_sha256": hashlib.sha256(args.archive.read_bytes()).hexdigest(),
            "package_sha256": hashlib.sha256(package.read_bytes()).hexdigest(),
            "version": tomllib.loads((wrapper / "Cargo.toml").read_text())["package"][
                "version"
            ],
            "files": len(files),
            "update_command": "python3 gfx/wr/third_party/update-wgpu.py --archive <downloaded-archive>",
        }
        (directory / "wgpu.json").write_text(json.dumps(metadata, indent=2) + "\n")
        print(f"Vendored {len(files)} upstream files at {revision}.")


if __name__ == "__main__":
    main()
