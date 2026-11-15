/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[cfg(target_os = "linux")]
mod native {
    use super::*;
    use std::os::raw::{c_char, c_int, c_ulong};
    use webrender::api::{DocumentId, FramePublishId, FrameReadyParams, RenderNotifier};

    #[link(name = "X11")]
    extern "C" {
        fn XOpenDisplay(name: *const c_char) -> *mut c_void;
        fn XCloseDisplay(display: *mut c_void) -> c_int;
        fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
        fn XDefaultScreen(display: *mut c_void) -> c_int;
        fn XCreateSimpleWindow(
            display: *mut c_void,
            parent: c_ulong,
            x: c_int,
            y: c_int,
            width: u32,
            height: u32,
            border: u32,
            border_pixel: c_ulong,
            background: c_ulong,
        ) -> c_ulong;
        fn XMapWindow(display: *mut c_void, window: c_ulong) -> c_int;
        fn XDestroyWindow(display: *mut c_void, window: c_ulong) -> c_int;
        fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    }

    struct NativeDisplay(*mut c_void);
    impl Drop for NativeDisplay {
        fn drop(&mut self) {
            unsafe {
                XCloseDisplay(self.0);
            }
        }
    }
    struct NativeWindow {
        display: Rc<NativeDisplay>,
        window: c_ulong,
    }
    impl Drop for NativeWindow {
        fn drop(&mut self) {
            unsafe {
                XDestroyWindow(self.display.0, self.window);
                XSync(self.display.0, 0);
            }
        }
    }
    unsafe extern "C" fn retain_window(owner: *mut c_void) {
        Rc::increment_strong_count(owner as *const NativeWindow);
    }
    unsafe extern "C" fn release_window(owner: *mut c_void) {
        Rc::decrement_strong_count(owner as *const NativeWindow);
    }
    unsafe extern "C" fn retain_display(owner: *mut c_void) {
        Rc::increment_strong_count(owner as *const NativeDisplay);
    }
    unsafe extern "C" fn release_display(owner: *mut c_void) {
        Rc::decrement_strong_count(owner as *const NativeDisplay);
    }
    fn native_window(display: Rc<NativeDisplay>) -> Rc<NativeWindow> {
        let window = unsafe { XCreateSimpleWindow(display.0, XDefaultRootWindow(display.0), 0, 0, 64, 48, 0, 0, 0) };
        assert_ne!(window, 0);
        unsafe {
            XMapWindow(display.0, window);
            XSync(display.0, 0);
        }
        Rc::new(NativeWindow { display, window })
    }
    fn native_config(window: &Rc<NativeWindow>) -> WrVulkanConfig {
        WrVulkanConfig {
            window: WrWindowHandle::Xlib {
                display: window.display.0,
                window: window.window as u64,
                screen: unsafe { XDefaultScreen(window.display.0) },
            },
            window_owner: WrVulkanOwner {
                object: Rc::as_ptr(window) as *mut c_void,
                retain: Some(retain_window),
                release: Some(release_window),
            },
            display_owner: WrVulkanOwner {
                object: Rc::as_ptr(&window.display) as *mut c_void,
                retain: Some(retain_display),
                release: Some(release_display),
            },
            validation: true,
            vsync: true,
            transparent: false,
        }
    }
    struct Notice;
    impl RenderNotifier for Notice {
        fn clone(&self) -> Box<dyn RenderNotifier> {
            Box::new(Notice)
        }
        fn wake_up(&self, _: bool) {}
        fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
    }

    #[test]
    #[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
    fn ffi_native_surface_detach_releases_window_before_display() {
        for detached in [false, true] {
            let display = Rc::new(NativeDisplay(unsafe { XOpenDisplay(std::ptr::null()) }));
            assert!(!display.0.is_null());
            let window = native_window(display.clone());
            let weak_window = Rc::downgrade(&window);
            let weak_display = Rc::downgrade(&display);
            let mut config = native_config(&window);
            if detached {
                config.window = WrWindowHandle::Xlib {
                    display: display.0,
                    window: 0,
                    screen: unsafe { XDefaultScreen(display.0) },
                };
                config.window_owner = empty_owner();
            }
            let backend = unsafe { create_backend(&config) }.unwrap();
            let (mut renderer, sender) = webrender::create_webrender_instance(
                backend,
                Box::new(Notice),
                webrender::WebRenderOptions {
                    enable_debugger: false,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            assert_eq!(
                renderer
                    .render(webrender::api::units::DeviceIntSize::new(64, 48), 0)
                    .unwrap()
                    .present_result,
                None
            );
            if detached {
                unsafe { set_surface(&mut renderer, Some(&native_config(&window))) }.unwrap();
            }
            drop(window);
            assert!(weak_window.upgrade().is_some());
            renderer.set_surface_paused(true).unwrap();
            assert!(weak_window.upgrade().is_some());
            unsafe { set_surface(&mut renderer, None) }.unwrap();
            assert!(weak_window.upgrade().is_none());
            let before = renderer.gpu_submission_status().unwrap().unwrap();
            let replacement = native_window(display.clone());
            unsafe { set_surface(&mut renderer, Some(&native_config(&replacement))) }.unwrap();
            renderer.set_surface_paused(false).unwrap();
            let after = renderer.gpu_submission_status().unwrap().unwrap();
            assert!(after.submitted >= before.submitted && after.completed >= before.completed);
            drop(replacement);
            unsafe { set_surface(&mut renderer, None) }.unwrap();
            drop(display);
            assert!(weak_display.upgrade().is_some());
            drop(sender);
            renderer.deinit();
            assert!(weak_display.upgrade().is_none());
        }
    }
}
