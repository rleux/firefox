/* Any copyright is dedicated to the Public Domain.
   http://creativecommons.org/publicdomain/zero/1.0/ */

"use strict";

const { Troubleshoot } = ChromeUtils.importESModule(
  "resource://gre/modules/Troubleshoot.sys.mjs"
);

add_task(async function test_webrender_backend() {
  const backend = JSON.parse(
    await window.windowUtils.getWebRenderBackendInfo()
  );
  info(`WebRender backend: ${JSON.stringify(backend)}`);
  ok(
    ["SWGL", "OpenGL", "OpenGL ES", "Vulkan"].includes(backend.backend),
    "The live compositor identifies its rendering API"
  );
  ok(backend.renderer, "The renderer identifies itself");
  is(
    typeof backend.driver,
    "string",
    "The renderer reports its driver information"
  );
  Assert.greater(
    backend.maxTextureSize,
    0,
    "The renderer reports its texture limit"
  );
  ok(
    ["Parent", "GPU"].includes(backend.process),
    "The created API reports its owning process"
  );
  Assert.deepEqual(
    JSON.parse(await window.windowUtils.getWebRenderBackendInfo()),
    backend,
    "Repeated queries use the same API's cached facts"
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

  await BrowserTestUtils.withNewTab({ gBrowser, url: "about:support" }, async browser => {
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


add_task(async function test_closed_window_backend_is_unavailable() {
  const win = await BrowserTestUtils.openNewBrowserWindow();
  const utils = win.windowUtils;
  await BrowserTestUtils.closeWindow(win);
  await Assert.rejects(
    Promise.resolve().then(() => utils.getWebRenderBackendInfo()),
    /NS_ERROR_NOT_AVAILABLE/,
    "A closed window cannot report a live backend"
  );
});


add_task(async function test_windowless_backend_is_unavailable() {
  const browser = Services.appShell.createWindowlessBrowser(true);
  try {
    const utils = browser.docShell.domWindow.windowUtils;
    await Assert.rejects(
      Promise.resolve().then(() => utils.getWebRenderBackendInfo()),
      /NS_ERROR_NOT_AVAILABLE/,
      "A windowless browser cannot initialize a renderer for diagnostics"
    );
  } finally {
    browser.close();
  }
});
