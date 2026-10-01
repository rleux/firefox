/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::GpuBackendConfig;
use crate::device::vulkan::{
    Options,
    tests::{validation_logging, ERRORS},
};
use crate::render_api::Transaction;
use api::*;
use std::sync::{mpsc, atomic::Ordering};

struct Notice(mpsc::Sender<()>);

impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self(self.0.clone()))
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {
        let _ = self.0.send(());
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_draws_a_display_list_through_device_construction() {
    render_display_list(false);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_uses_pooled_uploads_when_shared_instances_are_requested() {
    render_display_list(true);
}

fn render_display_list(enable_shared_instance_buffer: bool) {
    validation_logging();
    let (tx, rx) = mpsc::channel();
    let options = crate::WebRenderOptions {
        enable_subpixel_aa: false,
        enable_debugger: false,
        enable_shared_instance_buffer,
        ..Default::default()
    };
    let (mut renderer, sender) = crate::create_webrender_instance(
        GpuBackendConfig::Vulkan(Options {
            validation: true,
            ..Default::default()
        }),
        Box::new(Notice(tx)),
        options,
        None,
    )
    .unwrap();
    assert!(!renderer.use_shared_instance_buffer);
    assert!(renderer.vaos.shared_instance_buffer.is_none());
    let mut api = sender.create_api();
    let size = DeviceIntSize::new(32, 32);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let rect = LayoutRect::from_size(LayoutSize::new(32.0, 32.0));
    let info = CommonItemProperties {
        clip_rect: rect,
        clip_chain_id: ClipChainId::INVALID,
        spatial_id: SpatialId::root_scroll_node(pipeline),
        flags: PrimitiveFlags::default(),
    };
    for epoch in 0..2 {
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        builder.push_rect(&info, rect, ColorF::new(1.0, 0.0, 0.0, 1.0));
        let inset = LayoutRect::from_origin_and_size(
            LayoutPoint::new(8.0, 8.0),
            LayoutSize::new(16.0, 16.0),
        );
        let alpha = if epoch == 0 { 0.5 } else { 1.0 };
        builder.push_rect(&info, inset, ColorF::new(0.0, 1.0, 0.0, alpha));
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        renderer.render(size, 0).unwrap();
        let output = renderer.device.vulkan_test_output().unwrap();
        let pixels = output
            .readback(DeviceIntRect::from_size(size))
            .unwrap()
            .wait()
            .unwrap();
        for (index, pixel) in pixels.chunks_exact(4).enumerate() {
            if (8..24).contains(&(index % 32)) && (8..24).contains(&(index / 32)) {
                let expected = if epoch == 0 {
                    [127u8, 128, 0, 255]
                } else {
                    [0, 255, 0, 255]
                };
                assert!(
                    pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1),
                    "pixel {}: {:?}",
                    index,
                    pixel
                );
            } else {
                assert_eq!(pixel, [255, 0, 0, 255], "pixel {}", index);
            }
        }
    }
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
