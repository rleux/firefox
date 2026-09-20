# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import argparse
import hashlib
import json
import math
import os
import platform
import signal
import subprocess
import sys
import time
from pathlib import Path

from renderer_benchmark_metrics import validate_report

WORKLOADS = ("static", "css", "dirty", "scroll", "canvas", "filters")


def install_termination_handler():
    def terminate(signum, _frame):
        signal.signal(signum, signal.SIG_IGN)
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, terminate)


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def group_exists(group):
    try:
        os.killpg(group, 0)
        return True
    except ProcessLookupError:
        return False


def stop(process, grace=5):
    group = process.pid
    if group_exists(group):
        try:
            os.killpg(group, signal.SIGTERM)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + grace
        while group_exists(group) and time.monotonic() < deadline:
            process.poll()
            time.sleep(0.05)
    if group_exists(group):
        try:
            os.killpg(group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=10)


def main():
    root = Path(__file__).resolve().parents[3]
    parser = argparse.ArgumentParser(description="Measure a fixed local WR workload")
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--backend", choices=("gl", "vulkan"), required=True)
    parser.add_argument("--display", choices=("xvfb", "native"), default="xvfb")
    parser.add_argument("--gpu-process", choices=("true", "false"), default="true")
    parser.add_argument(
        "--phase", choices=("smoke", "timing", "diagnostic", "memory"), default="smoke"
    )
    parser.add_argument("--workload", choices=WORKLOADS, default="static")
    parser.add_argument("--duration", type=float, default=2)
    parser.add_argument("--warmup", type=float, default=1)
    parser.add_argument("--viewport", nargs=2, type=int, default=[890, 617])
    parser.add_argument("--renderer")
    parser.add_argument("--allow-software", action="store_true")
    parser.add_argument("--icd", type=Path)
    parser.add_argument("--loader-directory", type=Path)
    parser.add_argument("--validation-layers", type=Path)
    parser.add_argument("--software-presentation", action="store_true")
    parser.add_argument("--force-full-composition", action="store_true")
    parser.add_argument("--force-full-present", action="store_true")
    args = parser.parse_args()
    if (
        not all(math.isfinite(v) for v in (args.duration, args.warmup))
        or not 0.1 <= args.duration <= 600
        or not 0 <= args.warmup <= 120
    ):
        parser.error("Duration must be 0.1–600 seconds and warmup 0–120 seconds")
    if args.viewport[0] < 800 or args.viewport[1] < 600:
        parser.error("Viewport must be at least 800 by 600 pixels")
    if args.phase == "smoke" and args.duration > 5:
        parser.error("Smoke intervals are limited to five seconds")
    if args.display == "native" and (
        not os.environ.get("DISPLAY") or args.software_presentation
    ):
        parser.error("Native runs require DISPLAY and native presentation")
    if args.phase in ("timing", "memory") and (
        args.validation_layers
        or args.loader_directory
        or args.allow_software
        or args.display != "native"
    ):
        parser.error(
            "Timing/memory require native hardware with the system loader and no validation"
        )
    if args.phase in ("timing", "memory") and not args.renderer:
        parser.error("Timing/memory require an expected renderer identity")
    if args.backend == "gl" and (
        args.force_full_composition or args.force_full_present
    ):
        parser.error("HAL full-render controls require Vulkan")
    if args.backend == "gl" and args.phase == "diagnostic":
        parser.error("GL has no HAL diagnostic schema")
    if args.display == "xvfb" and os.environ.get("WR_RENDERER_BENCHMARK_XVFB") != "1":
        env = os.environ.copy()
        env["WR_RENDERER_BENCHMARK_XVFB"] = "1"
        process = subprocess.Popen(
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
            start_new_session=True,
        )
        try:
            return process.wait()
        finally:
            stop(process, grace=30)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    binary = args.binary.resolve(strict=True)
    env = os.environ.copy()
    for key in list(env):
        if key.startswith(("WR_", "WGPU_", "VK_")) or key in (
            "MOZ_WR_BACKEND",
            "MOZ_WR_DEFAULT_BACKEND",
            "MOZ_RUN_GTEST",
            "MOZ_HEADLESS",
            "MOZ_WR_VULKAN_VALIDATION",
            "MOZ_WR_VULKAN_ADAPTER",
            "MESA_VK_WSI_DEBUG",
            "LIBGL_ALWAYS_SOFTWARE",
            "GALLIUM_DRIVER",
            "MESA_LOADER_DRIVER_OVERRIDE",
            "MOZ_LOG",
            "RUST_LOG",
        ):
            env.pop(key, None)
    env.update(
        GDK_BACKEND="x11",
        MOZ_NO_REMOTE="1",
        WGPU_VALIDATION="0",
        WGPU_DEBUG="0",
        LD_LIBRARY_PATH=str(binary.parent),
    )
    env["WR_RENDERER_BENCHMARK_OUTPUT"] = str(output)
    expected = {
        "backend": args.backend,
        "renderer": args.renderer,
        "process": "GPU" if args.gpu_process == "true" else "Parent",
        "viewport": args.viewport,
        "dpr": 1,
        "allowSoftware": args.allow_software,
        "phase": args.phase,
        "workload": args.workload,
    }
    config = {
        "expected": expected,
        "duration": args.duration,
        "warmup": args.warmup,
        "display": args.display,
    }
    (output / "config.json").write_text(json.dumps(config, indent=2) + "\n")
    if args.icd:
        env["VK_DRIVER_FILES"] = str(args.icd.resolve(strict=True))
    if args.loader_directory:
        env["LD_LIBRARY_PATH"] = (
            str(args.loader_directory.resolve(strict=True))
            + os.pathsep
            + env["LD_LIBRARY_PATH"]
        )
    if args.validation_layers:
        env.update(
            WGPU_VALIDATION="1",
            WGPU_DEBUG="1",
            MOZ_WR_VULKAN_VALIDATION="1",
            VK_LAYER_PATH=str(args.validation_layers.resolve(strict=True)),
            VK_LAYER_VALIDATE_SYNC="1",
        )
    if args.software_presentation:
        env["MESA_VK_WSI_DEBUG"] = "sw"
    if args.phase == "diagnostic" and args.backend == "vulkan":
        env["WR_HAL_RENDER_METRICS"] = "1"
    if args.force_full_composition:
        env["WR_HAL_FORCE_FULL_COMPOSITION"] = "1"
    if args.force_full_present:
        env["WR_HAL_FORCE_FULL_PRESENT"] = "1"
    prefs = {
        "remote.screenshot.use_readback": "true",
        "gfx.webrender.all": "true",
        "gfx.webrender.vulkan": str(args.backend == "vulkan").lower(),
        "gfx.webrender.software": "false",
        "gfx.color_management.mode": "0",
        "layout.css.devPixelsPerPx": "1.0",
        "browser.display.os-zoom-behavior": "0",
        "layers.gpu-process.enabled": args.gpu_process,
        "privacy.reduceTimerPrecision": "false",
    }
    timeout = math.ceil(args.duration + args.warmup) + 90
    command = [
        str(root / "mach"),
        "marionette-test",
        "--binary",
        str(binary),
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
        command.extend(["--setpref", f"{name}={value}"])
    command.append(str(Path(__file__).with_name("test_renderer_benchmark.py")))
    libxul = binary.with_name("libxul.so").resolve(strict=True)
    sources = [
        Path(__file__),
        Path(__file__).with_name("test_renderer_benchmark.py"),
        Path(__file__).with_name("renderer_benchmark.html"),
        Path(__file__).with_name("renderer_benchmark_metrics.py"),
        binary,
        libxul,
    ]
    hashes = {str(path): sha256(path) for path in sources}
    runtime = {
        "binary": {"path": str(binary), "sha256": hashes[str(binary)]},
        "libxul": {"path": str(libxul), "sha256": hashes[str(libxul)]},
    }
    expected["runtime"] = runtime
    (output / "config.json").write_text(json.dumps(config, indent=2) + "\n")
    manifest = {
        "schemaVersion": 1,
        "config": config,
        "command": command,
        "host": platform.uname()._asdict(),
        "hashes": hashes,
        "runtime": runtime,
        "environment": {
            k: v
            for k, v in env.items()
            if k.startswith(("WR_", "WGPU_", "VK_", "MESA_", "MOZ_", "GDK_"))
            or k in ("DISPLAY", "LD_LIBRARY_PATH")
        },
        "prefs": prefs,
        "status": "starting",
    }
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    status = 1
    with (output / "driver.log").open("w") as log, (output / "window-manager.log").open(
        "w"
    ) as wm_log:
        wm = (
            subprocess.Popen(
                ["openbox", "--sm-disable"],
                env=env,
                stdout=wm_log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            if args.display == "xvfb"
            else None
        )
        process = None
        started = time.monotonic()
        try:
            process = subprocess.Popen(
                command,
                cwd=root,
                env=env,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            status = process.wait(timeout=timeout + 150)
        except subprocess.TimeoutExpired:
            status = 124
        finally:
            if process:
                stop(process)
            if wm:
                stop(wm)
            report_errors = []
            report_path = output / "report.json"
            try:
                report = json.loads(report_path.read_text())
                report_errors = validate_report(report, expected)
                manifest["reportSha256"] = sha256(report_path)
            except (OSError, json.JSONDecodeError) as error:
                report_errors = [f"report unavailable: {error}"]
            if status == 0 and report_errors:
                status = 125
            manifest["reportValidationErrors"] = report_errors
            manifest.update(
                status=status, launcherElapsedSeconds=time.monotonic() - started
            )
            (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Renderer benchmark: {output}; exit {status}", flush=True)
    return status


if __name__ == "__main__":
    install_termination_handler()
    raise SystemExit(main())
