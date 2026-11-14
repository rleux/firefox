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
The change adds one AtomicBool per bind group. Wrench DWARF reports a total
BindGroup size of 296 bytes before and 304 bytes after this change; allocator
size-class overhead and Firefox struct layout were not measured.

The native inline regression covers dropped recordings, partial initialization,
later writes, same-layout group switches and destruction. Existing texture,
submission and bundle regressions also remain enabled.

## Required and occupied slots

Vertex state keeps required/occupied masks; bind-layout state keeps its occupied
mask. Existing pipeline/vertex setters update required masks and limits, slot
set/clear updates occupancy, and bundle resets start with empty masks. Draws
still check the first missing required buffer, combined slot limits and their
actual vertex/instance bounds. The fixed HAL capacities (16 vertex slots and
8 groups) fit u32 masks. The regression clears high slots and required slots,
checks combined limits and out-of-range draws, then verifies a valid draw.

This adds two u32 fields to vertex state and one to the binder manager. Earlier
independent dynamic pairs improved about 3.4%; repeated static results included
regressions. This is not an established general static-path speedup.

## Resource lifetime validation reuse

Bind groups cache successful buffer/texture liveness validation against the
existing device snatch-lock generation. The read guard prevents destruction
while encoding, so repeat bindings in that generation reuse the result.
Every exclusive snatch-lock acquisition advances the saturating generation;
even unrelated canvas retirement conservatively invalidates cached results.
At usize::MAX the cache always misses. The group state and backend handle are
still checked on every call, and failed resource validation publishes no hit.

Submission rechecks every referenced bind group, including overwritten groups.
It can reuse encoding's validation only if the generation is unchanged.
Separate submission mapping/state checks remain active. One AtomicUsize is
added per group, without another lock or reverse-reference graph. The inline
regression warms caches, destroys resources before/after encoding, checks an
overwritten binding, rejects a mapped buffer and succeeds after unmap. The
texture regression also executes inline draws around discard and destruction.

Earlier independent transient-target measurements reduced static submission
by 26-37%, about 0.6-1.0 ms in those runs, and dynamic encoding about 2.6%.
Those percentages apply to their respective intervals, not total browser CPU.
A persistent-target control was stronger and is not substituted for canvas
retirement measurements.

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
