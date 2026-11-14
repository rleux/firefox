# Animometer: cache invalidation during browser canvas publication

The installed release at `ca1518a82532` still replayed bind groups in the GPU
process. The previous offscreen benchmark kept one target alive and missed the
canvas lifecycle that disabled the optimization. The installed source stamp and
the running browser's mapped libxul inode were checked; this was not a stale
installation.

## Cause and fix

The initial native cache used the device's snatch generation as a condition for
reuse. Any exclusive snatch acquisition changed that generation. A changed
generation forced full CPU replay, including 100,000 bind-group commands.

Shared canvas publication calls `Texture::destroy_and_retain()` in
`gfx/wr/wgpu_shared/src/canvas.rs::seal`. Normal core presentation also takes the
exclusive snatch lock. Thus unrelated per-frame attachment retirement invalidated
the cached validation even though the bundle's vertex/uniform buffers remained
alive. The original persistent-target test did not exercise this.

Commit `bf02686dfd0d` adds `--transient-target` to Wrench, destroying and recreating
an actual attachment every frame, including in full-image comparisons. It also
permits `--warmup 0` for first-use measurements. The same original binary with
these two modes reproduced warmed encoding increasing from 0.0045 ms with a
persistent target to 4.247 ms with retirement.

Commit `ee880aec16e2` retains the unique buffers and textures referenced by the
normalized bind-group, vertex and index commands alongside the first native
recording. After a generation change it validates only those resources under the
existing read guard, then reuses the recorded secondary commands. Native
admission already excludes external/AS bindings. Ordinary bind-group snatches
cascade from destruction of a retained buffer or texture; pipeline/sampler
ownership remains held by the bundle. Failed checks still reject execution.

Metadata comes from normalized commands, preserving the original queue-submit
validation path for overwritten unused bindings. Buffer initialization and
texture discard handling remain active. Query/state/unsupported-command
fallbacks retain ordinary CPU replay. Metadata is allocated once, with no new
crate and no allocation during repeated validation. The fix changes production
Rust by **+82 / -6, net +76 lines**, excluding tests, benchmark and checksums.

## Reproduction

```sh
wrench animometer --triangles 100000 --transient-target --output transient.json
wrench animometer --triangles 100000 --transient-target --dynamic-offsets --output dynamic.json
wrench animometer --triangles 100000 --transient-target --warmup 0 --iterations 2 --output first-use.json
wrench animometer --triangles 100000 --transient-target --validation --check --warmup 1 --iterations 1
```

Use explicit hardware ICDs. Correctness runs attest actual Khronos synchronization
validation. Timing runs attest the NVIDIA/Intel driver is loaded and that the
validation library is absent. Each warmed process uses five warmups and thirty
samples; five AB/BA process pairs are prewarmed and CPU-pinned. Encoding is
elapsed CPU-side work. The summed phases are update, encode, submit and
completion wait. They do not measure browser FPS or isolated GPU execution time.

## Wrench results

| Hardware and mode | Encoding, installed behavior → fix | Summed timed phases |
| --- | --- | --- |
| NVIDIA, transient static | 4.142 → 0.00745 ms | 5.004 → 0.835 ms |
| NVIDIA, transient dynamic | 3.748 → 0.00741 ms | 4.538 → 0.781 ms |
| Intel, transient static | 55.050 → 0.0744 ms | 75.681 → 21.999 ms |
| Intel, transient dynamic | 34.972 → 0.0750 ms | 55.936 → 20.785 ms |

NVIDIA uses RTX 5060 Ti driver 615.71.09, CPU 3; Intel uses Iris Xe RPL-P,
Mesa 26.2.6, i5-1340P CPU 2. The local desktop remained active and showed
background/frequency variation. Small differences are not causal claims. The
NVIDIA persistent control stayed around 0.0042 ms; Intel stayed around 0.049 ms.

