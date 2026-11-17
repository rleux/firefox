# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import mozinfo

from marionette_harness import MarionetteTestCase


HELPERS = r"""
async function device() {
  const adapter = await navigator.gpu.requestAdapter();
  if (!adapter) throw new Error("No WebGPU adapter");
  return adapter.requestDevice();
}
function canvas() {
  const element = document.createElement("canvas");
  element.width = 4;
  element.height = 4;
  document.body.appendChild(element);
  return element;
}
async function frames() {
  await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
}
async function paint(device, context, color) {
  const texture = context.getCurrentTexture();
  const encoder = device.createCommandEncoder();
  const pass = encoder.beginRenderPass({colorAttachments: [{
    view: texture.createView(), clearValue: color, loadOp: "clear", storeOp: "store"
  }]});
  pass.end();
  device.queue.submit([encoder.finish()]);
  await device.queue.onSubmittedWorkDone();
  await frames();
  return texture;
}
async function pixel(element, expected) {
  const copy = document.createElement("canvas");
  copy.width = element.width;
  copy.height = element.height;
  const context = copy.getContext("2d", {willReadFrequently: true});
  const deadline = performance.now() + 5000;
  let actual;
  do {
    context.clearRect(0, 0, copy.width, copy.height);
    context.drawImage(element, 0, 0);
    actual = Array.from(context.getImageData(0, 0, 1, 1).data);
    if (actual.every((value, index) => value === expected[index])) return actual;
    await frames();
  } while (performance.now() < deadline);
  throw new Error("Canvas pixel differs: " + JSON.stringify({actual, expected}));
}
"""


