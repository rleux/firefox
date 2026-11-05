# Vulkan WebRender implementation

The [overview](VulkanWebRenderOverview.md) describes the backend's scope. This page
describes the ownership and observable behavior that changes must preserve.

## Construction and the core boundary

`Device::new` dispatches `GpuBackendConfig::Vulkan` to the core-backed
`RenderDevice`. `device::wgpu::Device` owns the core instance, adapter, device
and queue. Ordinary textures, buffers, views, samplers, bind groups, pipelines,
encoders and command buffers are core objects.

A window surface is created before adapter selection so the selected adapter can
present to the actual native surface. Device construction checks a Vulkan 1.1
instance and physical device, required render/copy formats, and actual surface
capabilities. Optional features are requested only when exposed, including
dual-source blending, float filtering, normalized 16-bit texture formats and
component swizzles. `MAPPABLE_PRIMARY_BUFFERS` is requested only when supported
on integrated or CPU adapters; discrete adapters retain core staging. Shader
passthrough is required. Core validates resource usages and pipeline descriptors;
WR checks the reflected shader interfaces.

The native device-opening callback may enable supported Linux external-memory
and semaphore extensions. Core wraps that opened HAL device using the same
validated features, limits and physical adapter. Enabling those extensions alone
does not implement or authorize a native-image publication.

Core owns ordinary initialization, transitions and submitted resource retention.
WR still owns semantic resource selection, pipeline keys, format/conversion
policy, `TexturePool` and other reuse caches, and frame-to-submission association.
Do not rebuild the old
HAL per-mip tracker or submission keep-lists around core. Raw native work for
interoperability has separate explicit safety obligations.

## Native-window safety

`SurfaceWindow` is an unsafe trait. Its owner must keep the exact native window
and display handles valid and unchanged until the last surface reference is
released. Merely holding an `Rc` does not prove that GTK, another owner or an
external process cannot destroy the native object.

`WrVulkanConfig` carries window/display owners with balanced retain/release
callbacks. `OwnedVulkanConfig` and the Rust bridge preserve those leases.
`WindowSurface` drops the core surface before its native window owner. A retained
display owner supports startup without an attached window and replacement on the
same display. Replacing the display identity is rejected.

The reviewed X11 integration coordinates native-window pause/detach/replacement with
the widget and compositor. Its native-XID replacement tests preserve scene
content across a new XID; they do not merely recreate a new scene with the same
colors.

Before GTK destroys an old XID, `NotifyVulkanXWindowChanged(oldWindow, window)`
must acknowledge retirement. The compositor pauses and waits for render-thread
surface retirement, including initialization work with no established renderer.
IPC failure requires explicit local retirement or confirmed process exit; a live
external process or an arbitrary callback cannot grant safe destruction.
Replacement resumes the retained scene without silently reviving a failed device.

Windows is deliberately gated off by the user-approved policy: retaining a
CompositorWidget does not pin a Win32 child/parent HWND against independent
destruction, and existing DXGI CPU staging requires the D3D11 software compositor.
Android startup remains on OpenGL for SurfaceTexture video. A Windows Rust target
check is useful compile coverage but does not remove either gate.

## Shader artifacts and validation

`webrender_build` defines the shader catalog.
`webrender_shader_build` builds each generated vertex/fragment variant on the
host. glslang compiles and links the GLSL stages for Vulkan 1.1; SPIRV-Tools
validates and reflects them. Binding remapping preserves the WR interface.

The exact remapped SPIR-V words pass `spirv-val` for Vulkan 1.1 and are shipped
unchanged. There is no Naga parse/validate/regenerate round trip, builtin-block
normalization, varying flattening or texel-fetch-offset lowering for Naga.
Reflection checks a single `main` entry point for the expected stage, an audited
capability set (Shader, ImageQuery and DerivativeControl), no SPIR-V extensions,
32-bit interface types, descriptor set zero, the 64-byte projection layout,
read-only storage layouts, vertex declarations, fragment outputs and matching
vertex/fragment locations and interpolation. Sample interpolation is rejected.
Stages are also linked with glslang. Unsupported requirements fail the build;
compiler/validator errors identify the shader, features and stage through its path.
WR projection conversion flips Y for the HAL negative-height viewport and maps
GL depth into [0, 1]. The projection-free `ps_copy` vertex shader applies the same
Vulkan Y flip explicitly. Dense binding remapping is followed by validation and
comparison of every reflected binding and interface against the original.

