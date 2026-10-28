# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

from pathlib import Path
import subprocess

from marionette_driver import errors
from marionette_driver.wait import Wait
from marionette_harness import MarionetteTestCase


SHARED_CAPABILITY_FAILURES = {
    "Opening shared renderer: Shared devices require two graphics/compute queues in the presentation-compatible queue family",
    "Opening shared renderer: Shared devices require native timeline semaphores",
    "Opening shared renderer: Shared presentation requires Vulkan 1.1 and enabled surface/swapchain maintenance1 support",
}
SHARED_INSTANCE_REQUIREMENTS = (
    "Shared presentation requires Vulkan 1.1, KHR_surface, "
    "KHR_get_surface_capabilities2 and EXT_surface_maintenance1"
)


def parse_shared_native_probe(output, backend="vulkan"):
    lines = output.splitlines()

    def field(name, required=True):
        indexes = [i for i, line in enumerate(lines) if line == name]
        if not indexes and not required:
            return None
        if len(indexes) != 1 or indexes[0] + 1 >= len(lines):
            raise ValueError(f"Incomplete or duplicate probe field {name}: {output}")
        return lines[indexes[0] + 1]

    status = field("VULKAN_WEBRENDER_SHARED")
    if status not in ("TRUE", "FALSE"):
        raise ValueError(f"Invalid shared probe status: {output}")
    reason = field("VULKAN_WEBRENDER_SHARED_REASON", required=False)
    device = field("VULKAN_WEBRENDER_SHARED_DEVICE", required=False)
    if status == "TRUE":
        if reason is not None or not device:
            raise ValueError(f"Inconsistent successful shared probe: {output}")
    else:
        expected = (
            reason == "Unified WebGPU requires the Vulkan backend"
            if backend == "gl"
            else reason in SHARED_CAPABILITY_FAILURES
            or (
                reason
                and reason.startswith("Initializing shared Vulkan instance:")
                and SHARED_INSTANCE_REQUIREMENTS in reason
            )
        )
        if not expected:
            raise ValueError(f"Shared probe failed beyond hardware prerequisites: {output}")
    return {"supported": status == "TRUE", "reason": reason, "device": device}


def shared_native_probe(test, backend="vulkan"):
    with test.marionette.using_context("chrome"):
        directory = test.marionette.execute_script(
            'return Services.dirsvc.get("XREExeF", Ci.nsIFile).parent.path;'
        )
    command = [
        str(Path(directory) / "gfxtest"),
        "vulkan", "--webrender-shared", "--require-hardware",
        "--webgpu-backend", backend,
    ]
    result = subprocess.run(
        command, env=test.marionette.instance.runner.env,
        capture_output=True, text=True, timeout=30,
    )
    test.assertEqual(result.returncode, 0, (command, result.stdout, result.stderr))
    test.assertNotRegex(result.stderr, r"VUID-|SYNC-HAZARD|Validation Error")
    report = parse_shared_native_probe(result.stdout, backend)
    test.logger.info(f"Independent shared native prerequisite: {report}")
    return report


def require_shared_native_hardware(test, renderer):
    report = shared_native_probe(test)
    if not report["supported"]:
        test.skipTest(f"Independent shared HAL hardware prerequisite: {report['reason']}")
    test.assertEqual(report["device"], renderer["renderer"], report)


