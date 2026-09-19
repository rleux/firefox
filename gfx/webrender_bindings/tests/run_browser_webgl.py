# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import argparse
import os
import signal
import subprocess
import sys
from pathlib import Path


def main():
    root = Path(__file__).resolve().parents[3]
    parser = argparse.ArgumentParser(description="Test WebGL DMA-BUF presentation")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--binary", type=Path, default=root / "obj-x86_64-pc-linux-gnu/dist/bin/firefox"
    )
    parser.add_argument("--synchronization", choices=["sync", "async"], default="async")
    parser.add_argument("--display", choices=["xvfb", "native"], default="xvfb")
    parser.add_argument("--viewport", nargs=2, type=int, metavar=("WIDTH", "HEIGHT"))
    parser.add_argument("--gpu-process", choices=["true", "false"], default="true")
    parser.add_argument("--icd", type=Path)
    parser.add_argument("--validation-layers", type=Path)
    parser.add_argument("--loader-directory", type=Path)
    parser.add_argument("--adapter")
    parser.add_argument("--drm-device", type=Path, default=Path("/dev/dri/renderD128"))
    parser.add_argument("--software-presentation", action="store_true")
    parser.add_argument("--benchmark-frames", type=int, default=0)
    parser.add_argument("--benchmark-only", action="store_true")
    parser.add_argument("--sync-instrumentation", action="store_true")
    parser.add_argument(
        "--process-metrics", choices=["full", "light", "off"], default="full"
    )
    parser.add_argument(
        "--scenario",
        choices=["basic", "lifecycle", "windows", "context-loss", "offscreen"],
        default="basic",
    )
    args = parser.parse_args()
    if args.viewport and min(args.viewport) <= 0:
        parser.error("Viewport dimensions must be positive")
    if args.display == "native":
        if not os.environ.get("DISPLAY"):
            parser.error("Native presentation requires an existing X11 DISPLAY")
        if args.software_presentation:
            parser.error("Native presentation cannot use software presentation")
    if args.benchmark_frames < 0 or (args.benchmark_only and not args.benchmark_frames):
        parser.error("Benchmark-only requires a positive frame count")
    benchmark_timeout = args.benchmark_frames // 20 + 30
    if args.display == "xvfb" and os.environ.get("WR_WEBGL_XVFB") != "1":
        env = os.environ.copy()
        env["WR_WEBGL_XVFB"] = "1"
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
        "MOZ_HEADLESS",
        "MOZ_WR_VULKAN_VALIDATION",
        "VK_LAYER_PATH",
        "VK_INSTANCE_LAYERS",
        "VK_LAYER_VALIDATE_SYNC",
        "MESA_VK_WSI_DEBUG",
        "WR_WEBGL_FORCE_SYNC",
        "WR_WEBGPU_BENCHMARK_QUIET",
        "WR_WEBGPU_SYNC_INSTRUMENTATION",
        "WR_WEBGPU_FORCE_DMABUF_COPY",
    ]:
        env.pop(name, None)
    env.update(
        WR_WEBGL_OUTPUT=str(output),
        WR_WEBGL_SYNCHRONIZATION=args.synchronization,
        WR_WEBGL_FORCE_SYNC="1" if args.synchronization == "sync" else "0",
        WR_WEBGL_DISPLAY_MODE=args.display,
        WR_WEBGL_VIEWPORT=("x".join(map(str, args.viewport)) if args.viewport else ""),
        WR_WEBGL_PROCESS="GPU" if args.gpu_process == "true" else "Parent",
        WR_WEBGL_BENCHMARK_FRAMES=str(args.benchmark_frames),
        WR_WEBGL_BENCHMARK_TIMEOUT=str(benchmark_timeout),
        WR_WEBGL_BENCHMARK_ONLY="1" if args.benchmark_only else "0",
        WR_WEBGL_BENCHMARK_QUIET=(
            "1" if args.benchmark_only and not args.sync_instrumentation else "0"
        ),
        WR_WEBGL_SYNC_INSTRUMENTATION="1" if args.sync_instrumentation else "0",
        WR_WEBGL_PROCESS_METRICS=args.process_metrics,
        WR_WEBGL_SCENARIO=args.scenario,
        WR_WEBGL_LOADER_DIRECTORY=(
            str(args.loader_directory.resolve()) if args.loader_directory else ""
        ),
        MOZ_DRM_DEVICE=str(args.drm_device.resolve()),
        GDK_BACKEND="x11",
        MOZ_X11_EGL="1",
        MOZ_NO_REMOTE="1",
        MOZ_LOG=(
            "webrender_bindings::hal_image::linux:2,WebGL:2"
            if args.benchmark_only and not args.sync_instrumentation
            else "webrender_bindings::hal_image::linux:3,WebGL:3"
        ),
    )
    if args.synchronization == "async":
        env.pop("WR_WEBGL_FORCE_SYNC", None)
    if args.icd:
        env["VK_DRIVER_FILES"] = str(args.icd.resolve())
    if args.loader_directory:
        env["LD_LIBRARY_PATH"] = str(args.loader_directory.resolve()) + (
            os.pathsep + env["LD_LIBRARY_PATH"] if env.get("LD_LIBRARY_PATH") else ""
        )
    if args.validation_layers:
        env.update(
            MOZ_WR_VULKAN_VALIDATION="1",
            VK_LAYER_PATH=str(args.validation_layers.resolve()),
            VK_LAYER_VALIDATE_SYNC="1",
        )
    if args.adapter:
        env["MOZ_WR_VULKAN_ADAPTER"] = args.adapter
    if args.software_presentation:
        env["MESA_VK_WSI_DEBUG"] = "sw"
    prefs = {
        "remote.screenshot.use_readback": "true",
        "gfx.webrender.all": "true",
        "gfx.webrender.vulkan": "true",
        "gfx.webrender.software": "false",
        "gfx.color_management.mode": "0",
        "layout.css.devPixelsPerPx": "1.0",
        "browser.display.os-zoom-behavior": "0",
        "layers.gpu-process.enabled": args.gpu_process,
        "webgl.disabled": "false",
        "webgl.force-enabled": "true",
        "webgl.sanitize-unmasked-renderer": "false",
        "gfx.x11-egl.force-enabled": "true",
        "widget.dmabuf.force-enabled": "true",
        "widget.dmabuf-webgl.enabled": "true",
    }
    if args.benchmark_frames:
        prefs["privacy.reduceTimerPrecision"] = "false"
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
        str(benchmark_timeout + 30),
    ]
    for name, value in prefs.items():
        command += ["--setpref", f"{name}={value}"]
    command.append(str(Path(__file__).with_name("test_webgl_dmabuf_browser.py")))
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
            process = subprocess.Popen(
                command,
                cwd=root,
                env=env,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                status = process.wait(timeout=240 + benchmark_timeout)
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