At runtime, `prepare_shader` calls unsafe core
`Device::create_shader_module_passthrough` with
`ShaderModuleDescriptorPassthrough`, those words and explicit `main` metadata.
Device construction requires and requests `Features::PASSTHROUGH_SHADERS` and
rejects unsupported devices through backend initialization. This is confined to
WebRender built-in shaders; WebGPU content shader handling is unchanged.

Passthrough skips Naga shader validation, shader-interface reflection and runtime
bounds/index/loop instrumentation. WR retains reflected vertex-descriptor checks,
checks varying locations against the actual device limit and storage-stage
requirements against downlevel flags, and builds explicit reflected layouts.
Core still validates those layouts and bindings (including buffer sizes), vertex
buffer descriptors, render formats, resources and commands. Dual-source blending
is checked against the requested device feature. Render-target/blending and
sampling/filtering format checks remain in the draw/texture paths. Core shader
and pipeline errors enter the existing renderer failure path. Host validation
alone does not establish an adapter can create every pipeline or render pixels.

The safety assumption is trusted generated code and WR-owned draw data, rather
than arbitrary client SPIR-V. The current catalog uses texture tables, not runtime
storage arrays. `GpuBufferBuilderImpl` keeps multi-texel records in a row and pads
finished rows; shader addresses come from WR-generated primitive/task records.
Projection bindings cover a single mat4, and the reflected vertex format/stride
must match WR descriptors. Shader local arrays use fixed member accesses or bounded
loops; component-transfer clamps its table index to [0, 255] before its
vector subscript. These remain CPU/shader contracts: `spirv-val` proves SPIR-V validity, not runtime index
safety, and passthrough supplies no Naga guard if those contracts are broken.
Future shader/table changes must audit these assumptions and extend the explicit
requirements checks and pixel coverage.

Tool lookup uses `GLSLANG_VALIDATOR`, `SPIRV_VAL`, `SPIRV_DIS`, configured
fetches, then PATH. glslang remains pinned at 15.1.0; `SPIRV_VAL` must identify
SPIRV-Tools 2026.1 or newer. Older validators can accept the invalid explicit
layouts that the internal-shader regression must reject. These are host tools,
including cross-compilation; the installed browser does not need them.

`webrender_shader_build` defaults to no external compiler feature. Ordinary
standalone default tests/builds remain tool-free, including a copied WR workspace
without Gecko's vendor tree. Explicit `glslang` tests and WR/Wrench `vulkan`
builds opt into the compiler; the optional build dependency disables implicit
default features.

The Gecko-only `gecko-core-shader-tests` feature additionally reads the actual
vendored core timestamp WGSL and enables Naga's WGSL frontend. Its required host
job checks the specialized internal shader for periods 80 and approximately
83.333333, with both DEBUG settings and the core internal-shader emission profile,
using external SPIRV-Tools. It is not enabled by standalone defaults.

The wgpu family remains pinned together at revision
`49325ca0a13bdc0787b1601138096cce743dd29e`. A narrow vendored core WGSL change
returns the shifted Uint96 as an SSA composite instead of a mutable Function
local, preventing invalid layout decorations. Naga sources and core's internal
shader path remain unchanged by passthrough. Because core embeds this shader in
Rust binaries, the landing includes a clobber requirement; a build stamp alone does not prove that
the rebuilt libxul, gfxtest and Rust archives contain the fixed source.

The inherited HAL filter for diagnostic 10684 is not shader-validity evidence.
A filtered driver/layer message cannot substitute for external validation of the
emitted binaries, all-catalog checks or observed pixels.

## Uploads, partial initialization and readback

Ordinary CPU-backed texture uploads copy producer bytes directly into a mapped
core buffer with MAP_WRITE | COPY_SRC usage, then unmap it before recording one
buffer-to-texture copy. Matching BGRA8 page images therefore use one CPU copy and
one GPU copy. Row packing and RGBA/BGRA conversion write straight into that same
mapping. MAP_WRITE selects core's direct mapping path, avoiding the additional
staging-buffer copy used by mapped-at-creation buffers without MAP_WRITE.

Each upload allocates fresh core staging; core retains it through submission.
This avoids persistent mappings and completion waits, but does not provide
Vulkan4's upload-buffer pooling. The renderer selects Immediate uploads so the
shared uploader does not first copy into a CPU PBO. Transfer-buffer compatibility
and readback storage remain separate from this ordinary image path.

