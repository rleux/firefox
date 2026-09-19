# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import argparse
import os
import signal
import subprocess
import sys
import time
from pathlib import Path


def main():
    root = Path(__file__).resolve().parents[3]
    parser = argparse.ArgumentParser(
        description="Measure synchronous and asynchronous native video transfer"
    )
    parser.add_argument("--clip", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--binary", type=Path, default=root / "obj-x86_64-pc-linux-gnu/dist/bin/firefox"
    )
    parser.add_argument("--synchronization", choices=["sync", "async"], required=True)
    parser.add_argument("--display", choices=["xvfb", "native"], default="xvfb")
    parser.add_argument(
        "--viewport", nargs=2, type=int, metavar=("WIDTH", "HEIGHT"), default=(890, 705)
    )
    parser.add_argument("--duration", type=float, default=100)
    parser.add_argument(
        "--process-metrics", choices=["full", "light", "off"], default="full"
    )
    parser.add_argument("--sync-instrumentation", action="store_true")
    parser.add_argument("--quiet", action="store_true")
    parser.add_argument("--icd", type=Path)
    parser.add_argument("--validation-layers", type=Path)
    parser.add_argument("--loader-directory", type=Path)
    args = parser.parse_args()
    if min(args.viewport) <= 0:
        parser.error("Viewport dimensions must be positive")
    if args.duration <= 0:
        parser.error("Duration must be positive")
    if args.display == "native" and not os.environ.get("DISPLAY"):
        parser.error("Native presentation requires an existing X11 DISPLAY")
    if args.display == "xvfb" and os.environ.get("WR_VIDEO_BENCHMARK_XVFB") != "1":
        env = os.environ.copy()
        env["WR_VIDEO_BENCHMARK_XVFB"] = "1"
        return subprocess.call(
            [
                "xvfb-run",
                "-a",
                "-s",
                "-screen 0 1280x1024x24 -nolisten tcp",
                sys.executable,
                str(Path(__file__).resolve()),
                *sys.argv[1:],
            ],
            env=env,
        )

    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    for name in [
        "MOZ_HEADLESS",
        "MOZ_RUN_GTEST",
        "MOZ_WR_BACKEND",
        "MOZ_WR_DEFAULT_BACKEND",
        "MOZ_WR_VULKAN_VALIDATION",
        "MESA_VK_WSI_DEBUG",
        "VK_DRIVER_FILES",
        "VK_INSTANCE_LAYERS",
        "VK_LAYER_PATH",
        "VK_LAYER_VALIDATE_SYNC",
        "WR_VIDEO_BENCHMARK_QUIET",
        "WR_VIDEO_FORCE_SYNC",
        "WR_VIDEO_SYNC_INSTRUMENTATION",
        "WR_WEBGL_BENCHMARK_QUIET",
        "WR_WEBGL_FORCE_SYNC",
        "WR_WEBGL_SYNC_INSTRUMENTATION",
        "WR_WEBGPU_BENCHMARK_QUIET",
        "WR_WEBGPU_FORCE_DMABUF_COPY",
        "WR_WEBGPU_SYNC_INSTRUMENTATION",
    ]:
        env.pop(name, None)
    env.update(
        WR_VIDEO_BENCHMARK_OUTPUT=str(output),
        WR_VIDEO_BENCHMARK_CLIP=str(args.clip.resolve()),
        WR_VIDEO_BENCHMARK_DISPLAY=args.display,
        WR_VIDEO_BENCHMARK_DURATION=str(args.duration),
        WR_VIDEO_BENCHMARK_PROCESS_METRICS=args.process_metrics,
        WR_VIDEO_BENCHMARK_SYNCHRONIZATION=args.synchronization,
        WR_VIDEO_BENCHMARK_VIEWPORT="x".join(map(str, args.viewport)),
        WR_VIDEO_BENCHMARK_LOADER_DIRECTORY=(
            str(args.loader_directory.resolve()) if args.loader_directory else ""
        ),
        GDK_BACKEND="x11",
        MOZ_NO_REMOTE="1",
        MOZ_LOG=(
            "webrender_bindings::hal_image::linux:2,PlatformDecoderModule:2"
            if args.quiet
            else "webrender_bindings::hal_image::linux:3,PlatformDecoderModule:5"
        ),
    )
    if args.synchronization == "sync":
        env["WR_VIDEO_FORCE_SYNC"] = "1"
    if args.sync_instrumentation:
        env["WR_VIDEO_SYNC_INSTRUMENTATION"] = "1"
    if args.quiet:
        env["WR_VIDEO_BENCHMARK_QUIET"] = "1"
    if args.icd:
        env["VK_DRIVER_FILES"] = str(args.icd.resolve())
    if args.validation_layers:
        env.update(
            MOZ_WR_VULKAN_VALIDATION="1",
            VK_LAYER_PATH=str(args.validation_layers.resolve()),
            VK_LAYER_VALIDATE_SYNC="1",
        )
    if args.loader_directory:
        env["LD_LIBRARY_PATH"] = str(args.loader_directory.resolve()) + (
            os.pathsep + env["LD_LIBRARY_PATH"] if env.get("LD_LIBRARY_PATH") else ""
        )

    prefs = {
        "remote.screenshot.use_readback": "true",
        "gfx.webrender.all": "true",
        "gfx.webrender.vulkan": "true",
        "gfx.webrender.software": "false",
        "gfx.color_management.mode": "0",
        "layout.css.devPixelsPerPx": "1.0",
        "browser.display.os-zoom-behavior": "0",
        "layers.gpu-process.enabled": "true",
        "media.hardware-video-decoding.enabled": "true",
        "media.hardware-video-decoding.force-enabled": "true",
        "media.hardware-video-decoding-vulkan.enabled": "false",
        "media.ffmpeg.vaapi.enabled": "true",
        "media.ffmpeg.vaapi.force-surface-zero-copy": "1",
        "privacy.reduceTimerPrecision": "false",
    }
    timeout = int(args.duration) + 90
    command = [
        str(root / "mach"),
        "marionette-test",
        "--binary",
        str(args.binary.resolve()),
        "--gecko-log",
        str(output / "gecko.log"),
        "--workspace",
        str(output / "workspace"),
        "--startup-timeout",
        "90",
        "--socket-timeout",
        str(timeout),
    ]
    for name, value in prefs.items():
        command += ["--setpref", f"{name}={value}"]
    command.append(str(Path(__file__).with_name("test_video_transfer_benchmark.py")))

    print(f"Output: {output}", flush=True)
    with (output / "test.log").open("w") as log, (output / "window-manager.log").open(
        "w"
    ) as wm_log:
        wm = (
            subprocess.Popen(
                ["openbox", "--sm-disable"],
                env=env,
                stdout=wm_log,
                stderr=subprocess.STDOUT,
            )
            if args.display == "xvfb"
            else None
        )
        try:
            deadline = time.monotonic() + 10
            while wm:
                ready = subprocess.run(
                    ["xprop", "-root", "_NET_SUPPORTING_WM_CHECK"],
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=5,
                    check=False,
                )
                if "window id #" in ready.stdout:
                    break
                if wm.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("Private Xvfb window manager did not start")
                time.sleep(0.1)
            process = subprocess.Popen(
                command,
                cwd=root,
                env=env,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                status = process.wait(timeout=int(args.duration) + 240)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                status = 124
        finally:
            if wm:
                wm.terminate()
                try:
                    wm.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    wm.kill()
                    wm.wait()
    print(f"Exit: {status}", flush=True)
    return status


if __name__ == "__main__":
    raise SystemExit(main())