First-use NVIDIA encoding increased from 15.744 to 16.407 ms; summed first-frame
phases increased from 17.622 to 18.434 ms. This is first bundle use in a prewarmed
measurement environment, not a full browser cold start. Metadata collection is a
one-time cost.

## Browser baseline evidence

A task-owned Firefox session created 100,000 bind groups and replayed one bundle
across real shared-canvas frames. It uses a simple triangle shader and a one-pixel
scissor to bound fragment work. It reproduces binding/command replay and the
canvas lifecycle rather than the public sample's complete image. The GPU process used NVIDIA Vulkan and shared
producer/consumer images. The display's original Off state remained unchanged,
so frame intervals are unsuitable for FPS claims.

Firefox's built-in profiler avoided the host's restriction on GDB attaching to
the owned GPU process. The matched baseline ran three canvas warmup frames
before starting 1 ms stack sampling, then twelve measured frames, with the
Khronos layer absent from GPU process maps.

Baseline GPU CanvasRenderer had 167 sampled stacks containing
`RenderBundle::execute_commands` and 90 containing `BindGroup::try_raw`, all 90
under that replay path. Its summed `threadCPUDelta` was 173.592 ms over the
profile. This includes the whole profiled thread interval and sample attribution;
it is not an exact call count or exact function duration. The original
validation-enabled profile independently showed the same replay path.

## Matched fixed browser result

The built Firefox at `ee880aec16e2`, BuildID `20261114115814`, ran the same
three warmup and twelve profiled canvas frames. NVIDIA was active and the
validation library was absent from the GPU process maps.

| CanvasRenderer profile metric | Installed release | Fixed release |
| --- | --- | --- |
| Summed thread CPU delta over profile | 173.592 ms | 5.779 ms |
| Wall-sampled stacks containing `execute_commands` | 167 | 0 |
| Wall-sampled stacks containing `BindGroup::try_raw` | 90 | 0 |

The observed thread CPU delta decreased about 30x for this isolated workload.
The profile includes the complete measured thread interval, not exact per-draw
cost; zero samples do not prove a function was never called. This is neither
whole-GPU-process utilization nor browser FPS. Both sessions preserved the same
display-off state and used real shared canvas publication/acquisition.

## Validation

- All nine native bundle tests pass on NVIDIA and Intel with synchronization
  validation and zero VUID/SYNC diagnostics.
- Normal, transient and transient-dynamic 100k full-image comparisons pass on
  NVIDIA. Transient static/dynamic comparisons pass on Intel with actual sync
  validation and matching baseline image hashes.
- The new native test exercises vertex/index input, unrelated texture retirement,
  changing uniform data, and destruction of vertex or index buffers. It requires
  the deferred encoding-error path, preserving early resource checks.
- Firefox rebuilt with actual modified core compilation, `-j10` and ThinLTO,
  source `ee880aec16e2c9fb92d1685c42281610ecf37d61`, BuildID `20261114115814`.
- The matched warmed browser profile passed and showed the result above.
- A separate browser correctness run with actual NVIDIA synchronization validation
  passed three warmup and twelve 100k bundle/canvas frames with zero VUID/SYNC
  diagnostics. Thirteen producer/consumer acquisitions were observed; the last
  producer was not observed acquired before shutdown. No task processes remained.

## Replacement release

The verified ThinLTO package is `firefox-162.0a1.en-US.linux-x86_64.tar.xz`,
106,444,484 bytes, BuildID `20261114115814`, source
`ee880aec16e2c9fb92d1685c42281610ecf37d61`. SHA-256 is
`8731205ad558513dbb11029781e7fdf0aa52ed343581c42f5331b9f1488226bc`.
Archive integrity and the local copy hash pass. Packaged ELF build IDs and all
allocated sections match the verified built binaries after packaging. No user
browser was installed/replaced, no global display/ptrace setting changed, and no
branch was pushed.

Further owned polling changes and their limits are recorded in
[Animometer polling changes](animometer-polling.md).
