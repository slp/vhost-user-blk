// Copyright (C) 2019 Red Hat, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright (C) 2019 Alibaba Cloud Computing. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs::File;
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::ptr::null_mut;
use std::slice;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::backend::StorageBackend;
use crate::eventfd::EventFd;
use crate::message::*;
use crate::vring::{Vring, VringFd, VringMmapRegion};
use libc;
use log::{debug, error};
use virtio_bindings::bindings::virtio_blk::*;

use vhostuser_rs::message::*;
use vhostuser_rs::{Error, Result, VhostUserSlave};

pub struct VhostMmapRegion {
    mmap_addr: u64,
    guest_phys_addr: u64,
    memory_size: u64,
    qemu_virt_addr: u64,
}

impl VhostMmapRegion {
    fn new(mmap_addr: u64, guest_phys_addr: u64, memory_size: u64, qemu_virt_addr: u64) -> Self {
        VhostMmapRegion {
            mmap_addr,
            guest_phys_addr,
            memory_size,
            qemu_virt_addr,
        }
    }

    fn qva_to_va(&self, addr: u64) -> Result<u64> {
        if addr < self.qemu_virt_addr || addr > self.qemu_virt_addr + self.memory_size {
            return Err(Error::InvalidParam);
        }

        Ok(self.mmap_addr + (addr - self.qemu_virt_addr))
    }
}

pub struct VhostUserBlk<S: StorageBackend> {
    backend: S,
    main_eventfd: EventFd,
    main_sender: Sender<VubMessage>,
    mmap_regions: Vec<VhostMmapRegion>,
    vring_num: usize,
    vrings: HashMap<usize, Arc<Mutex<Vring<S>>>>,
    vring_default_enabled: bool,
    owned: bool,
    features_acked: bool,
    acked_features: u64,
    acked_protocol_features: u64,
}

impl<S: StorageBackend> VhostUserBlk<S> {
    pub fn new(
        backend: S,
        main_eventfd: EventFd,
        main_sender: Sender<VubMessage>,
        vring_num: usize,
    ) -> Self {
        VhostUserBlk {
            backend,
            main_eventfd,
            main_sender,
            mmap_regions: vec![],
            vring_num,
            vrings: HashMap::new(),
            vring_default_enabled: false,
            owned: false,
            features_acked: false,
            acked_features: 0,
            acked_protocol_features: 0,
        }
    }

    pub fn get_vring(&self, index: usize) -> Result<Arc<Mutex<Vring<S>>>> {
        let vring = match self.vrings.get(&index) {
            Some(v) => v,
            None => return Err(Error::InvalidParam),
        };

        Ok(vring.clone())
    }

    fn find_region(&self, addr: u64) -> Result<&VhostMmapRegion> {
        for region in &self.mmap_regions {
            if addr >= region.qemu_virt_addr && addr <= region.qemu_virt_addr + region.memory_size {
                return Ok(region);
            }
        }

        error!("can't find region for guest address {:?}", addr);
        Err(Error::InvalidParam)
    }
}

impl<S: StorageBackend> VhostUserSlave for VhostUserBlk<S> {
    fn set_owner(&mut self) -> Result<()> {
        if self.owned {
            return Err(Error::InvalidOperation);
        }
        self.owned = true;
        Ok(())
    }

    fn reset_owner(&mut self) -> Result<()> {
        self.owned = false;
        self.features_acked = false;
        self.acked_features = 0;
        self.acked_protocol_features = 0;
        Ok(())
    }

    fn get_features(&mut self) -> Result<u64> {
        Ok(VhostUserVirtioFeatures::all().bits())
    }

    fn set_features(&mut self, features: u64) -> Result<()> {
        if !self.owned {
            return Err(Error::InvalidOperation);
        /*} else if self.features_acked {
        return Err(Error::InvalidOperation);*/
        } else if (features & VhostUserVirtioFeatures::all().bits()) != 0 {
            return Err(Error::InvalidParam);
        }

        self.acked_features = features;
        self.features_acked = true;

        // If VHOST_USER_F_PROTOCOL_FEATURES has not been negotiated,
        // the ring is initialized in an enabled state.
        // If VHOST_USER_F_PROTOCOL_FEATURES has been negotiated,
        // the ring is initialized in a disabled state. Client must not
        // pass data to/from the backend until ring is enabled by
        // VHOST_USER_SET_VRING_ENABLE with parameter 1, or after it has
        // been disabled by VHOST_USER_SET_VRING_ENABLE with parameter 0.
        let vring_enabled =
            self.acked_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() == 0;
        for (_vring_index, vring) in &mut self.vrings {
            vring.lock().unwrap().set_enabled(vring_enabled);
        }
        self.vring_default_enabled = true;

        Ok(())
    }

