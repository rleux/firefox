/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn supported_device() -> Requirements {
    Requirements {
        device_type: wgt::DeviceType::IntegratedGpu,
        max_texture_dimension_2d: 4096,
        swapchain: true,
        color: hal::TextureFormatCapabilities::COLOR_ATTACHMENT
            | hal::TextureFormatCapabilities::COPY_SRC,
        depth: hal::TextureFormatCapabilities::DEPTH_STENCIL_ATTACHMENT
            | hal::TextureFormatCapabilities::COPY_SRC,
        surface: hal::SurfaceCapabilities {
            formats: vec![wgt::SurfaceFormatCapabilities {
                format: wgt::TextureFormat::Bgra8Unorm,
                color_spaces: wgt::SurfaceColorSpaces::SRGB,
            }],
            maximum_frame_latency: 1..=2,
            current_extent: Some(wgt::Extent3d {
                width: 16,
                height: 16,
                depth_or_array_layers: 1,
            }),
            usage: wgt::TextureUses::COLOR_TARGET,
            present_modes: vec![wgt::PresentMode::Fifo],
            composite_alpha_modes: vec![wgt::CompositeAlphaMode::Opaque],
        },
    }
}

#[test]
fn baseline_without_optional_features() {
    let mut requirements = supported_device();
    assert_eq!(requirements.failure(), None);
    requirements.surface.current_extent = None;
    assert_eq!(requirements.failure(), None);
    requirements.surface.current_extent = Some(wgt::Extent3d {
        width: 4096,
        height: 4096,
        depth_or_array_layers: 1,
    });
    assert_eq!(requirements.failure(), None);
    for format in [
        wgt::TextureFormat::Bgra8Unorm,
        wgt::TextureFormat::Rgba8Unorm,
    ] {
        requirements.surface.formats = vec![wgt::SurfaceFormatCapabilities {
            format,
            color_spaces: wgt::SurfaceColorSpaces::SRGB | wgt::SurfaceColorSpaces::DISPLAY_P3,
        }];
        assert_eq!(requirements.failure(), None);
    }
}

#[test]
fn reject_software_adapters() {
    let mut requirements = supported_device();
    requirements.device_type = wgt::DeviceType::Cpu;
    assert!(requirements.failure().is_some());
}

#[test]
fn reject_missing_swapchain() {
    let mut requirements = supported_device();
    requirements.swapchain = false;
    assert!(requirements.failure().is_some());
}

#[test]
fn reject_missing_texture_usages() {
    for usage in [
        hal::TextureFormatCapabilities::COLOR_ATTACHMENT,
        hal::TextureFormatCapabilities::COPY_SRC,
    ] {
        let mut requirements = supported_device();
        requirements.color.remove(usage);
        assert!(requirements.failure().is_some());
    }
    for usage in [
        hal::TextureFormatCapabilities::DEPTH_STENCIL_ATTACHMENT,
        hal::TextureFormatCapabilities::COPY_SRC,
    ] {
        let mut requirements = supported_device();
        requirements.depth.remove(usage);
        assert!(requirements.failure().is_some());
    }
}

#[test]
fn reject_unusable_surface() {
    let mut requirements = supported_device();
    requirements.surface.usage = wgt::TextureUses::COPY_SRC;
    assert!(requirements.failure().is_some());
    requirements = supported_device();
    requirements.surface.composite_alpha_modes = vec![wgt::CompositeAlphaMode::PreMultiplied];
    assert!(requirements.failure().is_some());
    for formats in [
        vec![],
        vec![wgt::SurfaceFormatCapabilities {
            format: wgt::TextureFormat::Bgra8UnormSrgb,
            color_spaces: wgt::SurfaceColorSpaces::SRGB,
        }],
        vec![wgt::SurfaceFormatCapabilities {
            format: wgt::TextureFormat::Bgra8Unorm,
            color_spaces: wgt::SurfaceColorSpaces::DISPLAY_P3,
        }],
        vec![
            wgt::SurfaceFormatCapabilities {
                format: wgt::TextureFormat::Bgra8Unorm,
                color_spaces: wgt::SurfaceColorSpaces::DISPLAY_P3,
            },
            wgt::SurfaceFormatCapabilities {
                format: wgt::TextureFormat::Rgba16Float,
                color_spaces: wgt::SurfaceColorSpaces::SRGB,
            },
        ],
    ] {
        requirements = supported_device();
        requirements.surface.formats = formats;
        assert!(requirements.failure().is_some());
    }
    requirements = supported_device();
    requirements.surface.present_modes = vec![wgt::PresentMode::Immediate];
    assert!(requirements.failure().is_some());
    for (width, height) in [(0, 16), (16, 0), (4097, 16), (16, 4097)] {
        requirements = supported_device();
        requirements.surface.current_extent = Some(wgt::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        });
        assert!(requirements.failure().is_some());
    }
}

#[test]
fn adapter_selection_prefers_type_then_name() {
    let mut selected = wgt::AdapterInfo {
        name: "Z adapter".into(),
        vendor: 0,
        device: 0,
        device_type: wgt::DeviceType::DiscreteGpu,
        device_pci_bus_id: String::new(),
        driver: String::new(),
        driver_info: String::new(),
        backend: wgt::Backend::Vulkan,
        subgroup_min_size: 0,
        subgroup_max_size: 0,
        transient_saves_memory: None,
        limit_bucket: None,
    };
    let mut candidate = selected.clone();
    candidate.name = "A adapter".into();
    assert!(prefer_adapter(&candidate, &selected));
    assert!(!prefer_adapter(&selected, &candidate));
    selected.device_type = wgt::DeviceType::IntegratedGpu;
    assert!(prefer_adapter(&candidate, &selected));
    assert!(!prefer_adapter(&selected, &candidate));
}

#[test]
fn report_strings_are_bounded_and_terminated() {
    let mut output = [1; 256];
    copy_bytes(&mut output, &[b'x'; 300]);
    assert!(output[..255].iter().all(|&byte| byte == b'x' as c_char));
    assert_eq!(output[255], 0);
    copy_bytes(&mut output, b"GPU");
    assert_eq!(
        &output[..3],
        &[b'G' as c_char, b'P' as c_char, b'U' as c_char]
    );
    assert!(output[3..].iter().all(|&byte| byte == 0));
    copy_bytes(&mut output, b"");
    assert!(output.iter().all(|&byte| byte == 0));
}
