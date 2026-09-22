/* Any copyright is dedicated to the Public Domain.
   http://creativecommons.org/publicdomain/zero/1.0/ */

"use strict";

const { Troubleshoot } = ChromeUtils.importESModule(
  "resource://gre/modules/Troubleshoot.sys.mjs"
);

add_task(async function test_webrender_backend() {
  const backend = JSON.parse(await window.windowUtils.getWebRenderBackendInfo());
  ok(
    ["SWGL", "OpenGL", "OpenGL ES", "Vulkan (wgpu-hal)"].includes(backend.backend),
    "The live compositor identifies its rendering API"
  );
  ok(backend.renderer, "The renderer identifies itself");
  ok(backend.driver, "The renderer reports its driver version");
  ok(backend.maxTextureSize > 0, "The renderer reports its texture limit");
  is(
    backend.process,
    window.windowUtils.gpuProcessPid == -1 ? "Parent" : "GPU",
    "The backend identifies the process that owns the renderer"
  );
  if (window.windowUtils.layerManagerType.includes("Software")) {
    is(backend.backend, "SWGL", "Software WebRender reports SWGL");
    is(backend.driver, "SWGL", "SWGL reports the built-in software driver");
  }

  const snapshot = await Troubleshoot.snapshot();
  Assert.deepEqual(
    snapshot.graphics.webRenderBackend,
    backend,
    "The troubleshooting snapshot includes the active backend"
  );

  await BrowserTestUtils.withNewTab("about:support", async browser => {
    const displayed = await SpecialPowers.spawn(browser, [], async () => {
      const selector = '[data-l10n-id="web-render-backend"]';
      await ContentTaskUtils.waitForCondition(
        () => content.document.querySelector(selector),
        "The WebRender backend row is rendered"
      );
      return JSON.parse(
        content.document.querySelector(selector).nextElementSibling.textContent
      );
    });
    Assert.deepEqual(displayed, backend, "about:support displays the backend");
  });
});
