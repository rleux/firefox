# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import base64
import http.server
import json
import os
import re
import struct
import sys
import threading
from pathlib import Path

from marionette_harness import MarionetteTestCase

sys.path.insert(0, str(Path(__file__).parent))
from webgpu_process_metrics import ProcessMetrics


class TestWebGPUDMABuf(MarionetteTestCase):
    def setUp(self):
        super().setUp()
        self.output = Path(os.environ["WR_WEBGPU_OUTPUT"])
        self.report = {
            "cases": [],
            "knownFailures": [],
            "pixelMismatches": [],
            "snapshotMismatches": [],
            "display": os.environ.get("DISPLAY"),
            "presentation": {
                "mode": os.environ.get("WR_WEBGPU_DISPLAY_MODE", "xvfb"),
                "wsiDebug": os.environ.get("MESA_VK_WSI_DEBUG"),
            },
        }
        page = Path(__file__).with_name("webgpu_dmabuf.html").read_bytes()

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(page)))
                self.end_headers()
                self.wfile.write(page)

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.marionette.set_context("content")
        self.marionette.set_window_rect(x=80, y=80, width=900, height=800)
        self.url = f"http://localhost:{self.server.server_port}/"
        self.marionette.navigate(self.url)
        viewport = os.environ.get("WR_WEBGPU_VIEWPORT")
        if viewport:
            width, height = map(int, viewport.split("x"))
            for _ in range(3):
                current = self.marionette.execute_script(
                    "return [innerWidth, innerHeight];"
                )
                if current == [width, height]:
                    break
                outer = self.marionette.window_rect
                self.marionette.set_window_rect(
                    width=outer["width"] + width - current[0],
                    height=outer["height"] + height - current[1],
                )
            self.assertEqual(
                self.marionette.execute_script("return [innerWidth, innerHeight];"),
                [width, height],
            )
        if self.report["presentation"]["mode"] == "native":
            with self.marionette.using_context("chrome"):
                self.marionette.execute_script("window.focus();")
            self.marionette.find_element("css selector", "canvas").click()
        self.report["screen"] = self.marionette.execute_script("""
            return {
              width: screen.width, height: screen.height,
              availableWidth: screen.availWidth, availableHeight: screen.availHeight,
              innerWidth, innerHeight, outerWidth, outerHeight,
              devicePixelRatio, visibility: document.visibilityState,
              focused: document.hasFocus()
            };
        """)
        if self.report["presentation"]["mode"] == "native":
            self.assertIsNone(self.report["presentation"]["wsiDebug"])
            self.assertEqual(self.report["screen"]["devicePixelRatio"], 1)
            self.assertEqual(self.report["screen"]["visibility"], "visible")
            self.assertTrue(self.report["screen"]["focused"])

    def tearDown(self):
        try:
            (self.output / "report.json").write_text(
                json.dumps(self.report, indent=2) + "\n"
            )
            self.server.shutdown()
            self.server.server_close()
            self.thread.join()
        finally:
            super().tearDown()

    def call(self, method, *args):
        result = self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            Promise.resolve().then(() => window[arguments[0]](...arguments[1]))
              .then(done, error => done({error: String(error)}));
            """,
            script_args=[method, list(args)],
            sandbox=None,
            script_timeout=(
                int(os.environ["WR_WEBGPU_BENCHMARK_TIMEOUT"]) * 1000
                if method == "benchmark"
                else None
            ),
        )
        if isinstance(result, dict):
            self.assertNotIn("error", result)
        return result

    def log(self):
        return (self.output / "gecko.log").read_text(errors="replace")

    def pixels(self, label, points=None):
        screenshot = self.marionette.screenshot(full=False)
        png = base64.b64decode(screenshot)
        (self.output / f"{label}.png").write_bytes(png)
        size = list(struct.unpack(">II", png[16:24]))
        self.report.setdefault("screenshotSizes", {})[label] = size
        if viewport := os.environ.get("WR_WEBGPU_VIEWPORT"):
            self.assertEqual(size, list(map(int, viewport.split("x"))))
        return self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            const image = new Image();
            image.onload = () => {
              const canvas = document.createElement('canvas');
              canvas.width = image.width; canvas.height = image.height;
              const context = canvas.getContext('2d');
              context.drawImage(image, 0, 0);
              const points = arguments[1] || [[image.width / 2, image.height / 4], [image.width / 2, image.height * 0.75]];
              done(points.map(([x, y]) => [...context.getImageData(Math.floor(x), Math.floor(y), 1, 1).data]));
            };
            image.onerror = () => done({error: 'Screenshot decode failed'});
            image.src = 'data:image/png;base64,' + arguments[0];
            """,
            script_args=[screenshot, points],
        )

    def transport_generations(self):
        log = self.log()
        marker = (
            "directly sampled"
            if os.environ["WR_WEBGPU_TRANSPORT"] == "direct"
            else "materialized"
        )
        return (
            re.findall(
                r"WebGPU canvas transport: Vulkan DMA-BUF, generation=(\d+)", log
            ),
            re.findall(
                r"WebRender Vulkan DMA-BUF " + marker + r": generation=(\d+)", log
            ),
        )

    def measure(self, frames):
        native = self.report["presentation"]["mode"] == "native"
        if native:
            self.marionette.execute_script(
                """
                window.presentationEvents = [];
                window.addEventListener('blur', () =>
                  window.presentationEvents.push({type: 'blur', time: performance.now()}));
                document.addEventListener('visibilitychange', () =>
                  window.presentationEvents.push({type: document.visibilityState, time: performance.now()}));
            """,
                sandbox=None,
            )
        mode = os.environ.get("WR_WEBGPU_PROCESS_METRICS", "full")
        if mode == "off":
            self.report["benchmark"] = self.call("benchmark", frames)
            self.report["processMetrics"] = []
        else:
            with self.marionette.using_context("chrome"):
                pid = self.marionette.execute_script(
                    "return Services.appinfo.processID;"
                )
            with ProcessMetrics(pid, include_memory=mode == "full") as metrics:
                self.report["benchmark"] = self.call("benchmark", frames)
            self.report["processMetrics"] = metrics.samples
        self.assertEqual(len(self.report["benchmark"]["samples"]), frames)
        if native:
            self.report["presentationEvents"] = self.marionette.execute_script(
                "return window.presentationEvents;", sandbox=None
            )
            self.assertEqual(self.report["presentationEvents"], [])
            self.assertTrue(
                self.marionette.execute_script("return document.hasFocus();")
            )

    def check_errors(self, allow_reset=False):
        log = self.log()
        if os.environ["WR_WEBGPU_TRANSPORT"] == "direct":
            self.assertNotIn("WebRender Vulkan DMA-BUF materialized:", log)
        errors = [
            "Validation Error",
            "VUID-",
            "panicked",
            "Failed to render",
            "Vulkan canvas publication failed",
        ]
        if not allow_reset:
            errors.append("DeviceReset")
        for error in errors:
            self.assertNotIn(error, log)
        self.assertEqual(
            self.marionette.execute_script(
                "return [...window.gpuErrors];", sandbox=None
            ),
            [],
        )

    def render_checked(self, label):
        published, consumed = self.transport_generations()
        self.assertEqual(self.call("renderCase", "bgra8unorm", "opaque", False, 0), [])
        pixels = self.pixels(label)
        self.assertEqual(pixels, [[255, 0, 0, 255], [0, 0, 255, 255]])
        after, sampled = self.transport_generations()
        self.assertGreater(len(after), len(published))
        self.assertIn(after[-1], sampled[len(consumed) :])
        return pixels

    def check_lifecycle(self):
        self.report["resizes"] = []
        for width, height in [(17, 13), (640, 360), (1280, 720), (256, 128)]:
            self.assertEqual(self.call("resizeAndRender", width, height), [])
            self.report["resizes"].append({
                "size": [width, height],
                "pixels": self.render_checked(f"resize-{width}-{height}"),
            })
        published, consumed = self.transport_generations()
        points = self.call("multipleCanvases")
        pixels = self.pixels("multiple-canvases", points)
        self.assertEqual(pixels, [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]])
        after, sampled = self.transport_generations()
        self.assertGreaterEqual(len(after) - len(published), 3)
        self.assertTrue(
            set(after[len(published) :]).issubset(set(sampled[len(consumed) :]))
        )
        self.report["multipleCanvases"] = pixels
        self.assertEqual(self.call("clearExtraCanvases"), {"remaining": 1})
        self.render_checked("after-canvas-removal")
        self.report["deviceLoss"] = self.call("destroyDevice")
        self.assertEqual(self.report["deviceLoss"]["reason"], "destroyed")
        self.call("recreateDevice")
        self.render_checked("after-device-recreation")

    def check_windows(self):
        original = self.marionette.current_window_handle
        self.report["windows"] = []
        for kind in ["window", "tab", "window"]:
            handle = self.marionette.open(type=kind, focus=True)["handle"]
            try:
                self.marionette.switch_to_window(handle)
                self.marionette.navigate(self.url)
                self.report["windows"].append({
                    "kind": kind,
                    "pixels": self.render_checked(
                        f"other-{len(self.report['windows'])}"
                    ),
                })
                self.call("benchmark", 20)
            finally:
                self.marionette.close()
                self.marionette.switch_to_window(original)
            self.render_checked(f"after-{kind}-closure")

    def check_offscreen(self):
        self.call("useOffscreenCanvas")
        self.report["offscreen"] = []
        for alpha, expected in [
            ("opaque", [[128, 0, 0, 255], [0, 0, 128, 255]]),
            ("premultiplied", [[255, 127, 127, 255], [127, 127, 255, 255]]),
        ]:
            published, consumed = self.transport_generations()
            self.assertEqual(self.call("renderCase", "bgra8unorm", alpha, False, 8), [])
            pixels = self.pixels("offscreen-" + alpha)
            snapshot = self.call("snapshotPixels")
            self.report["offscreen"].append({
                "alpha": alpha,
                "pixels": pixels,
                "snapshot": snapshot,
            })
            self.assertEqual(pixels, expected)
            self.assertEqual(snapshot, expected)
            after, sampled = self.transport_generations()
            self.assertGreater(len(after), len(published))
            self.assertIn(after[-1], sampled[len(consumed) :])

    def check_reset(self):
        crash = os.environ["WR_WEBGPU_SCENARIO"] == "crash"
        if crash:
            self.call("expectDeviceLoss")
        before = self.log().count("WebRender backend: Vulkan (wgpu-hal)")
        with self.marionette.using_context("chrome"):
            if crash:
                self.assertGreater(
                    self.marionette.execute_script(
                        "return window.windowUtils.gpuProcessPid;"
                    ),
                    0,
                )
                self.marionette.execute_script(
                    "window.windowUtils.terminateGPUProcess();"
                )
            else:
                self.marionette.execute_script(
                    "window.windowUtils.triggerDeviceReset();"
                )
        self.wait_for_condition(
            lambda _: self.log().count("WebRender backend: Vulkan (wgpu-hal)") > before,
            timeout=30,
        )
        self.report["resetInitialPixels"] = self.pixels("reset-initial")
        if crash:
            self.wait_for_condition(
                lambda _: (
                    self.marionette.execute_script(
                        "return window.gpuLosses.length;", sandbox=None
                    )
                    > 0
                ),
                timeout=20,
            )
            self.call("recreateDevice")
        self.report["resetPixels"] = self.render_checked("after-reset")

    def test_formats_alpha_initialization_and_repeated_present(self):
        with self.marionette.using_context("chrome"):
            self.report["backend"] = self.marionette.execute_async_script("""
                const done = arguments[arguments.length - 1];
                window.windowUtils.getWebRenderBackendInfo().then(
                  value => done(JSON.parse(value)), error => done({error: String(error)}));
            """)
        vulkan = os.environ.get("WR_WEBGPU_BACKEND", "vulkan") == "vulkan"
        self.assertIn(
            self.report["backend"]["backend"],
            ["Vulkan (wgpu-hal)"] if vulkan else ["OpenGL", "OpenGL ES"],
        )
        self.assertEqual(
            self.report["backend"]["process"], os.environ["WR_WEBGPU_PROCESS"]
        )
        if self.report["presentation"]["mode"] == "native":
            self.assertNotRegex(
                self.report["backend"]["renderer"].lower(),
                "llvmpipe|lavapipe|swiftshader|software",
            )
        if os.environ.get("WR_WEBGPU_LOADER_DIRECTORY"):
            with self.marionette.using_context("chrome"):
                pid = self.marionette.execute_script(
                    "return window.windowUtils.gpuProcessPid;"
                    if self.report["backend"]["process"] == "GPU"
                    else "return Services.appinfo.processID;"
                )
            self.assertGreater(pid, 0)
            mappings = [
                line.split(maxsplit=5)[-1]
                for line in Path(f"/proc/{pid}/maps").read_text().splitlines()
                if "libvulkan.so" in line
            ]
            self.report["vulkanLoaderMappings"] = sorted(set(mappings))
            self.assertTrue(mappings)
            self.assertTrue(
                all(
                    path.startswith(os.environ["WR_WEBGPU_LOADER_DIRECTORY"] + os.sep)
                    for path in mappings
                )
            )
        frames = int(os.environ.get("WR_WEBGPU_BENCHMARK_FRAMES", "0"))
        if os.environ.get("WR_WEBGPU_BENCHMARK_ONLY") == "1":
            self.measure(frames)
            red = (frames + 14) % 2
            self.assertEqual(
                self.pixels("benchmark-final"),
                [[red * 255, 0, (1 - red) * 255, 255]] * 2,
            )
            if os.environ.get("WR_WEBGPU_BENCHMARK_QUIET") == "1":
                selected = re.findall(
                    r"WebRender Vulkan DMA-BUF selected transport: (direct|copy)",
                    self.log(),
                )
                self.assertEqual(set(selected), {os.environ["WR_WEBGPU_TRANSPORT"]})
                self.report["selectedTransports"] = selected
                self.assertNotIn("HAL rendered WR frame:", self.log())
            else:
                published, consumed = self.transport_generations()
                self.assertTrue(published and consumed)
                self.assertEqual(published[-1], consumed[-1])
                self.report["presentationCounts"] = {
                    "published": len(set(published)),
                    "consumed": len(set(consumed)),
                }
            self.check_errors()
            return
        direct = os.environ["WR_WEBGPU_TRANSPORT"] == "direct"
        marker = (
            "WebRender Vulkan DMA-BUF directly sampled:"
            if direct
            else "WebRender Vulkan DMA-BUF materialized:"
        )
        for fmt in ["bgra8unorm", "rgba8unorm"]:
            cases = [("opaque", False, phase) for phase in range(9)]
            cases += [
                ("opaque", True, 0),
                ("premultiplied", False, 0),
                ("premultiplied", True, 0),
            ]
            for alpha, discard, phase in cases:
                label = f"{fmt}-{alpha}-{discard}-{phase}"
                before = self.log().count(marker)
                self.assertEqual(
                    self.call("renderCase", fmt, alpha, discard, phase), []
                )
                pixels = self.pixels(label)
                if discard:
                    expected = [
                        [0, 0, 0, 255] if alpha == "opaque" else [255, 255, 255, 255]
                    ] * 2
                elif alpha == "premultiplied":
                    expected = [[255, 127, 127, 255], [127, 127, 255, 255]]
                elif phase >= 8:
                    expected = [[128, 0, 0, 255], [0, 0, 128, 255]]
                else:
                    expected = [[255, 0, 0, 255], [0, 0, 255, 255]]
                if phase % 2:
                    expected.reverse()
                snapshot = self.call("snapshotPixels")
                self.report["cases"].append({
                    "case": label,
                    "pixels": pixels,
                    "snapshot": snapshot,
                })
                if snapshot != expected:
                    self.report["snapshotMismatches"].append({
                        "case": label,
                        "expected": expected,
                        "actual": snapshot,
                    })
                if (
                    os.environ.get("WR_WEBGPU_BASELINE_DEFECTS") == "1"
                    and not direct
                    and alpha == "opaque"
                    and (
                        (
                            phase == 8
                            and pixels == [[255, 127, 127, 255], [127, 127, 255, 255]]
                        )
                        or (discard and pixels == [[255, 255, 255, 255]] * 2)
                    )
                ):
                    self.report["knownFailures"].append({
                        "case": label,
                        "expected": expected,
                        "actual": pixels,
                    })
                elif pixels != expected:
                    self.report["pixelMismatches"].append({
                        "case": label,
                        "expected": expected,
                        "actual": pixels,
                    })
                if not vulkan:
                    continue
                log = self.log()
                self.assertGreater(log.count(marker), before, label)
                published = re.findall(
                    r"WebGPU canvas transport: Vulkan DMA-BUF, generation=(\d+)", log
                )
                consumed = re.findall(re.escape(marker) + r" generation=(\d+)", log)
                self.assertTrue(published)
                self.assertEqual(consumed[-1], published[-1], label)
        self.report["adapter"] = self.marionette.execute_script(
            "return window.adapterInfo;", sandbox=None
        )
        if direct or os.environ.get("WR_WEBGPU_BASELINE_DEFECTS") != "1":
            self.report["transformedAlpha"] = []
            for alpha, expected in [
                ("opaque", [[192, 128, 128, 255], [128, 128, 192, 255]]),
                ("premultiplied", [[255, 191, 191, 255], [191, 191, 255, 255]]),
            ]:
                self.assertEqual(
                    self.call("renderCase", "bgra8unorm", alpha, False, 8), []
                )
                self.call("setDimmed", True)
                pixels = self.pixels("transformed-" + alpha)
                self.report["transformedAlpha"].append({
                    "alpha": alpha,
                    "pixels": pixels,
                })
                if any(
                    abs(value - reference) > 1
                    for actual, wanted in zip(pixels, expected)
                    for value, reference in zip(actual, wanted)
                ):
                    self.report["pixelMismatches"].append({
                        "case": "transformed-" + alpha,
                        "expected": expected,
                        "actual": pixels,
                    })
                self.call("setDimmed", False)
        if frames:
            self.measure(frames)
        self.check_errors()
        self.assertEqual(self.report["snapshotMismatches"], [])
        self.assertEqual(self.report["pixelMismatches"], [])
        scenario = os.environ.get("WR_WEBGPU_SCENARIO", "basic")
        if scenario != "basic":
            getattr(self, "check_" + ("reset" if scenario == "crash" else scenario))()
            self.check_errors(allow_reset=scenario in ["reset", "crash"])
