# Core-owned WebGPU canvas images on shared Vulkan

The default-off `gfx.webrender.vulkan.shared-webgpu` startup gate selects separate
WebGPU and WebRender core devices over one native Vulkan device. This path keeps
the original canvas VkImage from normal core allocation through WR composition.
It does not export/import memory, use a DMA-BUF modifier, transfer EXTERNAL or
FOREIGN queue-family ownership, or copy the canvas into a transport image.
Normal WebGPU commands retain their core validation, initialization, recording,
submission and callback paths. WR still uses its glslang/SPIR-V passthrough
pipelines and samples the finished canvas through normal external-image drawing.

## Why the canvas remains an independent image

Sharing the original canvas removes transport allocation and copying. It does
not make application commands interchangeable with a draw into the final page
target. CSS opacity, masks, blending and filters apply to the finished canvas as
a group. Applying them to individual application draws changes overlapping
pixels. CSS scaling, transforms and clipping must not change application fragment
coordinates, derivatives, side effects or query results. Snapshots and allowed
canvas copies observe the canvas independently of overlays and other page pixels.

The compositor can recompose a canvas while its application is idle. Replaying
application commands could repeat storage writes and queries after their inputs
changed. Applications also own multi-pass, depth/stencil, resolve and attachment
semantics. A general direct-to-page target would require substantial semantic
virtualization and observation/isolation machinery. This implementation retains
the independent canvas bitmap and optimizes its publication only. Overlay or
other restricted direct paths are separate future decisions, not implicit parts
of this handoff.

## Admission and producer ownership

`canvas::create_target` uses the narrow trusted core API to create an ordinary
allocator-backed texture with internal native sampling capability. The public
WebGPU descriptor and validation permissions are unchanged. Core retains its
normal initialization and discarded-store clearing. Common single-layer,
single-sample RGBA8/BGRA8 canvases without view formats are admitted; unsupported
formats/usages/views keep normal export or CPU fallback behavior.

At presentation, a matching canvas owner must possess the exact registered core
texture and producer queue. `CanvasPublication::seal` reserves its actual HAL
allocation footprint against six admitted images and 256 MiB. This bounds shared
lease backing, including pending and consumer-held images; it does not claim to
bound every ordinary application texture allocation. The application requires
its ordinary canvas allocation even when sharing is unavailable. Quota failure
occurs before sealing and leaves that texture available to ordinary fallback.
Dedicated/external backing without an observable allocation footprint is not
admitted.

The actor serializes normal content submissions, queue writes and sealing.
Sealing performs a normal core Load/Store pass to initialize untouched/discarded
pixels, transitions through the trusted core tracker to RESOURCE, checks the
accepted submission and all internal error scopes, then calls
`destroy_and_retain`. Logical destruction invalidates every producer alias,
including previously recorded commands. No producer command may submit between
the accepted transition and destruction. Unsubmitted recordings retain their
normal validation failure behavior.

## Completion and accounting

The retained `DestroyedTexture` owns the original HAL allocation. Producer queue
pending writes and scheduled resource retirement share that same object through
normal core lifetime tracking. A private pending record retains it until all of:

* Its actor-thread core work-done callback has fired.
* The producer's own native core fence reaches the accepted producer submission.
* The retained raw object has exactly one strong reference, proving producer
  retirement records have drained after sealing.

