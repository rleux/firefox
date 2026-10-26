/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

struct CanvasCase {
    texture: Arc<core::resource::Texture>,
    lease: Option<Arc<dyn hal::shared_image::ImageLease>>,
    readback: Arc<core::resource::Buffer>,
    partial: bool,
    pixel: [u8; 4],
}

#[derive(Default)]
pub(super) struct CanvasCases {
    cases: Vec<CanvasCase>,
}

fn graphics_counts(graphics: &hal::DynOpenDevice) -> (isize, isize) {
    let counters = graphics.device.get_internal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

fn logical_counts(device: &core::device::Device) -> (isize, isize) {
    let counters = device.get_hal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

impl CanvasCases {
    pub(super) fn run(
        &mut self,
        device: &Arc<core::device::Device>,
        queue: &Arc<core::device::queue::Queue>,
        graphics: &Arc<hal::DynOpenDevice>,
        provider: &GraphicsProvider,
    ) {
        let counter = wgt::InternalCounter::new();
        counter.add(1);
        assert_eq!(
            counter.read(),
            1,
            "Canvas tests require wgpu-types/counters"
        );
        let graphics_before = graphics_counts(graphics);
        let logical_before = logical_counts(device);
        device.push_error_scope(wgt::error::ErrorFilter::Validation);
        let size = wgt::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        };
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            for render_pass in [true, false] {
                for partial in [false, true] {
                    let mut usage = wgt::TextureUsages::COPY_SRC | wgt::TextureUsages::COPY_DST;
                    if render_pass {
                        usage |= wgt::TextureUsages::RENDER_ATTACHMENT;
                    }
                    let desc = core::resource::TextureDescriptor {
                        label: Some(Cow::Borrowed("native canvas initialization")),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgt::TextureDimension::D2,
                        format,
                        usage,
                        view_formats: vec![format.add_srgb_suffix()],
                    };
                    let (texture, lease) =
                        unsafe { device.create_texture_with_shared_image(&desc, graphics.clone()) };
                    let lease = lease.expect("Native canvas allocation must succeed");
                    assert_eq!(texture.descriptor().usage, usage);
                    assert_eq!(texture.descriptor().view_formats, desc.view_formats);
                    let readback = device.create_buffer(&core::resource::BufferDescriptor {
                        label: None,
                        size: 512,
                        usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
                        mapped_at_creation: false,
                    });
                    self.cases.push(CanvasCase {
                        texture,
                        lease: Some(lease),
                        readback,
                        partial,
                        pixel: match format {
                            wgt::TextureFormat::Rgba8Unorm => [17, 51, 102, 255],
                            _ => [102, 51, 17, 255],
                        },
                    });
                }
            }
        }
        let allocated = graphics_counts(graphics);
        assert_eq!(allocated.0, graphics_before.0 + 8);
        assert!(allocated.1 > graphics_before.1);
        assert_eq!(
            logical_counts(device),
            (logical_before.0 + 8, logical_before.1)
        );

        let encoder = device.create_command_encoder(&Default::default());
        for case in &self.cases {
            if case.partial {
                queue.write_texture(
                    wgt::TexelCopyTextureInfo {
                        texture: case.texture.clone(),
                        mip_level: 0,
                        origin: wgt::Origin3d { x: 1, y: 1, z: 0 },
                        aspect: wgt::TextureAspect::All,
                    },
                    &case.pixel,
                    &wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: None,
                        rows_per_image: None,
                    },
                    &wgt::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
            }
            encoder.copy_texture_to_buffer(
                &wgt::TexelCopyTextureInfo {
                    texture: case.texture.clone(),
                    mip_level: 0,
                    origin: wgt::Origin3d::ZERO,
                    aspect: wgt::TextureAspect::All,
                },
                &wgt::TexelCopyBufferInfo {
                    buffer: case.readback.clone(),
                    layout: wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: None,
                    },
                },
                &size,
            );
        }
        let commands = encoder.finish(&Default::default());
        let submission = queue.submit(&[commands])
            .expect("Valid test copy must be submitted");
        let mapped = Arc::new(Mutex::new(Vec::new()));
        for case in &self.cases {
            let mapped = mapped.clone();
            case.readback.map_async(
                wgt::MapMode::Read,
                0,
                Some(512),
                Some(Box::new(move |result| mapped.lock().unwrap().push(result))),
            );
        }
        provider.release_receipts();
        device
            .poll(wgt::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(std::time::Duration::from_secs(30)),
            })
            .unwrap();
        assert!(device.pop_error_scope().unwrap().is_none());
        let mapped = mapped.lock().unwrap();
        assert_eq!(mapped.len(), 8);
        assert!(
            mapped.iter().all(Result::is_ok),
            "Canvas mapping failed: {mapped:?}"
        );
        drop(mapped);
        for case in &self.cases {
            let bytes = case
                .readback
                .get_mapped_range(0, Some(512))
                .unwrap()
                .read_slice()
                .to_vec();
            for (index, offset) in [0, 4, 256, 260].iter().copied().enumerate() {
                let expected = if case.partial && index == 3 {
                    case.pixel
                } else {
                    [0; 4]
                };
                assert_eq!(
                    &bytes[offset..offset + 4],
                    &expected,
                    "Canvas {:?}, usage {:?}, partial {}, pixel {index}",
                    case.texture.descriptor().format,
                    case.texture.descriptor().usage,
                    case.partial,
                );
            }
            case.readback.unmap();
            case.readback.destroy();
            case.texture.destroy();
        }
        device.poll(wgt::PollType::Poll).unwrap();
        assert_eq!(logical_counts(device), logical_before);
        assert_eq!(graphics_counts(graphics), allocated);
        for case in &mut self.cases {
            drop(case.lease.take());
        }
        assert_eq!(graphics_counts(graphics), graphics_before);
    }
}
