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


## `animometer`

The feature also enables Vulkan Wrench; configure the shader tools described in
the [implementation guide](../../docs/VulkanWebRenderImplementation.md).
On Linux, build from `gfx/wr` with
`cargo build -p wrench --release --features webgpu-perf`. Use the resulting Wrench
executable (normally `target/release/wrench` relative to that directory):

```sh
wrench animometer --triangles 100000 --validation --check --warmup 1 --iterations 1
wrench animometer --triangles 100000 --transient-target --warmup 5 --iterations 60 --output samples.json
```

Run correctness and timing separately. Attest Khronos synchronization validation
for the first command and its absence for timings. `--inline` records commands
instead of replaying bundles; `--dynamic-offsets` uses one bind group with dynamic
offsets. `--transient-target` destroys and recreates the attachment every frame;
the default persistent target is the control. Defaults are five warmups and
30 measured iterations.

The benchmark uses normal wgpu-core on a 256x256 offscreen Vulkan target and waits
for every submission. JSON separates update, encoding, submit and wait times.
`--check` compares complete inline/bundle pixels at two animation times. These
serialized CPU timings do not measure browser FPS or in-flight GPU throughput.
Save exact source/binary/driver identities and compare matched builds and inputs.
