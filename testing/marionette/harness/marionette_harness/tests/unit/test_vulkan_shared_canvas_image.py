# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import json
import mozinfo
from marionette_harness import MarionetteTestCase

HELPERS = r"""
async function frames() {
  await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
}
async function make(left, format = navigator.gpu.getPreferredCanvasFormat()) {
  const adapter = await navigator.gpu.requestAdapter();
  if (!adapter) throw new Error("No WebGPU adapter");
  const device = await adapter.requestDevice();
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 64;
  canvas.style = `position:absolute;left:${left}px;top:10px;width:64px;height:64px`;
  document.body.appendChild(canvas);
  const context = canvas.getContext("webgpu");
  context.configure({device, format, alphaMode:"premultiplied"});
  return {device, canvas, context, format};
}
async function paint(item, color, storeOp = "store") {
  const texture = item.context.getCurrentTexture();
  if (texture !== item.context.getCurrentTexture()) throw new Error("Current texture identity changed within frame");
  const encoder = item.device.createCommandEncoder();
  const pass = encoder.beginRenderPass({colorAttachments:[{
    view:texture.createView(), clearValue:color, loadOp:"clear", storeOp
  }]});
  pass.end();
  item.device.queue.submit([encoder.finish()]);
  await item.device.queue.onSubmittedWorkDone();
  await frames();
}
"""


