# Vulkan WebRender: architecture and implementation

This document describes the experimental implementation at `a3a23237cc0` on
`vulkan1`, based on Mozilla `main` at `f9a73919fe0`. The
[overview](VulkanWebRenderOverview.md) is the short introduction for discussion.
The source links below pin that implementation, rather than following a moving
branch. Older stage reports describe earlier implementations and are evidence
for the runs they record, not a specification of today's call path.

## Scope and platform policy

Firefox selection is currently Linux/GTK/X11 only. `gfxPlatform` requires hardware
WebRender, the startup preference `gfx.webrender.vulkan`, and an X11 display before
setting `UseWebRenderVulkan`. The preference defaults to `false`. Raw Wayland
surface support in lower layers does not establish a supported Firefox Wayland
route.

The standalone HAL code also contains Metal and platform-specific Vulkan import
adapters. Linux compile checks or common API tests do not establish native macOS,
Windows, or Android acceptance. The Firefox bindings enable the Vulkan feature
set on Linux; they do not enable a Metal browser compositor. D3D12 is not a
completed renderer backend. VA-API remains the video decoder in the Linux path;
Vulkan Video decoding is not implemented by this integration.

Explicit standalone HAL selection fails if its requested backend cannot be
created. Gecko separately retains its renderer recovery and software-fallback
policy. A failed Vulkan renderer must not continue submitting with stale state.

## Shared renderer boundary

Mozilla's device-layer split in [bug 2072784](https://bugzilla.mozilla.org/show_bug.cgi?id=2072784)
is the integration boundary. `GpuBackendConfig::Hal` supplies a boxed
`GpuBackend` implementation to normal WebRender initialization. The branch's
`device/gl.rs` and `renderer/init.rs` match the base revision; Vulkan-specific
behavior is adapted around the shared renderer.

| Layer | Responsibility | Main source entry points |
| --- | --- | --- |
| Gecko selection and window compositor | Select Vulkan, expose native window handles, service begin/end frame, visibility, completion, pause, and loss. | `gfxPlatform`, `RenderCompositorVulkan` |
| Browser Rust wrapper | Bridge Gecko frame IDs, external-image handlers, screenshots, recording, and presentation to the HAL facade. | `webrender_bindings::renderer::Renderer`, `VulkanRenderer` |
| HAL facade | Consume publications in order, preserve frame readiness and retained-output rules, and expose platform/capture APIs. | `renderer::hal::RendererCore`, `GpuRenderer` |
| Shared WebRender renderer | Update texture caches, process render tasks, batch and issue draws, and implement compositor orchestration. | `crate::Renderer`, `render_impl` |
| GPU adapter | Implement handles, uploads, state, draw calls, render passes, fences, and readback in the shared device contract. | `HalGpuBackend<A>` |
| HAL resources and executor | Own native resources, shader/pipeline caches, command recording, barriers, submission retirement, and surfaces. | `FrameRenderer<A>`, `SubmissionQueue<A>`, `Texture<A>`, `Buffer<A>` |

`GpuRenderer::new` constructs the shared renderer with `HalGpuBackend`.
`render_document` configures external-image metadata, output origin and damage,
then calls shared `Renderer::render_impl`. `FrameRenderer` remains the owner of
HAL resources and execution helpers. Earlier scene-walking code and helpers still
exist in that module; their presence does not mean the browser's ordinary frame
path bypasses the shared renderer. Removing obsolete paths is a possible cleanup
for an upstream patch series.

See the [GPU adapter][gpu-backend], [HAL facade][hal-facade],
[shared-renderer wrapper][gpu-renderer], and [Gecko wrapper][browser-renderer].

## Frame and completion flow

1. WebRender's render backend publishes a document and its resource updates.
   The HAL facade consumes messages in FIFO order. Required cache work from a
   superseded document is rendered before later updates can free its resources.
