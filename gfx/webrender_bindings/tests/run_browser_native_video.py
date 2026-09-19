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
        description="Test native video in an already-built Firefox"
    )
    parser.add_argument("--clip", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--native-display",
        action="store_true",
        help="Use the current display and its window manager instead of private Xvfb",
    )
    parser.add_argument(
        "--binary", type=Path, default=root / "obj-x86_64-pc-linux-gnu/dist/bin/firefox"
    )
    parser.add_argument(
        "--backend", choices=["vulkan", "gl", "software"], default="vulkan"
    )
    parser.add_argument(
        "--decoder", choices=["hardware", "software"], default="hardware"
    )
    parser.add_argument("--synchronization", choices=["sync", "async"], default="async")
    parser.add_argument(
        "--webgl-synchronization", choices=["sync", "async"], default="async"
    )
    parser.add_argument("--software-video", action="store_true")
    parser.add_argument("--force-hardware-decoder", action="store_true")
    parser.add_argument("--webgl-readback", action="store_true")
    parser.add_argument("--zero-copy-disabled", action="store_true")
    parser.add_argument("--resolution-change", action="store_true")
    parser.add_argument("--gpu-process", choices=["true", "false"], default="true")
    parser.add_argument("--icd", type=Path)
    parser.add_argument("--validation-layers", type=Path)
    parser.add_argument("--loader-directory", type=Path)
    parser.add_argument("--adapter")
    parser.add_argument("--render-node", type=Path)
    parser.add_argument("--software-presentation", action="store_true")
    parser.add_argument("--sync-instrumentation", action="store_true")
    parser.add_argument(
        "--scenario",
        choices=["basic", "reset", "crash", "windows", "pip", "lifecycle"],
        default="basic",
    )
    args = parser.parse_args()
    if not args.native_display and os.environ.get("WR_NATIVE_VIDEO_XVFB") != "1":
        env = os.environ.copy()
        env["WR_NATIVE_VIDEO_XVFB"] = "1"
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
        "MOZ_WR_BACKEND",
        "MOZ_WR_DEFAULT_BACKEND",
        "MOZ_RUN_GTEST",
        "MOZ_HEADLESS",
        "MOZ_WR_VULKAN_VALIDATION",
        "VK_LAYER_PATH",
        "VK_INSTANCE_LAYERS",
        "VK_LAYER_VALIDATE_SYNC",
        "WR_VIDEO_FORCE_SYNC",
        "WR_WEBGL_FORCE_SYNC",
        "WR_WEBGPU_BENCHMARK_QUIET",
        "WR_WEBGL_BENCHMARK_QUIET",
        "WR_WEBGPU_SYNC_INSTRUMENTATION",
        "WR_WEBGL_SYNC_INSTRUMENTATION",
    ]:
        env.pop(name, None)
    gecko_log = output / "gecko.log"
    env.update(
        WR_NATIVE_VIDEO_OUTPUT=str(output),
        WR_NATIVE_VIDEO_CLIP=str(args.clip.resolve()),
        WR_NATIVE_VIDEO_BACKEND=args.backend,
        WR_NATIVE_VIDEO_DECODER=args.decoder,
        WR_NATIVE_VIDEO_SYNCHRONIZATION=args.synchronization,
        WR_VIDEO_FORCE_SYNC="1" if args.synchronization == "sync" else "0",
        WR_WEBGL_FORCE_SYNC="1" if args.webgl_synchronization == "sync" else "0",
        WR_NATIVE_VIDEO_SCENARIO=args.scenario,
        WR_NATIVE_VIDEO_RESOLUTION_CHANGE="1" if args.resolution_change else "0",
        WR_NATIVE_VIDEO_PROCESS="GPU" if args.gpu_process == "true" else "Parent",
        WR_NATIVE_VIDEO_GECKO_LOG=str(gecko_log),
        WR_VIDEO_SYNC_INSTRUMENTATION="1" if args.sync_instrumentation else "0",
        WR_NATIVE_VIDEO_LOADER_DIRECTORY=(
            str(args.loader_directory.resolve()) if args.loader_directory else ""
        ),
        GDK_BACKEND="x11",
        MOZ_LOG="webrender_bindings::hal_image::linux:3,PlatformDecoderModule:5",
    )
    if args.synchronization == "async":
        env.pop("WR_VIDEO_FORCE_SYNC", None)
    if args.webgl_synchronization == "async":
        env.pop("WR_WEBGL_FORCE_SYNC", None)
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
    if args.adapter:
        env["MOZ_WR_VULKAN_ADAPTER"] = args.adapter
    if args.render_node:
        env["MOZ_DRM_DEVICE"] = str(args.render_node.resolve())
    if args.software_presentation:
        env["MESA_VK_WSI_DEBUG"] = "sw"
    prefs = {
        "remote.screenshot.use_readback": "true",
        "gfx.webrender.all": "true",
        "gfx.webrender.vulkan": "true" if args.backend == "vulkan" else "false",
        "gfx.webrender.software": "true" if args.backend == "software" else "false",
        "layers.gpu-process.enabled": args.gpu_process,
        "gfx.color_management.mode": "0",
        "layout.css.devPixelsPerPx": "1.0",
        "media.hardware-video-decoding-vulkan.enabled": "false",
    }
    if args.software_video:
        prefs["media.hardware-video-decoding.enabled"] = "false"
    if args.force_hardware_decoder:
        prefs["media.hardware-video-decoding.force-enabled"] = "true"
        prefs["media.ffmpeg.vaapi.force-surface-zero-copy"] = "1"
    if args.webgl_readback:
        prefs["widget.dmabuf-webgl.enabled"] = "false"
    if args.zero_copy_disabled:
        prefs["media.ffmpeg.vaapi.force-surface-zero-copy"] = "0"
    command = [
        str(root / "mach"),
        "marionette-test",
        "--binary",
        str(args.binary.resolve()),
        "--gecko-log",
        str(gecko_log),
        "--workspace",
        str(output / "workspace"),
        "--startup-timeout",
        "90",
        "--socket-timeout",
        "60",
    ]
    for name, value in prefs.items():
        command += ["--setpref", f"{name}={value}"]
    command.append(str(Path(__file__).with_name("test_native_video_browser.py")))
    log = output / "test.log"
    print(f"Log: {log}", flush=True)
    with log.open("w") as stream, (output / "window-manager.log").open("w") as wm_log:
        window_manager = (
            None
            if args.native_display
            else subprocess.Popen(
                ["openbox", "--sm-disable"],
                env=env,
                stdout=wm_log,
                stderr=subprocess.STDOUT,
            )
        )
        try:
            deadline = time.monotonic() + 10
            while window_manager:
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
                if window_manager.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("Private Xvfb window manager did not start")
                time.sleep(0.1)
            process = subprocess.Popen(
                command,
                cwd=root,
                env=env,
                stdout=stream,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                status = process.wait(timeout=180)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                status = 124
        finally:
            if window_manager:
                window_manager.terminate()
                try:
                    window_manager.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    window_manager.kill()
                    window_manager.wait()
    print(f"Exit: {status}", flush=True)
    return status


if __name__ == "__main__":
    raise SystemExit(main())
