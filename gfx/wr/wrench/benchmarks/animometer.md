# Animometer command benchmark

The Linux-only Wrench `animometer` command exercises normal wgpu-core on a
256×256 offscreen Vulkan target. It creates separate triangle draws with static
uniform bindings, or one bind group with dynamic offsets. The deterministic data
and embedded [shader](animometer.wgsl) follow the WebGPU Samples Animometer;
the shader retains its upstream revision and license notice.

## Build and run

The feature also enables Vulkan Wrench. Configure the shader toolchain described
in the [implementation guide](../../../docs/VulkanWebRenderImplementation.md).
From `gfx/wr`, using the checkout's configured Rust dependencies:

```sh
cargo build -p wrench --release --features webgpu-perf
./target/release/wrench animometer --triangles 100000 --validation --check --warmup 1 --iterations 1 --output correctness.json
```

Use the actual build artifact if `CARGO_TARGET_DIR` changes its location. This
command needs no window or `--headless` flag. `--validation` requests validation;
select and verify the loaded Khronos layer and synchronization-validation settings
separately. Preserve all VUID and synchronization diagnostics.

| Option | Behavior |
| --- | --- |
| Default | Replay a render bundle with static per-triangle bindings |
| `--inline` | Record the draws each frame |
| `--dynamic-offsets` | Use one bind group and per-draw dynamic offsets |
| `--transient-target` | Destroy and recreate the attachment each frame |
| `--triangles N` | Draw 1–200,000 triangles; default 100,000 |
| `--warmup N` | Exclude this many initial frames; default 5 |
| `--iterations N` | Measure this many frames; default 30, minimum 1 |
| `--output PATH` | Also write the JSON report to a file |

Persistent attachments are the default control. Use `--transient-target` when
checking behavior across canvas-like attachment retirement. Bind groups are
created before initial uniform uploads; those uploads reach submission after
bundle construction. Each later frame updates the time uniform.

## Correctness and path evidence

Run all eight combinations of inline/bundle, static/dynamic offsets and
persistent/transient attachments with validation and `--check`. The check compares
every bundle/inline output byte at two animation times, requires visible colored
pixels, and verifies that animation changes the image. Successful JSON contains
`correctness.bundle_matches_inline: true`, `correctness.animated: true` and two
different `correctness.pixel_hashes`. Without `--check`, `correctness` is null.

A render bundle can use CPU replay when native secondary commands are unavailable
or ineligible. Pixel parity alone does not establish native cache recording or
reuse. Confirm actual native execution separately when making that claim, and
report capability-dependent fallback distinctly. These checks complement resource
initialization, destruction, mapping, query and lifetime regression tests.

## Timing and interpretation

Run timings separately with validation disabled and verify that the validation
layer is absent. For example, from `gfx/wr`:

```sh
./target/release/wrench animometer --triangles 100000 --transient-target --warmup 5 --iterations 60 --output samples.json
```

The explicit 60 iterations override the default 30. Preserve individual samples
and repeat matched baseline/candidate runs; keep compiler/profile, input options,
GPU/driver and background workload comparable. Record exact source, executable,
shader, dependency configuration and tool identities with each result.

Every frame submits and waits for its own completion before starting the next.
`samples` separates `update_ns`, `encode_ns`, `submit_ns` and `wait_ns`.
In transient mode, `encode_ns` includes target destruction, recreation and view
creation as well as command encoding. `encode_median_ns` summarizes that interval.
`setup_ns` covers scene setup; `bundle_build_ns` measures core bundle encoding
and finish, not lazy native secondary recording. With five warmups, initial native recording for an eligible
cached bundle occurs before measured samples. These samples describe steady-state
serialized work and exclude that initial recording cost. Waiting includes driver
and synchronization effects and is not a GPU timestamp measurement.

The result describes serialized offscreen core work. It does not measure
in-flight throughput, browser FPS, WebRender display-list construction, browser
IPC or presentation. Changes to browser polling and memory policy require their
own browser workload evidence. No performance result is implied by native device
sharing, successful pixels or this benchmark's availability.
