/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn descriptor(format: wgt::TextureFormat) -> hal::TextureDescriptor<'static> {
    hal::TextureDescriptor {
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
        usage: wgt::TextureUses::COPY_SRC
            | wgt::TextureUses::COPY_DST
            | wgt::TextureUses::COLOR_TARGET,
        memory_flags: hal::MemoryFlags::empty(),
        view_formats: vec![if format == wgt::TextureFormat::Rgba8Unorm {
            wgt::TextureFormat::Rgba8UnormSrgb
        } else {
            wgt::TextureFormat::Bgra8UnormSrgb
        }],
    }
}

#[test]
fn shared_image_views_preserve_base_storage_and_explicit_sampling_semantics() {
    for (format, srgb) in [
        (
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Rgba8UnormSrgb,
        ),
        (
            wgt::TextureFormat::Bgra8Unorm,
            wgt::TextureFormat::Bgra8UnormSrgb,
        ),
    ] {
        let desc = descriptor(format);
        let original_usage = desc.usage;
        for view_format in [format, srgb] {
            for filter in [TextureFilter::Nearest, TextureFilter::Linear] {
                for opaque in [false, true] {
                    let view =
                        shared_image_view_descriptor(&desc, view_format, filter, opaque).unwrap();
                    assert_eq!(desc.format, format);
                    assert_eq!(desc.usage, original_usage);
                    assert!(!desc.usage.contains(wgt::TextureUses::RESOURCE));
                    assert_eq!(view.format, view_format);
                    assert_eq!(view.usage, wgt::TextureUses::RESOURCE);
                    assert_eq!(view.dimension, wgt::TextureViewDimension::D2);
                    assert_eq!(view.range.mip_level_count, Some(1));
                    assert_eq!(view.range.array_layer_count, Some(1));
                    assert_eq!(
                        view.swizzle.a,
                        if opaque {
                            wgt::ComponentSwizzle::One
                        } else {
                            wgt::ComponentSwizzle::A
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn shared_image_view_validation_rejects_noncanvas_and_undeclared_sampling() {
    for field in 0..12 {
        let mut desc = descriptor(wgt::TextureFormat::Rgba8Unorm);
        let mut view = wgt::TextureFormat::Rgba8Unorm;
        let mut filter = TextureFilter::Nearest;
        match field {
            0 => desc.size.width = 0,
            1 => desc.size.height = 0,
            2 => desc.size.depth_or_array_layers = 2,
            3 => desc.mip_level_count = 2,
            4 => desc.sample_count = 4,
            5 => desc.dimension = wgt::TextureDimension::D3,
            6 => desc.format = wgt::TextureFormat::Rgba16Float,
            7 => desc.usage = wgt::TextureUses::TRANSIENT | wgt::TextureUses::COLOR_TARGET,
            8 => {
                desc.view_formats.clear();
                view = wgt::TextureFormat::Rgba8UnormSrgb;
            }
            9 => view = wgt::TextureFormat::Bgra8UnormSrgb,
            10 => filter = TextureFilter::Trilinear,
            _ => desc.view_formats.push(wgt::TextureFormat::Bgra8UnormSrgb),
        }
        assert!(
            shared_image_view_descriptor(&desc, view, filter, false).is_err(),
            "descriptor field {field}"
        );
    }
}
