/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{
    tests::{validation_logging, VALIDATION_ERRORS as ERRORS},
    Options,
};
use crate::device::GpuBackendConfig;
use crate::render_api::{RenderApi, Transaction};
use api::*;
use std::sync::{atomic::Ordering, mpsc};

#[path = "vulkan_blend_tests.rs"]
mod blends;
#[path = "vulkan_clip_tests.rs"]
mod clips;
#[path = "vulkan_filter_tests.rs"]
mod filters;
#[path = "vulkan_glyph_tests.rs"]
mod glyphs;
#[cfg(all(target_os="linux",feature="debugger"))]
#[path = "vulkan_presentation_tests.rs"]
mod presentation;
#[path = "vulkan_image_tests.rs"]
mod images;
#[path = "vulkan_external_buffer_tests.rs"]
mod external_buffers;

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

fn renderer() -> (Renderer, RenderApi, mpsc::Receiver<()>) {
    renderer_with_clear_color(ColorF::WHITE)
}

fn renderer_with_clear_color(clear_color: ColorF) -> (Renderer, RenderApi, mpsc::Receiver<()>) {
    validation_logging();
    let (tx, rx) = mpsc::channel();
    let (renderer, sender) = crate::create_webrender_instance(
        GpuBackendConfig::Vulkan(Options {
            validation: true,
            ..Default::default()
        }),
        Box::new(Notice(tx)),
        crate::WebRenderOptions {
            clear_color,
            enable_subpixel_aa: false,
            enable_debugger: false,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    (renderer, sender.create_api(), rx)
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_draws_a_display_list_through_device_construction() {
    render_display_list(false);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_streams_draws_when_shared_instances_are_requested() {
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
    assert!(renderer.device.get_capabilities().supports_base_instance);
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
        let pixels = capture(&mut renderer, size);
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

fn capture(renderer: &mut Renderer, size: DeviceIntSize) -> Vec<u8> {
    let mut pixels = vec![0; size.width as usize * size.height as usize * 4];
    renderer.device.read_pixels_into(
        crate::device::ReadTarget::Default,
        DeviceIntRect::from_size(size).cast_unit(),
        ImageFormat::RGBA8,
        &mut pixels,
    );
    assert!(
        renderer.device.failure().is_none(),
        "{:?}",
        renderer.device.failure()
    );
    pixels
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_resource_only_submission_errors_are_ready_for_update_recovery() {
    let (mut renderer, mut api, rx) = renderer();
    assert!(renderer.active_documents.is_empty());
    assert!(renderer.device.failure().is_none());
    renderer.device.wgpu_test_fail_submit();
    assert!(renderer.device.failure().is_none());
    renderer.pending_result_msg = Some(ResultMsg::UpdateResources {
        resource_updates: crate::internal_types::ResourceUpdateList {
            native_surface_updates: Vec::new(),
            texture_updates: crate::internal_types::TextureUpdateList::new(),
        },
        memory_pressure: false,
        discard_active_documents: false,
        trim_upload_buffers: false,
    });
    renderer.update();
    let update_errors = mem::take(&mut renderer.renderer_errors);
    assert!(update_errors
        .iter()
        .any(|e| matches!(e, RendererError::Device(_))));
    assert!(renderer
        .render(DeviceIntSize::new(4, 4), 0)
        .unwrap_err()
        .iter()
        .any(|e| matches!(e, RendererError::Device(_))));
    let size = DeviceIntSize::new(4, 4);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin(60.0);
    let mut transaction = Transaction::new();
    transaction.set_root_pipeline(pipeline);
    transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
    transaction.generate_frame(1, true, false, RenderReasons::TESTING);
    api.send_transaction(document, transaction);
    rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
    renderer.debug_flags.insert(DebugFlags::SKIP_RENDERING);
    renderer.update();
    assert!(!renderer.active_documents.is_empty());
    assert!(renderer.render(size, 0).is_err());
    api.delete_document(document);
    renderer.deinit();
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_idle_completion_poll_errors_are_ready_for_update_recovery() {
    let (mut renderer, _api, _rx) = renderer();
    let fence = renderer.device.create_fence().unwrap();
    assert!(renderer.device.failure().is_none());
    renderer.device.wgpu_test_lose_device();
    assert_eq!(
        renderer.device.poll_fence(&fence),
        crate::device::FenceStatus::Error
    );
    renderer.update();
    assert!(renderer
        .renderer_errors
        .iter()
        .any(|e| matches!(e, RendererError::Device(_))));
    assert!(renderer.render(DeviceIntSize::new(4, 4), 0).is_err());
    renderer.device.delete_fence(fence);
    renderer.deinit();
}
