/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::os::raw::c_void;
use webrender::GpuBackendConfig;

#[repr(C)]
#[derive(Clone, Copy)]
pub enum WrWindowHandle {
    Xlib {
        display: *mut c_void,
        window: u64,
        screen: i32,
    },
    Xcb {
        connection: *mut c_void,
        window: u32,
        screen: i32,
    },
    Wayland {
        display: *mut c_void,
        surface: *mut c_void,
    },
    Win32 {
        hwnd: *mut c_void,
        hinstance: *mut c_void,
    },
    Android {
        native_window: *mut c_void,
    },
}

/// Callbacks run on the render thread and keep the referenced native resource alive.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WrVulkanOwner {
    pub object: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WrVulkanConfig {
    // A null native window and an empty window owner select deferred windowed startup.
    pub window: WrWindowHandle,
    pub window_owner: WrVulkanOwner,
    // Windows and Android have no native display resource and may leave this empty.
    pub display_owner: WrVulkanOwner,
    pub validation: bool,
    pub vsync: bool,
    pub transparent: bool,
}

#[cfg(not(all(
    feature = "vulkan",
    any(target_os = "linux", target_os = "windows", target_os = "android")
)))]
pub unsafe fn create_backend(_: &WrVulkanConfig) -> Result<GpuBackendConfig, String> {
    Err("Vulkan support was not built for this platform".into())
}

#[cfg(not(all(
    feature = "vulkan",
    any(target_os = "linux", target_os = "windows", target_os = "android")
)))]
pub unsafe fn set_surface(_: &mut webrender::Renderer, _: Option<&WrVulkanConfig>) -> Result<(), String> {
    Err("Vulkan support was not built for this platform".into())
}

#[cfg(all(
    feature = "vulkan",
    any(target_os = "linux", target_os = "windows", target_os = "android")
))]
pub use self::enabled::{create_backend, set_surface};

#[cfg(all(
    feature = "vulkan",
    any(target_os = "linux", target_os = "windows", target_os = "android")
))]
mod enabled {
    use super::*;
    use raw_window_handle::*;
    use std::{
        convert::TryFrom,
        num::{NonZeroIsize, NonZeroU32},
        ptr::NonNull,
        rc::Rc,
    };
    use webrender::vulkan::{Options, SurfaceOptions};

    struct Owner {
        object: *mut c_void,
        release: unsafe extern "C" fn(*mut c_void),
    }

    impl WrVulkanOwner {
        fn is_empty(&self) -> bool {
            self.object.is_null() && self.retain.is_none() && self.release.is_none()
        }

        fn validate(&self) -> Result<(), String> {
            if !self.is_empty() && (self.object.is_null() || self.retain.is_none() || self.release.is_none()) {
                return Err("Vulkan resource owner and retain/release callbacks are required".into());
            }
            Ok(())
        }
    }

    impl Owner {
        unsafe fn retain(owner: &WrVulkanOwner) -> Self {
            (owner.retain.unwrap())(owner.object);
            Self {
                object: owner.object,
                release: owner.release.unwrap(),
            }
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            unsafe { (self.release)(self.object) };
        }
    }

    struct Display {
        handle: RawDisplayHandle,
        _owner: Option<Owner>,
    }

    struct Window {
        window: RawWindowHandle,
        // Release the window lease before its display lease.
        _owner: Owner,
        display: Rc<Display>,
    }

