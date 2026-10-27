/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::unified;
use std::{
    rc::Rc,
    sync::{mpsc, Arc, Weak},
    time::Duration,
};
use webrender::api::{units::*, *};
use webrender::render_api::{RenderApi, Transaction};
use webrender::vulkan::{BufferPool, SubmissionQueue, Texture, TextureFilter};
use webrender_bindings::vulkan_native::WrVulkanNativePublication;
use wgpu_bindings::unified::{FinalizedCanvas, ProcessOwner};
use wgpu_core_remote::{global::Global, id::*};
use wgpu_hal::shared_image::ImageLease;
use wgpu_types as wgt;

const PIXELS: [[u8; 4]; 6] = [
    [17, 51, 102, 255],
    [100, 60, 20, 128],
    [7, 31, 53, 64],
    [137, 11, 83, 192],
    [23, 67, 101, 255],
    [11, 19, 31, 64],
];
const BACKGROUND: [u8; 4] = [23, 47, 89, 255];
const SENTINEL: [u8; 4] = [3, 29, 71, 255];

fn counts(owner: &Arc<ProcessOwner>) -> (isize, isize) {
    let owner = owner.graphics();
    let counters = owner.device.get_internal_counters();
    (counters.textures.read(), counters.texture_memory.read())
}

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
struct Handles([ExternalTextureHandle; 2]);
impl ExternalImageHandler for Handles {
    fn lock(&mut self, id: ExternalImageId, _: u8, _: bool) -> ExternalImage<'_> {
        ExternalImage {
            uv: TexelRect::new(0., 0., 3., 2.),
            source: ExternalImageSource::NativeTexture(self.0[(id.0 - 200) as usize]),
        }
    }
    fn unlock(&mut self, _: ExternalImageId, _: u8) {}
}

struct DrawContext {
    renderer: Option<webrender::Renderer>,
    api: RenderApi,
    document: DocumentId,
    ready: mpsc::Receiver<()>,
    frame: u64,
}
impl DrawContext {
    fn new(owner: &Arc<ProcessOwner>) -> Self {
        let device = unified::graphics(owner);
        let (tx, ready) = mpsc::channel();
        let (renderer, sender) = webrender::create_webrender_instance(
            webrender::GpuBackendConfig::VulkanShared(device),
            Box::new(Notice(tx)),
            webrender::WebRenderOptions {
                enable_debugger: false,
                enable_subpixel_aa: false,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert!(Arc::ptr_eq(
            &renderer.wgpu_external_textures().unwrap().device().hal_owner(),
            &owner.graphics()
        ));
        let mut api = sender.create_api();
        let document = api.add_document(DeviceIntSize::new(6, 2));
        Self {
            renderer: Some(renderer),
            api,
            document,
            ready,
            frame: 0,
        }
    }
    fn render(&mut self, handles: [ExternalTextureHandle; 2], format: ImageFormat) {
        let renderer = self.renderer.as_mut().unwrap();
        renderer.set_external_image_handler(Box::new(Handles(handles)));
        let pipeline = PipelineId(0, 0);
        let mut transaction = Transaction::new();
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.);
        let full = LayoutRect::from_size(LayoutSize::new(6., 2.));
        let common = CommonItemProperties::new(full, SpaceAndClipInfo::root_scroll(pipeline));
        builder.push_rect(
            &common,
            full,
            ColorF::new(
                BACKGROUND[0] as f32 / 255.,
                BACKGROUND[1] as f32 / 255.,
                BACKGROUND[2] as f32 / 255.,
                1.,
            ),
        );
        for panel in 0..2 {
            let key = self.api.generate_image_key();
            transaction.add_image(
                key,
                ImageDescriptor::new(3, 2, format, ImageDescriptorFlags::empty()),
                ImageData::External(ExternalImageData {
                    id: ExternalImageId(200 + panel),
                    channel_index: 0,
                    image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                    normalized_uvs: false,
                }),
                None,
            );
            builder.push_image(
                &common,
                LayoutRect::from_origin_and_size(LayoutPoint::new(panel as f32 * 3., 0.), LayoutSize::new(3., 2.)),
                ImageRendering::Pixelated,
                AlphaType::PremultipliedAlpha,
                key,
                ColorF::WHITE,
            );
        }
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(self.frame as u32), self.api.get_namespace_id(), builder.end());
        self.frame += 1;
        transaction.generate_frame(self.frame, true, false, RenderReasons::TESTING);
        self.api.send_transaction(self.document, transaction);
        self.ready.recv_timeout(Duration::from_secs(15)).unwrap();
        renderer.update();
        let result = renderer.render(DeviceIntSize::new(6, 2), 0).unwrap();
        assert!(!result.external_images_pending);
    }
    fn pixels(&mut self) -> Vec<u8> {
        self.renderer
            .as_mut()
            .unwrap()
            .read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(6, 2)))
    }
    fn finish(mut self) {
        self.api.delete_document(self.document);
        self.renderer.take().unwrap().deinit();
    }
}

