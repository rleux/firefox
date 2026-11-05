# Capturing WebRender screenshots for debugging

Default WebDriver, Marionette and DevTools screenshot paths re-render the
document through software `drawSnapshot` (`CrossProcessPaint`). They can produce
correct pixels while a WebRender-specific on-screen artifact remains.

The `remote.screenshot.use_readback` preference selects compositor readback for
content screenshots. This exercises the active WebRender backend, including
OpenGL, SWGL or Vulkan, rather than assuming a particular graphics API. Verify
the created backend through `about:support` or the privileged cached
`window.windowUtils.getWebRenderBackendInfo()` query before interpreting results.

## Usage

1. Set `remote.screenshot.use_readback` to `true`. See the Remote Agent
   [preferences](../../remote/doc/Prefs.md) documentation.
2. Use a visible foreground tab on a real display when testing hardware output.
3. Keep the WebDriver session in **content** scope, and capture with WebDriver
   BiDi `browsingContext.captureScreenshot`, Marionette/WebDriver classic
   *Take Screenshot*, or a client that drives those APIs.
4. Record the returned image dimensions, actual scale, content DOM rectangles and
   active backend. Crop the relevant area before inspecting or comparing pixels.

Leave the preference at its default (`false`) for normal automation that needs
document, element or clip-region screenshots.

## How it works

With the preference enabled, `capture.canvas` in `remote/shared/Capture.sys.mjs`
uses the top content browser's on-screen rectangle. It calls
`CanvasRenderingContext2D.drawWindow` with `DRAWWINDOW_USE_WIDGET_LAYERS` instead
of `drawSnapshot`. In the parent process, that flag drives
`WebRenderLayerManager::MakeSnapshotIfRequired`,
`WebRenderBridgeParent::RecvGetSnapshot`, `WebRenderAPI::Readback` and
`wr_renderer_readback`.

Readback follows the active device: OpenGL reads its framebuffer, while Vulkan
arms a capture of the next actual GPU frame. That frame copies its surface into
an owned snapshot before presentation; readback then copies the snapshot into
mapped staging storage. Ordinary frames do not refresh the snapshot after the
request is consumed. A failed or skipped capture must be requested again, and
failure propagates instead of consuming stale capture bytes. The mechanism is
not universally a `glReadPixels` call.

## Coordinates and expected pixels

The image is in device pixels. `capture.canvas` starts with the effective
`browsingContext.overrideDPPX` or the capture window's `devicePixelRatio` and may
reduce the scale for a requested maximum image size. Do not assume DPR 1, assume
the page's reported DPR always equals the capture scale, or scale an already
device-pixel WebRender coordinate twice.

`getBoundingClientRect()` returns CSS pixels. Convert a content-viewport rect
using the actual capture scale and origin, accounting for fractional positions
and image rounding. Record browser zoom, any DPPX override and the resulting PNG
dimensions. A scrolled document coordinate first needs conversion to the visible
content viewport.

For video or filtered images, build the CPU reference at the same scale and
pixel-center positions. Choose expected interior patches from that reference
before inspecting actual output. Preserve a failing edge sample separately;
finding a nearby matching output pixel does not validate the original sample.

For example, crop an already calculated device-pixel rectangle with:

```sh
magick in.png -crop 30x30+30+30 out.png
```

## Limits of the observation

- Every readback request degrades to the **full content area** of the
  **foreground tab**. Full-document (`origin: "document"`), element and clip
  requests return that viewport rather than the requested region.
- A chrome-scope capture has no content browser rectangle and continues through
  `drawSnapshot`. Use chrome scope for privileged diagnostics, then return to
  content scope for the pixel capture.
- Headless Firefox disables hardware compositing. Headless readback can test
  software WebRender but cannot establish hardware Vulkan or OpenGL output.
- Readback flushes scene builds and forces a fresh frame through
  `FlushFrameGeneration(RenderReasons::SNAPSHOT)`. It can hide damage-tracking,
  retained-frame or partial-presentation artifacts. Direct native-window pixel
  observation is a separate check for those behaviors.
- Apply DOM, scroll and zoom changes before capture. A producer's `requestAnimationFrame`
  or GPU work completion does not necessarily establish remote-image publication
  to the compositor. Preserve the **first** compositor capture before additional
  producer reads such as `getImageData` or `readPixels` that could change the
  producer path. If readiness needs polling, use bounded compositor-only captures
  and record every attempt and latency instead of overwriting the first result.
- On macOS, native-layer window readback is restricted to automation. The parent
  normally rejects `PNativeLayerRemote::RequestReadback`; this mode is not a
  general normal-browser macOS debugging path.

Correct pixels and a Vulkan backend identify rendering output, not the producer's
transport. Native GL FOREIGN or WebGPU publication requires correlated producer,
compositor acquisition and return evidence. Explicit compositor and native-canvas
snapshots perform observer readback even when native presentation avoids CPU
readback. See the
[Vulkan implementation guide](VulkanWebRenderImplementation.md) for the native
ownership and validation boundaries.
