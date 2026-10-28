# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

from pathlib import Path

from marionette_driver import errors
from marionette_driver.wait import Wait
from marionette_harness import MarionetteTestCase
from mozfile import load_source


require_shared_native_hardware = load_source(
    "unified_webgpu_startup_support",
    str(Path(__file__).with_name("test_unified_webgpu_startup.py")),
).require_shared_native_hardware


class TestUnifiedWebGPUCanvas(MarionetteTestCase):
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

    def native_page(self, worker=False):
        prefs = dict(self.original_prefs or {})
        prefs.update({
            "gfx.webrender.vulkan": True,
            "gfx.webrender.all": True,
            "layers.gpu-process.enabled": True,
            "layers.gpu-process.force-enabled": True,
            "gfx.webgpu.ignore-blocklist": True,
            "dom.webgpu.enabled": True,
            "dom.webgpu.testing.assert-hardware-adapter": True,
            "dom.webgpu.wgpu-backend": "vulkan",
            "dom.webgpu.hal-labels": False,
            "remote.screenshot.use_readback": True,
            "gfx.color_management.mode": 0,
        })
        for native in (False, True):
            prefs["gfx.webrender.vulkan.unified-webgpu"] = native
            self.marionette.restart(in_app=False, clean=True)
            self.marionette.enforce_gecko_prefs(prefs)
            self.marionette.timeout.script = 60
            with self.marionette.using_context("chrome"):
                backend = self.marionette.execute_async_script("""
                    const done = arguments[arguments.length - 1];
                    Services.wm.getMostRecentWindow("navigator:browser").windowUtils
                        .getWebRenderBackendInfo().then(
                            info => done(JSON.parse(info)), () => done(null));
                """)
            available = (
                backend
                and backend["backend"] == "Vulkan"
                and backend["process"] == "GPU"
            )
            if not native and not available:
                self.skipTest(
                    "Gate-off prerequisite: headful GPU-process Vulkan renderer"
                )
            self.assertTrue(available, backend)
            self.marionette.set_context("content")
            self.marionette.navigate(self.marionette.absolute_url("test.html"))
            if not native:
                require_shared_native_hardware(self, backend)
                hardware = self.run_canvas("""
                    return Boolean(isSecureContext && navigator.gpu &&
                        await navigator.gpu.requestAdapter());
                """)
                self.assertTrue(hardware, "Admitted hardware WebGPU adapter required")
                if worker:
                    capable = self.run_canvas("""
                        if (!window.Worker || !window.OffscreenCanvas) return false;
                        const source = `postMessage(Boolean(self.OffscreenCanvas && navigator.gpu &&
                            new OffscreenCanvas(2, 2).getContext("webgpu") &&
                            new OffscreenCanvas(2, 2).getContext("2d")));`;
                        const url = URL.createObjectURL(new Blob([source], {type: "text/javascript"}));
                        try {
                            return await new Promise((resolve, reject) => {
                                const worker = new Worker(url);
                                const timer = setTimeout(() => {
                                    worker.terminate(); reject(new Error("Worker prerequisite timeout"));
                                }, 10000);
                                worker.onmessage = event => {
                                    clearTimeout(timer); worker.terminate(); resolve(event.data);
                                };
                                worker.onerror = event => {
                                    clearTimeout(timer); worker.terminate(); reject(new Error(event.message));
                                };
                            });
                        } finally {
                            URL.revokeObjectURL(url);
                        }
                    """)
                    if not capable:
                        self.skipTest(
                            "Gate-off prerequisite: worker WebGPU and 2D OffscreenCanvas"
                        )

    def run_canvas(self, script, args=None):
        result = self.marionette.execute_async_script("""
            const done = arguments[arguments.length - 1];
            (async () => {
                const check = (condition, message) => {
                    if (!condition) throw new Error(message);
                };
                const solid = (pixel, width, height) =>
                    Array.from({length: width * height}, () => pixel).flat();
                const snapshot = source => {
                    const target = document.createElement("canvas");
                    target.width = source.width;
                    target.height = source.height;
                    const context = target.getContext("2d");
                    context.drawImage(source, 0, 0);
                    return Array.from(context.getImageData(0, 0,
                        target.width, target.height).data);
                };
                const equalPixels = (actual, expected, label, tolerance = 0) => {
                    check(actual.length === expected.length, label + " length");
                    check(actual.every((value, index) =>
                        Math.abs(value - expected[index]) <= tolerance),
                        label + ": " + JSON.stringify(actual));
                };
                const write = (device, texture, format, rgb) => {
                    const pixel = format === "bgra8unorm"
                        ? [rgb[2], rgb[1], rgb[0], 255] : [...rgb, 255];
                    device.queue.writeTexture({texture},
                        new Uint8Array(solid(pixel, texture.width, texture.height)),
                        {bytesPerRow: texture.width * 4},
                        {width: texture.width, height: texture.height});
                };
        """ + script + """
            })().then(value => done({ok: true, value}),
                error => done({ok: false, error: String(error), stack: error.stack}));
        """, script_args=args or [], sandbox=None)
        self.assertTrue(result.get("ok"), result)
        return result.get("value")

    def compositor_color(self, expected):
        actual = None

        def rendered(_):
            nonlocal actual
            screenshot = self.marionette.screenshot()
            actual = self.marionette.execute_async_script("""
                const done = arguments[arguments.length - 1];
                const image = new Image();
                image.onload = () => {
                    const canvas = document.createElement("canvas");
                    canvas.width = canvas.height = 1;
                    const context = canvas.getContext("2d");
                    context.drawImage(image, Math.floor(image.width / 2),
                        Math.floor(image.height / 2), 1, 1, 0, 0, 1, 1);
                    done(Array.from(context.getImageData(0, 0, 1, 1).data));
                };
                image.onerror = () => done(null);
                image.src = "data:image/png;base64," + arguments[0];
            """, script_args=[screenshot])
            return actual == expected

        try:
            Wait(self.marionette, timeout=30, interval=0.5).until(rendered)
        except errors.TimeoutException:
            self.assertEqual(actual, expected, "Actual compositor readback color")

    def test_current_identity_and_public_snapshots(self):
        self.native_page()
        cases = self.run_canvas("""
            check(isSecureContext && navigator.gpu, "Secure WebGPU context");
            const adapter = await navigator.gpu.requestAdapter();
            check(adapter, "Hardware adapter");
            const device = await adapter.requestDevice();
            const uncaptured = [];
            device.addEventListener("uncapturederror", event => {
                event.preventDefault();
                uncaptured.push(event.error.message);
            });
            const cases = [];
            for (const format of ["rgba8unorm", "bgra8unorm"]) {
                for (const alphaMode of ["opaque", "premultiplied"]) {
                    device.pushErrorScope("validation");
                    const canvas = document.createElement("canvas");
                    canvas.width = canvas.height = 2;
                    const context = canvas.getContext("webgpu");
                    const usage = GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_DST;
                    const configuration = {device, format, usage, alphaMode,
                        viewFormats: [format + "-srgb"]};
                    context.configure(configuration);
                    const texture = context.getCurrentTexture();
                    check(texture === context.getCurrentTexture(), "Same valid current object");
                    check(texture.usage === usage, "Exact native logical usage");
                    const zero = [0, 0, 0, alphaMode === "opaque" ? 255 : 0];
                    equalPixels(snapshot(canvas), solid(zero, 2, 2), "Untouched snapshot");
                    write(device, texture, format, [17, 31, 53]);
                    equalPixels(snapshot(canvas), solid([17, 31, 53, 255], 2, 2), "Writing snapshot");
                    check(texture === context.getCurrentTexture(), "Snapshot did not expire current");
                    const pixel = format === "bgra8unorm" ? [11, 7, 5, 255] : [5, 7, 11, 255];
                    device.queue.writeTexture({texture, origin: {x: 1, y: 1}},
                        new Uint8Array(pixel), {bytesPerRow: 4}, {width: 1, height: 1});
                    const partial = solid([17, 31, 53, 255], 2, 2);
                    partial.splice(12, 4, 5, 7, 11, 255);
                    equalPixels(snapshot(canvas), partial, "More writes after snapshot");
                    texture.destroy();
                    check(texture === context.getCurrentTexture(), "Same destroyed current object in task");
                    equalPixels(snapshot(canvas), partial, "Destroyed texture retained bitmap");
                    context.unconfigure();
                    equalPixels(snapshot(canvas), solid([0, 0, 0, 0], 2, 2), "Unconfigured blank");
                    context.configure(configuration);
                    write(device, context.getCurrentTexture(), format, [71, 113, 19]);
                    canvas.width = 3;
                    equalPixels(snapshot(canvas), solid(zero, 3, 2), "Resize blank");
                    const resized = context.getCurrentTexture();
                    check(resized !== texture, "Resize uses a fresh current object");
                    const encoder = device.createCommandEncoder();
                    const pass = encoder.beginRenderPass({colorAttachments: [{
                        view: resized.createView({format: format + "-srgb"}),
                        loadOp: "clear", storeOp: "store",
                        clearValue: {r: 0.25, g: 0.5, b: 0.75, a: 1},
                    }]});
                    pass.end();
                    device.queue.submit([encoder.finish()]);
                    equalPixels(snapshot(canvas), solid([137, 188, 225, 255], 3, 2),
                        "Declared sRGB view", 1);
                    context.unconfigure();
                    check(await device.popErrorScope() === null, "No unexpected validation error");
                    cases.push({format, alphaMode});
                }
            }
            device.pushErrorScope("validation");
            const invalid = document.createElement("canvas");
            invalid.width = 0;
            invalid.height = 2;
            const context = invalid.getContext("webgpu");
            context.configure({device, format: "rgba8unorm"});
            const errorTexture = context.getCurrentTexture();
            check(errorTexture === context.getCurrentTexture(), "Same error current object in task");
            errorTexture.createView();
            check(await device.popErrorScope() !== null, "Invalid current reports validation");
            context.unconfigure();
            check(uncaptured.length === 0, JSON.stringify(uncaptured));
            device.destroy();
            return cases;
        """)
        self.assertEqual(len(cases), 4)

    def test_no_submit_present_and_invalid_device_snapshot(self):
        self.native_page()
        for alpha_mode in ("opaque", "premultiplied"):
            self.run_canvas("""
                const adapter = await navigator.gpu.requestAdapter();
                check(adapter, "Hardware adapter");
                const device = await adapter.requestDevice();
                const canvas = document.createElement("canvas");
                canvas.width = canvas.height = 2;
                document.head.insertAdjacentHTML("beforeend", `<style>
                    html, body { margin: 0; }
                    canvas { position: fixed; inset: 0; width: 100vw; height: 100vh; }
                </style>`);
                document.body.replaceChildren(canvas);
                const context = canvas.getContext("webgpu");
                const format = navigator.gpu.getPreferredCanvasFormat();
                const usage = GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_DST;
                context.configure({device, format, usage, alphaMode: arguments[0]});
                const texture = context.getCurrentTexture();
                check(texture.usage === usage, "Native logical usage");
                write(device, texture, format, [17, 31, 53]);
                window.nativeCanvasState = {device, canvas, context, format, texture};
                return true;
            """, [alpha_mode])
            self.compositor_color([17, 31, 53, 255])
            result = self.run_canvas("""
                const {device, canvas, context, format, texture} = window.nativeCanvasState;
                const next = context.getCurrentTexture();
                check(next !== texture, "Observed A was presented and expired");
                write(device, next, format, [5, 7, 11]);
                equalPixels(snapshot(canvas), solid([5, 7, 11, 255], 2, 2), "Writing snapshot before more writes");
                check(next === context.getCurrentTexture(), "Writing snapshot keeps current");
                write(device, next, format, [71, 113, 19]);
                device.destroy();
                const beforePresent = snapshot(canvas);
                return {beforePresent};
            """)
            blank = [0, 0, 0, 255 if alpha_mode == "opaque" else 0] * 4
            self.assertNotEqual(result["beforePresent"], [71, 113, 19, 255] * 4,
                                "Unpresented B is not an invalid-device fallback")
            self.assertIn(result["beforePresent"], ([17, 31, 53, 255] * 4, blank))
            self.compositor_color([71, 113, 19, 255])
            self.run_canvas("""
                const {canvas, context} = window.nativeCanvasState;
                equalPixels(snapshot(canvas), solid([71, 113, 19, 255], 2, 2), "Presented sealed B after device destroy");
                context.unconfigure();
                delete window.nativeCanvasState;
                return true;
            """)

    def test_worker_offscreen_current_and_snapshot(self):
        self.native_page(worker=True)
        result = self.run_canvas("""
            if (!window.Worker || !window.OffscreenCanvas) {
                return {unsupported: "Worker or OffscreenCanvas is unavailable"};
            }
            const source = `
                (async () => {
                    if (!self.OffscreenCanvas || !navigator.gpu) {
                        return {unsupported: "Worker WebGPU or OffscreenCanvas is unavailable"};
                    }
                    const adapter = await navigator.gpu.requestAdapter();
                    if (!adapter) throw new Error("Worker adapter unavailable");
                    const device = await adapter.requestDevice();
                    const canvas = new OffscreenCanvas(2, 2);
                    const context = canvas.getContext("webgpu");
                    const target = new OffscreenCanvas(2, 2).getContext("2d");
                    if (!context || !target) {
                        device.destroy();
                        return {unsupported: "Worker canvas context capability unavailable"};
                    }
                    const format = navigator.gpu.getPreferredCanvasFormat();
                    const usage = GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_DST;
                    context.configure({device, format, usage, alphaMode: "opaque"});
                    const texture = context.getCurrentTexture();
                    if (texture !== context.getCurrentTexture() || texture.usage !== usage) {
                        throw new Error("Worker current identity or native usage");
                    }
                    const capture = () => {
                        target.clearRect(0, 0, 2, 2);
                        target.drawImage(canvas, 0, 0);
                        return Array.from(target.getImageData(0, 0, 2, 2).data);
                    };
                    const zero = capture();
                    if (zero.some((value, index) => value !== (index % 4 === 3 ? 255 : 0))) {
                        throw new Error("Worker untouched opaque snapshot: " + zero);
                    }
                    for (const color of [[17, 31, 53], [71, 113, 19]]) {
                        const pixel = format === "bgra8unorm" ? [color[2], color[1], color[0], 255] : [...color, 255];
                        device.queue.writeTexture({texture}, new Uint8Array([...pixel, ...pixel, ...pixel, ...pixel]),
                            {bytesPerRow: 8}, {width: 2, height: 2});
                        const pixels = capture();
                        if (pixels.some((value, index) => value !== [...color, 255][index % 4]) ||
                            texture !== context.getCurrentTexture()) {
                            throw new Error("Worker write/snapshot expired or incorrect: " + pixels);
                        }
                    }
                    context.unconfigure();
                    device.destroy();
                    return {ok: true};
                })().then(result => postMessage(result), error => postMessage({error: String(error)}));
            `;
            const url = URL.createObjectURL(new Blob([source], {type: "text/javascript"}));
            try {
                return await new Promise((resolve, reject) => {
                    const worker = new Worker(url);
                    const timer = setTimeout(() => { worker.terminate(); reject(new Error("Worker timeout")); }, 30000);
                    worker.onmessage = event => { clearTimeout(timer); worker.terminate(); resolve(event.data); };
                    worker.onerror = event => { clearTimeout(timer); worker.terminate(); reject(new Error(event.message)); };
                });
            } finally {
                URL.revokeObjectURL(url);
            }
        """)
        self.assertFalse(result.get("unsupported"), result)
        self.assertTrue(result.get("ok"), result)

    def test_visible_worker_native_presentation_and_actor_lifetime(self):
        prefs = dict(self.original_prefs or {})
        prefs["gfx.webrender.vulkan.unified-webgpu"] = False
        self.marionette.restart(in_app=False, clean=True)
        self.marionette.enforce_gecko_prefs(prefs)
        self.marionette.set_context("content")
        self.marionette.navigate(self.marionette.absolute_url("test.html"))
        transferable = self.run_canvas("""
            return typeof HTMLCanvasElement.prototype.transferControlToOffscreen === "function";
        """)
        if not transferable:
            self.skipTest("Gate-off prerequisite: transferable HTML canvas")
        self.native_page(worker=True)
        self.run_canvas("""
            const adapter = await navigator.gpu.requestAdapter();
            check(adapter, "Main hardware adapter");
            const device = await adapter.requestDevice();
            const canvas = document.createElement("canvas");
            canvas.width = canvas.height = 2;
            document.head.insertAdjacentHTML("beforeend", `<style>
                html, body { margin: 0; }
                canvas { position: fixed; inset: 0; width: 100vw; height: 100vh; }
            </style>`);
            document.body.replaceChildren(canvas);
            const context = canvas.getContext("webgpu");
            const format = navigator.gpu.getPreferredCanvasFormat();
            const usage = GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_DST;
            context.configure({device, format, usage, alphaMode: "opaque"});
            const texture = context.getCurrentTexture();
            check(texture.usage === usage, "Main native logical usage");
            const state = {device, canvas, context, format, usage,
                lost: null, errors: [], workerError: null};
            device.lost.then(info => { state.lost = info.reason; });
            device.addEventListener("uncapturederror", event => {
                event.preventDefault(); state.errors.push(event.error.message);
            });
            window.visibleWorkerState = state;
            write(device, texture, format, [17, 31, 53]);
            return true;
        """)
        self.compositor_color([17, 31, 53, 255])
        worker = self.run_canvas("""
            const state = window.visibleWorkerState;
            const canvas = document.createElement("canvas");
            canvas.width = canvas.height = 2;
            document.body.append(canvas);
            const offscreen = canvas.transferControlToOffscreen();
            const source = `
                self.onmessage = event => {
                    (async () => {
                        if (!navigator.gpu) throw new Error("Enabled worker WebGPU unavailable");
                        const adapter = await navigator.gpu.requestAdapter();
                        if (!adapter) throw new Error("Worker hardware adapter unavailable");
                        const device = await adapter.requestDevice();
                        device.addEventListener("uncapturederror", event => {
                            event.preventDefault(); postMessage({error: event.error.message});
                        });
                        const canvas = event.data.canvas;
                        const context = canvas.getContext("webgpu");
                        if (!context) throw new Error("Transferred worker WebGPU context unavailable");
                        const format = navigator.gpu.getPreferredCanvasFormat();
                        const usage = GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_DST;
                        context.configure({device, format, usage, alphaMode: "opaque"});
                        const texture = context.getCurrentTexture();
                        if (texture.usage !== usage) throw new Error("Worker native logical usage");
                        self.nativeCanvasState = {device, canvas, context, texture};
                        const pixel = format === "bgra8unorm" ? [113, 37, 149, 255] : [149, 37, 113, 255];
                        device.queue.writeTexture({texture},
                            new Uint8Array([...pixel, ...pixel, ...pixel, ...pixel]),
                            {bytesPerRow: 8}, {width: 2, height: 2});
                        return {ready: true, usage, requested: usage, format};
                    })().then(value => postMessage(value), error => postMessage({error: String(error)}));
                };
            `;
            state.workerUrl = URL.createObjectURL(new Blob([source], {type: "text/javascript"}));
            state.worker = new Worker(state.workerUrl);
            state.workerCanvas = canvas;
            return await new Promise((resolve, reject) => {
                const timer = setTimeout(() => reject(new Error("Visible worker readiness timeout")), 30000);
                state.worker.onmessage = event => {
                    if (event.data.error) {
                        state.workerError = event.data.error;
                        clearTimeout(timer); reject(new Error(event.data.error));
                    } else if (event.data.ready) {
                        clearTimeout(timer); resolve(event.data);
                    }
                };
                state.worker.onerror = event => {
                    state.workerError = event.message;
                    clearTimeout(timer); reject(new Error(event.message));
                };
                state.worker.postMessage({canvas: offscreen}, [offscreen]);
            });
        """)
        self.assertTrue(worker["ready"], worker)
        self.assertEqual(worker["usage"], worker["requested"], worker)
        try:
            self.compositor_color([149, 37, 113, 255])
            self.run_canvas("""
                const state = window.visibleWorkerState;
                check(!state.workerError, state.workerError);
                check(state.lost === null, "Worker activity lost the main device");
                check(state.errors.length === 0, JSON.stringify(state.errors));
                state.worker.terminate();
                state.workerCanvas.remove();
                URL.revokeObjectURL(state.workerUrl);
                state.worker = null;
                const texture = state.context.getCurrentTexture();
                check(texture.usage === state.usage, "Retained main device native usage");
                write(state.device, texture, state.format, [71, 113, 19]);
                return true;
            """)
            self.compositor_color([71, 113, 19, 255])
            self.run_canvas("""
                const state = window.visibleWorkerState;
                check(state.lost === null, "Worker termination lost the main device");
                check(state.errors.length === 0, JSON.stringify(state.errors));
                return true;
            """)
        finally:
            self.run_canvas("""
                const state = window.visibleWorkerState;
                if (state.worker) state.worker.terminate();
                state.workerCanvas.remove();
                URL.revokeObjectURL(state.workerUrl);
                state.context.unconfigure();
                state.device.destroy();
                delete window.visibleWorkerState;
                return true;
            """)