2. Resource updates are queued in the shared renderer. Resource-only messages
   explicitly flush texture-cache and native-surface updates, including when no
   visible frame follows.
3. Gecko requests a frame. The wrapper polls completion, handles dimensions and
   surface state, and the shared renderer executes offscreen and picture-cache
   passes before final composition.
4. The HAL adapter translates draw state and records native commands. Drawing,
   uploads and transfers use ordered submission queues. A presentable owned
   output is copied or drawn into the acquired swapchain image.
5. Pending Gecko frame records track drawing and external-image ownership-return
   completion. Polling advances completed frame IDs only when the relevant work
   has completed, releasing publications and resources accordingly.

The draw queue, external-image queue and browser pending-frame list each allow
at most three outstanding entries. Pressure introduces bounded waiting rather
than unbounded allocation. The browser completion timer polls pending work at
2 ms intervals; it is not an unconditional frame-end GPU wait. Idle completion
must make progress even if the page stops producing frames. Diagnostic synchronous
controls remain available for comparison.

`FrameTexturesUpdated` is a CPU-source lifetime checkpoint: it is delivered after
the shared renderer has consumed the uploads, not when the facade merely queues
them. GPU submission completion is a separate condition. Explicit readbacks wait
or poll for completion; they are not part of normal frame production.

The facade's `render_if_needed` can return rendered, reused, or skipped output.
Reuse requires valid output with matching document, dimensions, origin, clear
color and surface generation. A later no-render notification cannot cancel an
earlier pending render request. Gecko still decides whether to request rendering;
the backend does not infer frame equivalence from DOM or display-list equality.

## Resources, render passes, and failure semantics

`HalGpuBackend` maps the shared renderer's texture, buffer, program and framebuffer
handles to owned HAL resources. It records instances and texture bindings, groups
compatible pending draws, and flushes them before operations that change resource
contents or lifetime. The adapter converts GL-style clip coordinates to the HAL
Y orientation and depth range. Native compositor targets also carry viewport
origins and bounds.

Resources retain their creating device. Texture state tracks initialization and
usage per mip; transitions generate barriers, and submission-owned references
keep native allocations, views, buffers, descriptors and leases alive through GPU
completion. The implementation separately records committed state and planned
state. Failure poisons the renderer/device rather than treating discarded planned
commands as completed work.

Pipeline identity includes device, backend, shader input, ABI, filtering policy,
vertex layout, blend/depth state, and target formats. Descriptor identity includes
the actual allocation/view and sampler, not just a reusable renderer handle.
Upload and intermediate-texture pools each cache at most 64 MiB, with limits of
256 buffers and 128 textures; busy or oversized allocations do not become freely
reusable cache entries. Other rendering caches are bounded as well.

Three shared-renderer contracts have explicit regressions:

- **Preserve shared atlases.** An unspecified render area may describe cached
  border work sharing an atlas with images. `LoadOp::DontCare` does not authorize
  clearing that entire allocation. The adapter discards contents only for an
  explicit full-target area, subject to the output-damage guards.
- **Clear stale bindings on invalidation.** WebRender leaves unused samplers
  unchanged for `TextureSource::Invalid`. Invalidating a target removes its
  adapter bindings, so later unclipped text cannot inherit an invalidated mask.
  Explicitly binding and sampling uninitialized contents remains an error.
- **Notify after uploads.** Texture completion requests go to the shared renderer
  while uploads are pending, preventing early release of Gecko image sources.

These are implemented in `invalidate_render_target`, `begin_render_pass`, and
`GpuRenderer::notify_texture_update`. The regression tests are
`cached_target_clear_preserves_other_images`, `text_after_invalidated_clip_mask`,
and `texture_checkpoint_waits_for_uploads`.

See [resource ownership][resources] and [submission retirement][submissions].

## Shader build and execution