class TestUnifiedWebGPUStartup(MarionetteTestCase):
    unified_pref = "gfx.webrender.vulkan.unified-webgpu"
    vulkan_pref = "gfx.webrender.vulkan"
    requires_vulkan = "FEATURE_FAILURE_WEBGPU_UNIFIED_REQUIRES_VULKAN"
    unsupported_platform = "FEATURE_FAILURE_WEBGPU_UNIFIED_UNSUPPORTED_PLATFORM"
    requires_gpu_process = "FEATURE_FAILURE_WEBGPU_UNIFIED_REQUIRES_GPU_PROCESS"

    def setUp(self):
        super().setUp()
        prefs = self.marionette.instance.prefs
        self.original_prefs = prefs.copy() if prefs is not None else None

    def tearDown(self):
        capabilities = self.marionette.requested_capabilities
        try:
            self.marionette.quit(in_app=False, clean=True)
        finally:
            self.marionette.instance.prefs = self.original_prefs
            try:
                self.marionette.start_session(capabilities)
            finally:
                super().tearDown()

    def restart_with_prefs(self, unified, vulkan, **extra):
        prefs = dict(self.original_prefs or {})
        prefs.update(
            {
                self.unified_pref: unified,
                self.vulkan_pref: vulkan,
                "gfx.webgpu.ignore-blocklist": True,
            }
        )
        prefs.update(extra)
        self.marionette.restart(in_app=False, clean=True)
        self.marionette.enforce_gecko_prefs(prefs)

    def webgpu_failures(self):
        with self.marionette.using_context("chrome"):
            return self.marionette.execute_script(
                """
                const info = Cc["@mozilla.org/gfx/info;1"].getService(Ci.nsIGfxInfo);
                const feature = info.getFeatureLog().features.find(
                    feature => feature.name === "WEBGPU"
                );
                return feature.log.map(entry => entry.failureId).filter(Boolean);
                """
            )

    def assert_unified_unavailable(self):
        failures = set(self.webgpu_failures())
        self.assertTrue(
            failures.intersection(
                {
                    self.requires_vulkan,
                    self.unsupported_platform,
                    self.requires_gpu_process,
                }
            ),
            failures,
        )

    def assert_no_unified_failure(self):
        self.assertFalse(
            any("WEBGPU_UNIFIED" in failure for failure in self.webgpu_failures())
        )

    def assert_canvas_usage(self, native, color=None):
        with self.marionette.using_context("content"):
            self.marionette.navigate(self.marionette.absolute_url("test.html"))
            result = self.marionette.execute_async_script(
                """
                const color = arguments[0];
                const done = arguments[arguments.length - 1];
                (async () => {
                    if (!isSecureContext || !navigator.gpu) {
                        throw new Error("Secure WebGPU context required");
                    }
                    const adapter = await navigator.gpu.requestAdapter();
                    if (!adapter) {
                        throw new Error("WebGPU adapter required");
                    }
                    const device = await adapter.requestDevice();
                    device.pushErrorScope("validation");
                    const canvas = document.createElement("canvas");
                    canvas.width = canvas.height = 2;
                    const context = canvas.getContext("webgpu");
                    context.configure({device,
                        format: navigator.gpu.getPreferredCanvasFormat(),
                        usage: GPUTextureUsage.RENDER_ATTACHMENT});
                    const texture = context.getCurrentTexture();
                    const view = texture.createView();
                    if (color) {
                        const style = document.createElement("style");
                        style.textContent = `
                            html, body { margin: 0; background: black; }
                            canvas { display: block; inline-size: 100vw;
                                     block-size: 100vh; }
                        `;
                        document.head.appendChild(style);
                        document.body.replaceChildren(canvas);
                        const state = {device, canvas, context, lost: null};
                        window.__unifiedNativeRecovery = state;
                        device.lost.then(info => {
                            state.lost = {reason: info.reason, message: info.message};
                        });
                        const encoder = device.createCommandEncoder();
                        const pass = encoder.beginRenderPass({colorAttachments: [{
                            view, clearValue: color, loadOp: "clear", storeOp: "store"
                        }]});
                        pass.end();
                        device.queue.submit([encoder.finish()]);
                        await device.queue.onSubmittedWorkDone();
                    }
                    const error = await device.popErrorScope();
                    const result = {usage: texture.usage,
                        requested: GPUTextureUsage.RENDER_ATTACHMENT,
                        copySrc: GPUTextureUsage.COPY_SRC,
                        error: error ? error.message : null};
                    if (!color) {
                        context.unconfigure();
                        device.destroy();
                    }
                    return result;
                })().then(done, error => done({error: String(error)}));
                """,
                script_args=[color],
                sandbox=None,
            )
        self.assertIsNone(result.get("error"), result)
        expected = result["requested"]
        if not native:
            expected |= result["copySrc"]
        self.assertEqual(result["usage"], expected, result)

    def gpu_pid(self):
        with self.marionette.using_context("chrome"):
            return self.marionette.execute_script(
                """
                return Services.wm.getMostRecentWindow("navigator:browser")
                    .windowUtils.gpuProcessPid;
                """
            )

    def backend_info(self):
        with self.marionette.using_context("chrome"):
            return self.marionette.execute_async_script(
                """
                const done = arguments[arguments.length - 1];
                Services.wm.getMostRecentWindow("navigator:browser").windowUtils
                    .getWebRenderBackendInfo().then(
                        value => done(JSON.parse(value)),
                        () => done(null)
                    );
                """
            )

    def assert_readback_color(self, expected):
        with self.marionette.using_context("content"):
            self.marionette.execute_async_script(
                """
                const done = arguments[arguments.length - 1];
                requestAnimationFrame(() => requestAnimationFrame(() => done()));
                """
            )
            actual = None

            def rendered(_):
                nonlocal actual
                screenshot = self.marionette.screenshot()
                actual = self.marionette.execute_async_script(
                    """
                    const screenshot = arguments[0];
                    const done = arguments[arguments.length - 1];
                    const image = new Image();
                    image.onload = () => {
                        const canvas = document.createElement("canvas");
                        canvas.width = canvas.height = 1;
                        const ctx = canvas.getContext("2d");
                        ctx.drawImage(image, Math.floor(image.width / 2),
                            Math.floor(image.height / 2), 1, 1, 0, 0, 1, 1);
                        done(Array.from(ctx.getImageData(0, 0, 1, 1).data));
                    };
                    image.onerror = () => done(null);
                    image.src = "data:image/png;base64," + screenshot;
                    """,
                    script_args=[screenshot],
                )
                return actual == expected

            try:
                Wait(self.marionette, timeout=30, interval=0.5).until(rendered)
            except errors.TimeoutException:
                self.assertEqual(actual, expected, "Compositor readback color")

    def test_shared_probe_protocol_rejects_incomplete_and_bootstrap_failures(self):
        success = "VULKAN_WEBRENDER_SHARED_DEVICE\nGPU\nVULKAN_WEBRENDER_SHARED\nTRUE\n"
        self.assertTrue(parse_shared_native_probe(success)["supported"])
        failure = next(iter(SHARED_CAPABILITY_FAILURES))
        unsupported = f"VULKAN_WEBRENDER_SHARED_REASON\n{failure}\nVULKAN_WEBRENDER_SHARED\nFALSE\n"
        self.assertFalse(parse_shared_native_probe(unsupported)["supported"])
        for output in [
            "", "VULKAN_WEBRENDER_SHARED\nTRUE\n",
            success + "VULKAN_WEBRENDER_SHARED\nTRUE\n",
            success + "VULKAN_WEBRENDER_SHARED_REASON\n\n",
            "VULKAN_WEBRENDER_SHARED\nFALSE\n",
            "VULKAN_WEBRENDER_SHARED_REASON\nBootstrap failed\nVULKAN_WEBRENDER_SHARED\nFALSE\n",
        ]:
            with self.assertRaises(ValueError):
                parse_shared_native_probe(output)

    def test_requested_without_vulkan_requires_restart(self):
        self.restart_with_prefs(unified=True, vulkan=False)
        self.assert_unified_unavailable()

        self.marionette.set_pref(self.unified_pref, False)
        self.marionette.set_pref(self.vulkan_pref, True)
        self.assert_unified_unavailable()

        self.restart_with_prefs(unified=False, vulkan=False)
        self.assert_no_unified_failure()

    def test_disabled_gate_ignores_live_enable(self):
        self.restart_with_prefs(unified=False, vulkan=False)
        self.assert_no_unified_failure()

        self.marionette.set_pref(self.unified_pref, True)
        self.assert_no_unified_failure()

        self.restart_with_prefs(unified=True, vulkan=False)
        self.assert_unified_unavailable()

    def test_non_vulkan_webgpu_backend_keeps_ordinary_vulkan_ui(self):
        if self.marionette.session_capabilities.get("platformName") != "linux":
            self.skipTest("Unified native canvas transport is Linux-only")
        options = {
            "gfx.webrender.all": True,
            "layers.gpu-process.enabled": True,
            "layers.gpu-process.force-enabled": True,
            "remote.screenshot.use_readback": True,
            "gfx.color_management.mode": 0,
            "dom.webgpu.enabled": True,
            "dom.webgpu.wgpu-backend": "gl",
            "dom.webgpu.testing.assert-hardware-adapter": True,
            "dom.webgpu.hal-labels": False,
        }
        self.restart_with_prefs(unified=False, vulkan=True, **options)
        backend = Wait(self.marionette, timeout=30).until(lambda _: self.backend_info())
        if (
            self.gpu_pid() == -1
            or backend["backend"] != "Vulkan"
            or backend["process"] != "GPU"
        ):
            self.skipTest("Requires a GPU-process Vulkan renderer on a real display")

        denied = shared_native_probe(self, "gl")
        self.assertFalse(denied["supported"], denied)
        self.restart_with_prefs(unified=True, vulkan=True, **options)
        with self.marionette.using_context("chrome"):
            self.marionette.execute_script("Services.console.reset();")
        with self.marionette.using_context("content"):
            self.marionette.navigate(self.marionette.absolute_url("test.html"))
            result = self.marionette.execute_async_script(
                """
                const done = arguments[arguments.length - 1];
                (async () => {
                    if (!isSecureContext || !navigator.gpu) {
                        throw new Error("Secure WebGPU context required");
                    }
                    const adapter = await navigator.gpu.requestAdapter();
                    const style = document.createElement("style");
                    style.textContent = `
                        html, body { margin: 0; background: rgb(0, 255, 0); }
                    `;
                    document.head.appendChild(style);
                    document.body.replaceChildren();
                    return {adapterIsNull: adapter === null};
                })().then(done, error => done({error: String(error)}));
                """
            )
        self.assertEqual(result, {"adapterIsNull": True})
        with self.marionette.using_context("chrome"):
            Wait(self.marionette, timeout=30).until(
                lambda _: self.marionette.execute_script(
                    """
                    return Services.console.getMessageArray().some(message =>
                        message.message.includes(
                            "WebGPU is unavailable with this graphics configuration."
                        )
                    );
                    """
                )
            )
        backend = Wait(self.marionette, timeout=30).until(lambda _: self.backend_info())
        self.assertNotEqual(self.gpu_pid(), -1)
        self.assertEqual(backend["backend"], "Vulkan")
        self.assertEqual(backend["process"], "GPU")
        self.assert_no_unified_failure()
        self.assert_readback_color([0, 255, 0, 255])

    def test_gpu_restart_keeps_parent_vulkan_selection(self):
        options = {
            "gfx.webrender.all": True,
            "layers.gpu-process.enabled": True,
            "layers.gpu-process.force-enabled": True,
            "remote.screenshot.use_readback": True,
            "gfx.color_management.mode": 0,
            "dom.webgpu.enabled": True,
            "dom.webgpu.testing.assert-hardware-adapter": True,
            "dom.webgpu.wgpu-backend": "vulkan",
            "dom.webgpu.hal-labels": False,
        }
        self.restart_with_prefs(unified=False, vulkan=True, **options)
        backend = Wait(self.marionette, timeout=30).until(lambda _: self.backend_info())
        if (
            self.gpu_pid() == -1
            or backend["backend"] != "Vulkan"
            or backend["process"] != "GPU"
        ):
            self.skipTest("Requires a GPU-process Vulkan renderer on a real display")
        if self.marionette.session_capabilities.get("platformName") != "linux":
            self.skipTest("Unified native canvas transport is Linux-only")
        require_shared_native_hardware(self, backend)
        self.assert_canvas_usage(native=False)

        self.restart_with_prefs(unified=True, vulkan=True, **options)
        backend = Wait(self.marionette, timeout=30).until(lambda _: self.backend_info())
        old_pid = self.gpu_pid()
        self.assertNotEqual(old_pid, -1)
        self.assertEqual(backend["backend"], "Vulkan")
        self.assertEqual(backend["process"], "GPU")
        self.assert_no_unified_failure()
        self.assert_canvas_usage(native=True, color=[1, 0, 0, 1])
        self.assert_readback_color([255, 0, 0, 255])
        with self.marionette.using_context("content"):
            self.assertIsNone(
                self.marionette.execute_script(
                    "return window.__unifiedNativeRecovery.lost;", sandbox=None
                )
            )

        self.marionette.set_pref(self.unified_pref, False)
        self.marionette.set_pref(self.vulkan_pref, False)
        with self.marionette.using_context("chrome"):
            self.marionette.execute_script(
                """
                Services.wm.getMostRecentWindow("navigator:browser")
                    .windowUtils.terminateGPUProcess();
                """
            )

        Wait(self.marionette, timeout=30).until(
            lambda _: self.gpu_pid() not in (-1, old_pid)
        )
        backend = Wait(self.marionette, timeout=30).until(lambda _: self.backend_info())
        self.assertEqual(backend["backend"], "Vulkan")
        self.assertEqual(backend["process"], "GPU")
        self.assert_no_unified_failure()
        with self.marionette.using_context("content"):
            lost = Wait(self.marionette, timeout=30, interval=0.1).until(
                lambda _: self.marionette.execute_script(
                    "return window.__unifiedNativeRecovery.lost;", sandbox=None
                )
            )
            self.assertEqual(lost["reason"], "unknown", lost)
        self.assert_canvas_usage(native=True, color=[0, 1, 0, 1])
        self.assert_readback_color([0, 255, 0, 255])
        with self.marionette.using_context("content"):
            self.assertIsNone(
                self.marionette.execute_script(
                    "return window.__unifiedNativeRecovery.lost;", sandbox=None
                )
            )
