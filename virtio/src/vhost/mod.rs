// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

pub mod kernel;
pub mod user;

use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use log::error;
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::EventFd;

use super::{Queue, QueueConfig, VirtioInterrupt, VirtioInterruptType};
use util::loop_context::{EventNotifier, EventNotifierHelper, NotifierCallback, NotifierOperation};

/// Vhost vring call notify structure.
pub struct VhostNotify {
    /// Used to register in vhost kernel, when virtio queue have io request will notify to vhost.
    pub notify_evt: Arc<EventFd>,
    /// The related virtio queue.
    pub queue: Arc<Mutex<Queue>>,
}

pub trait VhostOps {
    /// Set the current process as the (exclusive) owner of file descriptor
    /// of the vhost backend. This must be run before any other vhost commands.
    fn set_owner(&self) -> Result<()>;

    /// Give up ownership, reset the device. Add calling `set_owner` function
    /// later is permitted.
    fn reset_owner(&self) -> Result<()>;

    /// Get a bitmask of supported vhost specific features.
    fn get_features(&self) -> Result<u64>;

    /// Set the vhost backend supported vhost specific features. This should be
    /// a subset of supported features from VHOST_GET_FEATURES.
    ///
    /// # Arguments
    /// * `features` - Bitmask of features to set.
    fn set_features(&self, features: u64) -> Result<()>;

    /// Set memory layout.
    fn set_mem_table(&self) -> Result<()>;

    /// Set number of descriptors in ring. This parameter can not be modified
    /// while ring is running (bound to a device).
    ///
    /// # Arguments
    /// * `queue_idx` - Index of the queue to set.
    /// * `num` - Number of descriptors in the virtqueue.
    fn set_vring_num(&self, queue_idx: usize, num: u16) -> Result<()>;

    /// Set addresses for the vring.
    ///
    /// # Arguments
    /// * `config` - queue configuration.
    fn set_vring_addr(&self, queue: &QueueConfig, index: usize, flags: u32) -> Result<()>;

    /// Set base value where queue looks for available descriptors.
    ///
    /// # Arguments
    /// * `queue_idx` - Index of the queue to set.
    /// * `last_avail_idx` - Index of the available descriptor.
    fn set_vring_base(&self, queue_idx: usize, last_avail_idx: u16) -> Result<()>;

    /// Get address for the vring.
    ///
    /// # Arguments
    /// * `queue_idx` - Index of the queue to get.
    fn get_vring_base(&self, queue_idx: usize) -> Result<u16>;

    /// Set eventfd to signal when buffers have been used.
    ///
    /// # Arguments
    /// * `queue_idx` - Index of the queue to modify.
    /// * `fd` - EventFd to trigger.
    fn set_vring_call(&self, queue_idx: usize, fd: Arc<EventFd>) -> Result<()>;

    /// Set eventfd to poll for added buffers.
    ///
    /// # Arguments
    /// * `queue_idx` - Index of the queue to modify.
    /// * `fd` - EventFd that will be signaled from guest.
    fn set_vring_kick(&self, queue_idx: usize, fd: Arc<EventFd>) -> Result<()>;

    /// Set the status of ring.
    ///
    /// # Arguments
    /// * `_queue_idx` - Index of the queue to set.
    /// * `_status` - Status of the virtqueue.
    fn set_vring_enable(&self, _queue_idx: usize, _status: bool) -> Result<()> {
        Ok(())
    }

    fn set_socket(&self, _sender_fd: &UnixStream) -> Result<()> {
        Ok(())
    }

    fn reset_device(&self) -> Result<()> {
        Ok(())
    }
}

pub struct VhostIoHandler {
    interrupt_cb: Arc<VirtioInterrupt>,
    host_notifies: Vec<VhostNotify>,
    device_broken: Arc<AtomicBool>,
}

impl VhostIoHandler {
    /// Settle an event only when an eventfd counter was actually consumed.
    /// A stop barrier may already have drained a notification returned by
    /// epoll; replaying that stale event would mutate interrupt state later.
    fn drain_notification(&self, fd: RawFd) -> Result<()> {
        if self.device_broken.load(Ordering::SeqCst) {
            return Ok(());
        }
        for notify in &self.host_notifies {
            if notify.notify_evt.as_raw_fd() != fd {
                continue;
            }
            match notify.notify_evt.read() {
                Ok(_) => (self.interrupt_cb)(
                    &VirtioInterruptType::Vring,
                    Some(&notify.queue.lock().unwrap()),
                    false,
                )?,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub fn drain_notifications(&self) -> Result<()> {
        for notify in &self.host_notifies {
            self.drain_notification(notify.notify_evt.as_raw_fd())?;
        }
        Ok(())
    }
}

impl EventNotifierHelper for VhostIoHandler {
    fn internal_notifiers(vhost_handler: Arc<Mutex<Self>>) -> Vec<EventNotifier> {
        let mut notifiers = Vec::new();

        let vhost = vhost_handler.clone();
        let handler: Rc<NotifierCallback> = Rc::new(move |_, fd: RawFd| {
            let locked_vhost_handler = vhost.lock().unwrap();
            if let Err(e) = locked_vhost_handler.drain_notification(fd) {
                error!("Failed to settle vhost notification: {:?}", e);
            }
            None as Option<Vec<EventNotifier>>
        });
        for host_notify in vhost_handler.lock().unwrap().host_notifies.iter() {
            notifiers.push(EventNotifier::new(
                NotifierOperation::AddShared,
                host_notify.notify_evt.as_raw_fd(),
                None,
                EventSet::IN,
                vec![handler.clone()],
            ));
        }

        notifiers
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn capture_drains_pending_interrupt_without_replaying_stale_event() {
        let event = Arc::new(EventFd::new(libc::EFD_NONBLOCK).unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = calls.clone();
        let callback: Arc<VirtioInterrupt> = Arc::new(Box::new(move |_, _, _| {
            callback_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        let queue = Queue::new(QueueConfig::new(256), crate::QUEUE_TYPE_SPLIT_VRING).unwrap();
        let handler = VhostIoHandler {
            interrupt_cb: callback,
            host_notifies: vec![VhostNotify {
                notify_evt: event.clone(),
                queue: Arc::new(Mutex::new(queue)),
            }],
            device_broken: Arc::new(AtomicBool::new(false)),
        };
        event.write(1).unwrap();
        handler.drain_notifications().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        handler.drain_notification(event.as_raw_fd()).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        event.write(1).unwrap();
        handler.drain_notification(event.as_raw_fd()).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