Queue buffer writes obey four-byte offset/size alignment; tail padding is owned
and deterministic. Encoder buffer/texture copies use 256-byte destination row
pitches, including narrow and odd-width images. Producer offsets and strides are
validated independently and need not have that alignment. Queue texture writes
have their own rules.

Core owns texture initialization. First partial updates must preserve uploaded
pixels and initialize only what core requires; discarded recordings must not
turn unexecuted writes into content. Independent mips, atlas neighbors, repeated
writes and source-to-native channel conversion retain their observable behavior.

CPU external buffers may be overwritten as soon as the handler unlocks them.
The renderer consumes their RGB or strided YUV bytes into owned staging before
unlock. The focused staging regression poisons producer bytes on unlock and
checks repeated updates, RGBA/BGRA and odd strided three-plane YUV. GPU completion
does not extend the producer's CPU borrow.

Readback uses core copies and mapping/completion callbacks. It removes staging
padding and preserves top-down rows and RGBA/BGRA conversion. A mapping callback,
submission completion and successful presentation are distinct events. Pending
readback must progress while idle, and failure must leave the caller's output
buffer unchanged.

`RenderDevice::capture_external` keeps the core-copy path for COPY_SRC-capable
images. For sampling-only RESOURCE imports, it samples the acquired image's
actual view into an owned, renderable RGBA8 core image using `blit_level`, then
reads that target. The normal and force-opaque views preserve their component
swizzles, including alpha ONE. BGRA conversion and odd-width row padding are
handled by the readback path. Source usage is unchanged: capture does not add
COPY_SRC to an imported image. Acquisition, queued-write collection, submission
and native return use the existing external-access lifecycle.

## Draws and passes

Pipeline selection includes shader, vertex interface, blend/depth state and
attachment formats. Binding groups refer to the intended WR resources; a cache
hit must not reuse the wrong semantic texture or uniform values.

`RenderDevice` defers pass commands until final depth-store behavior is known.
An upload, copy, blit or readback flushes prior pass work before encoding the
intervening operation. The combined instance-upload/draw API copies the draw's
source slice directly into a fresh immutable mapped core buffer, removing the
intermediate CPU vector. The mapping is unmapped before use, with checked
four-byte sizing and zeroed padding. On supported integrated or CPU adapters,
`MAPPABLE_PRIMARY_BUFFERS` allows that buffer to be used directly; other adapters
retain core's staging-to-primary GPU copy. Projection buffers use an immutable
`Arc` reused until the transform changes, so deferred draws retain their original
values. Vulkan disables the renderer's shared-instance-buffer pool policy while
preserving the adapter's actual base-instance capability.

Shared atlas regions use preserving load/store behavior; a subrectangle clear is
not a whole-attachment discard.

Continuous Stretch gradient borders evaluate gradients directly at mapped source
coordinates rather than rendering nine source tasks and filtering those results.
The destination border rendering and antialiasing remain. Duplicate offsets with
different colors, discontinuous repeating/wrap cases and non-unit conic offset
ranges retain the old filtered-source path to preserve their pixels. Gradient
lookup and destination composition remain necessary.

Scissored clears use a draw. Blits and mipmaps use supported sampling/attachment
formats. Attachment-feedback cases use owned scratch storage rather than sampling
an illegally aliased active attachment.

## Surface presentation and snapshots

`Surface` wraps core configure/acquire/present/discard operations and
`SurfaceOptions`. Negotiation selects a directly renderable RGBA/BGRA UNORM
surface in the sRGB color space, a supported alpha mode and present mode.
Transfer-source usage is enabled only for a requested snapshot when supported.

The acquired core texture is the final render target. Its references are released
before presentation/reconfiguration. Detach, pause, replacement and teardown
preserve native-owner lifetime and discard unsubmitted work safely.

| Core status or condition | WR behavior |
| --- | --- |
| `Timeout` | Retry |
| `Occluded`, paused, detached or zero extent | Skip presentation while resource work can continue |
| `Outdated` | Reconfigure and retry |
| `Lost` | Recreate the surface, then retry |
| `Suboptimal` | Present the current frame and reconfigure subsequently |
| Native extent differs from document extent | Report `SizeMismatch`; update the view/target together |
| Validation/device failure | Fail the renderer and enter recovery |

