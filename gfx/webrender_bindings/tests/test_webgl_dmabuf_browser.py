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


class TestWebGLDMABuf(MarionetteTestCase):
    def setUp(self):
        super().setUp()
        self.output = Path(os.environ["WR_WEBGL_OUTPUT"])
        self.report = {
            "passed": False,
            "cases": [],
            "pixelMismatches": [],
            "snapshotMismatches": [],
            "display": os.environ.get("DISPLAY"),
            "presentation": {
                "mode": os.environ.get("WR_WEBGL_DISPLAY_MODE", "xvfb"),
                "wsiDebug": os.environ.get("MESA_VK_WSI_DEBUG"),
            },
        }
        page = Path(__file__).with_name("webgl_dmabuf.html").read_bytes()

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
        self.url = f"http://127.0.0.1:{self.server.server_port}/"
        self.marionette.navigate(self.url)
        viewport = os.environ.get("WR_WEBGL_VIEWPORT")
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
        self.report["screen"] = self.marionette.execute_script(
            """
            return {
              width: screen.width, height: screen.height,
              innerWidth, innerHeight, outerWidth, outerHeight,
              devicePixelRatio, visibility: document.visibilityState,
              focused: document.hasFocus()
            };
        """
        )
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
                int(os.environ["WR_WEBGL_BENCHMARK_TIMEOUT"]) * 1000
                if method == "benchmark"
                else None
            ),
        )
        if isinstance(result, dict):
            self.assertNotIn("error", result)
        return result

    def log(self):
        return (self.output / "gecko.log").read_text(errors="replace")

    def direct_presentations(self):
        return self.log().count(
            "WebGL canvas transport: direct Vulkan DMA-BUF sampling"
        )

    def present(self, method, *args):
        before = self.direct_presentations()
        result = self.call(method, *args)
        return result, before

    def verify_new_publication(self, before):
        self.wait_for_condition(
            lambda _: self.direct_presentations() > before, timeout=5
        )

    def pixels(self, label):
        screenshot = self.marionette.screenshot(full=False)
        png = base64.b64decode(screenshot)
        (self.output / f"{label}.png").write_bytes(png)
        size = list(struct.unpack(">II", png[16:24]))
        self.report.setdefault("screenshotSizes", {})[label] = size
        if viewport := os.environ.get("WR_WEBGL_VIEWPORT"):
            self.assertEqual(size, list(map(int, viewport.split("x"))))
        return self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            const image = new Image();
            image.onload = () => {
              const canvas = document.createElement("canvas");
              canvas.width = image.width; canvas.height = image.height;
              const context = canvas.getContext("2d");
              context.drawImage(image, 0, 0);
              done([0.25, 0.75].map(y => [...context.getImageData(
                Math.floor(image.width / 2), Math.floor(image.height * y), 1, 1).data]));
            };
            image.onerror = () => done({error: "Screenshot decode failed"});
            image.src = "data:image/png;base64," + arguments[0];
            """,
            script_args=[screenshot],
        )

    def expected_pixels(self, alpha, phase):
        red = [255, 127, 127, 255] if alpha else [255, 0, 0, 255]
        blue = [127, 127, 255, 255] if alpha else [0, 0, 255, 255]
        return [blue, red] if phase % 2 else [red, blue]

    def benchmark_expected_pixels(self, frames):
        counter = frames + 15
        low = counter & 255
        high = (counter >> 8) & 255
        return [[low, high, 64, 255], [96, low, high, 255]]

    def assert_pixels(self, label, actual, expected, snapshot=False):
        mismatch = len(actual) != len(expected) or any(
            len(observed) != len(wanted)
            or any(
                abs(value - reference) > 1 for value, reference in zip(observed, wanted)
            )
            for observed, wanted in zip(actual, expected)
        )
        if mismatch:
            key = "snapshotMismatches" if snapshot else "pixelMismatches"
            self.report[key].append({
                "case": label,
                "expected": expected,
                "actual": actual,
            })
        self.assertFalse(mismatch, (label, actual, expected))

    def backend(self):
        with self.marionette.using_context("chrome"):
            return self.marionette.execute_async_script(
                """
                const done = arguments[arguments.length - 1];
                window.windowUtils.getWebRenderBackendInfo().then(
                  value => done(JSON.parse(value)), error => done({error: String(error)}));
            """
            )

    def verify_transport(self):
        log = self.log()
        modes = re.findall(
            r"WebRender Vulkan WebGL selected transport: direct; synchronization: (sync|async)",
            log,
        )
        expected = os.environ["WR_WEBGL_SYNCHRONIZATION"]
        self.assertEqual(set(modes), {expected})
        self.report["transport"] = {
            "synchronizationMarkers": modes,
        }

    def check_errors(self):
        log = self.log()
        for error in [
            "Validation Error",
            "VUID-",
            "panicked",
            "Failed to render",
            "Remote texture creation failed",
        ]:
            self.assertNotIn(error, log)
        self.assertEqual(
            self.marionette.execute_script(
                "return [...window.webglErrors];", sandbox=None
            ),
            [],
        )

    def record_context(self):
        info = self.call("contextInfo")
        contexts = self.report.setdefault("glContexts", [])
        if info not in contexts:
            contexts.append(info)
        renderer = info.get("unmaskedRenderer")
        self.assertTrue(renderer, info)
        for software in ("llvmpipe", "softpipe", "swiftshader", "software"):
            self.assertNotIn(software, renderer.lower())

    def measure(self, frames):
        native = self.report["presentation"]["mode"] == "native"
        if native:
            self.marionette.execute_script(
                """
                window.presentationEvents = [];
                window.addEventListener("blur", () => window.presentationEvents.push("blur"));
                document.addEventListener("visibilitychange", () =>
                  window.presentationEvents.push(document.visibilityState));
            """,
                sandbox=None,
            )
        mode = os.environ.get("WR_WEBGL_PROCESS_METRICS", "full")
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
                self.marionette.execute_script(
                    "return document.hasFocus();", sandbox=None
                )
            )

    def render_checked(self, label):
        _, before = self.present("renderCase", 2, False, True, 0)
        pixels = self.pixels(label)
        self.assert_pixels(label, pixels, self.expected_pixels(False, 0))
        self.verify_new_publication(before)
        return pixels

    def check_lifecycle(self):
        self.report["resizes"] = []
        for width, height in [(17, 13), (640, 360), (1280, 720), (256, 128)]:
            _, before = self.present("resizeAndRender", width, height)
            pixels = self.pixels(f"resize-{width}-{height}")
            self.assert_pixels(
                f"resize-{width}-{height}",
                pixels,
                self.expected_pixels(False, 0),
            )
            self.verify_new_publication(before)
            self.report["resizes"].append({"size": [width, height], "pixels": pixels})

    def check_windows(self):
        original = self.marionette.current_window_handle
        self.report["windows"] = []
        for kind in ["window", "tab"]:
            handle = self.marionette.open(type=kind, focus=True)["handle"]
            try:
                self.marionette.switch_to_window(handle)
                self.marionette.navigate(self.url)
                self.report["windows"].append({
                    "kind": kind,
                    "pixels": self.render_checked(f"other-{kind}"),
                })
            finally:
                self.marionette.close()
                self.marionette.switch_to_window(original)
            self.render_checked(f"after-{kind}")

    def check_context_loss(self):
        self.render_checked("before-context-loss")
        self.report["contextLoss"], before = self.present("loseAndRestore")
        self.assertTrue(self.report["contextLoss"]["sameContext"])
        self.assertEqual(
            self.marionette.execute_script(
                "return window.contextEvents;", sandbox=None
            ),
            ["lost", "restored"],
        )
        pixels = self.pixels("after-context-loss")
        self.assert_pixels("after-context-loss", pixels, self.expected_pixels(False, 0))
        self.verify_new_publication(before)
        self.report["contextLossPixels"] = pixels

    def check_offscreen(self):
        _, before = self.present("renderCase", 2, False, True, 0, True)
        pixels = self.pixels("offscreen")
        self.assert_pixels("offscreen", pixels, self.expected_pixels(False, 0))
        self.verify_new_publication(before)
        _, before = self.present("workerOffscreen")
        worker_pixels = self.pixels("worker-offscreen")
        self.assert_pixels(
            "worker-offscreen", worker_pixels, self.expected_pixels(False, 0)
        )
        self.verify_new_publication(before)
        _, before = self.present("recoverWorkerOffscreen")
        recovered = self.pixels("worker-recovered")
        self.assert_pixels(
            "worker-recovered", recovered, self.expected_pixels(False, 0)
        )
        self.verify_new_publication(before)
        self.report["offscreen"] = {
            "mainThread": pixels,
            "worker": worker_pixels,
            "recovered": recovered,
        }

    def test_webgl_dmabuf(self):
        self.report["backend"] = self.backend()
        self.assertEqual(self.report["backend"]["backend"], "Vulkan (wgpu-hal)")
        self.assertEqual(
            self.report["backend"]["process"], os.environ["WR_WEBGL_PROCESS"]
        )
        with self.marionette.using_context("chrome"):
            pid = self.marionette.execute_script(
                "return window.windowUtils.gpuProcessPid;"
                if self.report["backend"]["process"] == "GPU"
                else "return Services.appinfo.processID;"
            )
        mappings = [
            line.split(maxsplit=5)[-1]
            for line in Path(f"/proc/{pid}/maps").read_text().splitlines()
            if "libvulkan.so" in line
        ]
        self.report["vulkanLoaderMappings"] = sorted(set(mappings))
        self.assertTrue(mappings)
        if directory := os.environ.get("WR_WEBGL_LOADER_DIRECTORY"):
            self.assertTrue(
                all(path.startswith(directory + os.sep) for path in mappings)
            )
        frames = int(os.environ.get("WR_WEBGL_BENCHMARK_FRAMES", "0"))
        if os.environ.get("WR_WEBGL_BENCHMARK_ONLY") == "1":
            self.measure(frames)
            self.record_context()
            pixels = self.pixels("benchmark-final")
            self.assert_pixels(
                "benchmark-final",
                pixels,
                self.benchmark_expected_pixels(frames),
            )
            self.report["benchmarkFinalPixels"] = pixels
            self.verify_transport()
            self.check_errors()
            self.report["passed"] = True
            return
        for version in [1, 2]:
            for alpha in [False, True]:
                for preserve in [False, True]:
                    for phase in [0, 1]:
                        label = f"webgl{version}-{alpha}-{preserve}-{phase}"
                        if phase == 0:
                            _, before = self.present(
                                "renderCase", version, alpha, preserve, phase
                            )
                        else:
                            self.call("renderCase", version, alpha, preserve, phase)
                        if phase == 0 and not preserve:
                            self.record_context()
                        pixels = self.pixels(label)
                        expected = self.expected_pixels(alpha, phase)
                        self.assert_pixels(label, pixels, expected)
                        if phase == 0:
                            self.verify_new_publication(before)
                        result = {
                            "version": version,
                            "alpha": alpha,
                            "preserveDrawingBuffer": preserve,
                            "phase": phase,
                            "pixels": pixels,
                        }
                        if preserve:
                            snapshot = self.call("snapshotPixels")
                            self.assert_pixels(label, snapshot, expected, snapshot=True)
                            result["snapshot"] = snapshot
                        self.report["cases"].append(result)
        if frames:
            self.measure(frames)
        scenario = os.environ.get("WR_WEBGL_SCENARIO", "basic")
        if scenario != "basic":
            getattr(self, "check_" + scenario.replace("-", "_"))()
        self.verify_transport()
        self.check_errors()
        self.report["passed"] = True
