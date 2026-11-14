# Animometer inline validation optimizations

These small wgpu-core changes target repeated CPU work while encoding inline
draws. They preserve validation errors and the normal validated WebGPU path.
Message representation, decoding, serialization and command-copy work is deferred.

## Completed buffer initialization

Each bind group caches whether all its saved buffer initialization actions are
already satisfied. Buffer initialization is monotonic. The cache is set only
after every saved action finds no uninitialized range; merely recording or
dropping a command buffer does not satisfy it. Texture initialization and
discard repair remain active, and resource destruction checks are separate.
The change adds one AtomicBool per bind group; struct padding was not measured.

The native inline regression covers dropped recordings, partial initialization,
later writes, same-layout group switches and destruction. Existing texture,
submission and bundle regressions also remain enabled.

## Checks and measurement scope

Build Wrench and the assertion-enabled owned native tests with the normal
vendored configuration and `-j10`. For correctness, select a hardware Vulkan ICD,
enable and attest Khronos synchronization validation, and run serially:

```sh
wrench animometer --inline --transient-target --triangles 100000 \
  --validation --check --warmup 1 --iterations 1
wrench animometer --inline --dynamic-offsets --transient-target \
  --triangles 100000 --validation --check --warmup 1 --iterations 1
cargo test -p wgpu_shared --test bundle_replay -- --ignored --test-threads=1
```

Time separately without validation using five warmups and thirty measured
frames, with alternating baseline/candidate order and a fixed CPU. Encoding,
submission and completion wait are separate intervals. The 256x256 offscreen
test excludes browser JS/IPC, WR composition and presentation; it does not
establish a Firefox CPU/FPS improvement. Release Wrench uses opt-level 2 and
LTO off. The existing initial experiments had noisy static timings and about
2.6% lower dynamic encoding time for initialization caching; do not interpret
that as a guaranteed production speedup.
