# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

from urllib.parse import quote

from marionette_driver import errors
from marionette_driver.wait import Wait
from marionette_harness import MarionetteTestCase


class TestUnifiedWebGPUStartup(MarionetteTestCase):
    unified_pref = "gfx.webrender.vulkan.unified-webgpu"
    vulkan_pref = "gfx.webrender.vulkan"
    requires_vulkan = "FEATURE_FAILURE_WEBGPU_UNIFIED_REQUIRES_VULKAN"
    not_implemented = "FEATURE_FAILURE_WEBGPU_UNIFIED_NOT_IMPLEMENTED"

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

    def assert_no_unified_failure(self):
        self.assertFalse(
            any("WEBGPU_UNIFIED" in failure for failure in self.webgpu_failures())
        )

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

    def assert_readback_color(self, color, expected):
        with self.marionette.using_context("content"):
            self.marionette.navigate(
                "data:text/html;charset=utf-8,"
                + quote(
                    "<!doctype html><style>html { background: "
                    + color
                    + "; } body { margin: 0; min-height: 100vh; }</style><body>"
                )
            )
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

    def test_requested_without_vulkan_requires_restart(self):
        self.restart_with_prefs(unified=True, vulkan=False)
        self.assertIn(self.requires_vulkan, self.webgpu_failures())

        self.marionette.set_pref(self.unified_pref, False)
        self.marionette.set_pref(self.vulkan_pref, True)
        self.assertIn(self.requires_vulkan, self.webgpu_failures())

        self.restart_with_prefs(unified=False, vulkan=False)
        self.assert_no_unified_failure()

    def test_disabled_gate_ignores_live_enable(self):
        self.restart_with_prefs(unified=False, vulkan=False)
        self.assert_no_unified_failure()

        self.marionette.set_pref(self.unified_pref, True)
        self.assert_no_unified_failure()

        self.restart_with_prefs(unified=True, vulkan=False)
        self.assertIn(self.requires_vulkan, self.webgpu_failures())

    def test_gpu_restart_keeps_parent_vulkan_selection(self):
        self.restart_with_prefs(
            unified=True,
            vulkan=True,
            **{
                "gfx.webrender.all": True,
                "layers.gpu-process.enabled": True,
                "layers.gpu-process.force-enabled": True,
                "remote.screenshot.use_readback": True,
                "gfx.color_management.mode": 0,
            },
        )
        backend = Wait(self.marionette, timeout=30).until(lambda _: self.backend_info())
        old_pid = self.gpu_pid()
        if (
            old_pid == -1
            or backend["backend"] != "Vulkan"
            or backend["process"] != "GPU"
        ):
            self.skipTest("Requires a GPU-process Vulkan renderer on a real display")

        self.assertIn(self.not_implemented, self.webgpu_failures())
        self.assert_readback_color("red", [255, 0, 0, 255])

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
        self.assert_readback_color("lime", [0, 255, 0, 255])
        self.assertIn(self.not_implemented, self.webgpu_failures())