    impl HasDisplayHandle for Display {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
            Ok(unsafe { DisplayHandle::borrow_raw(self.handle) })
        }
    }

    impl HasDisplayHandle for Window {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
            self.display.display_handle()
        }
    }

    impl HasWindowHandle for Window {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            Ok(unsafe { WindowHandle::borrow_raw(self.window) })
        }
    }

    fn handles(handle: WrWindowHandle) -> Result<(RawDisplayHandle, Option<RawWindowHandle>), String> {
        let pointer = |p| NonNull::new(p).ok_or_else(|| "Null Vulkan window/display handle".to_owned());
        Ok(match handle {
            WrWindowHandle::Xlib {
                display,
                window,
                screen,
            } => {
                let window = std::os::raw::c_ulong::try_from(window)
                    .map_err(|_| "Vulkan Xlib window exceeds native handle size")?;
                (
                    RawDisplayHandle::Xlib(XlibDisplayHandle::new(Some(pointer(display)?), screen)),
                    (window != 0).then(|| RawWindowHandle::Xlib(XlibWindowHandle::new(window))),
                )
            },
            WrWindowHandle::Xcb {
                connection,
                window,
                screen,
            } => (
                RawDisplayHandle::Xcb(XcbDisplayHandle::new(Some(pointer(connection)?), screen)),
                NonZeroU32::new(window).map(|window| RawWindowHandle::Xcb(XcbWindowHandle::new(window))),
            ),
            WrWindowHandle::Wayland { display, surface } => (
                RawDisplayHandle::Wayland(WaylandDisplayHandle::new(pointer(display)?)),
                NonNull::new(surface).map(|surface| RawWindowHandle::Wayland(WaylandWindowHandle::new(surface))),
            ),
            WrWindowHandle::Win32 { hwnd, hinstance } => {
                let window = NonZeroIsize::new(hwnd as isize)
                    .map(|hwnd| {
                        let mut window = Win32WindowHandle::new(hwnd);
                        window.hinstance = Some(NonZeroIsize::new(hinstance as isize).ok_or("Null Vulkan HINSTANCE")?);
                        Ok::<_, String>(RawWindowHandle::Win32(window))
                    })
                    .transpose()?;
                (RawDisplayHandle::Windows(WindowsDisplayHandle::new()), window)
            },
            WrWindowHandle::Android { native_window } => (
                RawDisplayHandle::Android(AndroidDisplayHandle::new()),
                NonNull::new(native_window)
                    .map(|window| RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(window))),
            ),
        })
    }

    unsafe fn owned_window(config: &WrVulkanConfig) -> Result<(Option<Rc<Window>>, Rc<Display>), String> {
        let (display, window) = handles(config.window)?;
        config.window_owner.validate()?;
        config.display_owner.validate()?;
        if window.is_some() == config.window_owner.is_empty() {
            return Err("Vulkan window handle and owner must both be present or both absent".into());
        }
        if !matches!(display, RawDisplayHandle::Windows(_) | RawDisplayHandle::Android(_))
            && config.display_owner.is_empty()
        {
            return Err("Vulkan native display requires an owner".into());
        }
        let display = Rc::new(Display {
            handle: display,
            _owner: (!config.display_owner.is_empty()).then(|| Owner::retain(&config.display_owner)),
        });
        let window = window.map(|window| {
            Rc::new(Window {
                window,
                _owner: Owner::retain(&config.window_owner),
                display: display.clone(),
            })
        });
        Ok((window, display))
    }

    fn surface_options(config: &WrVulkanConfig) -> SurfaceOptions {
        SurfaceOptions {
            vsync: config.vsync,
            transparent: config.transparent,
        }
    }

    pub unsafe fn create_backend(config: &WrVulkanConfig) -> Result<GpuBackendConfig, String> {
        let (window, display) = owned_window(config)?;
        Ok(GpuBackendConfig::Vulkan(Options {
            window: window.map(|window| window as Rc<dyn webrender::vulkan::SurfaceWindow>),
            display_owner: Some(display),
            validation: config.validation,
            surface_options: surface_options(config),
            ..Default::default()
        }))
    }

    pub unsafe fn set_surface(
        renderer: &mut webrender::Renderer,
        config: Option<&WrVulkanConfig>,
    ) -> Result<(), String> {
        let (window, options) = match config {
            Some(config) => {
                let (window, _) = owned_window(config)?;
                let window = window.ok_or("Vulkan surface attachment requires a native window")?;
                (
                    Some(window as Rc<dyn webrender::vulkan::SurfaceWindow>),
                    surface_options(config),
                )
            },
            None => (None, SurfaceOptions::default()),
        };
        renderer
            .set_vulkan_surface(window, options)
            .map_err(|error| format!("{:?}", error))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::Cell;

        unsafe extern "C" fn retain(owner: *mut c_void) {
            let count = &*(owner as *const Cell<usize>);
            count.set(count.get() + 1);
        }

        unsafe extern "C" fn release(owner: *mut c_void) {
            let count = &*(owner as *const Cell<usize>);
            count.set(count.get() - 1);
        }

        fn config(window: &Cell<usize>, display: &Cell<usize>) -> WrVulkanConfig {
            let pointer = NonNull::<u8>::dangling().as_ptr() as *mut c_void;
            let owner = |count: &Cell<usize>| WrVulkanOwner {
                object: count as *const _ as *mut c_void,
                retain: Some(retain),
                release: Some(release),
            };
            WrVulkanConfig {
                window: WrWindowHandle::Wayland {
                    display: pointer,
                    surface: pointer,
                },
                window_owner: owner(window),
                display_owner: owner(display),
                validation: true,
                vsync: false,
                transparent: true,
            }
        }

        #[test]
        fn window_owner_survives_until_last_surface_reference() {
            let window_count = Cell::new(0usize);
            let display_count = Cell::new(0usize);
            let config = config(&window_count, &display_count);
            let backend = unsafe { create_backend(&config) }.unwrap();
            assert_eq!((window_count.get(), display_count.get()), (1, 1));
            let options = match &backend {
                GpuBackendConfig::Vulkan(options) => options,
                _ => unreachable!(),
            };
            assert!(options.validation && options.surface_options.transparent);
            assert!(!options.surface_options.vsync);
            let window = options.window.as_ref().unwrap().clone();
            drop(backend);
            assert_eq!((window_count.get(), display_count.get()), (1, 1));
            drop(window);
            assert_eq!((window_count.get(), display_count.get()), (0, 0));
        }

        #[test]
        fn display_owner_survives_window_release() {
            let window_count = Cell::new(0usize);
            let display_count = Cell::new(0usize);
            let backend = unsafe { create_backend(&config(&window_count, &display_count)) }.unwrap();
            let display = match &backend {
                GpuBackendConfig::Vulkan(options) => options.display_owner.as_ref().unwrap().clone(),
                _ => unreachable!(),
            };
            drop(backend);
            assert_eq!((window_count.get(), display_count.get()), (0, 1));
            drop(display);
            assert_eq!(display_count.get(), 0);
        }

        #[test]
        fn window_release_precedes_its_last_display_reference() {
            unsafe extern "C" fn release_window(object: *mut c_void) {
                let display_count = &*(object as *const Cell<usize>);
                assert_eq!(display_count.get(), 1);
            }
            unsafe extern "C" fn retain_window(_: *mut c_void) {}
            let window_count = Cell::new(0usize);
            let display_count = Cell::new(0usize);
            let mut config = config(&window_count, &display_count);
            config.window_owner = WrVulkanOwner {
                object: &display_count as *const _ as *mut c_void,
                retain: Some(retain_window),
                release: Some(release_window),
            };
            let backend = unsafe { create_backend(&config) }.unwrap();
            let window = match &backend {
                GpuBackendConfig::Vulkan(options) => options.window.as_ref().unwrap().clone(),
                _ => unreachable!(),
            };
            drop(backend);
            drop(window);
            assert_eq!(display_count.get(), 0);
        }

        #[test]
        fn invalid_handles_or_callbacks_do_not_retain_owners() {
            let window_count = Cell::new(0usize);
            let display_count = Cell::new(0usize);
            let valid = config(&window_count, &display_count);
            let mut invalid = valid;
            invalid.window = WrWindowHandle::Xlib {
                display: std::ptr::null_mut(),
                window: 0,
                screen: 0,
            };
            assert!(unsafe { create_backend(&invalid) }.is_err());
            for window in [false, true] {
                for field in 0..3 {
                    let mut invalid = valid;
                    let owner = if window {
                        &mut invalid.window_owner
                    } else {
                        &mut invalid.display_owner
                    };
                    match field {
                        0 => owner.object = std::ptr::null_mut(),
                        1 => owner.retain = None,
                        _ => owner.release = None,
                    }
                    assert!(unsafe { create_backend(&invalid) }.is_err());
                    assert_eq!((window_count.get(), display_count.get()), (0, 0));
                }
            }
        }

        fn empty_owner() -> WrVulkanOwner {
            WrVulkanOwner {
                object: std::ptr::null_mut(),
                retain: None,
                release: None,
            }
        }

        #[test]
        fn deferred_windows_retain_only_the_display() {
            let pointer = NonNull::<u8>::dangling().as_ptr() as *mut c_void;
            let null = std::ptr::null_mut();
            for handle in [
                WrWindowHandle::Xlib {
                    display: pointer,
                    window: 0,
                    screen: 2,
                },
                WrWindowHandle::Xcb {
                    connection: pointer,
                    window: 0,
                    screen: 2,
                },
                WrWindowHandle::Wayland {
                    display: pointer,
                    surface: null,
                },
                WrWindowHandle::Win32 {
                    hwnd: null,
                    hinstance: null,
                },
                WrWindowHandle::Android { native_window: null },
            ] {
                let window_count = Cell::new(0usize);
                let display_count = Cell::new(0usize);
                let mut config = config(&window_count, &display_count);
                config.window = handle;
                config.window_owner = empty_owner();
                let backend = unsafe { create_backend(&config) }.unwrap();
                let options = match &backend {
                    GpuBackendConfig::Vulkan(options) => options,
                    _ => unreachable!(),
                };
                assert!(options.window.is_none());
                assert!(options.display_owner.is_some());
                assert!(options.validation && options.surface_options.transparent);
                assert!(!options.surface_options.vsync);
                assert_eq!((window_count.get(), display_count.get()), (0, 1));
                drop(backend);
                assert_eq!(display_count.get(), 0);
            }
        }

        #[test]
        fn windows_and_android_displays_need_no_native_owner() {
            let pointer = NonNull::<u8>::dangling().as_ptr() as *mut c_void;
            for window in [std::ptr::null_mut(), pointer] {
                for handle in [
                    WrWindowHandle::Win32 {
                        hwnd: window,
                        hinstance: pointer,
                    },
                    WrWindowHandle::Android { native_window: window },
                ] {
                    let window_count = Cell::new(0usize);
                    let display_count = Cell::new(0usize);
                    let mut config = config(&window_count, &display_count);
                    config.window = handle;
                    config.display_owner = empty_owner();
                    if window.is_null() {
                        config.window_owner = empty_owner();
                    }
                    let backend = unsafe { create_backend(&config) }.unwrap();
                    let options = match &backend {
                        GpuBackendConfig::Vulkan(options) => options,
                        _ => unreachable!(),
                    };
                    assert_eq!(options.window.is_some(), !window.is_null());
                    assert!(options.display_owner.is_some());
                    assert_eq!(window_count.get(), usize::from(!window.is_null()));
                    assert_eq!(display_count.get(), 0);
                    drop(backend);
                    assert_eq!(window_count.get(), 0);
                }
            }
        }

        #[test]
        fn absent_or_partial_owners_are_rejected_before_callbacks() {
            let count = Cell::new(0usize);
            let mut valid = config(&count, &count);
            valid.window_owner.release = Some(retain);
            valid.display_owner.release = Some(retain);
            let mut missing_window_owner = valid;
            missing_window_owner.window_owner = empty_owner();
            let mut missing_display_owner = valid;
            missing_display_owner.display_owner = empty_owner();
            let mut missing_window = valid;
            if let WrWindowHandle::Wayland { ref mut surface, .. } = missing_window.window {
                *surface = std::ptr::null_mut();
            }
            for invalid in [missing_window_owner, missing_display_owner, missing_window] {
                assert!(unsafe { create_backend(&invalid) }.is_err());
                assert_eq!(count.get(), 0);
            }
            for window in [false, true] {
                for callback in [false, true] {
                    let mut invalid = valid;
                    invalid.window = WrWindowHandle::Android {
                        native_window: std::ptr::null_mut(),
                    };
                    invalid.window_owner = empty_owner();
                    invalid.display_owner = empty_owner();
                    let owner = if window {
                        &mut invalid.window_owner
                    } else {
                        &mut invalid.display_owner
                    };
                    if callback {
                        owner.retain = Some(retain);
                    } else {
                        owner.object = &count as *const _ as *mut c_void;
                    }
                    assert!(unsafe { create_backend(&invalid) }.is_err());
                    assert_eq!(count.get(), 0);
                }
            }
        }

        #[test]
        fn platform_descriptors_preserve_handles_and_reject_invalid_displays() {
            let pointer = NonNull::<u8>::dangling().as_ptr() as *mut c_void;
            let (display, window) = handles(WrWindowHandle::Xlib {
                display: pointer,
                window: 7,
                screen: 3,
            })
            .unwrap();
            assert!(
                matches!(display, RawDisplayHandle::Xlib(d) if d.display.unwrap().as_ptr() == pointer && d.screen == 3)
            );
            assert!(matches!(window, Some(RawWindowHandle::Xlib(w)) if w.window == 7));
            let (display, window) = handles(WrWindowHandle::Xcb {
                connection: pointer,
                window: 9,
                screen: 2,
            })
            .unwrap();
            assert!(
                matches!(display, RawDisplayHandle::Xcb(d) if d.connection.unwrap().as_ptr() == pointer && d.screen == 2)
            );
            assert!(matches!(window, Some(RawWindowHandle::Xcb(w)) if w.window.get() == 9));
            let (_, window) = handles(WrWindowHandle::Win32 {
                hwnd: pointer,
                hinstance: pointer,
            })
            .unwrap();
            assert!(
                matches!(window, Some(RawWindowHandle::Win32(w)) if w.hwnd.get() == pointer as isize && w.hinstance.unwrap().get() == pointer as isize)
            );
            let (_, window) = handles(WrWindowHandle::Android { native_window: pointer }).unwrap();
            assert!(matches!(window, Some(RawWindowHandle::AndroidNdk(w)) if w.a_native_window.as_ptr() == pointer));
            let null = std::ptr::null_mut();
            for handle in [
                WrWindowHandle::Xlib {
                    display: null,
                    window: 7,
                    screen: 0,
                },
                WrWindowHandle::Xcb {
                    connection: null,
                    window: 9,
                    screen: 0,
                },
                WrWindowHandle::Wayland {
                    display: null,
                    surface: pointer,
                },
                WrWindowHandle::Win32 {
                    hwnd: pointer,
                    hinstance: null,
                },
            ] {
                assert!(handles(handle).is_err());
            }
        }
    }
}
