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
from urllib.parse import urlsplit

from marionette_harness import MarionetteTestCase

sys.path.insert(0, str(Path(__file__).parent))
from webgpu_process_metrics import ProcessMetrics


class TestVideoTransferBenchmark(MarionetteTestCase):
    def setUp(self):
        super().setUp()
        self.output = Path(os.environ["WR_VIDEO_BENCHMARK_OUTPUT"])
        self.report = {
            "passed": False,
            "display": os.environ.get("DISPLAY"),
            "presentation": os.environ["WR_VIDEO_BENCHMARK_DISPLAY"],
            "synchronization": os.environ["WR_VIDEO_BENCHMARK_SYNCHRONIZATION"],
            "durationSeconds": float(os.environ["WR_VIDEO_BENCHMARK_DURATION"]),
            "quiet": os.environ.get("WR_VIDEO_BENCHMARK_QUIET") == "1",
        }
        clip = Path(os.environ["WR_VIDEO_BENCHMARK_CLIP"]).read_bytes()
        page = Path(__file__).with_name("video_transfer_benchmark.html").read_bytes()

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                if urlsplit(self.path).path == "/clip.webm":
                    body, content_type = clip, "video/webm"
                else:
                    body, content_type = page, "text/html; charset=utf-8"
                self.send_response(200)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server_thread = threading.Thread(
            target=self.server.serve_forever, daemon=True
        )
        self.server_thread.start()
        self.marionette.set_context("content")
        self.marionette.set_window_rect(x=80, y=80, width=900, height=800)
        self.marionette.navigate(f"http://127.0.0.1:{self.server.server_port}/")
        width, height = map(int, os.environ["WR_VIDEO_BENCHMARK_VIEWPORT"].split("x"))
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
        if self.report["presentation"] == "native":
            with self.marionette.using_context("chrome"):
                self.marionette.execute_script("window.focus();")
            self.marionette.find_element("css selector", "video").click()
        self.report["screen"] = self.marionette.execute_script(
            """
            return {
              width: screen.width, height: screen.height,
              availableWidth: screen.availWidth, availableHeight: screen.availHeight,
              innerWidth, innerHeight, outerWidth, outerHeight,
              devicePixelRatio, visibility: document.visibilityState,
              focused: document.hasFocus()
            };
            """
        )
        if self.report["presentation"] == "native":
            self.assertEqual(self.report["screen"]["devicePixelRatio"], 1)
            self.assertEqual(self.report["screen"]["visibility"], "visible")
            self.assertTrue(self.report["screen"]["focused"])

    def tearDown(self):
        try:
            (self.output / "report.json").write_text(
                json.dumps(self.report, indent=2) + "\n"
            )
            self.marionette.navigate("about:blank")
            self.server.shutdown()
            self.server.server_close()
            self.server_thread.join()
        finally:
            super().tearDown()

    def call(self, method, *args, timeout=None):
        result = self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            const [method, args] = arguments;
            Promise.resolve().then(() => window.videoTransferBenchmark[method](...args))
              .then(done, error => done({error: String(error)}));
            """,
            script_args=[method, list(args)],
            sandbox=None,
            script_timeout=timeout,
        )
        self.assertNotIn("error", result, result)
        return result

    def debug_info(self):
        return self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            document.querySelector("video").mozRequestDebugInfo().then(done,
              error => done({error: String(error)}));
            """,
            sandbox="system",
        )

    def backend_info(self):
        with self.marionette.using_context("chrome"):
            self.report["features"] = self.marionette.execute_script(
                "return Cc['@mozilla.org/gfx/info;1'].getService(Ci.nsIGfxInfo).getFeatures();"
            )
            return self.marionette.execute_async_script(
                """
                const done = arguments[arguments.length - 1];
                window.windowUtils.getWebRenderBackendInfo().then(
                  info => done(JSON.parse(info)), error => done({error: String(error)}));
                """
            )

    def record_loader(self, process):
        with self.marionette.using_context("chrome"):
            pid = self.marionette.execute_script(
                "return window.windowUtils.gpuProcessPid;"
                if process == "GPU"
                else "return Services.appinfo.processID;"
            )
        mappings = sorted({
            line.split(maxsplit=5)[-1]
            for line in Path(f"/proc/{pid}/maps").read_text().splitlines()
            if "libvulkan.so" in line
        })
        self.report["vulkanLoaderMappings"] = mappings
        self.assertTrue(mappings)
        if directory := os.environ.get("WR_VIDEO_BENCHMARK_LOADER_DIRECTORY"):
            self.assertTrue(
                all(path.startswith(directory + os.sep) for path in mappings)
            )

    def screenshot_pixels(self, points):
        shot = self.marionette.screenshot(full=False)
        png = base64.b64decode(shot)
        (self.output / "final-compositor.png").write_bytes(png)
        self.report["screenshotSize"] = list(struct.unpack(">II", png[16:24]))
        return self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            const image = new Image();
            image.onload = () => {
              const canvas = document.createElement("canvas");
              canvas.width = image.width;
              canvas.height = image.height;
              const context = canvas.getContext("2d");
              context.drawImage(image, 0, 0);
              const mean = (x, y) => {
                const data = context.getImageData(Math.floor(x) - 2, Math.floor(y) - 2,
                                                  5, 5).data;
                const result = [0, 0, 0, 0];
                for (let i = 0; i < data.length; i += 4) {
                  for (let channel = 0; channel < 4; ++channel) {
                    result[channel] += data[i + channel];
                  }
                }
                return result.map(value => value / 25);
              };
              done(arguments[1].map(([x, y]) => mean(x, y)));
            };
            image.onerror = () => done({error: "Screenshot decode failed"});
            image.src = "data:image/png;base64," + arguments[0];
            """,
            script_args=[shot, points],
        )

    def log(self):
        return (self.output / "gecko.log").read_text(errors="replace")

    def summarize_metrics(self, samples):
        if not samples:
            return None
        return {
            "wallSeconds": samples[-1]["time"] - samples[0]["time"],
            "cpuSeconds": samples[-1]["cpuSeconds"] - samples[0]["cpuSeconds"],
            "peakRssBytes": max(sample["sumRssBytes"] for sample in samples),
            "peakPssBytes": max(sample["sumPssBytes"] for sample in samples),
            "peakPrivateBytes": max(sample["sumPrivateBytes"] for sample in samples),
            "peakProcesses": max(sample["processes"] for sample in samples),
        }

    def test_video_transfer(self):
        self.report["backend"] = self.backend_info()
        self.assertEqual(self.report["backend"]["backend"], "Vulkan (wgpu-hal)")
        self.assertEqual(self.report["backend"]["process"], "GPU")
        self.record_loader("GPU")
        self.report["video"] = self.call("ready")
        self.assertGreater(
            self.report["video"]["duration"], self.report["durationSeconds"] + 2
        )
        self.report["warmup"] = self.call("warmup", 2, timeout=10000)
        self.report["decoderBefore"] = self.debug_info()
        self.assertTrue(
            self.report["decoderBefore"]["decoder"]["reader"][
                "videoHardwareAccelerated"
            ],
            self.report["decoderBefore"],
        )

        if self.report["presentation"] == "native":
            self.marionette.execute_script(
                """
                window.presentationEvents = [];
                window.addEventListener("blur", () =>
                  window.presentationEvents.push({type: "blur", now: performance.now()}));
                document.addEventListener("visibilitychange", () =>
                  window.presentationEvents.push({type: document.visibilityState,
                                                  now: performance.now()}));
                """,
                sandbox=None,
            )
        mode = os.environ["WR_VIDEO_BENCHMARK_PROCESS_METRICS"]
        timeout = int((self.report["durationSeconds"] + 30) * 1000)
        if mode == "off":
            self.report["measurement"] = self.call(
                "measure", self.report["durationSeconds"], timeout=timeout
            )
            samples = []
        else:
            with self.marionette.using_context("chrome"):
                pid = self.marionette.execute_script(
                    "return Services.appinfo.processID;"
                )
            with ProcessMetrics(pid, include_memory=mode == "full") as metrics:
                self.report["measurement"] = self.call(
                    "measure", self.report["durationSeconds"], timeout=timeout
                )
            samples = metrics.samples
        self.report["processMetrics"] = samples
        self.report["processMetricsSummary"] = self.summarize_metrics(samples)
        self.report["decoderAfter"] = self.debug_info()
        self.assertTrue(
            self.report["decoderAfter"]["decoder"]["reader"][
                "videoHardwareAccelerated"
            ],
            self.report["decoderAfter"],
        )
        self.report["decoderNames"] = []
        for debug in [self.report["decoderBefore"], self.report["decoderAfter"]]:
            name = debug["decoder"]["reader"].get("videoDecoderName")
            if name is not None:
                self.assertTrue(name)
                self.assertNotEqual(name, "unavailable")
                self.report["decoderNames"].append(name)

        measurement = self.report["measurement"]
        self.assertIsNone(measurement["aborted"], measurement)
        self.assertGreaterEqual(
            measurement["elapsed"], self.report["durationSeconds"] * 1000
        )
        self.assertLess(
            measurement["elapsed"], self.report["durationSeconds"] * 1000 + 2000
        )
        self.assertEqual(measurement["events"], [])
        self.assertGreater(len(measurement["samples"]), 0)
        self.assertGreater(
            measurement["samples"][-1]["mediaTime"]
            - measurement["samples"][0]["mediaTime"],
            self.report["durationSeconds"] * 0.9,
        )
        start_quality = measurement["quality"]["start"]
        end_quality = measurement["quality"]["end"]
        total_frames = (
            end_quality["totalVideoFrames"] - start_quality["totalVideoFrames"]
        )
        dropped_frames = (
            end_quality["droppedVideoFrames"] - start_quality["droppedVideoFrames"]
        )
        self.report["qualityDelta"] = {
            "totalVideoFrames": total_frames,
            "droppedVideoFrames": dropped_frames,
            "corruptedVideoFrames": (
                end_quality["corruptedVideoFrames"]
                - start_quality["corruptedVideoFrames"]
                if end_quality["corruptedVideoFrames"] is not None
                and start_quality["corruptedVideoFrames"] is not None
                else None
            ),
        }
        self.assertGreater(total_frames, 0)
        self.assertGreaterEqual(dropped_frames, 0)
        presented = [sample["presentedFrames"] for sample in measurement["samples"]]
        self.assertEqual(presented, sorted(presented))

        self.report["finalReadback"] = self.call("finalReadback", timeout=10000)
        self.assertAlmostEqual(
            self.report["finalReadback"]["mediaTime"],
            measurement["mediaTime"],
            delta=0.05,
        )
        displayed = self.screenshot_pixels(self.report["finalReadback"]["points"])
        self.report["finalCompositorPixels"] = displayed
        for actual, expected in zip(displayed, self.report["finalReadback"]["pixels"]):
            self.assertAlmostEqual(actual[3], 255, delta=0.01)
            for value, reference in zip(actual, expected):
                self.assertLessEqual(abs(value - reference), 6, (displayed, expected))

        if self.report["presentation"] == "native":
            self.report["presentationEvents"] = self.marionette.execute_script(
                "return window.presentationEvents;", sandbox=None
            )
            self.assertEqual(self.report["presentationEvents"], [])
            self.assertEqual(
                self.marionette.execute_script("return document.visibilityState;"),
                "visible",
            )
            self.assertTrue(
                self.marionette.execute_script("return document.hasFocus();")
            )

        log = self.log()
        synchronization = self.report["synchronization"]
        markers = re.findall(
            r"WebRender Vulkan video selected transport: direct NV12; synchronization: (sync|async)",
            log,
        )
        self.report["synchronizationMarkers"] = markers
        self.assertEqual(markers, [synchronization])
        per_frame_marker = "Video transport: direct Vulkan NV12 sampling"
        if self.report["quiet"]:
            self.assertNotIn(per_frame_marker, log)
        else:
            self.assertIn(per_frame_marker, log)
        self.report["syncMetrics"] = [
            line for line in log.splitlines() if "Video DMA-BUF sync metrics:" in line
        ]
        if os.environ.get("WR_VIDEO_SYNC_INSTRUMENTATION") == "1":
            self.assertTrue(self.report["syncMetrics"])
        for error in [
            "Validation Error",
            "VUID-",
            "DeviceReset",
            "Failed to render",
            "Native VAAPI initialization failed",
            "CreateImageVAAPI(): failed to get VideoFrameSurface",
            "Unable to export and synchronize VAAPI frame",
            "Unsupported native VAAPI frame",
            "VAAPI native renderer changed",
            "Vulkan video transport is not enabled",
            "Vulkan video capability was revoked",
            "Using fallback software codec",
            "falling back to copy",
            "Reusing live dmabuf surface",
            "Timed out",
            "panicked",
        ]:
            self.assertNotIn(error, log)
        self.report["passed"] = True