class TestVulkanSharedCanvasImage(MarionetteTestCase):
    def setUp(self):
        super().setUp()
        if mozinfo.info.get("os") != "linux" or not self.marionette.get_pref(
            "gfx.webrender.vulkan.shared-webgpu"
        ):
            self.skipTest("Requires opt-in shared Vulkan on remote GTK/X11 hardware")
        self.marionette.enforce_gecko_prefs({
            "gfx.webrender.vulkan": True,
            "gfx.webrender.vulkan.shared-webgpu": True,
            "layers.gpu-process.enabled": True,
            "layers.gpu-process.force-enabled": True,
            "dom.webgpu.enabled": True,
            "dom.webgpu.wgpu-backend": "vulkan",
            "remote.screenshot.use_readback": True,
        })
        self.marionette.timeout.script = 60
        with self.marionette.using_context("chrome"):
            backend = self.marionette.execute_async_script("""
              const done = arguments[arguments.length - 1];
              window.windowUtils.getWebRenderBackendInfo().then(
                info => done(JSON.parse(info)), error => done({error:String(error)}));
            """)
        self.assertEqual(backend.get("backend"), "Vulkan", backend)
        self.assertEqual(backend.get("process"), "GPU", backend)
        self.marionette.set_context("content")
        self.marionette.navigate(self.marionette.absolute_url("empty.html"))
        self.assertTrue(self.marionette.execute_script("return window.isSecureContext"))
        self.marionette.execute_script("document.body.style='margin:0;background:white'")

    def gpu(self, body):
        result = self.marionette.execute_async_script(HELPERS + """
          const done = arguments[arguments.length - 1];
          (async () => {
        """ + body + """
          })().then(value => done({value}), error => done({error:String(error),stack:error.stack}));
        """, new_sandbox=False, sandbox="owned-shared-canvas")
        self.assertNotIn("error", result, result)
        return result.get("value")

    def compositor_pixels(self, label, points, expected):
        self.assertTrue(self.marionette.get_pref("remote.screenshot.use_readback"))
        screenshot = self.marionette.screenshot(format="base64", full=False)
        result = self.marionette.execute_async_script("""
          const [png, points] = arguments;
          const done = arguments[arguments.length - 1];
          const image = new Image();
          image.onerror = () => done({error:"Compositor PNG decode failed"});
          image.onload = () => {
            const canvas = document.createElement("canvas");
            canvas.width=image.width; canvas.height=image.height;
            const ctx=canvas.getContext("2d",{willReadFrequently:true});
            ctx.drawImage(image,0,0);
            done({pixels:points.map(([x,y]) => Array.from(ctx.getImageData(
              Math.floor(x*devicePixelRatio),Math.floor(y*devicePixelRatio),1,1).data))});
          };
          image.src="data:image/png;base64,"+png;
        """, script_args=[screenshot, points])
        self.assertNotIn("error", result, result)
        self.assertEqual(result["pixels"], expected)
        print("SAME_IMAGE_BROWSER_PIXELS " + json.dumps({
            "label": label, "actual": result["pixels"], "expected": expected,
            "source": "hardware-compositor-readback", "pass": True,
        }, sort_keys=True))

    def test_independent_canvases_and_logical_device_destroy(self):
        self.gpu("""
          window.a=await make(10,"rgba8unorm"); window.b=await make(90,"bgra8unorm");
          await paint(window.a,[1,0,0,1]); await paint(window.b,[0,1,0,1]);
          window.a.device.destroy(); await frames();
        """)
        self.compositor_pixels("rgba-bgra-logical-destroy", [[30,30],[110,30]],
                               [[255,0,0,255],[0,255,0,255]])
        self.gpu("""
          window.b.canvas.width=32; window.b.canvas.height=32;
          await paint(window.b,[0,0,1,1]);
        """)
        self.compositor_pixels("resize", [[30,30],[110,30]],
                               [[255,0,0,255],[0,0,255,255]])

    def test_discard_snapshot_and_canvas_group_opacity(self):
        snapshot = self.gpu("""
          window.a=await make(10,"rgba8unorm");
          await paint(window.a,[1,0,0,1],"discard");
          const copy=document.createElement("canvas");copy.width=copy.height=64;
          const ctx=copy.getContext("2d",{willReadFrequently:true});ctx.drawImage(window.a.canvas,0,0);
          const discarded=Array.from(ctx.getImageData(0,0,1,1).data);
          window.a.canvas.style.opacity="0.25";
          const shader=window.a.device.createShaderModule({code:`
            @vertex fn vs(@builtin(vertex_index) i:u32)->@builtin(position) vec4f {
              let x=f32((i<<1u)&2u);let y=f32(i&2u);return vec4f(x*2.0-1.0,y*2.0-1.0,0,1);
            }
            @fragment fn fs()->@location(0) vec4f {return vec4f(0,0,1,1);}`});
          const pipeline=window.a.device.createRenderPipeline({layout:"auto",vertex:{module:shader,entryPoint:"vs"},
            fragment:{module:shader,entryPoint:"fs",targets:[{format:window.a.format}]}});
          const encoder=window.a.device.createCommandEncoder();
          const pass=encoder.beginRenderPass({colorAttachments:[{view:window.a.context.getCurrentTexture().createView(),
            clearValue:[1,0,0,1],loadOp:"clear",storeOp:"store"}]});
          pass.setPipeline(pipeline);pass.setScissorRect(0,0,32,64);pass.draw(3);pass.end();
          window.a.device.queue.submit([encoder.finish()]);await window.a.device.queue.onSubmittedWorkDone();await frames();
          ctx.clearRect(0,0,64,64);ctx.drawImage(window.a.canvas,0,0);
          return {discarded, original:Array.from(ctx.getImageData(10,10,1,1).data)};
        """)
        self.assertEqual(snapshot["discarded"], [0,0,0,0])
        self.assertEqual(snapshot["original"], [0,0,255,255])
        self.compositor_pixels("canvas-group-opacity", [[20,30],[60,30]],
                               [[191,191,255,255],[255,191,191,255]])

    def test_worker_and_view_format_fallback(self):
        self.gpu("""
          const canvas=document.createElement("canvas");canvas.width=canvas.height=64;
          canvas.style="position:absolute;left:10px;top:10px;width:64px;height:64px";document.body.appendChild(canvas);
          const offscreen=canvas.transferControlToOffscreen();
          const url=URL.createObjectURL(new Blob([`onmessage=async event=>{try {
            const device=await (await navigator.gpu.requestAdapter()).requestDevice();
            const context=event.data.getContext("webgpu");context.configure({device,format:"rgba8unorm",alphaMode:"opaque"});
            const encoder=device.createCommandEncoder();const pass=encoder.beginRenderPass({colorAttachments:[{
              view:context.getCurrentTexture().createView(),clearValue:[0,1,0,0],loadOp:"clear",storeOp:"store"}]});
            pass.end();device.queue.submit([encoder.finish()]);await device.queue.onSubmittedWorkDone();
            await new Promise(resolve=>requestAnimationFrame(resolve));postMessage({ok:true});
          } catch(error) {postMessage({error:String(error)});}};`],{type:"application/javascript"}));
          window.worker=new Worker(url);
          await new Promise((resolve,reject)=>{window.worker.onmessage=event=>event.data.error?reject(Error(event.data.error)):resolve();window.worker.onerror=reject;window.worker.postMessage(offscreen,[offscreen]);});
          URL.revokeObjectURL(url); await frames();
          window.b=await make(90,"rgba8unorm");
          window.b.context.configure({device:window.b.device,format:window.b.format,viewFormats:["rgba8unorm-srgb"]});
          await paint(window.b,[0,0,1,1]);
        """)
        self.compositor_pixels("worker-opaque-and-view-fallback", [[30,30],[110,30]],
                               [[0,255,0,255],[0,0,255,255]])
