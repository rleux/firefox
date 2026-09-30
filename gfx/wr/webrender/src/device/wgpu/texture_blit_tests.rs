/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{BufferPool, Options};
use super::super::tests::{validation_logging, ERRORS};
use std::sync::atomic::Ordering;

#[path = "mipmap_tests.rs"]
mod mipmaps;

struct Context {
    device: Rc<Device>,
    queue: SubmissionQueue,
    scratch: TexturePool,
    quad: Rc<Buffer>,
    samplers: Rc<Samplers>,
}

impl Context {
    fn new() -> Self {
        validation_logging();
        let device = Rc::new(
            Device::new(&Options {
                validation: true,
                ..Options::default()
            })
            .unwrap(),
        );
        eprintln!("Vulkan adapter: {:?}", device.info());
        let pool = Rc::new(BufferPool::new(&device));
        let queue = SubmissionQueue::new(&pool, 2).unwrap();
        let scratch = TexturePool::new(&device);
        let quad = Buffer::new(
            &device,
            &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
            wgt::BufferUses::VERTEX,
        )
        .unwrap();
        let samplers = Rc::new(Samplers::new(&device).unwrap());
        Self {
            device,
            queue,
            scratch,
            quad,
            samplers,
        }
    }

    fn blitter(&self, format: wgt::TextureFormat) -> TextureBlitter {
        TextureBlitter::new(&self.device, format, &self.quad, &self.samplers).unwrap()
    }

    fn texture(
        &self,
        width: u32,
        height: u32,
        format: wgt::TextureFormat,
        renderable: bool,
    ) -> Rc<Texture> {
        Texture::new(
            &self.device,
            width,
            height,
            format,
            TextureFilter::Nearest,
            renderable,
        )
        .unwrap()
    }

    fn record(&mut self, blitter: &TextureBlitter, blit: TextureBlit<'_>) -> Result<(), String> {
        blitter.record(
            &mut self.queue.recording()?,
            &self.queue,
            &mut self.scratch,
            blit,
        )
    }

    fn upload(&self, texture: &Rc<Texture>, bytes: &[u8]) {
        texture
            .upload(&self.queue, full(texture), bytes, None, 0, None)
            .unwrap();
    }
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> DeviceIntRect {
    DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(x, y), DeviceIntSize::new(w, h))
}

fn full(texture: &Texture) -> DeviceIntRect {
    rect(
        0,
        0,
        texture.size().width as i32,
        texture.size().height as i32,
    )
}

fn pixels(texture: &Rc<Texture>) -> Vec<u8> {
    texture.readback(full(texture)).unwrap().wait().unwrap()
}

fn close(actual: &[u8], expected: &[u8]) {
    assert_eq!(actual.len(), expected.len());
    for (a, b) in actual.iter().zip(expected) {
        assert!(a.abs_diff(*b) <= 1, "{:?} != {:?}", actual, expected);
    }
}