The HAL build derives a bounded shader catalog from WebRender's existing GLSL
and feature combinations. It assigns native texture/sampler bindings, explicit
locations and projection storage, then compiles and validates SPIR-V. Reflection
checks descriptor types, stages and vertex interfaces against WebRender's vertex
descriptors. Shader bytes and metadata are embedded; users do not need shader
compiler executables at browser runtime.

Native SPIR-V is the default Vulkan shader input. `WR_HAL_SHADER_INPUT=naga`
selects the optional translated route in builds enabling `hal-naga`. The Naga
translation layer normalizes supported SPIR-V constructs and validates the IR;
the Metal route uses translated shaders. This is not a claim of pixel equivalence
or native platform acceptance for every shader/backend combination.

Frame-table storage buffers retain WebRender's layouts and indexing conventions.
`gpu_backend/tables.rs` keeps CPU mirrors of table updates, selects storage buffers
when device limits permit, and otherwise uploads equivalent data textures.
Buffer references are released at frame boundaries; submission retention controls
when backing storage can actually be reused.

Standard native filtering is the default. The optional legacy brilinear profile
changes only audited mipmapped image sampling and participates in shader/pipeline
and capture identity. Existing Wrench tolerances have not been relaxed to hide
filtering differences; full reference-image acceptance remains open.

Build/dependency implications:

- The integration uses `wgpu-hal`, `wgpu-types` and Naga from wgpu revision
  `4f4dc63098fae64a0e6d1d3dc6f4f328da8fd5c8` (30.0.0). The standalone workspace
  resolves that source through Firefox's vendored crates.
- Linux Firefox enables `hal-naga` and `hal-linux-dmabuf`. Enabling the preference
  is distinct from compiling that code: the branch's Linux build requires shader
  tools even when Vulkan is not selected at runtime.
- Configure checks require `glslangValidator` 15.1.0 or newer and SPIR-V Tools
  2025.1 or newer (`spirv-val`, `spirv-dis`). Standalone GL-only builds do not
  generate the HAL shader catalog.
- Naga's optional `spv-in` parser brings in `petgraph` to order shader functions
  and reject call cycles. It is not WebRender's render-task graph implementation.
  Native SPIR-V execution does not itself require parsing shaders through Naga.

See the [shader translation code][translation], [feature definitions][features],
and [configure checks][shader-tools].

## External images and producer reuse

`ExternalImageProvider` returns an `ExternalImageLease` describing either CPU
bytes or a native image. The adapter presents this through the shared renderer's
`ExternalImageHandler`. Native handles are registered per device; leases preserve
the publication through recorded GPU reads, including repeated channel locks.
Unlock restores the required usage and initiates release handling. Logical
unlock, allocation lifetime and permission for the producer to overwrite are
different events.

Admission checks depend on the producer, including device/driver or DRM identity,
format and modifier, dimensions, plane layout, memory bounds, generation and
shared access-handle identity. Import-cache entries use publication identity and
retain pending ownership-return state even after weak image references expire.
Completion permits reuse; failed or uncertain ownership return abandons the
publication rather than recycling it.

| Producer | Handoff into Vulkan WebRender | Copies and synchronization that remain |
| --- | --- | --- |
| CPU/Canvas2D images | Owned byte lease, then shared renderer texture upload. | CPU snapshot/staging and GPU upload; not zero-copy. |
| WebGPU | Same-device/driver RGBA8 or BGRA8 DMA-BUF with an acquire sync-file; direct sampling when the exact tuple is supported, otherwise the copy route. | A producer-side export copy remains. Separate Vulkan logical devices transfer `EXTERNAL` ownership. |
| WebGL | Supported GL/EGL linear RGB DMA-BUF, native acquire fence and direct sampling. | Producer resolve/presentation work remains. GL/Vulkan transfers use `FOREIGN_EXT` ownership. |
| VA-API video | Supported same-device NV12/P010 DMA-BUF with shared plane views and publication lifetime. | Decoder readiness synchronization remains; explicit snapshots/readbacks copy. Transfer uses the foreign-producer contract. |