    fn get_protocol_features(&mut self) -> Result<VhostUserProtocolFeatures> {
        Ok(VhostUserProtocolFeatures::MQ | VhostUserProtocolFeatures::CONFIG)
    }

    fn set_protocol_features(&mut self, features: u64) -> Result<()> {
        // Note: slave that reported VHOST_USER_F_PROTOCOL_FEATURES must
        // support this message even before VHOST_USER_SET_FEATURES was
        // called.
        // What happens if the master calls set_features() with
        // VHOST_USER_F_PROTOCOL_FEATURES cleared after calling this
        // interface?
        self.acked_protocol_features = features;
        Ok(())
    }

    fn set_mem_table(&mut self, regions: &[VhostUserMemoryRegion], fds: &[RawFd]) -> Result<()> {
        let mut i = 0;

        // TODO - clean up vrings
        // Reset the current memory_regions array
        self.mmap_regions = vec![];

        for region in regions.iter() {
            let file = unsafe { File::from_raw_fd(fds[i]) };
            let addr = unsafe {
                libc::mmap(
                    null_mut(),
                    region.memory_size as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    region.mmap_offset as i64,
                )
            };

            if addr == libc::MAP_FAILED {
                error!("error in mmap");
                return Err(Error::OperationFailedInSlave);
            }

            self.mmap_regions.push(VhostMmapRegion::new(
                addr as u64,
                region.guest_phys_addr,
                region.memory_size,
                region.userspace_addr,
            ));
            i += 1;
        }

        Ok(())
    }

    fn get_queue_num(&mut self) -> Result<u64> {
        Ok(self.vring_num as u64)
    }

    fn set_vring_num(&mut self, index: u32, num: u32) -> Result<()> {
        let vring_index: usize = index as usize;
        let size: u16 = u16::try_from(num).map_err(|_err| Error::InvalidParam)?;
        if !self.vrings.contains_key(&vring_index) {
            self.vrings.insert(
                vring_index,
                Arc::new(Mutex::new(Vring::new(self.backend.clone()))),
            );
        }

        let vring_mtx = self.vrings.get(&vring_index).unwrap();
        let mut vring = vring_mtx.lock().unwrap();
        vring.set_enabled(self.vring_default_enabled);
        vring.set_size(size);

        Ok(())
    }

    fn set_vring_addr(
        &mut self,
        index: u32,
        _flags: VhostUserVringAddrFlags,
        descriptor: u64,
        used: u64,
        available: u64,
        _log: u64,
    ) -> Result<()> {
        debug!("SET_VRING_ADDR");
        let region = self.find_region(descriptor)?;
        let desc_region = VringMmapRegion::new(region.mmap_addr, region.memory_size);
        let desc_addr = region.qva_to_va(descriptor)?;

        let region = self.find_region(used)?;
        let used_region = VringMmapRegion::new(region.mmap_addr, region.memory_size);
        let used_addr = region.qva_to_va(used)?;

        let region = self.find_region(available)?;
        let avail_region = VringMmapRegion::new(region.mmap_addr, region.memory_size);
        let avail_addr = region.qva_to_va(available)?;

        let vring_index: usize = index as usize;
        let vring = match self.vrings.get_mut(&vring_index) {
            Some(v) => v,
            None => return Err(Error::InvalidParam),
        };

        vring.lock().unwrap().set_mem_table(
            desc_region,
            desc_addr,
            used_region,
            used_addr,
            avail_region,
            avail_addr,
        )?;

        Ok(())
    }

    fn set_vring_base(&mut self, index: u32, base: u32) -> Result<()> {
        debug!("SET_VRING_BASE");
        let vring_index: usize = index as usize;
        let base_index: u16 = u16::try_from(base).map_err(|_err| Error::InvalidParam)?;
        let vring = match self.vrings.get_mut(&vring_index) {
            Some(v) => v,
            None => return Err(Error::InvalidParam),
        };

        vring.lock().unwrap().set_base_index(base_index);

        Ok(())
    }

