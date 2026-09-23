# Vulkan WebRender: overview

This branch adds an experimental Vulkan execution and presentation backend for
WebRender, with Linux DMA-BUF integration for WebGPU, WebGL, and VA-API video.
The immediate review target is **Firefox on Linux/X11**. It is opt-in through
`gfx.webrender.vulkan`, which defaults to `false` and requires a restart.

This document describes the implementation at `a3a23237cc0`, based on Mozilla
`main` at `f9a73919fe0`. See the [implementation document](VulkanWebRenderImplementation.md)
for contracts, source entry points, validation scope, and measurement provenance.

## Architecture and key decisions

- **Use Mozilla's shared renderer.** Following bug 2072784, `HalGpuBackend`
  implements `GpuBackend`. Scene construction, batching, render tasks,
  texture-cache updates, and draw orchestration stay shared with GL.
- **Use `wgpu-hal` below WebRender.** WebRender owns resource states, pipelines,
  submissions, synchronization, and retirement; it does not submit its scene
  through the WebGPU API or share WebGPU's logical device. Native presentation
  and external-memory correctness are therefore explicit backend responsibilities.
- **Reuse WebRender's shaders and data layouts.** The build translates the current
  GLSL catalog to validated SPIR-V. Vulkan uses native SPIR-V by default, with an
  optional Naga route. Frame tables use storage buffers where supported, with a
  texture fallback.
- **Treat native images as leased publications.** DMA-BUF handles alone do not
  authorize reuse. Device/layout compatibility, publication generation, GPU
  completion, and ownership return determine admission and release.
- **Bound asynchronous work.** Submission and pending-frame queues are bounded.
  Resources survive through GPU completion; uncertain completion abandons the
  publication. Renderer failure enters Gecko's recovery/software-fallback path.

## Implemented features

Features include text, images, clips, gradients, filters and YUV rendering;
Vulkan swapchain presentation; readback and capture/replay; retained output,
guarded partial composition, per-image presentation damage, and X11 visibility
handling.

Linux interoperation includes direct sampling of supported WebGPU and WebGL
DMA-BUF publications and supported VA-API NV12/P010 SDR frames. **Direct sampling
does not mean an entirely copy-free pipeline:** WebGPU retains its producer export
copy, WebGL retains producer resolve/presentation work, and CPU images still
require uploads. Native and Layer compositor adapters exist, but the Firefox
Vulkan window path currently selects the Draw compositor.

## Existing performance evidence

These are **historical local measurements**, quoted from checked-in reports;
they predate the current shared-renderer port. No new measurements were run for
these documents. Most were refresh-capped Intel Iris Xe/Mesa 26.2.2 tests.

| Comparison | Recorded result | Interpretation |
| --- | --- | --- |
| Earlier Vulkan versus GL renderer baseline | Active browser workloads used 17.4–97.9% more Firefox CPU with Vulkan; static results were mixed. | No general Vulkan speedup was demonstrated. |
| Four subsequent Vulkan upload changes, versus their preserved Vulkan baseline | Software Canvas2D median paired CPU reductions: 26.71% in the GPU process, 22.16% for Firefox. | A specific path improved; two GL reference runs still used less CPU. |
| WebGL asynchronous versus synchronous ownership transfer | About 5.1% less Firefox CPU; unchanged refresh-capped cadence. | A bounded CPU improvement on that workload. |
| WebGPU direct sampling versus consumer-side copy, before asynchronous completion changes | Native X11: about 10.1% fewer tracked render-engine cycles, about 12.6% more Firefox CPU; equal cadence. | A GPU-work/CPU tradeoff, not a throughput win. |
| VA-API asynchronous versus synchronous transfer | About 3.1% more Firefox CPU; zero reported dropped frames in both modes. | No CPU or playback-throughput improvement demonstrated. |

The [detailed performance section](VulkanWebRenderImplementation.md#existing-performance-records)
identifies the reports and comparison limits. These percentages have different
baselines and must not be added together or attributed to the current port.

## Validation and review boundaries

Recent local Linux/Intel validation includes a Firefox build, 71 Vulkan library
tests, 12 Wrench HAL tests, browser image and clip-mask regressions, WebGPU
presentation tests, and OceanDemo with Vulkan validation. Earlier records cover
native WebGL and VA-API import/playback cases. This is targeted coverage, not a
complete browser or cross-platform acceptance result. Full Wrench pixel parity
remains open, including documented filtering differences.

Review priorities are the `GpuBackend` contract, image ownership, frame
completion/recovery, and build footprint. Broader driver/platform coverage,
current-port performance measurements, and a reviewable patch split remain work
before default enablement.