fn source(
    owner: &Arc<ProcessOwner>,
    global: &Global,
    format: wgt::TextureFormat,
    srgb: bool,
    copy_src: bool,
) -> (FinalizedCanvas, wgpu_core::SubmissionIndex, Weak<dyn ImageLease>) {
    let device = global.resolve_device_id(DeviceId::zip(1, 1));
    let queue = global.resolve_queue_id(QueueId::zip(1, 1));
    let before = counts(owner);
    let logical_before = device.get_hal_counters().textures.read();
    let mut usage = wgt::TextureUsages::COPY_DST | wgt::TextureUsages::RENDER_ATTACHMENT;
    if copy_src {
        usage |= wgt::TextureUsages::COPY_SRC;
    }
    let (texture, lease) = unsafe {
        device.create_texture_with_shared_image(
            &wgpu_core::resource::TextureDescriptor {
                label: None,
                size: wgt::Extent3d {
                    width: 3,
                    height: 2,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format,
                usage,
                view_formats: if srgb {
                    vec![format.add_srgb_suffix()]
                } else {
                    Vec::new()
                },
            },
            owner.graphics(),
        )
    };
    let lease = lease.unwrap();
    assert_eq!(device.get_hal_counters().textures.read(), logical_before + 1);
    assert_eq!(counts(owner).0, before.0 + 1);
    assert!(counts(owner).1 > before.1);
    let bytes = PIXELS
        .iter()
        .flat_map(|pixel| {
            let mut pixel = *pixel;
            if format == wgt::TextureFormat::Bgra8Unorm {
                pixel.swap(0, 2);
            }
            pixel
        })
        .collect::<Vec<_>>();
    queue.write_texture(
        wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::All,
        },
        &bytes,
        &wgt::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(12),
            rows_per_image: None,
        },
        &wgt::Extent3d {
            width: 3,
            height: 2,
            depth_or_array_layers: 1,
        },
    );
    let weak = Arc::downgrade(&lease);
    let finalized = unsafe { queue.finalize_shared_canvas(&texture, lease) }.unwrap();
    let index = finalized.submission_index();
    (finalized, index, weak)
}
fn retire(global: &Global, index: wgpu_core::SubmissionIndex) {
    if global
        .resolve_device_id(DeviceId::zip(1, 1))
        .poll(wgt::PollType::Wait {
            submission_index: Some(index),
            timeout: Some(Duration::from_secs(30)),
        })
        .is_err()
    {
        std::process::exit(88);
    }
}

