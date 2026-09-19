# WebGPU DMA-BUF direct sampling

WebGPU and WebRender use separate Vulkan logical devices. WebGPU's
`wgpu_vkimage_prepare_webrender_present` copies the API texture into a dedicated
export image through wgpu-core, establishing initialized contents. It releases
that image in GENERAL layout to EXTERNAL ownership and exports a sync-file.
WebRender samples supported exports directly. The original importer copied
the export into an owned texture before sampling. The producer copy remains.

The producer's internal export usage already includes sampled and transfer
usage even when the canvas configuration does not request texture binding.
Public WebGPU usage validation remains independent of this internal usage.
The contract admits single-memory-plane RGBA8/BGRA8 on the same physical device
and driver, with matching DRM modifier, dimensions, pitch and offset. A
publication has a nonzero generation, retained handles and a shared access
lock. Ownership, completion and permission to overwrite are separate from
allocation lifetime. A copied FD alone does not grant reuse permission.

DMA-BUF imports consume their FD on successful allocation and require a
compatible physical device and driver, as specified by
[VkImportMemoryFdInfoKHR](https://docs.vulkan.org/refpages/latest/refpages/source/VkImportMemoryFdInfoKHR.html).
Preserve EXTERNAL ownership for this Vulkan producer; VA-API's FOREIGN
ownership contract is different. Dedicated import requirements are described
by [VkMemoryDedicatedAllocateInfo](https://docs.vulkan.org/refpages/latest/refpages/source/VkMemoryDedicatedAllocateInfo.html).

## Probe and browser tests

Build the standalone WebRender library tests from `gfx/wr`:

```sh
cargo test -p webrender --lib --features hal-naga,hal-linux-dmabuf \
  --release --no-run --offline --target-dir ../../artifacts/webgpu-build
```

Run the resulting `webrender-<hash>` binary with
`dmabuf_sampling_contract_probe --ignored --nocapture`. Select the target
Vulkan ICD and validation layers with the loader environment. The probe checks
sampling, linear filtering, transfer-source usage, import support and limits
for every transfer-compatible RGBA8/BGRA8 modifier. Unsupported hardware is
not a successful native run.

The browser runner uses the existing Firefox build, a private Xvfb display
and Openbox. It verifies the actual renderer/process, publication generations,
transport diagnostics and composited pixels. Install `xvfb-run`, `xauth` and
`openbox`, then run from the source root:

```sh
python3 gfx/webrender_bindings/tests/run_browser_webgpu.py \
  --output artifacts/webgpu-direct --transport direct \
  --software-presentation
```

Use `--icd` and `--validation-layers` to select the test driver and layers.
Repeat with `--gpu-process false` for parent-process rendering. Mesa's software
presentation option permits Xvfb without DRI3; it still uses the selected
Vulkan adapter for rendering and import. These runs do not measure native
desktop presentation performance.

Use `--display native --viewport 890 705` to measure the existing X11 display
with hardware presentation. Omit `--software-presentation`. This uses the
existing window manager and a private Firefox profile; Xvfb remains the default.
Native benchmarks require a visible, focused test window and reject focus or
visibility changes during measurement. Reports include the display, screen,
viewport, device-pixel ratio and screenshot dimensions.

Validation-enabled multi-window tests should use a Vulkan loader containing
the [upstream device-list synchronization fix](https://github.com/KhronosGroup/Vulkan-Loader/pull/1866).
Older loaders can race device teardown against debug-object naming. A local
loader can be selected with `--loader-directory`; record it with the test results.

The original copy baseline has an opaque-alpha defect: fractional alpha and
discarded opaque contents blend with the page background. To compare that
build, pass `--binary /path/to/copy/firefox --transport copy`.
`--record-baseline-defects` records those known failures in `report.json` and
skips the transformed-alpha checks; it cannot excuse failures in direct runs.
Without that option all pixel checks are strict.

Use `--backend gl --gpu-process false` for the default-renderer compatibility
control. It runs the same strict basic pixel, snapshot and CSS-opacity checks,
asserts the OpenGL backend and omits Vulkan transport assertions.
The llvmpipe control currently exposes opaque-alpha and discard failures in
both the saved copy baseline and this implementation. They remain strict
failures; compare complete reports to distinguish existing defects from changes.

The default matrix covers RGBA/BGRA, opaque/premultiplied alpha, initialized
discard, repeated presentation, canvas snapshots and transformed CSS opacity.
Additional `--scenario` choices are `lifecycle`, `windows`, `offscreen`, `reset`
and `crash`. They cover odd-size resize, simultaneous canvases, device
destruction/recreation, tab/window closure, OffscreenCanvas opacity and GPU
process loss. Use GPU-process rendering for `crash`. Recovery checks flush
the replacement compositor before testing resumed native presentation; they
do not establish unassisted repaint timing.

For timing comparisons use the same binary for both paths. `--transport copy`
sets `WR_WEBGPU_FORCE_DMABUF_COPY=1` to select the existing copy fallback even
when sampling is supported; `--transport direct` clears that override. Both
retain the same producer publication and recycling protocol.

Omit validation layers and use
`--benchmark-only --benchmark-frames 6000 --process-metrics light` for each
path, repeating pairs in both orders. This uses identical warmup without
alpha-validation scenarios. Timing mode suppresses per-frame transport,
renderer and allocation-note logging, and verifies a one-time transport marker.
The fixture continuously presents
a 1920x1080 premultiplied canvas containing opaque pixels. Frame samples
exclude 15 warmup frames; process counters include that warmup interval.
It records animation-frame intervals, CPU submission duration and a GPU
timestamp for the final producer render pass. Only the final frame maps a
timestamp buffer, avoiding a per-frame readback bottleneck. Producer GPU time
does not include WebRender's import or composition work. The isolated profile
disables reduced timer precision for this measurement.

`--process-metrics light` records CPU counters, summed RSS and per-client DRM
counters. `full` (the default) also records PSS and private resident memory;
`off` disables process sampling. Run full memory sampling separately from
primary timing. PSS/private samples include only processes whose
`smaps_rollup` is readable; `memorySampledProcesses` records that coverage.
DRM clients are deduplicated by device/client identity as required by the
[kernel DRM usage statistics documentation](https://docs.kernel.org/gpu/drm-usage-stats.html).
RSS can double-count shared pages; per-client memory can count shared GPU
allocations in both clients. Neither is a physical-memory total. Counter
availability depends on the driver; preserve the reported units when comparing
runs. Keep correctness/validation runs separate from timing runs.

Use `--sync-instrumentation --process-metrics off` for separate diagnostics.
`WR_WEBGPU_SYNC_INSTRUMENTATION` enables cumulative CPU wall-time histograms
for import, acquire, ownership-return and conditional backpressure operations, plus
per-publication allocation/reuse counters. Use the last histogram per
process/thread/event; periodic snapshots are cumulative and process termination
can omit the final partial batch. Spans can nest, so their totals must not be
added as independent costs. Quiet timing uses `WR_WEBGPU_BENCHMARK_QUIET`.

Before W4's asynchronous completion changes, on Intel Iris Xe/Mesa 26.2.2
under Xvfb, four balanced same-binary pairs of
6,000 frames had p95 intervals of 17.22–17.28 ms across both paths. The median
paired direct-minus-copy elapsed difference was -0.076 s over about 102 s.
Direct used 11.8–18.0% fewer tracked render-engine cycles, while process CPU
differences varied in sign. Two separate balanced 1,200-frame memory pairs had
peak PSS differences of +0.51 and -0.79 MiB for direct versus copy.
The [measurement record](WebGPUDMABufMeasurements.md) preserves the per-run
timing, memory, synchronization and host-condition data.

The matched native X11 rerun also maintained equal 60 Hz cadence. Direct
sampling used 9.0–14.9% more Firefox process CPU and 4.2–13.2% fewer tracked
render cycles. Its frame-completion wait averaged 7.07 ms; memory checks did
not reproduce the earlier PSS penalty. See the measurement record for native
display verification, window geometry and the limits of cross-display comparisons.

Earlier observations of 51.36 ms direct p95 and about 54 MiB extra PSS used
different binaries and heavier instrumentation, with unrecorded host load.
Those penalties did not reproduce in the controlled comparison; their cause
is unresolved. Host CPU, frequencies and temperatures were monitored, but a
whole-GPU busy sensor was unavailable. These refresh-capped Xvfb results do
not establish uncapped throughput, native presentation latency or a long-term
memory plateau.

Diagnostics observed four allocations and 611 reuses per 615 publications for
both paths, without retirement rejection. Direct acquisition averaged about
2.48 ms and its separate frame-completion point about 1.75 ms, including about
1.65 ms returning ownership. Those measurements describe W3's synchronous
implementation. W4 removes those unconditional waits; its performance must
be measured separately.

Direct sampling retains the publication through all GPU reads, serializes
snapshots and producer reuse, rejects stale generations and quarantines
uncertain completion through the publication/recycling protocol below.

## Standalone direct importer

`ExternalImageDevice::import_vulkan_dmabuf` imports the initialized allocation
as a sampled image, acquires EXTERNAL ownership, and returns shared leases.
It validates device/driver identity, the exact sampled/filterable modifier,
format limits, DMA-BUF bounds and Vulkan memory requirements before binding.
It returns ownership only after all leases and GPU uses finish. Import
rejection reports unused; uncertain submission or ownership return reports
abandoned and prevents further device use. Idle polling also reports a device
that failed while returning ownership.

Run the library binary with `vulkan_dmabuf_ --ignored --nocapture` in both
`WR_HAL_SHADER_INPUT=native` and `naga` modes. Tests verify distinct producer
and consumer devices, direct texture identity, RGBA/BGRA pixels for the
supported linear/tiled modifiers, retained duplicate reads, one terminal
release, rejected identities and injected import/submission/return failures.
The existing offscreen `gfx/wr/wrench/script/test_foreign_webgl.py` fixture
checks that GL producers still use FOREIGN ownership with the shared lifetime
implementation. The browser bridge uses direct leases for supported tuples
and retains an explicitly logged copy fallback for unsupported sampling tuples.

## Browser publication and recycling

The weak import cache identifies a publication by allocation, generation,
layout, shared access-handle identity and logical renderer device. Duplicate
reads share one publication and one lock. A lease token unlocks the surface
only if that token acquired it. Mismatched live publications are rejected.

For WebGPU's EXTERNAL ownership contract, acquisition submits the producer's
sync-file wait and ownership/layout barriers to the same Vulkan queue used
for subsequent WebRender reads. Import returns without a CPU completion wait.
The final sampling lease queues the ownership return after all GPU uses finish.
The callback retains the C++ publication and its access lock until that return
submission completes. Failure or uncertain completion reports abandonment.

Frame completion tracks both drawing and the resulting ownership-return
submission. A render-thread timer polls pending work every 2 ms and wakes the
normal compositor update path when completion advances, so an idle canvas does
not require another render to release resources. Renderer teardown stops the
timer before deleting its target and drains pending work. The draw queue,
external-image queue and pending frame list each have a limit of three.
Pressure waits have a five-second timeout. Vulkan driver teardown can still
block inside the driver's device/queue-idle operations.

The cache retains pending publication identity after its last weak image lease
expires. Reacquiring that identical publication polls its ownership return
outside the cache borrow before relocking; stale generations, access handles,
layouts and logical devices remain rejected. This conditional reuse wait is
separate from the removed per-frame wait. CPU snapshots remain explicit copies
under the same shared lock. A busy snapshot wait does not poison another reader's
publication. VA-API ownership and video frame completion remain synchronous.
The [WebGL RGB path](WebGLDMABuf.md) now also queues its FOREIGN ownership
transfers asynchronously.

Recycling atomically retires the old access handle before the allocation can
be reused. A busy or poisoned allocation is not returned to the producer's
reuse queue. Busy allocations remain in a pending list and are retried; poisoned
allocations cannot be retried. Each canvas has at most eight live Vulkan export
textures, tracked by weak references so the accounting does not retain textures.
At that limit the producer polls retirement instead of allocating more, with a
five-second timeout that rejects shared-texture creation and invokes the existing
readback fallback. Successful retirement creates a fresh access handle; stale
descriptors keep the retired handle and cannot read the next generation.
Remote-texture snapshots also pin compositor references while reading.

The producer fully initializes each new export before publication. It does
not preserve contents across reuse; the synchronization specification permits
discarding ownership transfers when previous contents are reinitialized after
an UNDEFINED layout transition. Completion before overwriting is still
required. See [queue family ownership transfer](https://docs.vulkan.org/spec/latest/chapters/synchronization.html#synchronization-queue-transfers).

Build the `browser_hal_image_leases` test target with the same standalone
flags and run its `native_ --ignored --nocapture` tests. They cover duplicate
readers, stale generations/access handles/devices, release before another
frame, and snapshot controls. `DMABufSurface.VulkanRecyclingRetiresOldReaders`
checks busy recycle rejection, retry eligibility and permanently retired old
mappings. The importer tests gate completion deterministically, verify the first
read before a CPU acquire wait, delayed exactly-once callbacks, bounded queue
pressure, shutdown draining and abandonment after a failed return submission.

### W4 local validation

The asynchronous path was checked on Intel Iris Xe (RPL-P), Mesa 26.2.2:

| Check | Result |
| --- | --- |
| Direct importer, native and Naga shaders | 6 tests per mode passed |
| Browser image bridge, native and Naga shaders | 3 tests per mode passed, including used-publication completion through nonblocking idle polls alone |
| Ordinary WebRender tests | 161 passed |
| External-image, submission and resource regressions | 2, 3 and 2 tests passed respectively |
| Renderer external-image regressions | 4 tests per shader mode passed |
| Real GL-to-Vulkan DMA-BUF fixture | 4 tests per shader mode passed |
| Real VA-API NV12 fixture | 4 HAL and 7 bridge tests per shader mode passed |
| Focused C++ regressions | 62 passed, including all 22 DMA-BUF surface tests |
| Xvfb browser matrix | 12 runs passed, each with 24 base pixel/snapshot cases and no waived failures |

The browser matrix covered GPU and parent rendering, multiple windows,
OffscreenCanvas, resize, device replacement, compositor reset and GPU-process
crash recovery. Copy controls also passed. It used the fixed Vulkan loader
1.4.363 and validation layers, with no validation errors. Both instrumented
600-frame lifecycle runs used at most five allocations in their main pool.
The direct run recorded 611 reuses and no retirement rejections. These are
correctness diagnostics under Xvfb, not native performance measurements.
The standalone tests force submission backpressure and the C++ tests reject
busy retirement. The browser runs did not force the eight-texture producer
limit or its timeout.

Firefox export, binary and gtest builds passed. Logs and reports are retained
under `artifacts/webgpu-zero-copy/w4/`. Native performance must be rerun after
the host is ready; the preceding measurement record describes W3.

## Opaque canvas views

For Vulkan composition, canvas configuration supplies the opaque flag through
the canvas renderer, texture-host wrappers and DMA-BUF image descriptor.
Other renderers retain their existing canvas opacity metadata. Vulkan acquires RGBA/BGRA
sampling views with alpha fixed to one for opaque images. Attachment views
keep identity mapping, and the shared allocation's alpha bytes are unchanged.
This implements the [WebGPU canvas alpha-mode contract](https://gpuweb.github.io/gpuweb/#gpucanvasalphamode)
without an extra copy or write to the source. Tests cover fractional alpha,
opaque discard, transformed CSS opacity and OffscreenCanvas readback. The
native importer test checks both the rendered opaque pixels and the unchanged
alpha bytes in the exported allocation.