class TestVulkanWebGPUCanvas(MarionetteTestCase):
    def setUp(self):
        super().setUp()
        self._configured = False
        if mozinfo.info.get("os") != "linux":
            self.skipTest("Shared Vulkan WebGPU is enabled only for GTK/X11")
        if not self.marionette.get_pref("gfx.webrender.vulkan.shared-webgpu"):
            self.skipTest("Requires the opt-in shared Vulkan WebGPU startup preference")
        self.marionette.enforce_gecko_prefs({
            "gfx.webrender.vulkan": True,
            "gfx.webrender.vulkan.shared-webgpu": True,
            "layers.gpu-process.enabled": True,
            "layers.gpu-process.force-enabled": True,
            "dom.webgpu.enabled": True,
            "dom.webgpu.wgpu-backend": "vulkan",
        })
        self._configured = True
        self.marionette.timeout.script = 60
        with self.marionette.using_context("chrome"):
            backend = self.marionette.execute_async_script("""
                const done = arguments[arguments.length - 1];
                window.windowUtils.getWebRenderBackendInfo()
                  .then(info => done(JSON.parse(info)), error => done({error: String(error)}));
            """)
        if backend.get("backend") != "Vulkan" or backend.get("process") != "GPU":
            self.skipTest(
                "Requires Vulkan WebRender in the GPU process: " + str(backend)
            )
        self.marionette.set_context("content")
        self.marionette.navigate(self.marionette.absolute_url("empty.html"))
        self.assertTrue(self.marionette.execute_script("return window.isSecureContext"))

    def tearDown(self):
        try:
            if self._configured:
                self.marionette.restart(in_app=False, clean=True)
        finally:
            super().tearDown()

    def run_gpu(self, body):
        result = self.marionette.execute_async_script(
            HELPERS
            + """
            const done = arguments[arguments.length - 1];
            (async () => {
            """
            + body
            + """
            })().then(done, error => done({error: String(error), stack: error.stack}));
            """
        )
        self.assertNotIn("error", result, result)
        return result

    def test_present_resize_and_device_destruction(self):
        result = self.run_gpu("""
            const first = await device();
            const second = await device();
            const element = canvas();
            const context = element.getContext("webgpu");
            const format = navigator.gpu.getPreferredCanvasFormat();
            context.configure({device: first, format, alphaMode: "premultiplied"});
            first.pushErrorScope("validation");
            first.createBuffer({size: 4, usage: 0});
            const error = await first.popErrorScope();
            if (!(error instanceof GPUValidationError)) throw new Error("Expected isolated validation error");
            await paint(first, context, {r: 1, g: 0, b: 0, a: 1});
            await pixel(element, [255, 0, 0, 255]);
            first.destroy();
            if ((await first.lost).reason !== "destroyed") throw new Error("Wrong logical loss reason");
            context.configure({device: second, format, alphaMode: "premultiplied"});
            element.width = 7;
            element.height = 5;
            await paint(second, context, {r: 0, g: 0, b: 1, a: 1});
            const actual = await pixel(element, [0, 0, 255, 255]);
            second.destroy();
            return {pixel: actual, size: [element.width, element.height]};
        """)
        self.assertEqual(result["pixel"], [0, 0, 255, 255])
        self.assertEqual(result["size"], [7, 5])

    def test_runtime_preference_change_keeps_startup_selection(self):
        self.marionette.set_pref("gfx.webrender.vulkan.shared-webgpu", False)
        result = self.run_gpu("""
            const gpu = await device();
            const element = canvas();
            const context = element.getContext("webgpu");
            context.configure({device: gpu, format: navigator.gpu.getPreferredCanvasFormat()});
            const old = await paint(gpu, context, {r: 0, g: 1, b: 0, a: 1});
            const actual = await pixel(element, [0, 255, 0, 255]);
            const expired = context.getCurrentTexture() !== old;
            gpu.destroy();
            return {expired, pixel: actual};
        """)
        self.assertTrue(result["expired"])
        self.assertEqual(result["pixel"], [0, 255, 0, 255])

    def test_gpu_process_restart_keeps_startup_selection(self):
        self.run_gpu("""
            window.savedDevice = await device();
            const element = canvas();
            const context = element.getContext("webgpu");
            context.configure({device: savedDevice, format: navigator.gpu.getPreferredCanvasFormat()});
            await paint(savedDevice, context, {r: 1, g: 0, b: 0, a: 1});
            return {ready: true};
        """)
        self.marionette.set_pref("gfx.webrender.vulkan.shared-webgpu", False)
        with self.marionette.using_context("chrome"):
            result = self.marionette.execute_async_script("""
                const done = arguments[arguments.length - 1];
                (async () => {
                    const previous = window.windowUtils.gpuProcessPid;
                    if (previous <= 0) throw new Error("No initial GPU process");
                    window.windowUtils.terminateGPUProcess();
                    const deadline = Date.now() + 45000;
                    do {
                        await new Promise(resolve => setTimeout(resolve, 100));
                        const current = window.windowUtils.gpuProcessPid;
                        if (current <= 0 || current === previous) continue;
                        let info;
                        try {
                            info = JSON.parse(await window.windowUtils.getWebRenderBackendInfo());
                        } catch {
                            continue;
                        }
                        if (info.backend === "Vulkan" && info.process === "GPU") {
                            return {previous, current};
                        }
                    } while (Date.now() < deadline);
                    throw new Error("GPU-process Vulkan recovery timed out");
                })().then(done, error => done({error: String(error)}));
            """)
        self.assertNotIn("error", result, result)
        self.assertNotEqual(result["previous"], result["current"])
        self.marionette.navigate(self.marionette.absolute_url("empty.html"))
        result = self.run_gpu("""
            const gpu = await device();
            const element = canvas();
            const context = element.getContext("webgpu");
            context.configure({device: gpu, format: navigator.gpu.getPreferredCanvasFormat()});
            const old = await paint(gpu, context, {r: 0, g: 0, b: 1, a: 1});
            const actual = await pixel(element, [0, 0, 255, 255]);
            const expired = context.getCurrentTexture() !== old;
            gpu.destroy();
            return {expired, pixel: actual};
        """)
        self.assertTrue(result["expired"])
        self.assertEqual(result["pixel"], [0, 0, 255, 255])

    def test_worker_offscreen_canvas_presents(self):
        result = self.run_gpu("""
            const element = canvas();
            const offscreen = element.transferControlToOffscreen();
            const script = new Blob([`
              onmessage = async event => {
                try {
                  const adapter = await navigator.gpu.requestAdapter();
                  if (!adapter) throw new Error("No worker WebGPU adapter");
                  const device = await adapter.requestDevice();
                  const context = event.data.getContext("webgpu");
                  context.configure({device, format: navigator.gpu.getPreferredCanvasFormat()});
                  const encoder = device.createCommandEncoder();
                  const pass = encoder.beginRenderPass({colorAttachments: [{
                    view: context.getCurrentTexture().createView(),
                    clearValue: {r: 0, g: 1, b: 0, a: 1}, loadOp: "clear", storeOp: "store"
                  }]});
                  pass.end();
                  device.queue.submit([encoder.finish()]);
                  await device.queue.onSubmittedWorkDone();
                  postMessage({ok: true});
                } catch (error) { postMessage({error: String(error)}); }
              };
            `], {type: "text/javascript"});
            const url = URL.createObjectURL(script);
            const worker = new Worker(url);
            try {
              const ready = new Promise((resolve, reject) => {
                worker.onmessage = event => event.data.error ? reject(new Error(event.data.error)) : resolve();
                worker.onerror = event => reject(new Error(event.message));
              });
              worker.postMessage(offscreen, [offscreen]);
              await ready;
              await frames();
              return {pixel: await pixel(element, [0, 255, 0, 255])};
            } finally {
              worker.terminate();
              URL.revokeObjectURL(url);
            }
        """)
        self.assertEqual(result["pixel"], [0, 255, 0, 255])