For asynchronous paths, the producer wait and acquire barriers are submitted to
the consumer queue before sampling. After the last lease and GPU read complete,
ownership return is submitted; the release callback keeps the host publication
and its access lock alive until that return completes. Conditional reuse waits
and queue-pressure waits remain. The WebGPU producer has its own export limit;
it must not be confused with the three-entry submission limits or applied to the
WebGL swap chain.

NV12 uses R8/RG8 views; P010 uses R16/RG16 views over the imported image and
shared memory/layout state. The browser admits a restricted, capability-probed
set of even-sized, one-object/two-plane layouts and SDR color metadata. Separate
objects, protected frames, cross-device imports and HDR are not established
support. Decoder-frame retention prevents reuse while either plane or GPU work
still needs the publication. Unsupported tuples follow the existing negotiated
fallback path.

The CPU provider retains a bounded owned snapshot cache. The current
`ExternalImageHandler::lock` path acquires a lease and returns its byte snapshot
as `RawData`; shared uploads then stage those bytes. An earlier optimization
report measured a direct `with_buffer` callback path. Its measured copy savings
must not be assumed to survive this adapter unchanged.

See the [external-image bridge][external-bridge], [browser image provider][image-provider],
[WebGPU contract][webgpu-contract], [WebGL contract][webgl-contract], and
[video contract][video-contract] for supported tuples, controls and fixture details.

## Composition, presentation, and visibility

The Firefox window constructor currently selects the HAL Draw compositor. The
adapter also bridges HAL Native and Layer callbacks to shared WebRender
compositor interfaces. Their targets carry leases, bounds and return usage;
their existence does not mean Firefox uses a platform-native layered compositor
for the Vulkan window path.

Final output can be retained and partially recomposed when document, geometry,
surface generation and composition metadata match and damage is valid. The
guarded subset excludes deferred external images and external compositor
surfaces. Unknown damage, forced redraw, incompatible metadata, or unsupported
compositor modes cause full composition. Contributors intersecting damage are
composited in order; untouched pixels remain intact.

Presentation keeps separate output-version history for native swapchain images.
For a known image it unions damage since that image's last version, rather than
assuming consecutive frames acquire the same storage. History is bounded to 32
output changes and 16 images. Unknown images, expired history, reconfiguration,
discard, resize, loss or suboptimal presentation require full updates. The
preservation contract requires swapchain clipping to be disabled. An unchanged
image still needs acquire/present synchronization even if it needs no pixel copy.

X11 visibility handling distinguishes unmapped/unviewable or WM-hidden windows
from visible/unknown state. Focus or simple occlusion is not proof that a window
is hidden. Hidden-window work suppression must still service pending completion
and resource release. Driver teardown may block despite bounded application
queues; asynchronous submission is not a guarantee of wait-free execution.

See [window integration][window-compositor], [compositor adapters][compositor-adapter],
and the [retained-output and presentation contracts][rendering-contract].

## Capture, diagnostics, and validation

Readback and capture are explicit operations. Asynchronous readback retains the
requested frame and supports polling/cancellation; convenience APIs can block.
Captured native images become bytes in the capture, not portable OS handles.
HAL captures record backend, source/shader identity, ABI, shader route and
filtering metadata. Replay rejects incompatible identities before replacing GPU
caches. Rebuilding a scene for another API is different from replaying an already
built frame.

`about:support` and `getWebRenderBackendInfo` expose the actual renderer and
process. Useful opt-in diagnostics include `MOZ_WR_VULKAN_VALIDATION`,
`WR_HAL_RENDER_METRICS`, `WR_HAL_SHADER_INPUT`, and the synchronization controls
described in the interoperation reports. Counter deltas describe recorded work,
not GPU duration or scanout latency. Allocation preferences with both Vulkan
dedicated-allocation flags false are debug information, not graphics-critical
failures; actual allocation/import failures remain errors.

