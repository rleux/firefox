# Trusted native texture handoff

This downstream wgpu-core extension supports browser-owned presentation images
shared by isolated core devices on one native Vulkan device. It supplies allocation
capability and lifetime primitives; it does not publish canvases or enable the
default-off shared-WebGPU preference. Ordinary independent-device export and
readback remain the fallback.

## Why the vendor hooks are needed

Keeping an `Arc<Texture>` does not preserve the native allocation after
`Texture::destroy` moves that allocation into queue retirement. Keeping
`Texture::as_hal` alive instead holds the device-wide snatch lock. Neither is a
consumer lease.

Allocating through HAL and importing a writable producer with
`create_texture_from_hal` also loses ordinary core initialization: imports have
`TextureClearMode::None`. In particular, `StoreOp::Discard` followed by a load or
copy needs core's lazy zero initialization. Marking an imported allocation
initialized, or recording only a usage transition, does not repair that behavior.
These approaches must not replace the ordinary producer allocator.

DMA-BUF export/import produces an alias and introduces external ownership rules.
It does not implement the same-`VkImage`, same-`VkDevice` handoff described here.
The existing independent-device transport remains separate.

## API and ownership

`Device::create_texture_with_internal_sampling` validates the original descriptor
and its union with `TEXTURE_BINDING`, then uses the ordinary allocator, clear
views, initialization tracker, counters, and resource tracker. Only the native
allocation receives the extra sampling capability. The stored public descriptor
and its view, bind-group, and command validation permissions stay unchanged.
The internal remote registry bridge registers this texture and returns its Arc
to the trusted presentation owner. Allocation or validation failure is returned
to that owner; it must select fallback or report the failure.

`Texture::destroy_and_retain` removes the native allocation using the existing
snatch lock and invalidates producer aliases exactly as `Texture::destroy` does.
It returns `Arc<DestroyedTexture>` and schedules another reference through the
same pending-write or latest-submission retirement path. The keeper also owns
the original device, clear views, and deferred view/bind-group cleanup. The
original native image and memory are destroyed once, when the last reference
retires. Repeated destruction returns `None`.

Ordinary `Texture::destroy` retains its by-value temporary resource: only the
explicit retained path allocates and shares an Arc. Ordinary allocation does not
clone an extra descriptor or change native usage.

`DestroyedTexture::as_hal` is an unsafe borrow tied to the keeper, without a
global or snatch lock. It does not certify initialization, layout, or completion.
The trusted caller must satisfy all of the following:

1. Serialize every producer queue write, submission, and destruction while
   preparing and sealing the image. A native queue mutex alone does not prevent
   another producer core submission between preparation and sealing.
2. Initialize every subresource exposed to the consumer using ordinary core
   commands. For the admitted single-mip, single-layer color canvas, record a
   full `Load`/`Store` render pass. This invokes normal lazy initialization for
   untouched or discarded data. Record an explicit transition to `RESOURCE`
   afterward. A transition alone does not initialize memory.
3. Successfully submit those commands, seal the producer texture, and await the
   owning producer queue's actual successful completion before importing the
   consumer as initialized in `RESOURCE`. Never compare submission indices from
   different core devices. Abandon publication on submission failure or loss.
4. Confirm both core devices use the identical native `VkDevice`. Import the
   identical `VkImage` using `TextureMemory::External` and a drop callback that
   retains the keeper. The consumer descriptor must describe only capabilities
   actually present in the original allocation.
5. Retain that callback through the consumer's ordinary GPU retirement. Do not
   manually free the original image or memory, mutate the allocation through
   another alias, or reuse it while a consumer can access it. Publication removal
   alone does not establish completion.

The hooks do not implement lease budgets, canvas admission, synchronization
receipts, layout recovery after failure, loss propagation between clients,
cross-process authority, or image recycling. Those belong to the browser-owned
integration. In particular, a weak keeper reference becoming un-upgradeable does
not certify that its destructor has finished on another thread.

## Focused proof and maintenance

`gfx/wr/wgpu_shared/tests/texture_handoff.rs` is an opt-in native fixture. Its
dev dependencies enable native resource counters. The shared-device crate already
enables WGSL for runtime timestamp normalization; the fixture also uses it for
sampling checks. The production WR shader catalog continues to use
glslang, spirv-val, and trusted SPIR-V passthrough without changes.

Run remotely with the Khronos synchronization validation layer enabled:

```sh
cargo test --manifest-path gfx/wr/Cargo.toml -p wgpu_shared --test texture_handoff -- --ignored --nocapture --test-threads=1
```

The fixture checks identical native handles and sampled pixels for untouched,
discard-then-load, discard-then-copy, and rendered images; public usage rejection;
recorded command/view/bind-group invalidation; pending-write and submitted
producer retention; consumer retention; and balanced native texture counters.
It is a core-device proof, not browser canvas or WR production-shader acceptance.
Verify actual layer activation and inspect the complete log for VUID/SYNC errors.

The vendor patch is against the wgpu revision pinned in the Cargo manifests
(`49325ca0a13bdc0787b1601138096cce743dd29e`). Keep it separate from Gecko-owned
integration changes. On each vendor update, recheck descriptor validation,
`TextureClearMode`, snatch/submit locking, destruction scheduling, deferred
view/bind-group cleanup, HAL import/drop callbacks, and raw-resource counters.
Update each changed vendor file's `.cargo-checksum.json` hash. The APIs are
downstream trusted hooks, not an upstream compatibility promise; prefer upstream
equivalents if available and rerun the native fixture after adapting them.
