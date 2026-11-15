/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use raw_window_handle::*;

fn caps() -> wgpu_hal::SurfaceCapabilities {
    wgpu_hal::SurfaceCapabilities {
        formats: vec![wgt::SurfaceFormatCapabilities {
            format: wgt::TextureFormat::Bgra8Unorm,
            color_spaces: wgt::SurfaceColorSpaces::SRGB,
        }],
        maximum_frame_latency: 1..=3,
        current_extent: None,
        usage: wgt::TextureUses::COLOR_TARGET,
        present_modes: vec![wgt::PresentMode::Fifo],
        composite_alpha_modes: vec![
            wgt::CompositeAlphaMode::Opaque,
            wgt::CompositeAlphaMode::PreMultiplied,
        ],
    }
}
#[test]
fn surface_negotiates_direct_unorm_alpha_vsync_and_optional_snapshot_usage() {
    let mut c = caps();
    let config = |c: &wgpu_hal::SurfaceCapabilities, options, readback| {
        configuration(c, [64, 48], options, 4096, readback).unwrap()
    };
    let options = SurfaceOptions::default();
    for format in [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
    ] {
        c.formats[0].format = format;
        for supported in [false, true] {
            c.usage.set(wgt::TextureUses::COPY_SRC, supported);
            for requested in [false, true] {
                let chosen = config(&c, options, requested);
                assert_eq!(chosen.format, format);
                assert_eq!(chosen.color_space, wgt::SurfaceColorSpace::Srgb);
                assert_eq!(
                    chosen.usage.contains(wgt::TextureUsages::COPY_SRC),
                    supported && requested
                );
                assert!(!chosen.usage.contains(wgt::TextureUsages::COPY_DST));
            }
        }
    }
    assert_eq!(
        config(
            &c,
            SurfaceOptions {
                vsync: false,
                transparent: true
            },
            false
        )
        .alpha_mode,
        wgt::CompositeAlphaMode::PreMultiplied
    );
    assert_eq!(
        config(
            &c,
            SurfaceOptions {
                vsync: false,
                ..options
            },
            false
        )
        .present_mode,
        wgt::PresentMode::Fifo
    );
    c.present_modes.push(wgt::PresentMode::Immediate);
    assert_eq!(
        config(
            &c,
            SurfaceOptions {
                vsync: false,
                ..options
            },
            false
        )
        .present_mode,
        wgt::PresentMode::Immediate
    );
    c.composite_alpha_modes = vec![wgt::CompositeAlphaMode::Inherit];
    assert_eq!(
        config(
            &c,
            SurfaceOptions {
                transparent: true,
                ..options
            },
            false
        )
        .alpha_mode,
        wgt::CompositeAlphaMode::Inherit
    );
    assert!(configuration(&c, [64, 48], options, 4096, false).is_err());
}
#[test]
fn surface_rejects_incompatible_caps_and_reports_native_extent() {
    for format in [
        wgt::TextureFormat::Bgra8UnormSrgb,
        wgt::TextureFormat::Rgba16Float,
    ] {
        let mut c = caps();
        c.formats[0].format = format;
        assert!(configuration(&c, [64, 48], SurfaceOptions::default(), 4096, false).is_err());
    }
    let mut c = caps();
    c.formats[0].color_spaces = wgt::SurfaceColorSpaces::empty();
    assert!(configuration(&c, [64, 48], SurfaceOptions::default(), 4096, false).is_err());
    c = caps();
    c.usage = wgt::TextureUses::COPY_DST;
    assert!(configuration(&c, [64, 48], SurfaceOptions::default(), 4096, false).is_err());
    c = caps();
    c.current_extent = Some(wgt::Extent3d {
        width: 17,
        height: 19,
        depth_or_array_layers: 1,
    });
    let chosen = configuration(&c, [64, 48], SurfaceOptions::default(), 4096, false).unwrap();
    assert_eq!([chosen.width, chosen.height], [17, 19]);
    for size in [[0, 48], [4097, 48]] {
        assert!(configuration(&c, size, SurfaceOptions::default(), 4096, false).is_err());
    }
}
#[test]
fn invalid_native_display_is_rejected_before_vulkan_initialization() {
    struct Unavailable;
    impl HasDisplayHandle for Unavailable {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
            Err(HandleError::Unavailable)
        }
    }
    impl HasWindowHandle for Unavailable {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            panic!("display must be checked first")
        }
    }
    // No native handle can be returned by this owner.
    unsafe impl SurfaceWindow for Unavailable {}
    let options = Options {
        window: Some(Rc::new(Unavailable)),
        ..Default::default()
    };
    assert!(Device::new(&options)
        .err()
        .unwrap()
        .contains("Getting Vulkan display handle"));
}

#[cfg(all(target_os = "linux", feature = "debugger"))]
#[path = "surface_x11_tests.rs"]
mod x11;