`Renderer::prepare_readback` arms a pending request after resource updates and
before rendering. Beginning the next actual GPU frame consumes that request;
resource-only work does not. Only that frame copies the surface into an owned
snapshot image before present. A later explicit readback copies the snapshot
image into a mapped buffer and removes padding into the caller's output. Ordinary
presentation makes neither capture copy, including after an earlier request;
retaining transfer-source capability does not arm another capture.

Snapshot readiness is invalidated for failed/skipped presentation and surface
replacement. A request consumed by such a frame must be rearmed before retrying;
Wrench does so after its resource updates. A previous frame's capture cannot be
reported as a successful new capture. The boolean success result propagates
through `wr_renderer_readback` and Gecko's GetSnapshot path so failed captures do
not consume stale bytes.

Native-window observation must account for presentation latency. Completing the
rendering submission does not establish that XGetImage already sees the new
frame. The X11 fixture uses bounded strict pixel polling, including after resize;
it does not add production GPU waits.

## Completion and recovery

Core submission indices back `GpuSubmissionStatus`.
`RenderDevice` records the actual core queue tail, including internal work
associated with presenting an otherwise empty frame. Completion publication stays
monotonic when other core polling occurs concurrently. In-flight work is bounded.

`VulkanFrameTracker` associates Gecko frame IDs with submitted indices and retires
them only when completion reaches the relevant index. The compositor timer
continues polling pending frames when no new scene arrives. Its polling method is
not an unconditional frame-end wait.

Core's uncaptured-error and device-loss callbacks record failure centrally.
Shared resource/upload, submit, acquire/present and completion boundaries observe
that state. A returned submission index alone does not prove successful submit.
CPU scene building does not need explicit per-operation device-loss checks.

`Renderer::update` preserves resource-only and idle-completion errors.
`wr_renderer_update` returns failure to `RendererOGL`, which calls Gecko's
existing WebRender error path. Failed/no-frame work must not wait for a future
visible frame to trigger recovery. Validation and device loss remain distinct
error kinds even when either prevents rendering.

## Producer paths and native-image boundary

Current reviewed Gecko paths retain CPU buffers, video fallback and ImageBridge
recovery. WebGPU canvas CPU fallback copies GPU output to a mapped readback buffer,
then copies pixels into the compositor's CPU buffer for upload. Opaque canvases
carry RGBX/BGRX presentation metadata while retaining RGBA/BGRA storage. The
existing CPU copy forces destination alpha to ONE without changing source texture
alpha or adding a pixel pass. This fallback retains GPU readback, the mapped-to-CPU
presentation copy, and the ordinary CPU upload and GPU texture transfer described
above.

Successful ordinary-canvas `Queue.WriteTexture` and `CopyExternalImageToTexture`
enqueues notify their exact destination canvas without a synthetic user submit.
Notifications ignore stale or reconfigured canvas associations; existing
shared-texture modes keep their handling. CPU presentation records the actual
snapshot-source texture only after its internal copy submission is accepted,
allowing snapshots after presentation without a user `QueueSubmit`. Immediate
pre-presentation `drawImage` without an established source remains a separate
limitation. Browser producer pixels must be tested through the active compositor
rather than inferred from the existence of a buffer path.

The reviewed `IsVulkanCompositorRequested` policy combines build platform, shared
runtime-X11 fact, hardware/software selection and opt-in. It also controls GL,
WebGPU and GTK video producer admission/fallback in processes that may not have
GDK. Windows/Android/Wayland remain excluded; ordinary GL/SWGL behavior stays
available.

### Admission and publication

`ExternalTextureRegistry` binds admitted native allocations and views to the
actual core device. Admission validates device identity, dimensions, supported
format/modifier/usage, allocation layout and file descriptors before constructing
the raw import. Aliases share allocation access state. An opaque alias is a view
with alpha ONE; it is not a second independently owned publication.

The producer must fully initialize the admitted image, publish a genuine ready
signal, and keep every alias immutable until the required return condition. A
ready signal is not proof of initialization by itself. The first raw acquire
must complete before wrapping the image as an initialized core texture. Until
then the external-image handler can report `Pending`, withhold the affected
rendered epoch and schedule progress without treating readiness as renderer
failure.

A document awaiting native acquisition needs a real render on retry, preserving
its original present/offscreen intent even when the backend notification requests
update-only work. FIFO-blocked updates remain a separate state. Pending epochs
stay withheld until resolution. The pre-update defers pipeline-info flushing
until path selection; an actual draw flushes it once afterward, preserving
replacement-epoch acknowledgments.

