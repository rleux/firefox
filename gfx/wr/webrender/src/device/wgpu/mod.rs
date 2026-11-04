/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::fmt;
use std::sync::{Arc, Mutex};
use wgpu_core as core;
use wgpu_hal as hal;
use wgpu_types as wgt;
use wgt::error::{ErrorType, WebGpuError};

mod vulkan;

#[derive(Default)]
pub struct Options {
    pub adapter_name: Option<String>,
    pub validation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: ErrorType,
    pub message: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Failure {}

#[derive(Default)]
struct Errors(Mutex<Option<Failure>>);

impl Errors {
    fn record(&self, kind: ErrorType, message: String) {
        let mut failure = self.0.lock().unwrap();
        if failure.is_none() || kind == ErrorType::DeviceLost {
            *failure = Some(Failure { kind, message });
        }
    }
}

/// Core owns ordinary resources, command validation and submitted resource lifetimes.
pub struct Device {
    pub(super) adapter: Arc<core::instance::Adapter>,
    pub(super) device: Arc<core::device::Device>,
    pub(super) queue: Arc<core::device::queue::Queue>,
    errors: Arc<Errors>,
}

impl Device {
    pub fn new(options: &Options) -> Result<Self, String> {
        if options
            .adapter_name
            .as_ref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err("Adapter name must not be empty".into());
        }
        let (adapter, device, queue) = vulkan::open(options)?;
        let errors = Arc::new(Errors::default());
        let uncaptured = errors.clone();
        device.on_uncaptured_error(Arc::new(move |error: wgt::error::Error| {
            uncaptured.record(error.webgpu_error_type(), error.to_string());
        }));
        let lost = errors.clone();
        device.set_device_lost_closure(Box::new(move |reason, message| {
            lost.record(ErrorType::DeviceLost, format!("{reason:?}: {message}"));
        }));
        let result = Self {
            adapter,
            device,
            queue,
            errors,
        };
        result.check().map_err(|error| error.to_string())?;
        Ok(result)
    }

    pub fn info(&self) -> wgt::AdapterInfo {
        self.adapter.get_info()
    }

    pub fn features(&self) -> wgt::Features {
        *self.device.features()
    }

    pub fn limits(&self) -> &wgt::Limits {
        self.device.limits()
    }

    pub fn failure(&self) -> Option<Failure> {
        self.check().err()
    }

    /// Check at shared GPU boundaries; CPU frame preparation need not check for loss.
    pub(super) fn check(&self) -> Result<(), Failure> {
        // Core can become invalid before its deferred loss callback is fired by polling.
        if !self.device.is_valid() {
            let mut failure = self.errors.0.lock().unwrap();
            if failure
                .as_ref()
                .is_none_or(|error| error.kind != ErrorType::DeviceLost)
            {
                *failure = Some(Failure {
                    kind: ErrorType::DeviceLost,
                    message: "Vulkan device is lost".into(),
                });
            }
        }
        self.errors.0.lock().unwrap().clone().map_or(Ok(()), Err)
    }

    pub fn create_buffer(
        &self,
        desc: &core::resource::BufferDescriptor,
    ) -> Result<Arc<core::resource::Buffer>, Failure> {
        let buffer = self.device.create_buffer(desc);
        self.check()?;
        Ok(buffer)
    }

    pub fn create_texture(
        &self,
        desc: &core::resource::TextureDescriptor,
    ) -> Result<Arc<core::resource::Texture>, Failure> {
        let texture = self.device.create_texture(desc);
        self.check()?;
        Ok(texture)
    }

    pub fn create_command_encoder(
        &self,
        label: &str,
    ) -> Result<Arc<core::command::CommandEncoder>, Failure> {
        let encoder = self
            .device
            .create_command_encoder(&wgt::CommandEncoderDescriptor {
                label: Some(label.into()),
            });
        self.check()?;
        Ok(encoder)
    }

    pub fn submit(
        &self,
        commands: &[Arc<core::command::CommandBuffer>],
    ) -> Result<core::SubmissionIndex, Failure> {
        self.check()?;
        let index = self.queue.submit(commands);
        // A returned index can also accompany a failed core submission.
        self.check()?;
        Ok(index.expect("Successful core submission has an index"))
    }

    /// Poll even after failure so core can deliver mapping and device-loss callbacks.
    pub fn poll(&self) -> Result<wgt::PollStatus, Failure> {
        let status = self.device.poll(wgt::PollType::Poll);
        if let Err(error) = &status {
            let kind = match error {
                core::device::WaitIdleError::Device(error) => error.webgpu_error_type(),
                _ => ErrorType::Internal,
            };
            self.errors.record(kind, error.to_string());
        }
        self.check()?;
        Ok(status.expect("Polling errors are recorded at the GPU boundary"))
    }
}

fn select_adapter(
    adapters: &[(String, wgt::DeviceType)],
    requested: Option<&str>,
) -> Result<usize, String> {
    let requested = requested.map(str::to_lowercase);
    if let Some(index) = requested.as_ref().and_then(|name| {
        adapters
            .iter()
            .position(|adapter| adapter.0.to_lowercase().contains(name))
    }) {
        return Ok(index);
    }
    adapters
        .iter()
        .enumerate()
        .min_by_key(|(_, (_, kind))| match kind {
            wgt::DeviceType::DiscreteGpu => 0,
            wgt::DeviceType::IntegratedGpu => 1,
            wgt::DeviceType::VirtualGpu => 2,
            wgt::DeviceType::Cpu => 3,
            _ => 4,
        })
        .map(|(index, _)| index)
        .ok_or_else(|| "No Vulkan 1.1 adapters available".into())
}

#[cfg(test)]
mod tests;
