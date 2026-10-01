/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::GpuBackendConfig;
use crate::device::wgpu::{
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
        assert_eq!(renderer.render(size, 0).unwrap().present_result, None);
        let output = renderer.device.wgpu_test_output().unwrap();
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

#[test]
#[cfg(all(target_os = "linux", feature = "debugger"))]
#[ignore = "Requires 24-bit Xvfb, a presentation-capable Vulkan adapter and validation"]
fn vulkan_window_renderer_presents_display_lists_without_an_output_texture() {
    use crate::device::wgpu::{surface_testing, X11Window};
    use crate::PresentResult;
    use wgpu_hal::SurfaceError;
    validation_logging();
    let window = Rc::new(unsafe { X11Window::new() });
    let (tx, rx) = mpsc::channel();
    let mut options = crate::WebRenderOptions {
        enable_subpixel_aa: false,
        enable_debugger: false,
        ..Default::default()
    };
    if let CompositorConfig::Draw {
        max_partial_present_rects,
        draw_previous_partial_present_regions,
        ..
    } = &mut options.compositor_config
    {
        *max_partial_present_rects = 1;
        *draw_previous_partial_present_regions = true;
    }
    let (mut renderer, sender) = crate::create_webrender_instance(
        GpuBackendConfig::Vulkan(Options {
            window: Some(window.clone()),
            validation: true,
            ..Default::default()
        }),
        Box::new(Notice(tx)),
        options,
        None,
    )
    .unwrap();
    assert!(matches!(
        renderer.current_compositor_kind,
        CompositorKind::Draw {
            max_partial_present_rects: 0,
            ..
        }
    ));
    let mut api = sender.create_api();
    let document = api.add_document(DeviceIntSize::new(64, 48));
    assert_eq!(
        renderer.render(DeviceIntSize::new(64, 48), 0).unwrap().present_result,
        None,
    );
    let pipeline = PipelineId(0, 0);
    for (epoch, size) in [[64u32, 48], [64, 48], [32, 24], [32, 24]]
        .iter()
        .copied()
        .enumerate()
    {
        window.resize(size);
        let device_size = DeviceIntSize::new(size[0] as i32, size[1] as i32);
        let rect = LayoutRect::from_size(LayoutSize::new(size[0] as f32, size[1] as f32));
        let info = CommonItemProperties {
            clip_rect: rect,
            clip_chain_id: ClipChainId::INVALID,
            spatial_id: SpatialId::root_scroll_node(pipeline),
            flags: PrimitiveFlags::default(),
        };
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        builder.push_rect(&info, rect, ColorF::new(1.0, 0.0, 0.0, 1.0));
        let alpha = if epoch % 2 == 0 { 0.5 } else { 1.0 };
        builder.push_rect(
            &info,
            LayoutRect::from_origin_and_size(
                LayoutPoint::new(8.0, 8.0),
                LayoutSize::new((size[0] - 16) as f32, (size[1] - 16) as f32),
            ),
            ColorF::new(0.0, 1.0, 0.0, alpha),
        );
        let mut transaction = Transaction::new();
        transaction.set_document_view(DeviceIntRect::from_size(device_size));
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch as u32), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        assert_eq!(
            renderer.render(device_size, 0).unwrap().present_result,
            Some(PresentResult::Presented),
        );
        assert!(renderer.device.wgpu_test_output().is_none());
        let wait_for_pixels = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let pixels = window.pixels(size);
                let matches = pixels.iter().enumerate().all(|(index, &pixel)| {
                    let x = index as u32 % size[0];
                    let y = index as u32 / size[0];
                    if (8..size[0] - 8).contains(&x) && (8..size[1] - 8).contains(&y) {
                        let expected = if alpha == 1.0 {
                            [0u8, 255, 0]
                        } else {
                            [127, 128, 0]
                        };
                        [16, 8, 0].iter().zip(expected).all(|(&shift, channel)| {
                            ((pixel >> shift) as u8).abs_diff(channel) <= 1
                        })
                    } else {
                        pixel == 0xff0000
                    }
                });
                if matches {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "Presented Renderer pixels did not match frame {epoch}"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        wait_for_pixels();
        window.clear();
        assert_eq!(
            renderer.render(device_size, 0).unwrap().present_result,
            Some(PresentResult::Presented),
        );
        assert!(renderer.device.wgpu_test_output().is_none());
        wait_for_pixels();

        if epoch == 3 {
            for acquire in [true, false] {
                for (error, expected) in [
                    (SurfaceError::Timeout, PresentResult::Retry),
                    (SurfaceError::Outdated, PresentResult::Retry),
                    (SurfaceError::Occluded, PresentResult::Occluded),
                ] {
                    if acquire {
                        surface_testing::fail_acquire(error);
                    } else {
                        surface_testing::override_present(Err(error));
                    }
                    assert_eq!(
                        renderer.render(device_size, 0).unwrap().present_result,
                        Some(expected),
                    );
                    assert!(renderer.force_redraw);
                    assert!(renderer.device.failure().is_none());
                    assert_eq!(
                        renderer.render(DeviceIntSize::zero(), 0).unwrap().present_result,
                        None,
                    );
                    window.clear();
                    assert_eq!(
                        renderer.render(device_size, 0).unwrap().present_result,
                        Some(PresentResult::Presented),
                    );
                    assert!(renderer.device.wgpu_test_output().is_none());
                    wait_for_pixels();
                }
            }
            surface_testing::override_present(Ok(true));
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::Presented),
            );
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::Presented),
            );
            wait_for_pixels();

            window.resize([40, 30]);
            surface_testing::fail_acquire(SurfaceError::Outdated);
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::Retry),
            );
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::SizeMismatch),
            );
            window.resize(size);
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::Presented),
            );
            wait_for_pixels();

            surface_testing::override_present(Err(SurfaceError::Lost));
            let errors = renderer.render(device_size, 0).unwrap_err();
            assert!(errors.iter().any(|error| {
                matches!(error, RendererError::Device(message) if message.contains("surface was lost"))
            }));
        }
    }
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