Native sampling follows ordered raw acquire, ordinary core work, and raw release
submissions. The core image is explicitly transitioned to its agreed RESOURCE
state before the raw ownership release. Queue semaphore waits/signals and raw
command-buffer lifetime are tracked at this boundary; ordinary core retention
does not perform producer ownership handoffs.

The GTK GL path uses `SharedSurface_DMABUF`, serialized `DMABufSurfaceRGBA`,
`RenderDMABUFTextureHost` and the renderer registry. It admits only the actual
render node and supported RGB layout, with genuine EGL native-fence support.
The factory fully clears the image while preserving hostile caller GL state.
`FenceSet` flushes GL before duplicating the native-fence FD; FOREIGN publication
does not use `glFinish` as a failure fallback.

Shared DMA-BUF access stores generation and access state atomically. Beginning a
producer write, publishing its next generation and acquiring a consumer lock
check that shared state together. Serialization fails while writing. A retained
descriptor for generation N is rejected during writing and after publication
N+1; the current N+1 descriptor can still render and return normally. The
existing producer/consumer route is parent/GPU-side; this protocol adds no new
sandbox ioctl or sandbox-policy exemption. Unsupported factory admission returns
to the established Basic/CPU-buffer path.

### Native WebGPU canvas publication

The Linux Vulkan producer exports ordinary single-layer RGBA8/BGRA8 canvas
textures with RENDER_ATTACHMENT and COPY_SRC usage. A validated core Load/Store
pass initializes untouched storage before transition to COPY_SRC. A separate
raw-only encoder copies into a fresh exportable DMA-BUF, releases it in GENERAL
to EXTERNAL and signals a timeline ready value around the intended serialized
submission. Core and raw encoding remain separate; submission validity and
semaphore-hook consumption are checked before publication.

Admission checks enabled external-memory/semaphore features, device and driver
UUIDs, actual timeline support, single-memory-plane modifier/format/usage and
allocation bounds, and the active WebRender registry. Opaque publication requires
an alpha-ONE consumer view; the ordinary source and its alpha remain unchanged.
Each admitted presentation uses one GPU export transfer plus WebRender sampling,
with zero presentation CPU readbacks, maps or pixel copies. Ordinary compositor
rendering still occurs. Explicit source reads and snapshots are observer
readbacks and retain their copies.

Exports are immutable and never reused. At most six exports and 256 MiB of actual
allocation bytes may be outstanding. Their leases retain native resources through
consumer return and retire only after both native completion and core callback
delivery; uncertain native errors remain quarantined. Unsupported admission,
disabled no-readback presentation policy, allocation pressure or an older pending
CPU publication use the existing CPU fallback. The older WebGPU
`SharedTextureDMABuf` route remains gated; this producer uses ordinary canvases.
Existing GL, Windows and macOS shared-texture decisions remain unchanged.

Explicit native snapshots read the immutable consumer publication, so destroying
the original source after presentation does not invalidate its snapshot. Renderer
preflight selects a compatible owner before acquisition; sampling state is
restored before external release, and pending acquisition has bounded retry with
receipt progress. Missing or failed renderers return an unavailable snapshot.

### Return, abandonment and migration

`PendingExternalRelease` distinguishes these observable return states:

| State | Meaning |
| --- | --- |
| `Pending` | A safe producer return is not yet available; keep the borrow/pin |
| `Submitted(value)` | A Vulkan EXTERNAL release was successfully queued; its producer must wait for that timeline value before reuse |
| `Complete` | A healthy GL FOREIGN release signal has completed, allowing the GL producer lock to reopen |
| `Abandoned` | The borrow is permanently poisoned; no producer reuse is authorized |

Vulkan EXTERNAL and GL FOREIGN have different return contracts. A queued GL
release, a frame/epoch counter, a core work-done callback, or host destruction
cannot substitute for the actual FOREIGN return signal. GL keeps its shared
access lock and healthy-return global-reference pin until completion. An aborted
borrow has no healthy return job: its C++ global reference may drop immediately,
while access remains poisoned and neither `TryLockForeignRGB` nor ordinary
unlock may reopen producer reuse. Requiring a pin to survive every abort would
be an incorrect test expectation.

