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
    parser = argparse.ArgumentParser(description="Test WebGPU DMA-BUF presentation")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--binary", type=Path, default=root / "obj-x86_64-pc-linux-gnu/dist/bin/firefox"
    )
    parser.add_argument("--transport", choices=["copy", "direct"], default="copy")
    parser.add_argument("--backend", choices=["vulkan", "gl"], default="vulkan")
    parser.add_argument("--gpu-process", choices=["true", "false"], default="true")
    parser.add_argument("--icd", type=Path)
    parser.add_argument("--validation-layers", type=Path)
    parser.add_argument("--loader-directory", type=Path)
    parser.add_argument("--adapter")
    parser.add_argument("--software-presentation", action="store_true")
    parser.add_argument("--benchmark-frames", type=int, default=0)
    parser.add_argument("--benchmark-only", action="store_true")
    parser.add_argument(
        "--scenario",
        choices=["basic", "lifecycle", "windows", "reset", "crash", "offscreen"],
        default="basic",
    )
    parser.add_argument("--record-baseline-defects", action="store_true")
    args = parser.parse_args()
    if args.benchmark_frames < 0 or (args.benchmark_only and not args.benchmark_frames):
        parser.error("Benchmark-only requires a positive frame count")
    if args.backend == "gl" and (
        args.scenario != "basic"
        or args.benchmark_frames
        or args.record_baseline_defects
    ):
        parser.error("The GL control supports strict basic correctness checks only")
    benchmark_timeout = args.benchmark_frames // 20 + 30
    if os.environ.get("WR_WEBGPU_XVFB") != "1":
        env = os.environ.copy()
        env["WR_WEBGPU_XVFB"] = "1"
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
    ]:
        env.pop(name, None)
    env.update(
        WR_WEBGPU_OUTPUT=str(output),
        WR_WEBGPU_TRANSPORT=args.transport,
        WR_WEBGPU_BACKEND=args.backend,
        WR_WEBGPU_PROCESS="GPU" if args.gpu_process == "true" else "Parent",
        WR_WEBGPU_BENCHMARK_FRAMES=str(args.benchmark_frames),
        WR_WEBGPU_BENCHMARK_TIMEOUT=str(benchmark_timeout),
        WR_WEBGPU_BENCHMARK_ONLY="1" if args.benchmark_only else "0",
        WR_WEBGPU_SCENARIO=args.scenario,
        WR_WEBGPU_LOADER_DIRECTORY=(
            str(args.loader_directory.resolve()) if args.loader_directory else ""
        ),
        WR_WEBGPU_BASELINE_DEFECTS="1" if args.record_baseline_defects else "0",
        GDK_BACKEND="x11",
        WGPU_VALIDATION="1" if args.validation_layers else "0",
        WGPU_DEBUG="1" if args.validation_layers else "0",
        MOZ_LOG="wgpu_bindings::server:3,webrender_bindings::hal_image::linux:3,WebGPU:3",
    )
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
        "gfx.webrender.vulkan": "true" if args.backend == "vulkan" else "false",
        "gfx.webrender.software": "false",
        "gfx.color_management.mode": "0",
        "layout.css.devPixelsPerPx": "1.0",
        "layers.gpu-process.enabled": args.gpu_process,
        "dom.webgpu.enabled": "true",
        "dom.webgpu.force-enabled": "true",
        "dom.webgpu.allow-present-without-readback": "true",
        "dom.webgpu.hal-labels": "true",
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
    command.append(str(Path(__file__).with_name("test_webgpu_dmabuf_browser.py")))
    print(f"Output: {output}", flush=True)
    with (output / "test.log").open("w") as log, (output / "window-manager.log").open(
        "w"
    ) as wm_log:
        wm = subprocess.Popen(
            ["openbox", "--sm-disable"],
            env=env,
            stdout=wm_log,
            stderr=subprocess.STDOUT,
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