pub struct Fixture {
    owner: Arc<ProcessOwner>,
    foreign_owner: Arc<ProcessOwner>,
    actor: Option<Global>,
    draw: Option<DrawContext>,
    foreign: Option<DrawContext>,
    sentinel_handle: ExternalTextureHandle,
    weak: Option<Weak<dyn ImageLease>>,
    bgra: bool,
    srgb: bool,
    copy_src: bool,
    handles: [ExternalTextureHandle; 2],
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_new() -> *mut Fixture {
    let owner = unified::owner();
    let foreign_owner = unified::owner();
    let foreign = DrawContext::new(&foreign_owner);
    let registry = foreign.renderer.as_ref().unwrap().wgpu_external_textures().unwrap();
    let device = registry.device();
    let sentinel = Texture::new(
        device,
        3,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let pool = Rc::new(BufferPool::new(device));
    let queue = SubmissionQueue::new(&pool, 1).unwrap();
    sentinel
        .upload(
            &queue,
            DeviceIntRect::from_size(DeviceIntSize::new(3, 2)),
            &SENTINEL.repeat(6),
            None,
            0,
            Some(ImageFormat::RGBA8),
        )
        .unwrap();
    if queue.wait().is_err() {
        std::process::exit(88);
    }
    let sentinel_handle = registry.register(&sentinel).unwrap();
    drop((queue, pool, registry));
    let global = unified::actor(&owner, false);
    for wrong_owner in [false, true] {
        let (finalized, index, weak) = source(&owner, &global, wgt::TextureFormat::Rgba8Unorm, false, true);
        let result = unsafe {
            WrVulkanNativePublication::new(
                if wrong_owner {
                    foreign_owner.clone()
                } else {
                    owner.clone()
                },
                finalized,
                if wrong_owner {
                    wgt::TextureFormat::Rgba8Unorm
                } else {
                    wgt::TextureFormat::Bgra8Unorm
                },
            )
        };
        assert!(result.is_err());
        retire(&global, index);
        assert!(weak.upgrade().is_none());
        assert!(owner.is_ready());
    }
    drop(global);
    Box::into_raw(Box::new(Fixture {
        owner,
        foreign_owner,
        actor: None,
        draw: None,
        foreign: Some(foreign),
        sentinel_handle,
        weak: None,
        bgra: false,
        srgb: false,
        copy_src: false,
        handles: [ExternalTextureHandle(0); 2],
    }))
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_prepare(
    fixture: &mut Fixture,
    bgra: bool,
    srgb: bool,
    copy_src: bool,
) -> *mut WrVulkanNativePublication {
    assert!(fixture.draw.is_none() && fixture.actor.is_none());
    let global = unified::actor(&fixture.owner, false);
    fixture.draw = Some(DrawContext::new(&fixture.owner));
    let format = if bgra {
        wgt::TextureFormat::Bgra8Unorm
    } else {
        wgt::TextureFormat::Rgba8Unorm
    };
    let (finalized, index, weak) = source(&fixture.owner, &global, format, srgb, copy_src);
    let publication = unsafe {
        WrVulkanNativePublication::new(
            fixture.owner.clone(),
            finalized,
            if srgb { format.add_srgb_suffix() } else { format },
        )
    }
    .unwrap();
    retire(&global, index);
    assert_eq!(
        global
            .resolve_device_id(DeviceId::zip(1, 1))
            .get_hal_counters()
            .textures
            .read(),
        0
    );
    fixture.actor = Some(global);
    fixture.weak = Some(weak);
    fixture.bgra = bgra;
    fixture.srgb = srgb;
    fixture.copy_src = copy_src;
    Box::into_raw(Box::new(publication))
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_renderer(fixture: &mut Fixture, foreign: bool) -> &mut webrender::Renderer {
    if foreign {
        fixture.foreign.as_mut().unwrap()
    } else {
        fixture.draw.as_mut().unwrap()
    }
    .renderer
    .as_mut()
    .unwrap()
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_texture_count(fixture: &Fixture, foreign: bool) -> isize {
    counts(if foreign {
        &fixture.foreign_owner
    } else {
        &fixture.owner
    })
    .0
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_sentinel_handle(fixture: &Fixture) -> u64 {
    fixture.sentinel_handle.0
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_check_foreign(fixture: &mut Fixture) {
    let draw = fixture.foreign.as_mut().unwrap();
    draw.render([fixture.sentinel_handle; 2], ImageFormat::RGBA8);
    assert_eq!(draw.pixels(), SENTINEL.repeat(12));
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_drop_actor(fixture: &mut Fixture, close_owner: bool) {
    fixture
        .actor
        .as_ref()
        .unwrap()
        .resolve_device_id(DeviceId::zip(1, 1))
        .destroy();
    drop(fixture.actor.take());
    assert!(fixture.owner.is_ready());
    if close_owner {
        fixture.owner.begin_shutdown();
        assert!(!fixture.owner.is_ready());
    }
    assert!(fixture.weak.as_ref().unwrap().upgrade().is_some());
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_record(fixture: &mut Fixture, normal: u64, opaque: u64) {
    fixture.handles = [ExternalTextureHandle(normal), ExternalTextureHandle(opaque)];
    fixture.draw.as_mut().unwrap().render(
        [ExternalTextureHandle(normal), ExternalTextureHandle(opaque)],
        if fixture.bgra {
            ImageFormat::BGRA8
        } else {
            ImageFormat::RGBA8
        },
    );
}
fn linear(value: u8) -> f64 {
    let value = value as f64 / 255.;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_verify(fixture: &mut Fixture) {
    if fixture.copy_src {
        for handle in fixture.handles {
            assert!(fixture
                .draw
                .as_mut()
                .unwrap()
                .renderer
                .as_mut()
                .unwrap()
                .wgpu_snapshot_shared_image(handle, DeviceIntRect::from_size(DeviceIntSize::new(3, 2)))
                .is_err());
        }
    }
    assert!(fixture.weak.as_ref().unwrap().upgrade().is_some());
    let bytes = fixture.draw.as_mut().unwrap().pixels();
    assert_eq!(bytes.len(), 48);
    for row in 0..2 {
        for panel in 0..2 {
            for column in 0..3 {
                let pixel = PIXELS[row * 3 + column];
                let offset = (row * 6 + panel * 3 + column) * 4;
                for channel in 0..3 {
                    let source = if fixture.srgb {
                        linear(pixel[channel]) * 255.
                    } else {
                        pixel[channel] as f64
                    };
                    let blend = if panel == 0 {
                        BACKGROUND[channel] as f64 * (255 - pixel[3]) as f64 / 255.
                    } else {
                        0.
                    };
                    let expected = (source + blend).round() as i16;
                    let tolerance = if fixture.srgb || (panel == 0 && pixel[3] != 255) {
                        1
                    } else {
                        0
                    };
                    assert!(
                        (bytes[offset + channel] as i16 - expected).abs() <= tolerance,
                        "native shader row{row} panel{panel} column{column} channel{channel}: {} vs {expected}",
                        bytes[offset + channel]
                    );
                }
                assert_eq!(bytes[offset + 3], 255);
            }
        }
    }
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_finish(fixture: &mut Fixture) {
    assert!(fixture.actor.is_none());
    fixture.draw.take().unwrap().finish();
    assert!(fixture.weak.take().unwrap().upgrade().is_none());
}
#[no_mangle]
pub unsafe extern "C" fn wr_test_native_canvas_delete(fixture: *mut Fixture) {
    let mut fixture = Box::from_raw(fixture);
    if let Some(draw) = fixture.draw.take() {
        draw.finish();
    }
    if let Some(draw) = fixture.foreign.take() {
        draw.finish();
    }
    drop(fixture.actor.take());
    fixture.owner.begin_shutdown();
    fixture.foreign_owner.begin_shutdown();
    if fixture.owner.context().drain().is_err() || fixture.foreign_owner.context().drain().is_err() {
        std::process::exit(88);
    }
    drop(fixture);
}