The render-thread receipt queue progresses even without a new frame. Renderer
migration retains the old device and pending native work until the return is
safe; it cannot release the producer merely because a new renderer exists.
Abandoned or uncertain raw work is drained or quarantined by the native-only
retirement path. This lifetime bookkeeping is confined to interop allocations,
raw commands and synchronization objects, rather than ordinary core resources.

Generic Vulkan EXTERNAL fixtures establish the publisher protocol. The separate
native WebGPU browser checks below establish admitted ordinary-canvas production;
the CPU fallback remains required. GL FOREIGN browser transport still needs its
own directly correlated publication/acquire/return evidence.

## Cached backend diagnostics

At `WebRenderAPI` construction, `wr_renderer_get_backend_info` reads the actual
`Renderer::get_graphics_api_info`; the constructor also records the actual texture
limit and owning Parent/GPU process. SWGL and OpenGL ES use the corresponding
actual compositor/context override. The immutable owned `mBackendInfo` JSON is
copied by `WebRenderAPI::Clone`, alongside the actual per-session
`mUsesBackendPresentation` value.

`PWebRenderBridge::GetBackendInfo` returns that cache only for an initialized,
live API. `nsIDOMWindowUtils.getWebRenderBackendInfo()` exposes it asynchronously
to privileged callers; closed, missing or uninitialized bridges report
`NS_ERROR_NOT_AVAILABLE`. Closed windows fail before creating a Promise in their
closed global. Neither the widget lookup nor the bridge handler creates a
compositor to answer the query.

`Troubleshoot.snapshot()` includes the optional graphics `webRenderBackend`
object, and `about:support` displays it in the WebRender Backend row. The object
has `backend`, `renderer`, `driver`, `maxTextureSize` and `process` fields. It does
not contain a PID or native handles. Runtime backend logs separately identify the
renderer process PID for correlation with process maps. A requested Vulkan
preference or a global flag is not a replacement for these per-API facts.

## Development and verification

From `gfx/wr`, standalone default host tests do not invoke external shader tools:

```sh
cargo test -p webrender_shader_build --lib
```

With glslang 15.1.0 and SPIRV-Tools 2026.1+ configured, explicit shader/Vulkan
operations are:

```sh
cargo test -p webrender_shader_build --features glslang --lib
WR_SHADER_COMPILER=glslang cargo build -p wrench --features vulkan
```

Only inside Gecko, run the actual core-shader regression explicitly:

```sh
cargo test -p webrender_shader_build --features gecko-core-shader-tests --lib
```

Use the current Wrench flags, from `gfx/wr/wrench` after that Vulkan build:

```sh
../target/debug/wrench --vulkan --vulkan-validation test_init
../target/debug/wrench --vulkan --vulkan-validation test_invalidation
../target/debug/wrench --vulkan --vulkan-validation show reftests/clip/clip-between-picclip-and-lca.yaml
```

A custom `CARGO_TARGET_DIR` changes the executable path. `--vulkan` uses a
native X11 window and conflicts with `--headless`; do not route it through the
OSMesa headless wrapper. Headless GL/SWGL reftests remain separate reference checks.
Wrench invalidation checks cache metadata and cannot replace pixel checks.

GTK exposes the separate startup probe. From the actual object directory:

```sh
./dist/bin/gfxtest vulkan --webrender
```

Use the real driver/display environment. Read
`VULKAN_WEBRENDER` and the diagnostic fields; exit zero alone is not a positive
capability result. The Vulkan video probe is a separate contract.

Native import validation requires the audited Khronos layer 1.4.341 with
synchronization validation. Select its existing audited directory explicitly
before the native commands below and verify the loaded library and version, not only the manifest:

```sh
export VK_LAYER_PATH=/absolute/path/to/audited-khronos-1.4.341
export VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation
export VK_LAYER_VALIDATE_SYNC=1
```

Do not silently use the system default. Intel imports crashed with system layer
1.3.275 in both existing and new tests; both passed with 1.4.341. The observed
version dependency does not establish a driver or layer root cause. Keep raw
VUID/SYNC-HAZARD logs; inherited message filters are not independent proof.

For browser pixel evidence, set `remote.screenshot.use_readback=true`, capture
in content scope on a real display, and verify the active Vulkan backend.
Headless Firefox cannot establish hardware WR output. Readback forces a new frame,
so native X11 observations separately test retained scene/restoration behavior.
See [WebRender screenshot debugging](DebuggingWebRenderScreenshots.md).

