# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import base64
import http.server
import json
import os
import re
import subprocess
import threading
import uuid
from pathlib import Path
from urllib.parse import urlsplit

from marionette_harness import MarionetteTestCase


class TestNativeVideoBrowser(MarionetteTestCase):
    def setUp(self):
        super().setUp()
        self.output = Path(os.environ["WR_NATIVE_VIDEO_OUTPUT"])
        self.report = {
            "passed": False,
            "display": os.environ.get("DISPLAY"),
            "synchronization": os.environ["WR_NATIVE_VIDEO_SYNCHRONIZATION"],
        }
        self.pixel_tolerance = (
            2 if os.environ["WR_NATIVE_VIDEO_DECODER"] == "hardware" else 4
        )
        clip = Path(os.environ["WR_NATIVE_VIDEO_CLIP"]).read_bytes()
        page = Path(__file__).with_name("native_video.html").read_bytes()

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
        self.url = f"http://127.0.0.1:{self.server.server_port}/"
        self.marionette.set_context("content")
        self.marionette.set_window_rect(x=80, y=80, width=900, height=800)
        self.marionette.navigate(self.url)
        self.report["video"] = self.call("ready")

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

    def call(self, method, *args):
        result = self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            const [method, args] = arguments;
            Promise.resolve().then(() => window.nativeVideo[method](...args))
              .then(done, error => done({error: String(error)}));
        """,
            script_args=[method, list(args)],
            sandbox=None,
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
            self.report["featureLog"] = self.marionette.execute_script(
                "return Cc['@mozilla.org/gfx/info;1'].getService(Ci.nsIGfxInfo).getFeatureLog();"
            )
            self.assertTrue(
                self.marionette.execute_script(
                    "return Services.prefs.getBoolPref('remote.screenshot.use_readback');"
                )
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
        mappings = [
            line.split(maxsplit=5)[-1]
            for line in Path(f"/proc/{pid}/maps").read_text().splitlines()
            if "libvulkan.so" in line
        ]
        self.report["vulkanLoaderMappings"] = sorted(set(mappings))
        self.assertTrue(mappings)
        if directory := os.environ.get("WR_NATIVE_VIDEO_LOADER_DIRECTORY"):
            self.assertTrue(
                all(path.startswith(directory + os.sep) for path in mappings)
            )

    def compositor_pixels(self, label):
        rectangles = self.marionette.execute_script(
            "return window.nativeVideo.rectangles();", sandbox=None
        )
        shot = self.marionette.screenshot(full=False)
        (self.output / f"{label}.png").write_bytes(base64.b64decode(shot))
        return self.decode_pixels(shot, rectangles)

    def decode_pixels(self, shot, rectangles=None):
        return self.marionette.execute_async_script(
            """
            const [shot, rectangles] = arguments;
            const done = arguments[arguments.length - 1];
            const image = new Image();
            image.onload = () => {
              const canvas = document.createElement("canvas");
              canvas.width = image.width; canvas.height = image.height;
              const context = canvas.getContext("2d");
              context.drawImage(image, 0, 0);
              const regions = rectangles || [[[image.width / 2, image.height / 4],
                                               [image.width / 2, image.height * 0.55]]];
              done(regions.map(points => points.map(([x, y]) =>
                [...context.getImageData(Math.floor(x), Math.floor(y), 1, 1).data])));
            };
            image.onerror = () => done({error: "Screenshot decode failed"});
            image.src = "data:image/png;base64," + shot;
        """,
            script_args=[shot, rectangles],
        )

    def compare(self, actual, expected, tolerance):
        self.assertEqual(len(actual), len(expected))
        for observed, reference in zip(actual, expected):
            self.assertEqual(observed[3], 255)
            for value, wanted in zip(observed, reference):
                self.assertLessEqual(abs(value - wanted), tolerance, (actual, expected))

    def log(self):
        return Path(os.environ["WR_NATIVE_VIDEO_GECKO_LOG"]).read_text(errors="replace")

    def check_reset(self):
        before = self.log().count("WebRender backend: Vulkan (wgpu-hal)")
        expected = self.report["seeks"][-1]["readers"]["canvasPixels"]
        crash = os.environ.get("WR_NATIVE_VIDEO_SCENARIO") == "crash"
        with self.marionette.using_context("chrome"):
            if crash:
                pid = self.marionette.execute_script(
                    "return window.windowUtils.gpuProcessPid;"
                )
                self.assertGreater(pid, 0)
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
        if crash:
            self.report["resetInitialPixels"] = self.compositor_pixels("reset-initial")
            self.call("resetWebGL")
        paused = self.call("seek", 2.1)
        self.report["reset"] = {"paused": paused, "backend": self.backend_info()}
        displayed = self.compositor_pixels("after-reset-paused")
        self.report["reset"]["displayed"] = displayed
        for pixels in displayed:
            self.compare(pixels, expected, 4)
        self.report["reset"]["playback"] = self.call("playFrames", 72, True)
        self.report["reset"]["debug"] = self.debug_info()
        self.assertFalse(
            self.report["reset"]["debug"]["decoder"]["reader"][
                "videoHardwareAccelerated"
            ]
        )

    def check_lifecycle(self):
        self.report["lifecycle"] = []
        for width in [384, 128, 256]:
            self.marionette.execute_script(
                """
                for (const element of document.querySelectorAll('video, canvas')) {
                  element.style.width = arguments[0] + 'px';
                  element.style.height = arguments[0] / 2 + 'px';
                }
                """,
                script_args=[width],
            )
            self.call("playFrames", 8, True)
            readers = self.call("seek", 0.7)
            displayed = self.compositor_pixels(f"scaled-{width}")
            for pixels in displayed:
                self.compare(pixels, readers["canvasPixels"], self.pixel_tolerance)
            self.report["lifecycle"].append({"width": width, "displayed": displayed})
        with self.marionette.using_context("chrome"):
            self.marionette.execute_script("window.minimize();")
            self.wait_for_condition(
                lambda _: self.marionette.execute_script(
                    "return window.windowState === window.STATE_MINIMIZED;"
                ),
                timeout=10,
            )
            self.marionette.execute_script("window.restore(); window.focus();")
            self.wait_for_condition(
                lambda _: self.marionette.execute_script(
                    "return window.windowState !== window.STATE_MINIMIZED;"
                ),
                timeout=10,
            )
        self.call("playFrames", 12, True)
        readers = self.call("seek", 0.7)
        for pixels in self.compositor_pixels("restored"):
            self.compare(pixels, readers["canvasPixels"], self.pixel_tolerance)
        self.assertEqual(
            self.debug_info()["decoder"]["reader"]["videoHardwareAccelerated"],
            os.environ["WR_NATIVE_VIDEO_DECODER"] == "hardware",
        )

    def check_windows(self):
        original = self.marionette.current_window_handle
        self.report["windows"] = []
        for index in range(2):
            other = self.marionette.open(type="window", focus=True)["handle"]
            try:
                self.marionette.switch_to_window(other)
                self.marionette.set_window_rect(x=100, y=100, width=900, height=800)
                self.marionette.navigate(self.url)
                self.call("ready")
                self.call("playFrames", 12, True)
                readers = self.call("seek", 0.7)
                displayed = self.compositor_pixels(f"other-window-{index}")
                self.report["windows"].append({
                    "readers": readers,
                    "displayed": displayed,
                    "debug": self.debug_info(),
                })
                for pixels in displayed:
                    self.compare(pixels, readers["canvasPixels"], self.pixel_tolerance)
                self.assertEqual(
                    self.report["windows"][-1]["debug"]["decoder"]["reader"][
                        "videoHardwareAccelerated"
                    ],
                    os.environ["WR_NATIVE_VIDEO_DECODER"] == "hardware",
                )
            finally:
                self.marionette.close()
                self.marionette.switch_to_window(original)
            readers = self.call("seek", 0.1)
            for pixels in self.compositor_pixels(f"original-window-{index}"):
                self.compare(pixels, readers["canvasPixels"], self.pixel_tolerance)

    def check_pip(self):
        self.marionette.execute_script(
            """
            const video = document.querySelector("video");
            window.windowUtils.dispatchEventToChromeOnly(video,
              new CustomEvent("MozTogglePictureInPicture", {bubbles: true}));
        """,
            sandbox="system",
        )
        self.wait_for_condition(
            lambda _: self.marionette.execute_script(
                "return document.querySelector('video').isCloningElementVisually;",
                sandbox="system",
            ),
            timeout=20,
        )
        try:
            self.call("playFrames", 12, True)
            readers = self.call("seek", 0.7)
            title = "native-video-acceptance-" + uuid.uuid4().hex
            with self.marionette.using_context("chrome"):
                backend = self.marionette.execute_async_script(
                    """
                    const done = arguments[arguments.length - 1];
                    const win = Services.wm.getMostRecentWindow("Toolkit:PictureInPicture");
                    win.document.title = arguments[0];
                    win.resizeTo(512, 256);
                    win.moveTo(500, 100);
                    win.windowUtils.getWebRenderBackendInfo().then(
                      info => done(JSON.parse(info)), error => done({error: String(error)}));
                """,
                    script_args=[title],
                )
            self.assertEqual(backend["backend"], "Vulkan (wgpu-hal)")
            self.report["pip"] = {"backend": backend, "readers": readers}
            window_info = subprocess.check_output(
                ["xwininfo", "-name", title], text=True
            )
            window_id = re.search(r"Window id: (0x[0-9a-f]+)", window_info)[1]
            path = self.output / "pip.png"

            def matches(_):
                subprocess.run(["import", "-window", window_id, str(path)], check=True)
                pixels = self.decode_pixels(
                    base64.b64encode(path.read_bytes()).decode()
                )[0]
                self.report["pip"]["pixels"] = pixels
                return all(
                    abs(a - b) <= self.pixel_tolerance
                    for actual, expected in zip(pixels, readers["canvasPixels"])
                    for a, b in zip(actual, expected)
                )

            self.wait_for_condition(matches, timeout=15)
        finally:
            with self.marionette.using_context("chrome"):
                self.marionette.execute_script(
                    """
                    Services.wm.getMostRecentWindow("Toolkit:PictureInPicture")?.close();
                """
                )
        self.call("playFrames", 12, True)
        self.assertEqual(
            self.debug_info()["decoder"]["reader"]["videoHardwareAccelerated"],
            os.environ["WR_NATIVE_VIDEO_DECODER"] == "hardware",
        )

    def test_playback_readers_and_seek(self):
        backend = self.backend_info()
        self.report["backend"] = backend
        expected_backend = os.environ["WR_NATIVE_VIDEO_BACKEND"]
        self.assertIn(
            backend["backend"],
            {
                "vulkan": ["Vulkan (wgpu-hal)"],
                "gl": ["OpenGL", "OpenGL ES"],
                "software": ["SWGL"],
            }[expected_backend],
        )
        if expected_backend == "software":
            self.assertEqual(
                self.report["features"]["compositor"], "webrender_software"
            )
        if expected_backend != "software":
            self.assertEqual(backend["process"], os.environ["WR_NATIVE_VIDEO_PROCESS"])
        if expected_backend == "vulkan":
            self.record_loader(backend["process"])
        self.report["playback"] = self.call("playFrames", 72, True)
        samples = self.report["playback"]["samples"]
        self.assertEqual(len(samples), 72)
        if os.environ.get("WR_NATIVE_VIDEO_RESOLUTION_CHANGE") == "1":
            self.assertGreater(
                len({(s["readers"]["width"], s["readers"]["height"]) for s in samples}),
                1,
            )
        self.assertTrue(
            any(
                a["readers"]["time"] > b["readers"]["time"]
                for a, b in zip(samples, samples[1:])
            ),
            "Playback must cross a loop boundary",
        )
        self.assertGreater(len({sample["mediaTime"] for sample in samples}), 12)
        for sample in samples:
            for kind in ("canvasPixels", "webglPixels"):
                for pixel in sample["readers"][kind]:
                    self.assertEqual(pixel[3], 255)
                    self.assertGreater(sum(pixel[:3]), 40)
        expected_hardware = os.environ["WR_NATIVE_VIDEO_DECODER"] == "hardware"
        tolerance = 2 if expected_hardware else 4
        self.report["seeks"] = []
        for time in [0.3, 1.2, 0.1, 2.1]:
            readers = self.call("seek", time)
            displayed = self.compositor_pixels(f"seek-{time}")
            self.report["seeks"].append({"readers": readers, "displayed": displayed})
            self.compare(readers["webglPixels"], readers["canvasPixels"], tolerance)
            for pixels in displayed:
                self.compare(pixels, readers["canvasPixels"], tolerance)
        self.report["debug"] = self.debug_info()
        hardware = self.report["debug"]["decoder"]["reader"]["videoHardwareAccelerated"]
        self.assertEqual(hardware, expected_hardware, self.report["debug"])
        log = Path(os.environ["WR_NATIVE_VIDEO_GECKO_LOG"]).read_text(errors="replace")
        native = "Video transport: direct Vulkan NV12 sampling"
        self.assertEqual(
            native in log, expected_backend == "vulkan" and expected_hardware
        )
        modes = re.findall(
            r"WebRender Vulkan video selected transport: direct NV12; synchronization: (sync|async)",
            log,
        )
        if expected_backend == "vulkan" and expected_hardware:
            self.assertEqual(
                set(modes), {os.environ["WR_NATIVE_VIDEO_SYNCHRONIZATION"]}
            )
        else:
            self.assertEqual(modes, [])
        self.report["synchronizationMarkers"] = modes
        for error in [
            "Validation Error",
            "VUID-",
            "DeviceReset",
            "panicked",
            "Failed to render",
            "Unsupported HAL external image",
            "Timed out completing native video reads",
        ]:
            self.assertNotIn(error, log)
        scenario = os.environ.get("WR_NATIVE_VIDEO_SCENARIO", "basic")
        if scenario != "basic":
            getattr(self, "check_" + ("reset" if scenario == "crash" else scenario))()
            for error in [
                "Validation Error",
                "VUID-",
                "panicked",
                "Timed out completing native video reads",
            ]:
                self.assertNotIn(error, self.log())
        self.report["passed"] = True
