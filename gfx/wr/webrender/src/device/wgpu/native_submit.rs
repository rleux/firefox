/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{core, hal, Device, Failure};
use ash::vk;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) struct NativeWait {
    pub semaphore: vk::Semaphore,
    pub value: Option<u64>,
}

pub(super) struct NativeSignal {
    pub semaphore: vk::Semaphore,
    pub value: u64,
}

pub(super) struct NativeSubmit {
    pub result: Result<core::SubmissionIndex, Failure>,
    // Consumed hooks can also accompany a failed vkQueueSubmit; retain native refs.
    pub may_be_in_flight: bool,
    pub core_done: Arc<AtomicBool>,
}

impl Device {
    /// Every semaphore belongs to this device and has the stated binary/timeline kind.
    /// The native batch retains them and its allocation references until native retirement.
    /// The retirement timeline is private, strictly increasing, and has one signaller.
    pub(super) unsafe fn submit_native(
        &self,
        commands: &[Arc<core::command::CommandBuffer>],
        waits: &[NativeWait],
        signals: &[NativeSignal],
        retirement: NativeSignal,
    ) -> NativeSubmit {
        let core_done = Arc::new(AtomicBool::new(false));
        let rejected = |message| NativeSubmit {
            result: Err(Failure::validation(message)),
            may_be_in_flight: false,
            core_done: core_done.clone(),
        };
        if retirement.value == 0 {
            return rejected("Invalid native retirement point");
        }
        let mut unique_waits = HashMap::new();
        for wait in waits {
            if wait.semaphore == retirement.semaphore {
                return rejected("Native retirement semaphore must be private");
            }
            let previous = unique_waits.entry(wait.semaphore).or_insert(wait.value);
            match (*previous, wait.value) {
                (Some(a), Some(b)) => *previous = Some(a.max(b)),
                (None, None) => {}
                _ => return rejected("Conflicting native semaphore kinds"),
            }
        }
        let mut unique_signals = HashMap::new();
        for signal in signals {
            if signal.semaphore == retirement.semaphore || signal.value == 0 {
                return rejected("Invalid native return signal");
            }
            let previous = unique_signals
                .entry(signal.semaphore)
                .or_insert(signal.value);
            *previous = (*previous).max(signal.value);
        }
        let _queue_access = self.queue_access();
        if let Err(error) = self.check() {
            return NativeSubmit {
                result: Err(error),
                may_be_in_flight: false,
                core_done: core_done.clone(),
            };
        }
        let queue = unsafe { self.queue.clone().as_hal::<hal::api::Vulkan>() }
            .expect("Native image submission requires Vulkan");
        for (&semaphore, &value) in &unique_waits {
            queue.add_wait_semaphore(semaphore, value, vk::PipelineStageFlags::TOP_OF_PIPE);
        }
        for (&semaphore, &value) in &unique_signals {
            queue.add_signal_semaphore(semaphore, Some(value));
        }
        queue.add_signal_semaphore(retirement.semaphore, Some(retirement.value));
        let index = self.queue.submit(commands);
        let mut result = self
            .check()
            .map(|()| index.expect("Successful submission has an index"));
        let mut leftover = false;
        let mut consumed = false;
        for &semaphore in unique_waits.keys() {
            let pending = queue.remove_wait_semaphore(semaphore);
            leftover |= pending;
            consumed |= !pending;
        }
        for &semaphore in unique_signals.keys() {
            let pending = queue.remove_signal_semaphore(semaphore);
            leftover |= pending;
            consumed |= !pending;
        }
        let may_be_in_flight = !queue.remove_signal_semaphore(retirement.semaphore);
        if result.is_ok() && (leftover || !may_be_in_flight) {
            result = Err(Failure::validation(
                "Native submission did not consume its synchronization",
            ));
        }
        if may_be_in_flight || consumed {
            let completed = core_done.clone();
            // Hooks have been removed above; callback registration cannot consume them.
            self.queue.on_submitted_work_done(Box::new(move || {
                completed.store(true, Ordering::Release);
            }));
            if let Err(error) = self.check() {
                result = Err(error);
            }
        }
        NativeSubmit {
            result,
            may_be_in_flight: may_be_in_flight || consumed,
            core_done,
        }
    }
}