Focused Rust hardware tests are ignored by default. With a Vulkan device,
Khronos validation and the shader tools configured, these commands from `gfx/wr`
exercise CPU staging, native X11 presentation and native-canvas snapshots/retry:

```sh
cargo test -p webrender --features vulkan,capture,debugger --lib \
  vulkan_renderer_stages_external_rgb_and_video_bytes_before_unlock \
  -- --ignored --test-threads=1 --nocapture
cargo test -p webrender --features vulkan,capture,debugger --lib \
  native_surface_direct_output_discard_resize_and_snapshot \
  -- --ignored --test-threads=1 --nocapture
cargo test -p webrender --features vulkan,capture,debugger --lib \
  vulkan_native_canvas_snapshot_preserves_alpha_and_restores_sampling_state \
  -- --ignored --test-threads=1 --nocapture
cargo test -p webrender --features vulkan,capture,debugger --lib \
  vulkan_native_pending_document_survives_backend_noop_and_retains_epoch \
  -- --ignored --test-threads=1 --nocapture
```

`renderer::vulkan_tests::native_images` covers actual-renderer EXTERNAL imports,
aliases, capture, Pending epochs, idle return, renderer migration and logical
loss. Its ignored tests require native DMA-BUF/timeline capabilities, rather
than treating missing capabilities as positive results. The ignored
`vulkan_renderer_foreign_gl_reuses_only_after_complete` test additionally requires
an inherited genuine GL producer socket and fence FDs (`WR_FOREIGN_SOCKET_FD`
and `WR_FOREIGN_FAILURE`). Running its filter without that producer is not a
self-contained GL publication test.

The C++ `VulkanForeignImages` fixture exercises the actual shared-surface factory,
serialized/imported DMA-BUF host, renderer sampling/capture, render-thread receipt
queue, lock and global-reference return. From the repository root, on a machine
expected to admit the native path:

```sh
WR_NATIVE_IMAGE_TEST=1 WR_GL_FOREIGN_EXPECT_NATIVE=1 \
  ./mach gtest 'VulkanForeignImages.*'
```

The expected-native value must match the real device's supported factory path.
Use `WR_GL_FOREIGN_EXPECT_NATIVE=0` for the NVIDIA negative-admission/Basic
fallback case; that result is not a native positive. The three maintained cases
are `FactoryInitializationAndCompletedProducerReuse`,
`OldDeviceMigrationWaitsForComplete` and `AbandonedBorrowNeverReopensProducer`.
These deliberately configured gtests do not establish normal-browser selection,
sandbox/IPC transport or window presentation.

The diagnostics and existing schema checks can run without a hardware display:

```sh
./mach mochitest --flavor browser --headless \
  toolkit/content/tests/browser/browser_aboutsupport_webrender.js \
  toolkit/modules/tests/browser/browser_Troubleshoot.js
```

That headless run checks SWGL. Repeat the backend test on a real display with
the startup Vulkan preference set or unset to check the created API, rather than
asserting that the requested preference chose a particular driver.

### Passthrough follow-up coverage

The follow-up validated all 40 catalog variants / 80 stages for Vulkan 1.1 with
glslang 15.1.0 and SPIRV-Tools 2026.1. Fresh compilation reproduced every shipped
word except the explicitly remapped descriptor-binding operands. On NVIDIA RTX
5060 Ti / driver 615.71.09, all 80 depth/no-depth catalog pipelines were created,
and 79 focused native core/renderer checks passed, including clip, blend, filter,
glyph, RGB/YUV, mipmap, capture and presentation pixels. The ordinary Rust unit
run passed 201 tests (57 hardware tests ignored); those focused native checks
separately ran their ignored fixtures.

Wrench initialization, invalidation, PNG rendering and a strict asymmetric
3,072-pixel X11 observation with normal window-close exit passed on an Xvfb
surface using the NVIDIA Vulkan device. Khronos layer 1.4.341 reported no VUID or
SYNC-HAZARD messages. This is native Vulkan rendering to an X11 test surface,
not evidence of physical display scanout. Default host tests also passed with
shader-tool paths deliberately unavailable. The Gecko-only timestamp regression
passed with the fixed internal WGSL.

Firefox and standalone Cargo graphs no longer enable Naga SPIR-V input. The
newly vendored petgraph 0.8.3 / fixedbitset 0.5.7 and their lock entries were
removed. Standalone lalrpop still needs petgraph 0.6.3 / fixedbitset 0.4.2; those
lock entries remain. Naga, its output backends and other Firefox shader consumers
remain in use. Earlier browser/package evidence below describes the old shader
route and does not certify a rebuilt passthrough browser or release archive.