For browser pixel checks use `remote.screenshot.use_readback=true` and a visible
hardware-rendered window. Ordinary automation screenshots can use software
`drawSnapshot`; headless mode does not establish hardware WebRender correctness.
A readback itself can force a fresh frame, so retained/damage behavior also needs
direct observation and focused tests. See
[WebRender screenshot debugging](DebuggingWebRenderScreenshots.md).

Recent local checks on the shared-renderer port include:

- A full Firefox build and 71 Vulkan library tests, including the atlas,
  checkpoint and stale clip-mask regressions.
- Twelve Wrench HAL tests, including retained updates, compositor adapters,
  capture/replay and resource reuse. These are not the complete Wrench reftest
  suite.
- Browser startup-image checks and a clipped-text case that reproduced SWGL
  fallback before the fix and stayed on Vulkan afterward.
- Twenty-four WebGPU browser cases on each direct/copy path. OceanDemo completed
  1,760 presentations with the system loader and 726 with local Vulkan validation,
  without graphics errors or renderer resets. These are correctness observations,
  not performance measurements.

Earlier stage records cover WebGL, NV12/P010, reader interoperability, GPU/parent
processes and failure injection. Do not treat that historical coverage as a fresh
run of every scenario on the current port. Recent local evidence is in
`artifacts/vulkan1-port/`; artifacts are not committed and must be supplied
separately if reviewers need the raw logs.

## Existing performance records

No benchmarks were run for this document. The following numbers are transcribed
from the committed reports linked here. They describe preserved earlier binaries,
mostly Intel Iris Xe RPL-P/Mesa 26.2.2 on native X11. Their runtime identities,
measurement methods and caveats belong to each record. In particular, there is
no post-port timing result establishing the performance of `HalGpuBackend` plus
the shared renderer.

| Record | Comparison and result | Limits |
| --- | --- | --- |
| [Renderer baseline and optimization series][renderer-measurements] | Stage 9.4a: four GL/Vulkan pairs per browser workload. Vulkan Firefox CPU was +34.8% for CSS transforms, +20.6% for small dirty updates, +17.4% for scrolling, +97.9% for software Canvas2D, and +38.5% for filters. Static direction was mixed. | Historical renderer; refresh-capped callbacks are not completed GPU frames. Wrench roundtrip results additionally include synchronous readback. |
| Same record, combined upload optimization comparison | Four Vulkan Canvas pairs: median GPU-process CPU -26.71%, whole-Firefox CPU -22.16%, versus the preserved snapshot-cache build. | Combined effect, not per-change attribution. Two GL references still used less CPU: 0.2922 versus 0.3917 whole-Firefox CPU-s/s, a descriptive +34.05% Vulkan excess, not a balanced GL/Vulkan trial. |
| [WebGL asynchronous transfer][webgl-measurements] | Four same-binary sync/async pairs, 6,000 frames each: median Firefox CPU -1.13 seconds, about -5.1%; unchanged cadence. | No consistent GPU-cycle or memory gain. Producer resolve work remains. |
| [WebGPU direct sampling][webgpu-measurements] | Native X11 W3, four same-binary copy/direct pairs: direct had about -10.1% tracked render-engine cycles and +12.6% Firefox CPU; equal 60 Hz cadence. | This predates W4 asynchronous completion. The producer export copy remains. It does not measure uncapped throughput or current-port behavior. |
| [VA-API asynchronous transfer][video-measurements] | Four same-binary pairs, 100 seconds each: median Firefox CPU +1.975 seconds, approximately +3.1%; zero reported dropped frames in both modes. | Three of four CPU changes were increases, with substantial desktop background activity. No throughput or consistent GPU-work gain established. |

The earlier snapshot-cache experiment separately reported -14.78% GPU-process
CPU and -12.25% whole-Firefox CPU for Canvas. It has its own baseline; those
percentages must not be added to the later four-change result. Similarly, the
documented partial-composition and presentation-area reductions count pixels
processed, not elapsed time.

