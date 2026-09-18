# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import base64
import http.server
import json
import os
import re
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
        self.report = {"cases": [], "knownFailures": [], "display": os.environ.get("DISPLAY")}
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
        self.marionette.navigate(f"http://localhost:{self.server.server_port}/")

    def tearDown(self):
        try:
            (self.output / "report.json").write_text(json.dumps(self.report, indent=2) + "\n")
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
            """, script_args=[method, list(args)], sandbox=None,
        )
        if isinstance(result, dict):
            self.assertNotIn("error", result)
        return result

    def log(self):
        return (self.output / "gecko.log").read_text(errors="replace")

    def pixels(self, label):
        screenshot = self.marionette.screenshot(full=False)
        (self.output / f"{label}.png").write_bytes(base64.b64decode(screenshot))
        return self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            const image = new Image();
            image.onload = () => {
              const canvas = document.createElement('canvas');
              canvas.width = image.width; canvas.height = image.height;
              const context = canvas.getContext('2d');
              context.drawImage(image, 0, 0);
              done([0.25, 0.75].map(y => [...context.getImageData(
                Math.floor(image.width / 2), Math.floor(image.height * y), 1, 1).data]));
            };
            image.onerror = () => done({error: 'Screenshot decode failed'});
            image.src = 'data:image/png;base64,' + arguments[0];
            """, script_args=[screenshot],
        )

    def test_formats_alpha_initialization_and_repeated_present(self):
        with self.marionette.using_context("chrome"):
            self.report["backend"] = self.marionette.execute_async_script("""
                const done = arguments[arguments.length - 1];
                window.windowUtils.getWebRenderBackendInfo().then(
                  value => done(JSON.parse(value)), error => done({error: String(error)}));
            """)
        self.assertEqual(self.report["backend"]["backend"], "Vulkan (wgpu-hal)")
        self.assertEqual(self.report["backend"]["process"], os.environ["WR_WEBGPU_PROCESS"])
        direct = os.environ["WR_WEBGPU_TRANSPORT"] == "direct"
        marker = "WebRender Vulkan DMA-BUF directly sampled:" if direct else "WebRender Vulkan DMA-BUF materialized:"
        for fmt in ["bgra8unorm", "rgba8unorm"]:
            cases = [("opaque", False, phase) for phase in range(9)]
            cases += [("premultiplied", False, 0), ("premultiplied", True, 0)]
            for alpha, discard, phase in cases:
                label = f"{fmt}-{alpha}-{discard}-{phase}"
                before = self.log().count(marker)
                self.assertEqual(self.call("renderCase", fmt, alpha, discard, phase), [])
                pixels = self.pixels(label)
                if discard:
                    expected = [[255, 255, 255, 255]] * 2
                elif alpha == "premultiplied":
                    expected = [[255, 127, 127, 255], [127, 127, 255, 255]]
                elif phase >= 8:
                    expected = [[128, 0, 0, 255], [0, 0, 128, 255]]
                else:
                    expected = [[255, 0, 0, 255], [0, 0, 255, 255]]
                if phase % 2:
                    expected.reverse()
                self.report["cases"].append({"case": label, "pixels": pixels})
                if (os.environ.get("WR_WEBGPU_BASELINE_DEFECTS") == "1"
                        and not direct and alpha == "opaque" and phase == 8
                        and pixels == [[255, 127, 127, 255], [127, 127, 255, 255]]):
                    self.report["knownFailures"].append({"case": label, "expected": expected, "actual": pixels})
                else:
                    self.assertEqual(pixels, expected, label)
                log = self.log()
                self.assertGreater(log.count(marker), before, label)
                published = re.findall(r"WebGPU canvas transport: Vulkan DMA-BUF, generation=(\d+)", log)
                consumed = re.findall(re.escape(marker) + r" generation=(\d+)", log)
                self.assertTrue(published)
                self.assertEqual(consumed[-1], published[-1], label)
        self.report["adapter"] = self.marionette.execute_script("return window.adapterInfo;", sandbox=None)
        frames = int(os.environ.get("WR_WEBGPU_BENCHMARK_FRAMES", "0"))
        if frames:
            with self.marionette.using_context("chrome"):
                pid = self.marionette.execute_script("return Services.appinfo.processID;")
            with ProcessMetrics(pid) as metrics:
                self.report["benchmark"] = self.call("benchmark", frames)
            self.report["processMetrics"] = metrics.samples
        log = self.log()
        if direct:
            self.assertNotIn("WebRender Vulkan DMA-BUF materialized:", log)
        for error in ["Validation Error", "VUID-", "DeviceReset", "panicked", "Failed to render", "Vulkan canvas publication failed"]:
            self.assertNotIn(error, log)
        self.assertEqual(self.marionette.execute_script("return [...window.gpuErrors];", sandbox=None), [])
