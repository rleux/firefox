# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import base64
import hashlib
import http.server
import json
import os
import sys
import threading
import time
from pathlib import Path

from marionette_harness import MarionetteTestCase

sys.path.insert(0, str(Path(__file__).parent))
from renderer_benchmark_metrics import Sampler, validate_environment, validate_report
from renderer_benchmark_perf import PerfRecorder
from renderer_benchmark_startup import wait_for_startup


class TestRendererBenchmark(MarionetteTestCase):
    def setUp(self):
        super().setUp()
        self.output = Path(os.environ["WR_RENDERER_BENCHMARK_OUTPUT"])
        self.report = {
            "schemaVersion": 1,
            "passed": False,
            "diagnostics": [],
            "processMetrics": [],
        }
        self.server = None
        self.thread = None
        self.cleaned = False
        self.perf = None
        try:
            self.configure_fixture()
        except BaseException:
            self.close_fixture()
            raise

    def configure_fixture(self):
        self.config = json.loads((self.output / "config.json").read_text())
        self.manifest = json.loads((self.output / "manifest.json").read_text())
        self.report["presentation"] = {
            "mode": self.config["display"],
            "wsiDebug": os.environ.get("MESA_VK_WSI_DEBUG"),
        }
        self.report["runtime"] = self.manifest["runtime"]
        page = Path(__file__).with_name("renderer_benchmark.html").read_bytes()

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
        self.marionette.set_window_rect(x=20, y=20, width=900, height=800)
        self.marionette.navigate(f"http://127.0.0.1:{self.server.server_port}/")
        target = self.config["expected"]["viewport"]
        for _ in range(4):
            actual = self.marionette.execute_script("return [innerWidth, innerHeight];")
            if actual == target:
                break
            outer = self.marionette.window_rect
            self.marionette.set_window_rect(
                width=outer["width"] + target[0] - actual[0],
                height=outer["height"] + target[1] - actual[1],
            )
        self.assertEqual(
            self.marionette.execute_script("return [innerWidth, innerHeight];"), target
        )
        with self.marionette.using_context("chrome"):
            self.marionette.execute_script("window.focus();")
        self.marionette.find_element("css selector", "#tile").click()

    def close_fixture(self):
        if self.cleaned:
            return
        self.cleaned = True
        try:
            try:
                if self.perf:
                    self.perf.close()
            finally:
                (self.output / "report.json").write_text(
                    json.dumps(self.report, indent=2) + "\n"
                )
        finally:
            if self.server:
                if self.thread and self.thread.is_alive():
                    self.server.shutdown()
                self.server.server_close()
            if self.thread and self.thread.ident is not None:
                self.thread.join()

    def tearDown(self):
        try:
            self.close_fixture()
        finally:
            super().tearDown()

    def call(self, method, *args):
        result = self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            const [method, args] = arguments;
            Promise.resolve().then(() => window.rendererBenchmark[method](...args))
              .then(value => done({value}), error => done({error: String(error)}));
            """,
            script_args=[method, list(args)],
            sandbox=None,
            script_timeout=int(
                (self.config["duration"] + self.config["warmup"] + 30) * 1000
            ),
        )
        self.assertNotIn("error", result)
        return result.get("value")

    def backend(self):
        with self.marionette.using_context("chrome"):
            return self.marionette.execute_async_script(
                """
                const done = arguments[arguments.length - 1];
                window.windowUtils.getWebRenderBackendInfo().then(
                  info => done(JSON.parse(info)), error => done({error: String(error)}));
                """
            )

    def geometry(self):
        return self.marionette.execute_script(
            "return {viewport:[innerWidth,innerHeight],dpr:devicePixelRatio,visible:document.visibilityState === 'visible',focused:document.hasFocus()};"
        )

    def identities(self):
        with self.marionette.using_context("chrome"):
            return self.marionette.execute_script(
                "return {parent:Services.appinfo.processID,gpu:window.windowUtils.gpuProcessPid};"
            )

    def mappings(self, pid):
        names = (
            "libvulkan",
            "libGL",
            "libEGL",
            "_dri.so",
            "libgallium",
            "libLLVM",
            "libdrm",
            "libxul.so",
        )
        return sorted({
            line.split(maxsplit=5)[-1]
            for line in Path(f"/proc/{pid}/maps").read_text().splitlines()
            if any(name in line for name in names)
        })

    def library_hashes(self, mappings):
        result = {}
        for name in mappings:
            path = Path(name)
            with path.open("rb") as stream:
                result[name] = hashlib.file_digest(stream, "sha256").hexdigest()
        return result

    def pixels(self):
        checkpoint = self.call("checkpoint")
        png = self.marionette.screenshot(full=False)
        (self.output / "final.png").write_bytes(base64.b64decode(png))
        result = self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length-1];
            const [png, points] = arguments;
            const image = new Image();
            image.onload = () => {
              const canvas = document.createElement('canvas');
              canvas.width = image.width; canvas.height = image.height;
              const context = canvas.getContext('2d'); context.drawImage(image,0,0);
              done({size:[image.width,image.height],pixels:points.map(([x,y])=>
                [...context.getImageData(x,y,1,1).data])});
            };
            image.onerror = () => done({error:'PNG decode failed'});
            image.src = 'data:image/png;base64,' + png;
            """,
            script_args=[png, checkpoint["points"]],
        )
        self.assertEqual(result.get("size"), self.config["expected"]["viewport"])
        self.assertEqual(result["pixels"], checkpoint["expected"])
        result["checkpoint"] = checkpoint
        return result

    def test_renderer_workload(self):
        expected = self.config["expected"]
        self.report["backend"] = self.backend()
        if expected["workload"] == "canvas":
            with self.marionette.using_context("chrome"):
                self.report["canvasPolicy"] = self.marionette.execute_script(
                    """
                    return {
                      accelerated: Services.prefs.getBoolPref('gfx.canvas.accelerated'),
                      forceEnabled: Services.prefs.getBoolPref('gfx.canvas.accelerated.force-enabled')
                    };
                    """
                )
        identities = self.identities()
        if expected.get("startupSettling"):
            self.report["startupSettling"] = {}
            wait_for_startup(
                Sampler(identities["parent"]), self.report["startupSettling"]
            )
            self.assertEqual(self.identities(), identities)
            self.assertEqual(self.backend(), self.report["backend"])
        self.report["processIds"] = identities
        renderer_pid = (
            identities["gpu"] if expected["process"] == "GPU" else identities["parent"]
        )
        self.assertGreater(renderer_pid, 0)
        self.call("prepare", expected["workload"])
        self.call("warmup", 200)
        self.report["mappingsBefore"] = self.mappings(renderer_pid)
        self.report["libraryHashesBefore"] = self.library_hashes(
            self.report["mappingsBefore"]
        )
        libxul = self.manifest["runtime"]["libxul"]
        self.assertEqual(
            self.report["libraryHashesBefore"].get(libxul["path"]), libxul["sha256"]
        )
        if expected["backend"] == "vulkan":
            self.assertTrue(
                any("libvulkan.so" in path for path in self.report["mappingsBefore"])
            )
        self.call("warmup", self.config["warmup"] * 1000)
        self.report["geometryBefore"] = self.geometry()
        self.assertEqual(validate_environment(self.report, expected), [])
        if expected["phase"] == "profile":
            config = expected["perf"]
            self.report["perf"] = {"version": config["version"]}
            self.perf = PerfRecorder(
                config["binary"],
                renderer_pid,
                self.output,
                self.report["perf"],
                event=config["event"],
            )
            self.perf.start()
        self.report["hostIntervalStart"] = time.monotonic()
        if expected["phase"] == "diagnostic":
            self.report["workload"] = self.call(
                "measure", self.config["duration"] * 1000
            )
        else:
            with Sampler(
                identities["parent"],
                include_memory=expected["phase"] == "memory",
                timing=expected.get("timingSampling", False),
                interval=expected.get("sampleIntervalSeconds", 0.25),
            ) as sampler:
                if self.perf:
                    self.perf.enable()
                try:
                    self.report["workload"] = self.call(
                        "measure", self.config["duration"] * 1000
                    )
                finally:
                    if self.perf and self.perf.recording:
                        self.perf.disable()
            self.report["processMetrics"] = sampler.samples
            self.report["samplingIntervalSeconds"] = sampler.interval
        self.report["hostIntervalEnd"] = time.monotonic()
        self.report["geometryAfter"] = self.geometry()
        self.call("stop")
        if self.perf:
            self.perf.finish()
        self.assertEqual(self.identities(), identities)
        self.assertEqual(self.backend(), self.report["backend"])
        self.report["mappingsAfter"] = self.mappings(renderer_pid)
        self.assertEqual(self.report["mappingsBefore"], self.report["mappingsAfter"])
        self.report["libraryHashesAfter"] = self.library_hashes(
            self.report["mappingsAfter"]
        )
        self.assertEqual(
            self.report["libraryHashesBefore"], self.report["libraryHashesAfter"]
        )
        self.report["pixels"] = self.pixels()
        log = (self.output / "gecko.log").read_text(errors="replace")
        prefix = "WR HAL render metrics: "
        for line in log.splitlines():
            if prefix in line:
                try:
                    self.report["diagnostics"].append(
                        json.loads(line.split(prefix, 1)[1])
                    )
                except json.JSONDecodeError:
                    pass
        if expected["phase"] != "diagnostic":
            self.assertEqual(self.report["diagnostics"], [])
        for failure in (
            "Validation Error",
            "VUID-",
            "panicked",
            "Failed to render",
            "DeviceReset",
        ):
            self.assertNotIn(failure, log)
        self.report["passed"] = True
        errors = validate_report(self.report, expected)
        self.report["validationErrors"] = errors
        self.report["passed"] = not errors
        self.assertEqual(errors, [])
