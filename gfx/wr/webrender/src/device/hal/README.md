# Experimental HAL renderer

The optional `hal-vulkan` feature provides a Vulkan bootstrap and a staged WR
frame executor using `wgpu-hal` 30.0.0 directly. The original GL Renderer remains
the default and retains its constructor and resource/cache interfaces. Only Linux
has been built and executed; this is not a completed replacement renderer.

## Current scene path

`create_vulkan_renderer` returns an additive HAL renderer and a normal
`RenderApiSender`. GL and HAL share backend/pool construction. Both use the current
scene builder, preparation, batches, task graph, texture-cache updates and GPU data
layouts. Wrench's renderer-independent YAML client works with either renderer;
there is no GL context or GL fallback in the HAL route.

The executor covers ordinary owned-image transfers and mipmaps, color/alpha
targets, masks, gradients/repetition, borders/lines/shadows, blur/scaling, text,
SVG filters, owned YUV planes, split composition and backdrop readbacks/resolves.
It retains WR's current tasks, instance layouts and GLSL algorithms. Stage 5
acceptance remains open: some selected Vulkan pixel comparisons exceed the
existing tolerances. Standard native filtering is retained; legacy Mesa filtering
is not emulated. Native targets and OS external-image resolution remain later
work. Unsupported operations fail. Full Vulkan Wrench acceptance remains Stage 7.

## Wrench commands

Inside the prescribed reference container, from `gfx/wr/wrench`:

```sh
python3 script/headless.py --backend hal --hal-validation test_hal
python3 script/headless.py --backend hal --hal-validation reftest reftests/image/tile-size.yaml
python3 script/headless.py --backend hal --hal-validation png scene.yaml output.png
```

GL remains `python3 script/headless.py reftest`. A dual-feature build can use both
selectors. The launcher builds `hal-vulkan` for HAL and bypasses OSMesa setup.
`WRENCH_VULKAN_ICD` optionally selects an ICD manifest through `VK_DRIVER_FILES`;
otherwise the loader environment/configuration applies. `--hal-adapter NAME`
requires one case-insensitive substring match. Without a filter, selection prefers
discrete, integrated, virtual, then CPU devices. Opening a selected device never
falls back to another backend. Validation requires the Khronos layer.

HAL PNG/reftest dimensions default to 1920x1080, matching GL. `--size` overrides
this. Reftests retain the original comparator, scales and per-test tolerances;
an explicit `.list` manifest is also accepted. Empty selections fail. Use an
explicit output path for diagnostic PNGs to preserve reference assets.

`test_init` checks bootstrap clear/readback; `test_hal` also checks native-image
ownership. The ignored Wrench tests under `hal::tests` exercise real frame
construction and persistent image updates/resizing. The supplemental fixtures in
`wrench/reftests/hal` have a separate GL/Vulkan precision check: alpha/depth,
nearest-filtered pixels and geometry are exact; the designated linear-image RGB
region permits 2/255 variation. Existing reftest tolerances are unchanged.

## Shader and resource contract

HAL builds preprocess current GLSL, derive a bounded catalog from
`get_shader_features`, split texture/sampler bindings, add projection storage and
locations, and compile/validate SPIR-V. GLSLANG_VALIDATOR and SPIRV_VAL can override
build-tool paths. GL-only builds do not run these tools. Shader bytes and typed
metadata are embedded; runtime needs neither generated files nor shader tools.
Per-shader reflection checks descriptor types/stages, dense native binding numbers,
vertex layouts and stage interfaces. Native SPIR-V input is Vulkan-specific. Metal/D3D12 shader outputs and
constructors remain unimplemented.

Pipeline layouts are checked against the existing WR vertex descriptors. Current
GpuBufferF/GpuBufferI, transform and task textures keep their layouts and texture
width. Persistent resources retain their creating device/instance through ordinary
reference ownership. Uploads validate bounds, pitch, formats and owner identity.
Buffers/views/bindings and encoders remain alive through GPU completion, including
cleanup. Cached descriptor identity includes physical allocation/view, sampler and
pipeline identity; reset/free operations invalidate affected cached bindings.

One graphics queue uses at most three in-flight contexts with completion-based
retirement and backpressure. Transfers and passes share submissions. Upload and
intermediate pools each cache at most 64 MiB, with count limits of 256 and 128;
oversized or busy allocations retire without entering the cache. Pipeline,
descriptor and projection caches are bounded. Readback remains synchronous;
offscreen work and resource updates do not wait per pass. `WR_HAL_SYNC` enables
diagnostic synchronization. Execution failure poisons the executor and requires
recreation rather than reusing planned states from discarded commands.

Frame publication is consumed in order, including required superseded cache work,
offscreen/no-present frames and resource-only updates. Readiness notifications
coalesce, and pipeline information has a public drain. Texture/render checkpoints
follow command submission, while readback waits for GPU completion.
The embedding/capture/profiling interfaces are incomplete. FrameOutput pixels are
packed top-down RGBA8; the Wrench-compatible read_pixels_rgba8 adapter accepts
framebuffer rectangles and returns bottom-up rows for the existing comparator.
The projection accounts for HAL Vulkan's negative-height viewport once.

## Native image probe and integration limits

The bootstrap's raw producer allocates an image on the same logical Vulkan device
and queue family, uploads a pattern, signals acquisition, and lends the image to
HAL. HAL waits, reads it, and signals release. A release callback verifies borrowed
ownership; the producer frees its allocation after completion. This does not prove
DMA-BUF, cross-process/device imports, video planes, queue-family ownership transfer
or native compositor integration. Those adapters must retain explicit device,
format/plane, subresource, valid/dirty-region and acquire/release contracts.

Standalone Cargo.lock pins released HAL/types 30.0.0 while retaining HAL 27 GUI
dependencies. Firefox's root workspace patches 30.0.0 to a distinct Git revision;
full Gecko resolution, patched-source compatibility, bindings and linking remain
unverified in the partial checkout. GL/GLES behavior on non-Linux platforms also
requires native verification. No global enablement or production-readiness claim
is made by this staged Linux result.
