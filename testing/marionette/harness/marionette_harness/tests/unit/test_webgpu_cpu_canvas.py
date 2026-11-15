# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

"""Regression coverage for ordinary WebGPU canvas readback presentation.
Requires startup dom.webgpu.allow-present-without-readback=false and the task
X11 pixel server on the browser host. No shared-WebGPU APIs or preference.
"""

import json
import os
import sys
import time
from urllib.request import urlopen
from urllib.parse import urlencode

from marionette_harness import MarionetteTestCase

HELPERS = r"""
async function frames() {
  await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
}
function locationOf(element) {
  const rect = element.getBoundingClientRect();
  return {x: Math.round((mozInnerScreenX + rect.left + rect.width / 2) * devicePixelRatio),
          y: Math.round((mozInnerScreenY + rect.top + rect.height / 2) * devicePixelRatio)};
}
async function pixel(element, expected) {
  const copy = document.createElement("canvas");
  copy.width = element.width; copy.height = element.height;
  const ctx = copy.getContext("2d", {willReadFrequently: true});
  const deadline = performance.now() + 5000;
  let actual;
  do {
    ctx.clearRect(0, 0, copy.width, copy.height);
    ctx.drawImage(element, 0, 0);
    actual = Array.from(ctx.getImageData(0, 0, 1, 1).data);
    if (actual.every((v, i) => v === expected[i])) return actual;
    await frames();
  } while (performance.now() < deadline);
  throw new Error(JSON.stringify({actual, expected}));
}
function write(gpu, ctx, format, operation, rgb, width, height) {
  const texture = ctx.getCurrentTexture();
  if (operation === "write") {
    const rgba = format === "bgra8unorm" ? [rgb[2], rgb[1], rgb[0], 0] : [...rgb, 0];
    const bytes = new Uint8Array(width * height * 4);
    for (let i = 0; i < bytes.length; i += 4) bytes.set(rgba, i);
    gpu.queue.writeTexture({texture}, bytes, {bytesPerRow: width * 4}, [width, height]);
  } else {
    const source = new OffscreenCanvas(width, height);
    const context = source.getContext("2d");
    context.fillStyle = `rgba(${rgb.join(",")},0.5)`;
    context.fillRect(0, 0, width, height);
    gpu.queue.copyExternalImageToTexture({source}, {texture, premultipliedAlpha: false}, [width, height]);
  }
  return texture;
}
"""