The reference-count check is an ownership condition, never a GPU completion
substitute. No future producer clones can appear after the raw texture is
snatched. The worker only queries the producer fence and checks ownership; it
never polls core devices or runs actor callbacks. A short-lived actor timer polls
pending producer devices at two milliseconds for roughly 32 ms, 16 ms until
roughly 250 ms, then 100 ms, and stops when their receipts retire. The worker uses
the same backoff and stops native queries once the actor marks a receipt ready.
New receipts and actual progress reset its cadence. The owner's first Ready
transition wakes the existing worker channel once, so an already-ready image does
not retain its allocation/quota until a stale backoff timeout. Repeated polls do
not enqueue additional wakes. Ordinary completion does not
wait for the normal 100 ms maintenance timer; after a producer remains delayed
past 250 ms, each backed-off actor/renderer stage has a nominal 100 ms period.
Producer cleanup followed by renderer observation can therefore add roughly
200 ms after native completion, plus scheduling and rendering time; this is not
a strict wall-clock bound. The render-thread retirement timer uses the same
backoff only while waiting for
shared producer views. Actual native operations, in-flight DMA batches, release
receipts and deferred callbacks retain the existing two-millisecond cadence. New
shared registrations and actual readiness progress reset the wait cadence;
repeated frame scheduling alone does not. Readiness still propagates through
NATIVE_PROGRESS to WakePendingNativeImages without a snapshot-triggered frame.
A stuck producer therefore causes at most about 30 combined handoff-loop wakeups
per second after backoff, excluding ordinary baseline device maintenance, rather
than permanent fast polling. No renderer/render-thread wait occurs during publication.

Actor destruction stops timers and immediately drops the remote Global on the
actor thread. Publications retain the producer Device, never its Queue; the
Device has only a Weak queue link. The final Queue drop waits for native idle,
drains normal core temporary resources and fires work-done closures on that same
thread, so pending receipts do not depend on a dead actor timer. Immediate queue
teardown has a focused native test.

The producer's native fence and submission index are paired. They are never
compared with a WR queue index. Logical producer GPUDevice destruction alone does
not invalidate a completed image; shared native-context failure does.
Native fence query failures, including native device loss, quarantine the keeper
and permit until process exit rather than treating failure as completion.
Indeterminate native failures retain the pending keeper rather than returning
quota or freeing potentially live resources. Worker setup/send failure similarly
quarantines the keeper until process exit.

Once ready, only `ImageKeeper` owns the original retained raw object. Every
consumer HAL wrapper is non-owning `TextureMemory::External`; its drop callback
holds a clone of `ImageKeeper`. Normal consumer core lifetime tracking retains
that callback until submitted uses retire. Consumer snapshots and capture keep
ordinary core resource references as well. On the final keeper drop, the
original allocation is synchronously destroyed first, then its quota permit is
released. Returning from a host lock or observing a submitted semaphore is not a
quota-release event. The raw image is never recycled into a producer.

## Private transport and WR state

The in-process opaque token owns an Arc publication and binds the native-context
generation, remote owner, publication and nonce. Only trusted GPU-process C++
glue receives it. `PushSharedVulkanTexture` requires a registered owner and routes
through the existing `RemoteTextureMap` texture-host machinery. No IPDL type
contains the token, native pointer, VkImage or core resource ID.

`RenderVulkanSharedTextureHost` validates the trusted route identities when
acquiring a WR registry view. The WR registry also validates the exact native
context identity/generation. It uses a separate shared-view map rather than the
DMA-BUF NativeImage ownership machine. Consumer wrapping happens only after the
producer completion proof. Consumer core descriptors allow TEXTURE_BINDING only
and import actual initialized RESOURCE state. Every wrapper samples immutably;
there are no layout changes or queue-family ownership transfers at repeated
acquisitions or renderer migration. A replacement renderer must share the same
live native context. An old generation never resolves into a replacement GPU
process/context.

The registry's internal retirement clock is a nonexportable Vulkan timeline,
independent of DMA-BUF capabilities. Existing independent-device export/import
paths retain their external timeline requirements. NVIDIA can therefore exercise
the positive shared path without external-memory DMA-BUF support.

Canvas snapshots sample the immutable shared view into a temporary ordinary WR
image and read back that output, reusing existing WR blit/capture code. This
on-demand operation does not transition or mutate the shared source. Capture
already takes the same sampling path for textures without COPY_SRC usage.
Opaque views preserve alpha by component swizzle; premultiplied views retain
original alpha. Resize, unconfigure and expired current textures keep their
existing producer identities and remote-host lifetimes.

## Baseline shader limitation

