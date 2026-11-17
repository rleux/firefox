# Shared Vulkan WebGPU canvas images

The default-off `gfx.webrender.vulkan.shared-webgpu` startup preference allows
WebGPU and WebRender to use separate core devices over one native Vulkan device.
See [Vulkan WebRender](VulkanWebRenderOverview.md) for platform and process gates.
For eligible canvases, publication retains the original core-allocated image:
there is no transport-image copy, DMA-BUF export/import, or external queue-family
transfer. Ordinary WebGPU validation and initialization remain active.

## Admission and sealing

The trusted allocation hook adds native sampling capability while preserving the
public texture descriptor and its usage restrictions. Admission accepts supported
single-layer, single-sample RGBA8/BGRA8 renderable textures without view formats.
Other descriptors retain native-export or CPU fallback. See the
[handoff contract](WgpuNativeTextureHandoff.md) for the required core hooks.

A publication must match its registered owner, producer texture, queue and live
native context. It reserves the actual allocation footprint against six shared
images and 256 MiB, including pending and consumer-held images. This bounds
publication backing, not all application texture allocations. Budget rejection
occurs before sealing, preserving fallback. External or dedicated backing with
no observable allocation footprint is not admitted.

On the producer actor thread, sealing records a core Load/Store pass to initialize
untouched or discarded contents, transitions to RESOURCE, submits successfully,
and calls `destroy_and_retain`. This invalidates producer aliases, including
previously recorded commands. The actor must serialize every producer write,
submission and seal; the native queue mutex alone cannot enforce this sequence.
The original image is never recycled for further producer writes.

The canvas remains an independent bitmap. Group opacity, masks, filters and
blending apply to the finished image; replaying application commands into the
page would change those semantics and could repeat storage writes or queries.
WR samples the finished canvas using its existing external-image drawing path.

## Completion and lifetime

A consumer can wrap the image only after all three conditions hold:

- The producer's native fence reaches its own accepted submission index.
- The owner-thread core work-done callback has fired.
- Only the publication retains the original raw allocation, proving that
  producer retirement records have drained.

Submission indices from different core devices are never compared. Reference
counts establish ownership, not GPU completion. The worker queries native fences
but never runs core callbacks. Actor maintenance first checks native progress,
then drains required core work on the owner thread. Pending polls back off from
2 to 16 to 100 milliseconds; new work and readiness progress reset the cadence.
Readiness wakes pending WR documents without requiring a screenshot request.

Publications retain the producer Device, not its Queue. Actor shutdown stops its
timers and drops the remote Global on the actor thread; final queue destruction
waits for completion and drains callbacks there. Logical destruction of one
WebGPU device does not invalidate a completed shared image. Native-context
failure does. Indeterminate native failures or worker setup/send failure retain
keepers and quota until process exit rather than free possibly live resources.
A retained failed context can prevent replacement within that process; GPU-process
replacement remains the recovery boundary.

Each consumer uses a sampling-only core wrapper over the original VkImage.
Its HAL drop callback retains the image keeper through ordinary GPU retirement.
The final keeper destroys the original allocation before returning its quota.
Host unlock or publication removal alone does not prove completion.

## Trusted route and snapshots

The opaque token owns a publication Arc bound to context generation, remote
owner, publication ID and nonce. It remains inside trusted GPU-process code;
no IPDL message transports the token, VkImage or core resource ID. Existing
RemoteTextureMap routing selects the owner and texture host. Both the host and
WR registry validate identities before acquiring a view. Renderer migration
requires the same live native context; old generations cannot resolve into a
replacement process or context.

Shared views use their own registry map and a nonexportable retirement timeline.
They do not require DMA-BUF admission. Independent-device export/import retains
its external-memory and timeline requirements. Unsupported shared admission can
therefore have a different fallback outcome on each adapter.

Snapshots sample the immutable shared view into an ordinary scratch image before
readback, leaving the source in RESOURCE state. Opaque views use alpha-ONE
swizzling; premultiplied views preserve original alpha. Resize, unconfigure and
expired current textures retain their existing identity and lifetime checks.
See the [screenshot guide](DebuggingWebRenderScreenshots.md) for observation limits.

## Validation

`wgpu_shared/tests/texture_handoff.rs` checks public usage restrictions, lazy
initialization, recorded alias rejection and producer/consumer retirement.
`tests/canvas_publication.rs` checks image identity, independent consumers,
logical destruction, owner/generation rejection, queue teardown and quota return.
WR shared-device and registry tests distinguish producer readiness from renderer
retirement and withhold native completion before owner polling.

Validate the production exporter with shared publication, admission denial and
native-export copying. An admission-denial case that still uses the same native
device does not prove independent-device transport. Native export has additional
capability requirements, so a skipped or unsupported case is not a successful
shared-publication test. Run each native suite serially with an exact inventory
and verified synchronization validation.

Browser tests must cover lifecycle, startup selection, process restart, workers,
discard, snapshots, opacity and fallback. Require actual Vulkan GPU-process
diagnostics and the shared-device admission marker, then pair
producer and consumer records by owner, publication, generation, nonce and native
image identity with compositor acquisition. Pixels or producer metadata alone do
not prove shared transport. Separate explicit snapshot readback from observation
of normal presentation.

This coverage does not establish broad WebGPU conformance, physical GPU-reset
recovery, memory-pressure qualification or performance. Preserve failed runs and
report capability-specific fallback separately from positive native evidence.