### Native WebGPU canvas coverage

Physical Intel/X11 browser probes passed 10,500 analytic pixels in each main
native/forced-CPU run and 2,500 in each queue-only native/forced-CPU run, with
source-byte and snapshot checks and retained output after producer destruction.
The queue-only first capture and snapshots preceded any user QueueSubmit, covering
both RGBA and BGRA. Every native publication ID had correlated compositor
acquisition and Submitted return, with no native-ID overlap in CPU-copy callbacks;
forced-CPU runs published no native IDs. Intentional fallback cases remain part of
the native runs. These source-bound observations establish admitted transport and
pixels, without a performance or reusable-allocation claim.

Native snapshot, protocol, capture, gradient and export-lifetime checks passed on
NVIDIA. The pending-document retry and replacement-epoch regression passed in
both present and offscreen modes on NVIDIA and physical Intel; Intel also passed
the existing snapshot test with audited Khronos 1.4.341 synchronization validation.
Dedicated C++ gtests were unavailable in the test-disabled verification build.
The browser/runtime evidence carries to the accepted source with byte-identical
production files; the final change touched only a hardware-test fixture.

### Established coverage

The following observations predate the SPIR-V passthrough follow-up and used
the earlier Naga shader path. They came from distinct source-bound build/test stages;
they do not mean every test ran on every device at the final revision. Preserve
source inputs, executable hashes, tool/layer identity, raw failures and normal
exit diagnostics when extending this coverage. A build stamp or a selected layer
manifest alone does not establish which binary or layer was executed.

| Check | Observed coverage | Boundary |
| --- | --- | --- |
| Core and renderer fixtures | Uploads, partial initialization, passes, YUV, completion/recovery, capture, surface and native protocols on NVIDIA; focused Intel core/renderer checks | Hardware-specific ignored tests need the stated capabilities |
| Genuine Intel GL publisher into Rust Renderer | Both RGB fourccs, 16 publication cycles, 4,896 framebuffer pixels and 9,792 exact direct-capture pixels, normal/opaque views, RESOURCE-only capture, queued writes and Complete/reuse | Separate genuine producer fixture, not a native browser WebGL transport claim |
| Generation/fence-fixed C++ fixtures | Three native-positive Intel FOREIGN cases; 32 CPU/NVIDIA tests including required negative factory admission | Abandonment requires permanent poison/nonreuse; a healthy global-reference pin is not required to survive an aborted borrow |
| Backend diagnostics | SWGL in Parent; OpenGL llvmpipe/Mesa in GPU; NVIDIA RTX 5060 Ti Vulkan driver 615.71.09 in GPU | OpenGL coverage here is software-driver GL, not NVIDIA hardware GL |
| Final normal Intel browser | Iris Xe/Mesa 26.2.6, first compositor capture before producer references, 35,934 strict RGB/WebGPU interior pixels plus five points, direct native X11 points, cached API/Troubleshoot/about:support consistency, normal exit | Actual native browser GL FOREIGN publication remains unproven |

The final Intel probe used device scale 1.25 and modern Khronos layer 1.4.341,
including synchronization validation through normal shutdown, with no VUID or
SYNC-HAZARD messages. Its first capture was ready without another attempt. All
8,168 runtime inputs and 14 diagnostic source inputs were unchanged across the
run. Three disjoint flat video patches supplied 27 reference-selected samples at
unchanged tolerance 8. The original gradient-edge single-pixel mismatch remains
recorded separately: choosing stable DPR-aligned interiors does not make that
old assertion pass. Expected patches were selected from the CPU reference only,
before inspecting actual compositor output.

Host/public-FFI Pending behavior and epoch withholding are tested. There is no
live `CompositorBridgeParent` notification-injection test. Native browser GL
transport needs directly correlated publication/acquire/Complete evidence;
pixels, cached Vulkan facts and unrelated DMA-BUF/fence logs are insufficient.

Native failure tests use validation failures or logical device destruction.
They do not establish physical GPU-reset recovery or performance results.

Windows and Android remain startup-gated as described above. No Windows/Android
runtime, physical GPU reset or performance acceptance is implied by the Linux
fixtures or cross-target Rust compile checks.
