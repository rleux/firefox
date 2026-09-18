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

For timing comparisons omit validation layers and add
`--benchmark-only --benchmark-frames 120` to both builds. This uses identical
warmup without alpha-validation scenarios. The fixture continuously presents
a 1920x1080 premultiplied canvas containing opaque pixels. Frame samples
exclude 15 warmup frames; process counters include that warmup interval.
It records animation-frame intervals, CPU submission duration and a GPU
timestamp for the final producer render pass. Only the final frame maps a
timestamp buffer, avoiding a per-frame readback bottleneck. Producer GPU time
does not include WebRender's import or composition work. The isolated profile
disables reduced timer precision for this measurement.

Process samples record CPU counters, summed RSS, PSS, private resident memory
and per-client DRM counters. PSS/private samples include only processes whose
`smaps_rollup` is readable; `memorySampledProcesses` records that coverage.
DRM clients are deduplicated by device/client identity as required by the
[kernel DRM usage statistics documentation](https://docs.kernel.org/gpu/drm-usage-stats.html).
RSS can double-count shared pages; per-client memory can count shared GPU
allocations in both clients. Neither is a physical-memory total. Counter
availability depends on the driver; preserve the reported units when comparing
runs. Keep correctness/validation runs separate from timing runs.

Local Intel Iris Xe/Mesa 26.2.2 measurements under Xvfb showed similar median
cadence in three short trials. One longer 6,000-frame pair had worse direct-import
tail cadence (51.36 ms p95 versus 17.32 ms) and about 54 MiB higher peak process
PSS. Peak summed client GPU residency was the same and declined late in both
runs. Process PSS/private memory still grew at similar late rates, so these
runs do not establish a total-memory plateau. Removing the materialization
copy does not by itself establish a performance or memory improvement.

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

Frame end drains native DMA-BUF sampling completion with a bounded wait,
allowing idle frames and snapshots to release source ownership. CPU snapshots
remain explicit copies under the same shared lock. A busy snapshot wait does
not poison another reader's publication.

Recycling atomically retires the old access handle before the allocation can
be reused. A busy or poisoned allocation is not returned to the producer's
reuse queue. Successful retirement creates a fresh access handle; stale
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
checks busy recycle rejection and permanently retired old mappings.

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
