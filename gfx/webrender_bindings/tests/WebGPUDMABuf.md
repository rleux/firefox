# WebGPU DMA-BUF direct sampling

WebGPU and WebRender use separate Vulkan logical devices. WebGPU's
`wgpu_vkimage_prepare_webrender_present` copies the API texture into a dedicated
export image through wgpu-core, establishing initialized contents. It releases
that image in GENERAL layout to EXTERNAL ownership and exports a sync-file.
The baseline WebRender importer copies the export into an owned texture before
sampling. Removing this second copy does not remove the producer copy.

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

## Probe and copy baseline

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
  --output artifacts/webgpu-baseline --transport copy \
  --software-presentation --record-baseline-defects
```

Use `--icd` and `--validation-layers` to select the test driver and layers.
Repeat with `--gpu-process false` for parent-process rendering. Mesa's software
presentation option permits Xvfb without DRI3; it still uses the selected
Vulkan adapter for rendering and import. These runs do not measure native
desktop presentation performance.

The original copy baseline has a known opaque-alpha defect: fractional alpha
in an opaque canvas blends with the page background. The explicit
`--record-baseline-defects` option records only those two known failures in
`report.json`; it cannot excuse them in direct-transport runs. Without that
option all pixel checks are strict.

For timing comparisons omit validation layers and add `--benchmark-frames 120`.
The fixture continuously presents a 1920x1080 canvas after 15 warmup frames.
It records animation-frame intervals, CPU submission duration and a GPU
timestamp for the final producer render pass. Only the final frame maps a
timestamp buffer, avoiding a per-frame readback bottleneck. Producer GPU time
does not include WebRender's import or composition work. The isolated profile
disables reduced timer precision for this measurement.

Process samples record CPU counters, summed RSS and per-client DRM counters.
DRM clients are deduplicated by device/client identity as required by the
[kernel DRM usage statistics documentation](https://docs.kernel.org/gpu/drm-usage-stats.html).
RSS can double-count shared pages; per-client memory can count shared GPU
allocations in both clients. Neither is a physical-memory total. Counter
availability depends on the driver; preserve the reported units when comparing
runs. Keep correctness/validation runs separate from timing runs.

WebGPU recycling currently resets its texture host, readiness semaphore and
generation after the remote texture map returns a resource. Direct sampling
must retain the publication through all GPU reads, serialize snapshots and
producer reuse, reject stale generations and quarantine uncertain completion.
Capability checks and bounded retention are required before browser enablement.
