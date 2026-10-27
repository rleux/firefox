/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[test]
#[ignore = "Requires Vulkan DMA-BUF modifiers and validation"]
fn immutable_capabilities_share_cached_native_queries() {
    use crate::device::wgpu::tests::{validation_logging, ERRORS};
    use std::sync::atomic::Ordering;
    validation_logging();
    for shared in [false, true] {
        let (owner, _context) = image::tests::consumer_device(shared);
        let first = owner.dma_buf_capabilities().unwrap();
        let second = owner.dma_buf_capabilities().unwrap();
        assert!(std::sync::Arc::ptr_eq(first.native.formats(), second.native.formats()));
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DmaBufCapabilities>();
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            let a = owner
                .dma_buf_formats(format, wgt::TextureUses::RESOURCE)
                .unwrap();
            let b = owner
                .dma_buf_formats(format, wgt::TextureUses::RESOURCE)
                .unwrap();
            let values = |formats: Vec<DmaBufFormat>| {
                formats
                    .iter()
                    .map(|entry| {
                        (
                            entry.format(),
                            entry.usage(),
                            entry.modifier(),
                            entry.max_size(),
                            entry.max_resource_size(),
                            entry.exportable(),
                            entry.dedicated_only(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(values(a), values(b));
        }
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
