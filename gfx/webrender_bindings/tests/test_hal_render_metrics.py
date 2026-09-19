# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import base64
import http.server
import json
import os
import threading
from collections import defaultdict
from pathlib import Path

from marionette_harness import MarionetteTestCase

PREFIX = "WR HAL render metrics: "
ENVELOPE = {
    "version",
    "pid",
    "deviceId",
    "rendererId",
    "sequence",
    "monotonicNs",
    "final",
    "counters",
    "gauges",
    "peaks",
    "lastWorkNs",
}


def metric_records(path):
    result = []
    for line in path.read_text(errors="replace").splitlines():
        if PREFIX not in line:
            continue
        try:
            value = json.loads(line.split(PREFIX, 1)[1])
        except json.JSONDecodeError:
            continue
        if not isinstance(value, dict) or ENVELOPE - value.keys():
            continue
        if value["version"] == 1:
            result.append(value)
    return result


def groups(values):
    result = defaultdict(list)
    for value in values:
        result[(value["pid"], value["deviceId"], value["rendererId"])].append(value)
    for samples in result.values():
        samples.sort(key=lambda sample: sample["sequence"])
    return result


class TestHalRenderMetrics(MarionetteTestCase):
    def setUp(self):
        super().setUp()
        self.output = Path(os.environ["WR_WEBGPU_OUTPUT"])
        self.report = {
            "passed": False,
            "metrics": os.environ.get("WR_HAL_RENDER_METRICS") == "1",
        }
        page = Path(__file__).with_name("hal_render_metrics.html").read_bytes()

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
        self.marionette.navigate(f"http://127.0.0.1:{self.server.server_port}/")
        self.scope = None

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
            const [method, args] = arguments;
            Promise.resolve(window.halMetrics[method](...args)).then(
              () => done(null), error => done(String(error)));
            """,
            script_args=[method, list(args)],
            sandbox=None,
            script_timeout=15000,
        )
        self.assertIsNone(result)

    def backend_info(self):
        with self.marionette.using_context("chrome"):
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
        self.assertTrue(mappings)
        directory = os.environ.get("WR_WEBGPU_LOADER_DIRECTORY")
        if directory:
            self.assertTrue(
                all(path.startswith(directory + os.sep) for path in mappings)
            )
        return mappings

    def records(self):
        log = self.output / "gecko.log"
        return metric_records(log) if log.exists() else []

    def log(self):
        return (self.output / "gecko.log").read_text(errors="replace")

    def latest(self):
        grouped = groups(self.records())
        if self.scope is None:
            candidates = [samples[-1] for key, samples in grouped.items() if key[2] > 0]
            self.assertTrue(candidates)
            renderer = max(
                candidates,
                key=lambda value: (
                    value["counters"]["presents"],
                    value["counters"]["executions"],
                    value["sequence"],
                ),
            )
            self.scope = (renderer["pid"], renderer["deviceId"], renderer["rendererId"])
        renderer = grouped[self.scope][-1]
        device_scope = (self.scope[0], self.scope[1], 0)
        self.assertIn(device_scope, grouped)
        return {"renderer": renderer, "device": grouped[device_scope][-1]}

    def sequences(self):
        if self.scope is None:
            return None
        grouped = groups(self.records())
        device_scope = (self.scope[0], self.scope[1], 0)
        if self.scope not in grouped or device_scope not in grouped:
            return None
        return grouped[self.scope][-1]["sequence"], grouped[device_scope][-1][
            "sequence"
        ]

    def checkpoint(self, name, index):
        self.call("idle", 1100)
        before = self.sequences()
        self.call("boundary", index)
        if before is None:
            self.wait_for_condition(
                lambda _: (
                    any(value["rendererId"] > 0 for value in self.records())
                    and any(value["rendererId"] == 0 for value in self.records())
                ),
                timeout=10,
            )
            self.latest()
        else:
            self.wait_for_condition(
                lambda _: (
                    self.sequences()
                    and all(
                        after > prior for after, prior in zip(self.sequences(), before)
                    )
                ),
                timeout=10,
            )
        return {"name": name, **self.latest()}

    def delta(self, before, after):
        result = {"name": after["name"]}
        for scope in ["renderer", "device"]:
            first = before[scope]["counters"]
            last = after[scope]["counters"]
            result[scope] = {
                name: last.get(name, 0) - first.get(name, 0)
                for name in sorted(first.keys() | last.keys())
            }
        return result

    def final_pixels(self):
        shot = self.marionette.screenshot(full=False)
        (self.output / "final.png").write_bytes(base64.b64decode(shot))
        return self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            const image = new Image();
            image.onload = () => {
              const canvas = document.createElement("canvas");
              canvas.width = image.width; canvas.height = image.height;
              const context = canvas.getContext("2d");
              context.drawImage(image, 0, 0);
              done([[100, 90], [72, 232], [72, 320]].map(([x, y]) =>
                [...context.getImageData(x, y, 1, 1).data]));
            };
            image.src = "data:image/png;base64," + arguments[0];
            """,
            script_args=[shot],
        )

    def test_counter_phases(self):
        backend = self.backend_info()
        self.assertEqual(backend["backend"], "Vulkan (wgpu-hal)")
        self.assertEqual(backend["process"], os.environ["WR_WEBGPU_PROCESS"])
        self.report["backend"] = backend
        self.report["vulkanLoaderMappings"] = self.record_loader(backend["process"])
        self.call("settle")
        enabled = os.environ.get("WR_HAL_RENDER_METRICS") == "1"
        if not enabled:
            self.call("transform")
            self.call("paint", 90)
            self.call("webgpu", 30)
            self.assertEqual(self.records(), [])
            self.assertNotIn(PREFIX, self.log())
            pixels = self.final_pixels()
            self.assertEqual(
                pixels, [[220, 40, 60, 255], [20, 220, 80, 255], [0, 0, 255, 255]]
            )
            for failure in ["Validation Error", "VUID-", "DeviceReset", "panicked"]:
                self.assertNotIn(failure, self.log())
            self.report.update(passed=True, records=0, pixels=pixels)
            return

        phases = [self.checkpoint("settled", 0)]
        self.call("idle", 3000)
        phases.append(self.checkpoint("static-idle", 1))
        self.call("raf", 180)
        phases.append(self.checkpoint("raf-no-change", 2))
        self.call("noop", 90)
        phases.append(self.checkpoint("noop-assignments", 3))
        self.call("transform")
        phases.append(self.checkpoint("transform", 4))
        self.call("paint", 90)
        phases.append(self.checkpoint("paint", 5))
        self.call("webgpu", 30)
        phases.append(self.checkpoint("webgpu", 6))
        deltas = [
            self.delta(before, after) for before, after in zip(phases, phases[1:])
        ]
        renderer = phases[-1]["renderer"]["counters"]
        self.assertGreater(renderer["executions"], 0)
        self.assertGreater(renderer["fullCompositions"], 0)
        self.assertGreater(renderer["presents"], 0)
        self.assertGreater(renderer["composedPixels"], 0)
        self.assertGreater(renderer["rasterizedTiles"], 0)
        self.assertGreater(phases[-1]["device"]["counters"]["queueSubmissions"], 0)
        pixels = self.final_pixels()
        self.assertEqual(
            pixels, [[220, 40, 60, 255], [20, 220, 80, 255], [0, 0, 255, 255]]
        )
        for failure in ["Validation Error", "VUID-", "DeviceReset", "panicked"]:
            self.assertNotIn(failure, self.log())
        self.report.update(
            passed=True,
            phases=phases,
            deltas=deltas,
            pixels=pixels,
        )