class TestWebGPUCPUCanvas(MarionetteTestCase):
    def setUp(self):
        super().setUp()
        if sys.platform != "linux" or os.environ.get("WR_CPU_CANVAS_TEST") != "1":
            self.skipTest(
                "Opt-in real X11 presentation gate: set WR_CPU_CANVAS_TEST=1 and run the pixel sampler"
            )
        self.pixel_token = os.environ.get("WR_CPU_CANVAS_PIXEL_TOKEN", "")
        self.assertGreaterEqual(
            len(self.pixel_token), 32, "Missing high-entropy pixel sampler credential"
        )
        self.assertTrue(self.pixel_token.isascii())
        self.assertIs(
            self.marionette.get_pref("dom.webgpu.allow-present-without-readback"),
            False,
            "Set readback preference before startup, not after session creation",
        )
        self.assertTrue(self.marionette.get_pref("dom.webgpu.enabled"))
        self.assertIs(
            self.marionette.get_pref("dom.webgpu.testing.assert-hardware-adapter"), True
        )
        self.assertEqual(self.marionette.get_pref("dom.webgpu.wgpu-backend"), "vulkan")
        self.marionette.timeout.script = 60
        self.marionette.set_context("content")
        browser_port = int(os.environ.get("WR_CPU_CANVAS_BROWSER_PORT", "2830"))
        self.marionette.navigate(f"http://127.0.0.1:{browser_port}/empty.html")
        self.assertTrue(
            self.marionette.execute_script(
                "return isSecureContext && !!navigator.gpu", sandbox=None
            )
        )
        self.marionette.execute_script(
            """
          document.body.style.cssText = "margin:0;background:rgb(255,0,255)";
          window.target = document.createElement("canvas");
          target.width = 67; target.height = 5;
          target.style.cssText = "display:block;width:268px;height:80px;background:rgb(255,0,255)";
          document.body.appendChild(target);
        """,
            sandbox=None,
        )
        where = self.run_js("await frames(); return locationOf(target);")
        self.displayed(where, [255, 0, 255], "X11 coordinate/background calibration")

    def run_js(self, body, args=None):
        result = self.marionette.execute_async_script(
            HELPERS
            + """
          const done = arguments[arguments.length - 1];
          const input = arguments[0];
          (async () => {
        """
            + body
            + """
          })().then(done, error => done({error:String(error), stack:error.stack}));
        """,
            script_args=[args],
            sandbox=None,
        )
        self.assertNotIn("error", result, result)
        return result

    def displayed(self, where, expected, label):
        port = int(os.environ.get("WR_CPU_CANVAS_PIXEL_PORT", "2830"))
        query = urlencode({"x": where["x"], "y": where["y"], "token": self.pixel_token})
        url = f"http://127.0.0.1:{port}/?{query}"
        deadline = time.monotonic() + 10
        actual = None
        while time.monotonic() < deadline:
            with urlopen(url, timeout=3) as response:
                actual = json.load(response)["rgb"]
            if actual == expected:
                print(f"V6CPU displayed {label}: {actual}", flush=True)
                return
            time.sleep(0.05)
        self.fail(
            f"{label}: displayed={actual}, expected={expected}, coordinates={where}"
        )

    def configure(self, format):
        self.run_js(
            """
          const adapter = await navigator.gpu.requestAdapter();
          if (!adapter) throw new Error("No WebGPU adapter");
          window.gpu = await adapter.requestDevice();
          window.errors = [];
          gpu.addEventListener("uncapturederror", event => errors.push(String(event.error)));
          gpu.queue.submit = () => { throw new Error("QueueSubmit forbidden in CPU queue-write gate"); };
          window.context = target.getContext("webgpu");
          context.configure({device:gpu, format:input, alphaMode:"opaque",
            usage:GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_DST | GPUTextureUsage.COPY_SRC});
          return {configured:true};
        """,
            format,
        )

    def test_queue_writes_opaque_formats_reuse_and_resize(self):
        for format in ("rgba8unorm", "bgra8unorm"):
            self.configure(format)
            for operation in ("write", "external"):
                # Many repeated publications exercise unmap-before-buffer-reuse.
                for index in range(12):
                    color = [255, 0, 0] if index % 2 == 0 else [0, 0, 255]
                    where = self.run_js(
                        """
                      const [format, operation, rgb, index] = input;
                      if (index === 6) { target.width += 4; target.height += 2; }
                      window.last = write(gpu, context, format, operation, rgb, target.width, target.height);
                      await frames();
                      return locationOf(target);
                    """,
                        [format, operation, color, index],
                    )
                    self.displayed(where, color, f"{format}/{operation}/{index}")
                # Only after real displayed pixels passed may a snapshot be taken.
                result = self.run_js("""
                  const actual = await pixel(target, [0,0,255,255]);
                  if (errors.length) throw new Error(errors.join(";"));
                  return {pixel:actual};
                """)
                self.assertEqual(result["pixel"], [0, 0, 255, 255])
            self.run_js("""context.unconfigure(); gpu.destroy();
              if ((await gpu.lost).reason !== "destroyed") throw new Error("Wrong loss reason");
              return {destroyed:true};""")

    def test_snapshot_after_logical_texture_destroy_and_replacement_device(self):
        self.configure("rgba8unorm")
        where = self.run_js("""window.last = write(gpu, context, "rgba8unorm", "write",
          [0,255,0], target.width, target.height); await frames(); return locationOf(target);""")
        self.displayed(where, [0, 255, 0], "before logical texture destroy")
        result = self.run_js("""
          last.destroy();
          const actual = await pixel(target, [0,255,0,255]);
          context.unconfigure(); gpu.destroy();
          if ((await gpu.lost).reason !== "destroyed") throw new Error("Wrong loss reason");
          return {pixel:actual};
        """)
        self.assertEqual(result["pixel"], [0, 255, 0, 255])
        self.configure("bgra8unorm")
        where = self.run_js("""target.width=73; target.height=11;
          last=write(gpu,context,"bgra8unorm","external",[255,0,0],73,11);
          await frames(); return locationOf(target);""")
        self.displayed(
            where, [255, 0, 0], "replacement device after logical destruction"
        )
        self.run_js("""if(errors.length) throw new Error(errors.join(";"));
          context.unconfigure(); gpu.destroy(); return {done:true};""")

    def test_worker_offscreen_queue_write_presentation(self):
        where = self.run_js(r"""
          const offscreen = target.transferControlToOffscreen();
          const code = new Blob([`
            onmessage = async event => {
              try {
                const adapter = await navigator.gpu.requestAdapter();
                if (!adapter) throw new Error("No worker adapter");
                const gpu = await adapter.requestDevice();
                gpu.queue.submit=()=>{throw new Error("No QueueSubmit allowed")};
                const canvas=event.data, context=canvas.getContext("webgpu");
                context.configure({device:gpu,format:"rgba8unorm",alphaMode:"opaque",
                  usage:GPUTextureUsage.RENDER_ATTACHMENT|GPUTextureUsage.COPY_DST});
                const bytes=new Uint8Array(canvas.width*canvas.height*4);
                for(let i=0;i<bytes.length;i+=4) bytes.set([0,255,0,0],i);
                gpu.queue.writeTexture({texture:context.getCurrentTexture()},bytes,
                  {bytesPerRow:canvas.width*4},[canvas.width,canvas.height]);
                self.retained={gpu,context,canvas};
                postMessage({ok:true});
              } catch(error) {postMessage({error:String(error)});}
            };
          `],{type:"text/javascript"});
          window.workerURL=URL.createObjectURL(code);
          window.worker=new Worker(workerURL);
          const ready=new Promise((resolve,reject)=>{
            worker.onmessage=e=>e.data.error?reject(new Error(e.data.error)):resolve();
            worker.onerror=e=>reject(new Error(e.message));
          });
          worker.postMessage(offscreen,[offscreen]); await ready; await frames();
          return locationOf(target);
        """)
        self.displayed(where, [0, 255, 0], "worker opaque queue.writeTexture")
        self.run_js(
            """worker.terminate(); URL.revokeObjectURL(workerURL); return {done:true};"""
        )
