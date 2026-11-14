# Animometer bundle replay: implementation and measurements

Measured 2026-11-14 on `vulkan5-webgpu`. The benchmark is a Wrench subcommand
that calls normal wgpu-core directly. It isolates the WebGPU Samples
[Animometer](https://webgpu.github.io/webgpu-samples/?sample=animometer), pinned
to upstream revision `e040ec1a20dbbe01afed30c812e825b2639cfe5b`.

**Canvas lifecycle correction:** the initial persistent-target experiment below
missed per-frame texture retirement, which disabled native cache reuse in the
installed browser. See [the correction and actual browser profiles](animometer-canvas.md).
Use `--transient-target` for canvas-oriented measurements.

## Reproduction and scope

Build the Linux Wrench target with the checkout's vendored Cargo/toolchain setup:

```sh
cargo build --release -j10 -p wrench --features webgpu-perf
wrench animometer --triangles 100000 --output result.json
wrench animometer --triangles 100000 --dynamic-offsets --output dynamic.json
wrench animometer --triangles 100000 --inline --output inline.json
```

Select an explicit hardware Vulkan ICD. Run correctness separately with
`--validation --check --warmup 1 --iterations 1`, actual Khronos synchronization
validation enabled, and confirm its activation in the log. Disable validation
and verify that its library is absent from the process maps for timing runs.

The fixture uses 100,000 separate three-vertex draws, the sample's vertex input
and uniform layout, deterministic data, and a 256x256 offscreen target. Static
mode creates a separate triangle bind group per draw. Dynamic mode reuses one
triangle group with dynamic offsets. Every frame writes animation time, encodes,
submits, and waits for completion. JSON reports these four intervals separately.
Full-image checks compare bundle and inline pixels at two animation times.

This measures elapsed CPU-side recording and the sum of timed offscreen phases.
It excludes browser IPC, WR scene construction/batching, presentation, and JSON
handling. It has no GPU timestamp instrumentation. It does not establish browser
FPS or a speedup for ordinary WR inline render passes.

Bind groups are created **before** initial uniform uploads, matching upstream.
GDB confirmed 100,001 captured initialization actions in both static and dynamic
100k modes. Results from the earlier reversed-order fixture were discarded.
The shader retains its upstream license and uses an equivalent output struct
constructor to avoid an existing pinned Naga function-local layout issue.
WebGPU WGSL uses its normal validated path; WR's glslang path is unchanged.

## Changes applied sequentially

| Step | Commit | NVIDIA paired encoding result | Production Rust change |
| --- | --- | --- | --- |
| Corrected benchmark | `1f59fbc25a4d` | Baseline | Benchmark only |
| Skip already-satisfied buffer initialization actions | `eb94472f7e7c` | -9.54% vs baseline | +18 / -11 |
| Deduplicate binding resources into vectors | Rejected, patch retained outside checkout | **+11.74% regression** vs preceding step | +40 / -3; not retained |
| Cache successful resource validation by snatch generation | `771a1297200c` | Static -4.71%; inline control -3.99%; dynamic -0.96% | +36 / -7 |
| Cache native Vulkan secondary commands | `ca1518a82532` | Static -99.897%; dynamic -99.888% vs preceding step | +382 / -11 |

Counts include blank lines and comments, exclude checksums, tests, benchmark
code and documentation. At `ca1518a82532`, the production diff was **+430 / -23, net +407**
Rust lines. No new crates were added to the production path.

At the original `ca1518a82532` snapshot, the generation change's standalone speedup was inconclusive: inline controls
improved too, and Intel's smaller comparisons were noisy. Its invalidation
mechanism is also required by the subsequent native cache. It adds one machine
word per bundle and a word inside the existing destruction lock, with no new
lock acquisition or resource-vector allocation. Initialization caching adds one
atomic flag per bundle. Texture initialization/discard processing stays active.

Native eligibility scanning increased measured bundle construction by about 5%
on NVIDIA versus the preceding step. This excludes the first native recording,
which happens on first execution. The initial first-use encoding cost was not measured. The CLI now accepts
`--warmup 0`; the canvas correction reports the subsequent first-use measurements.

## Final warmed results

Five process pairs per comparison, alternating AB/BA order; each process used
five warmups and thirty measured frames. Both executables were prewarmed.
Numbers below are medians of process medians for the original corrected
baseline versus the initial optimizations at `ca1518a82532`, using a persistent
target. They do not represent the canvas retirement lifecycle; see the correction.

| Hardware | CPU encoding, baseline → final | Sum of timed frame phases, baseline → final |
| --- | --- | --- |
| Remote RTX 5060 Ti, driver 615.71.09, CPU 3 | 4.597 → 0.00402 ms | 5.452 → 0.830 ms, 6.57x |
| Local Iris Xe RPL-P, Mesa 26.2.6, i5-1340P CPU 2 | 31.100 → 0.0886 ms | 49.286 → 13.800 ms, 3.57x |

Incrementally, native caching changed NVIDIA static encoding from 3.995 to
0.00413 ms and timed frame phases from 4.848 to 0.834 ms. Dynamic encoding
changed from 3.750 to 0.00421 ms; timed phases changed from 4.538 to 0.777 ms.
The NVIDIA inline control changed approximately -3.02%, rather than the roughly
1000x reduction seen with bundles. Completion wait stayed around 0.8 ms.

Local Intel measurements ran alongside the existing desktop and showed sizable
frequency/background-load variation. Their small per-step differences should
not be treated as causal improvements. The native-cache reduction was large in
every pair; the paired inline-control median changed +0.27% in encoding time.

Wrench binaries used Rust 1.99.0, release opt-level 2, LTO off, and frame pointers.
These timings do not come from the separately built Firefox ThinLTO executable.

## Native cache safety and limits

The optional path queries and enables `VK_EXT_nested_command_buffer` and its
`nestedCommandBuffer` feature. This permits a mixed inline/secondary subpass;
[plain inline subpasses prohibit secondary execution](https://docs.vulkan.org/refpages/latest/refpages/source/VkSubpassContents.html).
Devices without the capability retain CPU replay. No Vulkan binding crate was
updated to obtain newer maintenance extensions.

Each eligible bundle retains at most one command pool and one secondary buffer,
recorded with `RENDER_PASS_CONTINUE | SIMULTANEOUS_USE`. It inherits no specific
framebuffer. Its key matches the exact native render pass, viewport, scissor,
blend constants and stencil state. Different keys retain CPU replay. The owning
HAL device keeps the render-pass cache alive.

The existing snatch read guard prevents destruction during encoding. Every
exclusive snatch acquisition increments a saturating generation. A cache hit initially required successful full replay in that generation.
The canvas correction now validates only the cached bundle's unique normalized
resources after a generation change, under the same guard. Saturation always
requires that resource validation. Failed validation never publishes a generation.
Submission-time validation remains responsible for resources destroyed after
encoding. Buffer/texture destruction and discarded-texture regressions pass.

Existing core command-buffer trackers retain the owning bundle through GPU
retirement. A concurrent first recording publishes the winning cache **before**
execution; losing private pools are never submitted. Optional cache allocation
OOM falls back to CPU replay. Recording failures always restore the primary.

After secondary execution, the primary's viewport, scissor, blend and stencil
state are restored. Core already resets pipeline, binding, vertex/index and
immediate-data state around bundle execution. The
[Vulkan secondary-command rules](https://docs.vulkan.org/refpages/latest/refpages/source/vkCmdExecuteCommands.html)
also require compatible inheritance and simultaneous-use handling.

Active queries, multiview passes, indirect/mesh commands, stencil-reference
pipelines, external textures and acceleration-structure bindings retain CPU
replay. Supporting them, multiple cached keys, or finer invalidation would need
separate correctness tests and measurements.

## Validation

- Eight native bundle tests passed on both NVIDIA and Intel with actual
  synchronization validation and zero VUID/SYNC diagnostics. These cover zero
  initialization, partial initialization, dropped encodings, later uploads,
  destruction before/after encoding, texture discard, concurrent first use,
  repeated/pending use, early handle drop, dynamic state, replacement/resized
  targets, inline commands around bundles, and query fallback.
- Static and dynamic 100k full-image bundle/inline checks passed on both GPUs.
- All sixteen existing shared-device regressions passed on NVIDIA: four texture
  handoff, four canvas publication, seven WR shared-device and one WR wait test.
- GDB observed the HAL native cached-execution path over repeated 100k frames.
- Firefox integration builds passed for each retained optimization with actual
  modified vendor recompilation. The final build forced both HAL and core
  recompilation, used `-j10` and C/C++ ThinLTO, and stamped source
  `ca1518a825325ef747c66dfd85651ff165c618af`, BuildID `20261114102508`.
- The additional core snatch generation/saturation unit test remains unrun:
  Cargo refused dev-dependency testing of the vendored non-workspace crate.
  Native tests exercise invalidation after unrelated and referenced destruction.

The subsequent release and canvas correction are documented separately. No
browser FPS benchmark or full WebGPU conformance run was performed, and no
branch was pushed.