The measurement reports name raw artifact directories and preserve experiment
details. Raw Stage 9 artifacts were not available in this checkout when these
documents were written, so those numbers are report-based, not a new audit of
the raw samples. The checked-in records remain the public provenance; raw logs
should accompany any stronger performance claim in review.

## Review and remaining work

The implementation demonstrates an end-to-end Linux/X11 route and targeted
interoperation, while leaving important acceptance work open:

1. Review the device contract and remaining HAL facade/executor duplication
   against Mozilla's evolving shared renderer.
2. Audit ownership transfer, cancellation, pending callbacks, decoder reuse and
   failure paths across the C++/Rust boundary; broaden native driver coverage.
3. Complete Vulkan Wrench pixel acceptance without relaxing reference tolerances,
   and validate other native platforms before claiming their support.
4. Reassess current-port CPU upload/copy behavior and run a separately authorized
   performance comparison before assigning historical gains to it.
5. Review the unconditional Linux build footprint, shader-tool bootstrap,
   optional Naga dependency, and HAL/vendor changes; split the combined feature
   into independently reviewable patches before an upstream submission.

No default enablement, all-platform support, end-to-end zero-copy pipeline, or
general performance improvement is claimed by this document.

## Source and measurement references

The following links pin the documented snapshot in the development repository.
Source names in this document are relative to the Firefox repository.

- [GPU adapter and regression tests][gpu-backend]
- [HAL facade and publication handling][hal-facade]
- [Shared-renderer wrapper][gpu-renderer]
- [Native resources][resources] and [submission queues][submissions]
- [Browser renderer bridge][browser-renderer] and [native window compositor][window-compositor]
- [External-image adapter][external-bridge] and [browser image provider][image-provider]
- [Native/Layer compositor bridge][compositor-adapter]
- [Shader translation][translation], [features][features], and [shader-tool configuration][shader-tools]
- [Renderer measurement record][renderer-measurements]
- [WebGPU contract][webgpu-contract] and [measurements][webgpu-measurements]
- [WebGL contract][webgl-contract] and [measurements][webgl-measurements]
- [Video contract][video-contract] and [measurements][video-measurements]
- [Retained rendering and presentation details][rendering-contract]

[gpu-backend]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/wr/webrender/src/device/hal/render/gpu_backend.rs
[hal-facade]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/wr/webrender/src/renderer/hal.rs
[gpu-renderer]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/wr/webrender/src/renderer/hal/gpu_renderer.rs
[resources]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/wr/webrender/src/device/hal/resources.rs
[submissions]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/wr/webrender/src/device/hal/submission.rs
[browser-renderer]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/src/renderer.rs
[window-compositor]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/RenderCompositorVulkan.cpp
[external-bridge]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/wr/webrender/src/device/hal/render/gpu_backend/external.rs
[image-provider]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/src/hal_image.rs
[compositor-adapter]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/wr/webrender/src/device/hal/render/gpu_backend/compositor.rs
[translation]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/wr/webrender_build/src/hal/translate.rs
[features]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/wr/webrender/Cargo.toml
[shader-tools]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/build/moz.configure/webrender.configure
[renderer-measurements]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/tests/RendererBenchmark.md
[webgpu-contract]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/tests/WebGPUDMABuf.md
[webgpu-measurements]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/tests/WebGPUDMABufMeasurements.md
[webgl-contract]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/tests/WebGLDMABuf.md
[webgl-measurements]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/tests/WebGLDMABufMeasurements.md
[video-contract]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/tests/NativeVideo.md
[video-measurements]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/tests/NativeVideoMeasurements.md
[rendering-contract]: https://github.com/rleux/firefox/blob/a3a23237cc04250a5422602558b1c0dad317e8c4/gfx/webrender_bindings/tests/VulkanRendering.md
