# wrench

`wrench` is a tool for debugging webrender outside of a browser engine.

## Build

Build `wrench` with `cargo build --release` within the `wrench` directory.

## headless

`wrench` has an optional headless mode for use in continuous integration. To run in headless mode, instead of using `cargo run -- args`, use `./headless.py args`.

## `show`

If you are working on gecko integration you can capture a frame via the following steps.
* Visit about:support and check that the "Compositing" value in the "Graphics" table says "WebRender". Enable `gfx.webrender.all` in about:config if necessary to enable WebRender.
* Hit ctrl-shift-3 to capture the frame. The data will be put in `~/wr-capture`.
* View the capture with `wrench show ~/wr-capture`.

## `reftest`

Wrench also has a reftest system for catching regressions.
* To run all reftests, run `script/headless.py reftest`
* To run specific reftests, run `script/headless.py reftest path/to/test/or/dir`
* To examine test failures, use the [reftest analyzer](https://hg.mozilla.org/mozilla-central/raw-file/tip/layout/tools/reftest/reftest-analyzer.xhtml)
* To add a new reftest, create an example frame and a reference frame in `reftests/` and then add an entry to `reftests/reftest.list`


## Animometer render-bundle benchmark

On Linux, build with `cargo build --release -p wrench --features webgpu-perf`.
Run `wrench animometer --triangles 100000 --output baseline.json` for the
100,000 separate triangle draws and static per-triangle uniform bindings used
by the WebGPU Samples Animometer. `--dynamic-offsets` exercises its alternate
one-bind-group path; `--inline` records commands instead of replaying a bundle.
The deterministic data, vertex input and shader follow
https://github.com/webgpu/webgpu-samples/tree/e040ec1a20dbbe01afed30c812e825b2639cfe5b/sample/animometer .
The upstream shader and license notice are in `benchmarks/animometer.wgsl`.
Its output uses an equivalent struct constructor to avoid the pinned Naga
function-local struct layout warning; no shader compiler code is changed.

This command runs normal wgpu-core directly on a 256x256 offscreen Vulkan target,
without a window, VSync, browser IPC or WR display-list batching. Bind groups are created before the initial
uniform uploads, so they capture the sample's initialization actions. Uploads
remain queued until the first submission after bundle construction. Setup/bundle creation is outside repeated
measurements. Each frame updates time, records the pass, submits, and waits for
GPU retirement; JSON separates update, command encoding, submit and wait times.
Encoding is elapsed CPU-side recording time, not GPU execution time. Capture
multiple process runs with the same build/profile and explicit hardware driver.

Use `--validation --check --iterations 1 --warmup 1` separately for correctness.
The check compares every bundle/inline output byte at two animation times and
requires visible output and changed pixels after the time update. Enable and
attest actual Khronos synchronization validation for that run. For timings disable
validation and confirm the layer is absent; use the default five warmups and
30 samples or specify `--warmup`/`--iterations`. Save source/binary hashes and
compiler flags with each result. This is a replay benchmark, not browser FPS,
full-chain GPU time or a replacement for WebGPU conformance tests.
