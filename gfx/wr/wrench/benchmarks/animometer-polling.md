# Animometer polling changes

The active Intel GPU-process trace attributed 27.98% of sampled CPU weight to Vulkan budget queries and 26.06% to core device polling. The categories overlap. It did not show another sustained framebuffer CPU-copy path. The profile was from ee880aec16e2; all changes below start from vulkan5-webgpu f30a9d9c8c12.

## Implemented separately

| Change | Production lines added/removed | Checks and measurements |
| --- | --- | --- |
| Allocation-only Linux Vulkan budget policy (`a02d23a98b2f`) | +7/-2 | Retain 95% creation guard; disable optional 99% budget-triggered loss checks. Native errors still propagate. Both owner and independent WebGPU descriptors agree. |
| One WR owner poll per renderer check (`db3f2f460513`) | +7/-3 | Registry uses the identical owner. Map/completion test covers registry-present and absent paths on Intel and NVIDIA. |
| Producer native-fence gate (`0a359a662703`) | +24/-12 | Native-pending lease checks avoid full core maintenance; completion still requires native fence, core callback and exclusive original-allocation ownership. Ready leases avoid another full poll after an earlier lease has drained that device. |

Total production delta: +38/-17, net +21 lines; no new dependency or third-party source change. Two hardware regression tests add 145 lines.

## Measured limits

The original same-machine Intel probe measured a budget query around 6.44 us versus 0.364 us for basic properties and confirmed an extra `/proc/meminfo` read per query. The policy removes that query from optional loss checks during polling/submission. The 95% creation guard still queries on resource creation, including suballocations; it is not restricted to `vkAllocateMemory` calls.

The blocked-submission test runs exactly the same 1000 pending polls before and after the producer gate. In DEBUG builds with synchronization validation enabled, three trials measured NVIDIA median 5.124153 -> 0.380109 ms. Three CPU-2-pinned AB/BA pairs on Intel measured 3.172568 -> 1.035206 ms. Desktop load varied. These are diagnostic loop timings, not optimized browser CPU time, FPS, GPU execution time or an Animometer score.

The short task-owned browser replay uses 100k bind groups, three warmup and twelve measured frames, and waits for submitted work. It does not reproduce a continuously queued public Animometer workload. One NVIDIA pair for policy showed CanvasRenderer sampled CPU delta 2.033 -> 2.343 ms; policy -> single-poll showed Renderer 3.943 -> 4.543 ms and CanvasRenderer 2.343 -> 2.449 ms. With 1 ms sampling, these do not establish an improvement. They also cannot establish a regression from one short pair. Single-poll -> producer-gate showed Renderer 4.543 -> 4.385 ms and CanvasRenderer 2.449 -> 2.599 ms, also inconclusive. Compositor had no separately named sampled thread. Matching-libxul symbolication found two Renderer poll stacks in the baseline and none in the other three captures (187-189 samples per thread); these are sampled stacks, not invocation counts. No budget stacks were sampled in these short NVIDIA runs. There is no end-to-end speedup claim.

Existing `wrench --vulkan bench --render` is OpenGL-only and panics before measurement. The separate Wrench Animometer directly exercises core and does not exercise WR's owner polling; its encoding timings are not evidence for these changes.

## Review and deferred ideas

The owner and native registry share the same Arc<Device>. Registry polling still drives normal core maintenance, mapping/completion callbacks, image acquisition and retirement, including error abandonment. There is a direct-poll fallback without a registry.

Producer callbacks stay on the existing owner/actor thread. The shared-image worker only inspects native completion and callback/alias state. Native errors still poll core then quarantine the publication and retire its context. The independent general WebGPU device-maintenance timer remains active for unrelated mapping and loss callbacks. No time cache, new queue locking, image-layout change, image reuse or copy optimization is introduced.

The empty canvas-seal Load/Store pass stays. It initializes untouched or discarded source pixels before the external consumer imports the image as initialized. A resource transition alone does not provide that initialization. Core's initialization tracker is private; inferring readiness from usage flags would be unsafe. Merging the seal into application commands requires a handoff API/layout/initialization design that also handles presentation without an application submission. This is deferred rather than bypassing initialization or modifying vendored core in this change.

A broader WR descriptor cache is deferred. The current cache already matches pipeline, immutable projection snapshot, texture views and filtering; equal transforms already preserve projection snapshots. Each new canvas publication has a new view identity, so reusing its old bind group would sample a retired frame. The profile attributes 2.07% inclusive to bind-group creation, but about 1.07 percentage points have truncated callers. About 0.77 pp is recovered under normal WR draw and 0.23 pp under clear draws. Trace actual repeated binding keys before adding another retention cache. This is a much smaller measured target than budget polling.

The Vulkan command pool already resets with empty flags, retaining its storage. Changing RELEASE_RESOURCES cannot help because that flag is not used. Encoder/reset memset samples alone do not show a safe command-buffer reuse opportunity. General WR polling also services maps/callbacks, so skipping all polls solely from a renderer submission index would miss work. Native retirement-specific gating needs its own progress/callback proof. Vertex-vector zeroing is still not established as an active hotspot.

## Validation

Release Wrench and Firefox ThinLTO builds pass with `-j10`; Firefox explicitly recompiles the changed owned crates. Task-owned 100k bundle/canvas browser runs pass on NVIDIA with validation absent for timing and marked-process cleanup. NVIDIA shared publication, handoff, bundle, WR shared/retirement/polling and readiness unit gates pass with actual synchronization validation, no VUID/SYNC diagnostics. Intel passes the new pending test plus all four publication and four handoff tests, also with actual synchronization validation; its registry/fallback mapping test passed separately.

Not checked: artificial global low-memory pressure, injected hardware loss, Windows runtime, full MotionMark suite. Disabling the proactive budget-loss heuristic is the requested policy change; ordinary native errors remain handled.

## Reproduction

Run from `gfx/wr` with the normal vendored Cargo/toolchain configuration and a hardware Vulkan ICD. Enable and attest Khronos synchronization validation for correctness.

```sh
cargo test -p webrender --features vulkan,shared-webgpu shared_canvas_poll_waits_for_native_progress_then_drains_owner_callbacks -- --ignored --test-threads=1 --nocapture
cargo test -p webrender --features vulkan,shared-webgpu native_backend_poll_drives_completion_and_mapping_with_or_without_registry -- --ignored --test-threads=1 --nocapture
cargo test -p wgpu_shared --test canvas_publication -- --ignored --test-threads=1 --nocapture
cargo test -p wgpu_shared --test texture_handoff -- --ignored --test-threads=1 --nocapture
```

The pending-poll test prints diagnostic elapsed time with the chosen build/layer configuration. Keep the configuration identical across variants. Its semaphore guard always signals and drains the blocked native queue before destruction. Do not compare these debug+validation numbers with release browser utilization.

The saved source, build/test logs, review notes, symbolicated profiles, Intel pairs and release identity are under `vulkan5/animometer-cpu-20261114` on the analysis host; remote build artifacts are under `/home/mike/animometer-cpu-20261114`. The pre-change backup is the Git branch `vulkan5-webgpu-before-residual-cpu-20261114`. No user browser was installed or changed and no full MotionMark run was used.