    fn get_vring_base(&mut self, index: u32) -> Result<VhostUserVringState> {
        let vring_index: usize = index as usize;
        let vring_mtx = match self.vrings.get_mut(&vring_index) {
            Some(v) => v,
            None => return Err(Error::InvalidParam),
        };
        let mut vring = vring_mtx.lock().unwrap();

        // Follow vhost-user spec and stop the ring
        vring.set_started(false);
        self.main_sender
            .send(VubMessage::DisableVring(DisableVringMsg {
                index: vring_index,
            }))
            .unwrap();
        self.main_eventfd.write(1u64).unwrap();

        //let queue = vring.get_queue();
        //let last_index = queue.get_last_index(mem);
        //Ok(VhostUserVringState::new(index, last_index.into()))
        Ok(VhostUserVringState::new(
            index,
            vring.get_base_index() as u32,
        ))
    }

    fn set_vring_kick(&mut self, index: u8, fd: Option<RawFd>) -> Result<()> {
        let vring_index: usize = index as usize;
        let vring_mtx = match self.vrings.get_mut(&vring_index) {
            Some(v) => v,
            None => return Err(Error::InvalidParam),
        };
        let mut vring = vring_mtx.lock().unwrap();

        vring.set_fd(fd, VringFd::Kick);

        if vring.is_enabled() {
            self.main_sender
                .send(VubMessage::EnableVring(EnableVringMsg {
                    index: vring_index,
                    fd: vring.get_fd(VringFd::Kick).unwrap(),
                }))
                .unwrap();
            self.main_eventfd.write(1u64).unwrap();
        }

        Ok(())
    }

    fn set_vring_call(&mut self, index: u8, fd: Option<RawFd>) -> Result<()> {
        let vring_index: usize = index as usize;
        let vring_mtx = match self.vrings.get_mut(&vring_index) {
            Some(v) => v,
            None => return Err(Error::InvalidParam),
        };
        let mut vring = vring_mtx.lock().unwrap();

        vring.set_fd(fd, VringFd::Call);

        Ok(())
    }

    fn set_vring_err(&mut self, index: u8, fd: Option<RawFd>) -> Result<()> {
        let vring_index: usize = index as usize;
        let vring_mtx = match self.vrings.get_mut(&vring_index) {
            Some(v) => v,
            None => return Err(Error::InvalidParam),
        };
        let mut vring = vring_mtx.lock().unwrap();

        vring.set_fd(fd, VringFd::Error);

        Ok(())
    }

    fn set_vring_enable(&mut self, index: u32, enable: bool) -> Result<()> {
        let vring_index: usize = index as usize;
        // This request should be handled only when VHOST_USER_F_PROTOCOL_FEATURES
        // has been negotiated.
        if self.acked_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() == 0 {
            return Err(Error::InvalidOperation);
        }
        let vring_mtx = match self.vrings.get_mut(&vring_index) {
            Some(v) => v,
            None => return Err(Error::InvalidParam),
        };
        let mut vring = vring_mtx.lock().unwrap();

        // Slave must not pass data to/from the backend until ring is
        // enabled by VHOST_USER_SET_VRING_ENABLE with parameter 1,
        // or after it has been disabled by VHOST_USER_SET_VRING_ENABLE
        // with parameter 0.
        vring.set_enabled(enable);

        Ok(())
    }

    fn get_config(&mut self, buf: &[u8], _flags: VhostUserConfigFlags) -> Result<Vec<u8>> {
        // TODO - Why?
        /*if self.acked_features & VhostUserProtocolFeatures::CONFIG.bits() == 0 {
                return Err(Error::InvalidOperation);
        }*/
        if buf.len() != mem::size_of::<virtio_blk_config>() {
            return Err(Error::InvalidParam);
        }

        let config: virtio_blk_config = self.backend.get_config().clone();

        let buf = unsafe {
            slice::from_raw_parts(
                &config as *const virtio_blk_config as *const _,
                mem::size_of::<virtio_blk_config>(),
            )
        };

        Ok(buf.to_vec())
    }

    fn set_config(&mut self, buf: &[u8], offset: u32, _flags: VhostUserConfigFlags) -> Result<()> {
        if self.acked_features & VhostUserProtocolFeatures::CONFIG.bits() == 0 {
            return Err(Error::InvalidOperation);
        } else if buf.len() != mem::size_of::<virtio_blk_config>() || offset as usize > buf.len() {
            return Err(Error::InvalidParam);
        }
        // TODO - Implement wce change
        Ok(())
    }
}