The first opacity fixture used a dynamically indexed function-local WGSL array
for triangle vertices. The unchanged imported Naga 30.0.0 SPIR-V backend emitted
an ArrayStride-decorated array in Function storage, producing
`VUID-StandaloneSpirv-None-10684` with current synchronization validation. The
same exact shader and validation error were reproduced with startup sharing off;
visual assertions passed in both runs. These diagnostic failures are retained
in the acceptance artifacts rather than counted as clean validation runs.

The fixture now computes the identical three vertices arithmetically, retaining
its opacity, overlap and scissor checks. No Naga or other shader compiler changes
are included. This targeted image-handoff acceptance does not establish broad
WGSL compiler conformance; the baseline local-array compiler issue remains
separate work.

## Review and remote acceptance

The vendor support is a separate, narrowly scoped commit. This integration must
stay vendor-clean. Updating wgpu requires rechecking internal sampling validation,
core initialization after discard, alias destruction, producer TempResource and
consumer wrapper retirement, and the native fence/submission pairing.

Focused native tests in `wgpu_shared/tests/canvas_publication.rs` exercise the
actual owned publication, identical native device/image handles, independent
consumer core identities, untouched pixels, exact sampled output, logical
producer destruction, owner/publication mismatch, and retired context rejection.
The supporting core tests separately cover public usage restrictions, discard,
recorded aliases and pending writes.

`test_vulkan_shared_canvas_image.py` uses real hardware compositor screenshot
readback in content scope, rather than default document snapshots. It exercises
RGBA/BGRA, two independent devices, logical destruction, resize, discard, snapshot
contents before CSS, group opacity, worker/opaque presentation and view-format
fallback. Its exact-pixel records must be paired with per-publication producer
and consumer logs proving equal native device/image handles and successful
compositor acquisition. Producer metadata by itself is not acceptance evidence.

On 2026-11-14, the remote j10 ThinLTO Firefox build, two Wrench checks and
16 native tests passed on NVIDIA RTX 5060 Ti / 615.71.09. The native matrix covered
four core-support proofs, four owned publications, seven shared-device
regressions and the shared-wait/retirement distinction. Accepted correctness runs
used the actual Khronos 1.4.341 synchronization layer with zero VUID/SYNC errors.
Thirteen targeted X11 browser cases covered the new canvas tests, isolated
natural presentation, legacy lifecycle/startup/GPU-process replacement and gate
off. A validation-on normal-core API replay also checked compute/storage,
render bundles, occlusion queries and optional timestamps at 128/512 canvas sizes.

Isolated positive cases correlated the original and consumer VkDevice/VkImage,
validated owner/publication identities, actual WR acquisitions and exact pixels.
Existing X11 window pixels were observed before browser readback, then snapshots
and compositor PNGs were checked. These establish natural idle presentation;
they do not claim that every browser case necessarily entered Pending first.
That state/progress transition has a separate focused native proof. Fallback
controls selected the existing native export/copy route on this host; CPU
fallback remains available. Old assumptions that NVIDIA could only use CPU
transport were corrected from the observed route.

The short three-mode replay recorded 214 direct shared publications with zero
transport copies, versus 214 native export/copies in each independent and shared
view-format-fallback control. There were 213 logged direct acquisition matches;
publication counts are not proof that every image reached display. Timing runs
used validation off and reported CPU API return, work-done callback, rAF and
process CPU/RSS scopes, not full-chain GPU execution, scanout latency or total
memory. An initially sleeping DPMS display produced 1 Hz/roughly three-second
observations in every mode. With the display awake at 60 Hz, diagnostic examples
were roughly 41/81 ms; these are liveness observations, not latency benchmarks.
The original DPMS state and timeout settings were restored.

Keep the feature off by default and retain independent DMA/CPU fallbacks. This
is targeted implementation acceptance, not broad WebGPU CTS, full memory-pressure
qualification or physical device-reset coverage. Process replacement and
simulated failure do not establish physical-reset behavior. The baseline shader
limitation remains documented separately.
