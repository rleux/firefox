/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::{khr, vk};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::time::Duration;
use webrender::vulkan::{Device, Options, SharedTimeline, Submission, SyncFileWait};

#[repr(C)]
struct SyncobjCreate {
    handle: u32,
    flags: u32,
}

#[repr(C)]
struct SyncobjExport {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
}

pub(super) fn signaled_fence(owner: &Rc<Device>) -> Result<OwnedFd, String> {
    let device = owner.raw_device();
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    unsafe {
        device.shared_instance().raw_instance().get_physical_device_properties2(
            device.raw_physical_device(),
            &mut vk::PhysicalDeviceProperties2::default().push_next(&mut drm),
        );
    }
    if drm.has_render != vk::TRUE || drm.render_major < 0 || drm.render_minor < 0 {
        return Err("Real sync-file fixtures require a DRM render node".into());
    }
    let info = std::fs::read_to_string(format!(
        "/sys/dev/char/{}:{}/uevent", drm.render_major, drm.render_minor,
    )).map_err(|error| format!("Reading DRM render node: {error}"))?;
    let node = info.lines().find_map(|line| line.strip_prefix("DEVNAME="))
        .ok_or("DRM render node has no device name")?;
    // Closing this private DRM file releases its syncobj handles.
    let file = std::fs::OpenOptions::new().read(true).write(true)
        .open(format!("/dev/{node}"))
        .map_err(|error| format!("Opening DRM render node: {error}"))?;
    let mut create = SyncobjCreate { handle: 0, flags: 1 };
    if unsafe {
        libc::ioctl(file.as_raw_fd(), libc::_IOWR::<SyncobjCreate>(b'd'.into(), 0xbf), &mut create)
    } < 0 {
        return Err(format!("Creating signaled DRM syncobj requires SYNCOBJ support: {}",
            std::io::Error::last_os_error()));
    }
    let mut export = SyncobjExport { handle: create.handle, flags: 1, fd: -1, pad: 0 };
    if unsafe {
        libc::ioctl(file.as_raw_fd(), libc::_IOWR::<SyncobjExport>(b'd'.into(), 0xc1), &mut export)
    } < 0 {
        return Err(format!("Exporting signaled DRM sync_file: {}",
            std::io::Error::last_os_error()));
    }
    if export.fd < 0 {
        return Err("DRM syncobj export did not return a real descriptor".into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(export.fd) })
}

pub(super) unsafe fn ready_fence(owner: &Rc<Device>) -> Result<OwnedFd, String> {
    let device = owner.raw_device();
    let raw = device.raw_device();
    let mut send = Submission::new(owner)?;
    let mut export =
        vk::ExportSemaphoreCreateInfo::default().handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let signal = raw.create_semaphore(
        &vk::SemaphoreCreateInfo::default().push_next(&mut export), None,
    ).map_err(|error| format!("Creating SYNC_FD signal: {error:?}"))?;
    owner.queue().add_signal_semaphore(signal, None);
    let submitted = send.submit();
    owner.queue().remove_signal_semaphore(signal);
    if let Err(error) = submitted {
        std::mem::forget(send);
        return Err(format!("Submitting SYNC_FD signal: {error}"));
    }
    let exported = khr::external_semaphore_fd::Device::new(device.shared_instance().raw_instance(), raw)
        .get_semaphore_fd(
            &vk::SemaphoreGetFdInfoKHR::default()
                .semaphore(signal)
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
        )
        .map_err(|error| format!("Exporting SYNC_FD signal: {error:?}"))
        .and_then(|fd| {
            if fd == -1 {
                Ok(None)
            } else if fd < 0 {
                Err("SYNC_FD export returned an invalid negative descriptor".into())
            } else {
                Ok(Some(OwnedFd::from_raw_fd(fd)))
            }
        });
    match send.wait(Some(Duration::from_secs(5))) {
        Ok(true) => raw.destroy_semaphore(signal, None),
        result => {
            std::mem::forget(send);
            return Err(format!("SYNC_FD signal retirement uncertain: {result:?}"));
        }
    }
    exported?.map_or_else(|| signaled_fence(owner), Ok)
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_vulkan_sync_file_wait() {
    let producer = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let consumer = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    for (kind, fd) in [
        ("Vulkan export", ready_fence(&producer).unwrap()),
        ("completed payload", signaled_fence(&producer).unwrap()),
    ] {
        eprintln!("SYNC_FD fixture: {kind}");
        assert!(fd.as_raw_fd() >= 0);
        let wrong = SyncFileWait::import(&producer, fd.as_fd()).unwrap();
        let mut receive = Submission::new(&consumer).unwrap();
        assert!(receive.recording().unwrap().wait_sync_file(wrong).is_err());
        let wait = SyncFileWait::import(&consumer, fd.as_fd()).unwrap();
        assert!(fd.try_clone().is_ok());
        let gate = SharedTimeline::new(&producer).unwrap();
        let imported_gate = SharedTimeline::import(&consumer, &gate.export().unwrap()).unwrap();
        let complete = SharedTimeline::new(&consumer).unwrap();
        {
            let mut commands = receive.recording().unwrap();
            commands.wait_sync_file(wait).unwrap();
            commands.wait_timeline(&imported_gate, 1).unwrap();
            commands.signal_timeline(&complete, 1).unwrap();
        }
        receive.submit().unwrap();
        let premature = receive.wait(Some(Duration::from_millis(50))).unwrap();
        let mut release = Submission::new(&producer).unwrap();
        release.recording().unwrap().signal_timeline(&gate, 1).unwrap();
        release.submit().unwrap();
        assert!(receive.wait(Some(Duration::from_secs(5))).unwrap());
        assert!(release.wait(Some(Duration::from_secs(5))).unwrap());
        assert!(!premature);
        let mut next = Submission::new(&consumer).unwrap();
        next.recording().unwrap().wait_timeline(&complete, 1).unwrap();
        next.submit().unwrap();
        assert!(next.wait(Some(Duration::from_secs(5))).unwrap());
    }
}