#[test]
fn source_clipping_preserves_scale_and_direction() {
    assert_eq!(
        clip_source_axis([-1, 3], [0, 8], 2, 8),
        Some([0.0, 2.0, 2.0, 6.0])
    );
    assert_eq!(
        clip_source_axis([3, -1], [0, 8], 2, 8),
        Some([2.0, 0.0, 2.0, 6.0])
    );
    assert_eq!(
        clip_source_axis([0, 2], [3, -1], 2, 2),
        Some([0.0, 2.0, 3.0, -1.0])
    );
    assert_eq!(
        clip_source_axis([0, 2], [-1, 3], 2, 2),
        Some([0.0, 2.0, -1.0, 3.0])
    );
    for (source, target) in [
        ([0, 0], [0, 2]),
        ([0, 2], [1, 1]),
        ([3, 4], [0, 2]),
        ([0, 2], [2, 4]),
    ] {
        assert_eq!(clip_source_axis(source, target, 2, 2), None);
    }
    assert!(clip_source_axis([i32::MIN, i32::MAX], [i32::MIN, i32::MAX], 2, 2).is_some());
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shader_blits_scale_clip_filter_and_flip() {
    let mut ctx = Context::new();
    let blitter = ctx.blitter(wgt::TextureFormat::Rgba8Unorm);
    let source = ctx.texture(2, 1, wgt::TextureFormat::Rgba8Unorm, false);
    ctx.upload(&source, &[255, 0, 0, 255, 0, 0, 255, 255]);
    for filter in [TextureFilter::Nearest, TextureFilter::Linear] {
        let target = ctx.texture(4, 1, source.format(), true);
        ctx.record(
            &blitter,
            TextureBlit {
                source: &source,
                target: &target,
                source_rect: full(&source),
                target_rect: full(&target),
                filter,
            },
        )
        .unwrap();
        ctx.queue.wait().unwrap();
        if filter == TextureFilter::Nearest {
            assert_eq!(
                pixels(&target),
                [255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255]
            );
        } else {
            close(
                &pixels(&target),
                &[
                    255, 0, 0, 255, 191, 0, 64, 255, 64, 0, 191, 255, 0, 0, 255, 255,
                ],
            );
        }
    }
    let target = ctx.texture(2, 1, source.format(), true);
    ctx.record(
        &blitter,
        TextureBlit {
            source: &source,
            target: &target,
            source_rect: full(&source),
            target_rect: rect(-1, 0, 4, 1),
            filter: TextureFilter::Linear,
        },
    )
    .unwrap();
    ctx.queue.wait().unwrap();
    close(&pixels(&target), &[191, 0, 64, 255, 64, 0, 191, 255]);

    let target = ctx.texture(3, 1, source.format(), true);
    ctx.record(
        &blitter,
        TextureBlit {
            source: &source,
            target: &target,
            source_rect: rect(-1, 0, 3, 1),
            target_rect: full(&target),
            filter: TextureFilter::Nearest,
        },
    )
    .unwrap();
    ctx.queue.wait().unwrap();
    assert_eq!(
        pixels(&target),
        [0, 0, 0, 0, 255, 0, 0, 255, 0, 0, 255, 255]
    );

    let source = ctx.texture(2, 2, source.format(), false);
    let colors = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
    ];
    ctx.upload(&source, &colors);
    let target = ctx.texture(2, 2, source.format(), true);
    ctx.record(
        &blitter,
        TextureBlit {
            source: &source,
            target: &target,
            source_rect: full(&source),
            target_rect: rect(2, 2, -2, -2),
            filter: TextureFilter::Nearest,
        },
    )
    .unwrap();
    ctx.queue.wait().unwrap();
    assert_eq!(
        pixels(&target),
        colors
            .chunks_exact(4)
            .rev()
            .flatten()
            .copied()
            .collect::<Vec<_>>()
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shader_blits_convert_formats_and_preserve_partial_targets() {
    let mut ctx = Context::new();
    let formats = [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
        wgt::TextureFormat::R8Unorm,
    ];
    for source_format in formats {
        let source = ctx.texture(1, 1, source_format, false);
        ctx.upload(
            &source,
            match source_format {
                wgt::TextureFormat::Rgba8Unorm => &[60, 40, 20, 128],
                wgt::TextureFormat::Bgra8Unorm => &[20, 40, 60, 128],
                _ => &[60],
            },
        );
        for target_format in formats {
            let blitter = ctx.blitter(target_format);
            let target = ctx.texture(3, 1, target_format, true);
            let channels = if target_format == wgt::TextureFormat::R8Unorm {
                1
            } else {
                4
            };
            ctx.upload(&target, &vec![7; 3 * channels]);
            ctx.record(
                &blitter,
                TextureBlit {
                    source: &source,
                    target: &target,
                    source_rect: full(&source),
                    target_rect: rect(1, 0, 1, 1),
                    filter: TextureFilter::Nearest,
                },
            )
            .unwrap();
            ctx.queue.wait().unwrap();
            let color = if source_format == wgt::TextureFormat::R8Unorm {
                [60, 0, 0, 255]
            } else {
                [60, 40, 20, 128]
            };
            let converted = match target_format {
                wgt::TextureFormat::Bgra8Unorm => vec![color[2], color[1], color[0], color[3]],
                wgt::TextureFormat::Rgba8Unorm => color.to_vec(),
                _ => vec![color[0]],
            };
            let mut expected = vec![7; 3 * channels];
            expected[channels..2 * channels].copy_from_slice(&converted);
            assert_eq!(pixels(&target), expected);
        }
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn overlapping_shader_blits_use_retained_scratch_images() {
    let mut ctx = Context::new();
    let blitter = ctx.blitter(wgt::TextureFormat::Rgba8Unorm);
    let target = ctx.texture(4, 1, wgt::TextureFormat::Rgba8Unorm, true);
    ctx.upload(
        &target,
        &[
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ],
    );
    for (source_rect, target_rect) in [
        (rect(0, 0, 3, 1), rect(1, 0, 3, 1)),
        (rect(1, 0, 3, 1), rect(0, 0, 3, 1)),
    ] {
        ctx.record(
            &blitter,
            TextureBlit {
                source: &target,
                target: &target,
                source_rect,
                target_rect,
                filter: TextureFilter::Nearest,
            },
        )
        .unwrap();
    }
    assert_eq!(ctx.scratch.bytes(), 32);
    assert_eq!(ctx.queue.wait().unwrap(), 1);
    let expected = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255,
    ];
    assert_eq!(pixels(&target), expected);
    ctx.record(
        &blitter,
        TextureBlit {
            source: &target,
            target: &target,
            source_rect: full(&target),
            target_rect: full(&target),
            filter: TextureFilter::Linear,
        },
    )
    .unwrap();
    ctx.queue.wait().unwrap();
    assert_eq!(ctx.scratch.bytes(), 32);
    assert_eq!(pixels(&target), expected);
    ctx.record(
        &blitter,
        TextureBlit {
            source: &target,
            target: &target,
            source_rect: rect(0, 0, 3, 1),
            target_rect: rect(1, 0, 3, 1),
            filter: TextureFilter::Nearest,
        },
    )
    .unwrap();
    ctx.queue.discard_recording();
    assert_eq!(pixels(&target), expected);
    assert_eq!(ctx.scratch.bytes(), 32);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shader_blits_select_mips_and_reject_invalid_resources() {
    let mut ctx = Context::new();
    let blitter = ctx.blitter(wgt::TextureFormat::Rgba8Unorm);
    let source = Texture::new(
        &ctx.device,
        4,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        false,
    )
    .unwrap();
    let mid = source.mip_view(1).unwrap();
    let last = source.mip_view(2).unwrap();
    assert!(ctx
        .record(
            &blitter,
            TextureBlit {
                source: &source,
                target: &mid,
                source_rect: full(&source),
                target_rect: full(&mid),
                filter: TextureFilter::Linear
            }
        )
        .is_err());
    assert!(!mid.initialized());
    ctx.upload(&source, &[255, 0, 0, 255].repeat(8));
    assert!(!source.sample_initialized());
    ctx.record(
        &blitter,
        TextureBlit {
            source: &source,
            target: &mid,
            source_rect: full(&source),
            target_rect: full(&mid),
            filter: TextureFilter::Linear,
        },
    )
    .unwrap();
    ctx.record(
        &blitter,
        TextureBlit {
            source: &mid,
            target: &last,
            source_rect: full(&mid),
            target_rect: full(&last),
            filter: TextureFilter::Linear,
        },
    )
    .unwrap();
    assert!(source.sample_initialized());
    assert_eq!(ctx.scratch.bytes(), 0);
    ctx.queue.wait().unwrap();
    assert_eq!(pixels(&mid), [255, 0, 0, 255].repeat(2));
    assert_eq!(pixels(&last), [255, 0, 0, 255]);
    let target = ctx.texture(2, 2, source.format(), true);
    for target_rect in [rect(2, 0, 2, 2), rect(0, 0, 0, 2)] {
        ctx.record(
            &blitter,
            TextureBlit {
                source: &source,
                target: &target,
                source_rect: full(&source),
                target_rect,
                filter: TextureFilter::Nearest,
            },
        )
        .unwrap();
        assert!(!target.initialized());
    }
    assert!(ctx
        .record(
            &blitter,
            TextureBlit {
                source: &source,
                target: &target,
                source_rect: full(&source),
                target_rect: full(&target),
                filter: TextureFilter::Trilinear
            }
        )
        .is_err());
    let unrenderable = ctx.texture(2, 2, source.format(), false);
    assert!(ctx
        .record(
            &blitter,
            TextureBlit {
                source: &source,
                target: &unrenderable,
                source_rect: full(&source),
                target_rect: full(&unrenderable),
                filter: TextureFilter::Nearest
            }
        )
        .is_err());
    assert!(TextureBlitter::new(
        &ctx.device,
        wgt::TextureFormat::Rgba32Sint,
        &ctx.quad,
        &ctx.samplers
    )
    .is_err());
    let foreign = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    assert!(TextureBlitter::new(&foreign, source.format(), &ctx.quad, &ctx.samplers).is_err());
    assert!(!target.initialized());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
