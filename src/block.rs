// Copyright (C) 2019 Red Hat, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright (C) 2019 Alibaba Cloud Computing. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::mem;
use std::os::unix::io::{FromRawFd, RawFd};
use std::result;
use std::slice;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::backend::StorageBackend;
use crate::eventfd::EventFd;
use crate::message::*;
use crate::queue::{DescriptorChain, Queue};
use bitflags::bitflags;
use log::{debug, error};
use virtio_bindings::bindings::virtio_blk::*;
//use virtio_bindings::bindings::virtio_ring::VRING_USED_F_NO_NOTIFY;
use vm_memory::{
    Bytes, GuestAddress, GuestMemory, GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion,
    GuestRegionMmap, MmapRegion,
};

use vhostuser_rs::message::*;
use vhostuser_rs::{Error, Result, VhostUserSlave};

bitflags! {
    pub struct VhostUserBlkFeatures: u64 {
        const MQ = 0x1000;
        const EVENT_IDX = 0x20000000;
        const PROTOCOL_FEATURES = 0x40000000;
    }
}

#[derive(Debug)]
enum ExecuteError {
    BadRequest(Error),
    Flush(io::Error),
    Read(GuestMemoryError),
    Seek(io::Error),
    Write(GuestMemoryError),
    Unsupported(u32),
}

impl ExecuteError {
    fn status(&self) -> u32 {
        match *self {
            ExecuteError::BadRequest(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Flush(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Read(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Seek(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Write(_) => VIRTIO_BLK_S_IOERR,
            ExecuteError::Unsupported(_) => VIRTIO_BLK_S_UNSUPP,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RequestType {
    In,
    Out,
    Flush,
    GetDeviceID,
    Unsupported(u32),
}

fn request_type(
    mem: &GuestMemoryMmap,
    desc_addr: GuestAddress,
) -> result::Result<RequestType, Error> {
    let (region, addr) = mem.to_region_addr(desc_addr).unwrap();
    let type_ = region.read_obj(addr).unwrap();
    match type_ {
        VIRTIO_BLK_T_IN => {
            debug!("VIRTIO_BLK_T_IN");
            Ok(RequestType::In)
        }
        VIRTIO_BLK_T_OUT => {
            debug!("VIRTIO_BLK_T_OUT");
            Ok(RequestType::Out)
        }
        VIRTIO_BLK_T_FLUSH => {
            debug!("VIRTIO_BLK_T_FLUSH");
            Ok(RequestType::Flush)
        }
        VIRTIO_BLK_T_GET_ID => {
            debug!("VIRTIO_BLK_T_GET_ID");
            Ok(RequestType::GetDeviceID)
        }
        t => {
            debug!("unsupported request: {}", t);
            Ok(RequestType::Unsupported(t))
        }
    }
}

fn sector(mem: &GuestMemoryMmap, desc_addr: GuestAddress) -> result::Result<u64, Error> {
    const SECTOR_OFFSET: usize = 8;
    let (region, addr) = mem.to_region_addr(desc_addr).unwrap();
    let addr = region.checked_offset(addr, SECTOR_OFFSET).unwrap();
    Ok(region.read_obj(addr).unwrap())
}

enum ExecuteType {
    Sync(usize),
    Async(usize),
}

struct Request {
    request_type: RequestType,
    sector: u64,
    data_addr: GuestAddress,
    data_len: u32,
    status_addr: GuestAddress,
    desc_index: u16,
}

impl Request {
    fn parse(
        avail_desc: &DescriptorChain,
        mem: &GuestMemoryMmap,
    ) -> result::Result<Request, Error> {
        if avail_desc.is_write_only() {
            error!("unexpected write only descriptor");
            return Err(Error::OperationFailedInSlave);
        }

        let mut req = Request {
            request_type: request_type(&mem, avail_desc.addr)?,
            sector: sector(&mem, avail_desc.addr)?,
            data_addr: GuestAddress(0),
            data_len: 0,
            status_addr: GuestAddress(0),
            desc_index: avail_desc.index,
        };

        let data_desc;
        let status_desc;
        let desc = avail_desc.next_descriptor().unwrap();

        if !desc.has_next() {
            status_desc = desc;
            // Only flush requests are allowed to skip the data descriptor.
            if req.request_type != RequestType::Flush {
                error!("request without data descriptor!");
                return Err(Error::OperationFailedInSlave);
            }
        } else {
            data_desc = desc;
            status_desc = data_desc.next_descriptor().unwrap();

            if data_desc.is_write_only() && req.request_type == RequestType::Out {
                error!("unexpected write only descriptor");
                return Err(Error::OperationFailedInSlave);
            }
            if !data_desc.is_write_only() && req.request_type == RequestType::In {
                error!("unexpected read only descriptor");
                return Err(Error::OperationFailedInSlave);
            }
            if !data_desc.is_write_only() && req.request_type == RequestType::GetDeviceID {
                error!("unexpected read only descriptor");
                return Err(Error::OperationFailedInSlave);
            }

            req.data_addr = data_desc.addr;
            req.data_len = data_desc.len;
        }

        // The status MUST always be writable.
        if !status_desc.is_write_only() {
            error!("unexpected read only descriptor");
            return Err(Error::OperationFailedInSlave);
        }

        if status_desc.len < 1 {
            error!("descriptor length is too small");
            return Err(Error::OperationFailedInSlave);
        }

        req.status_addr = status_desc.addr;

        Ok(req)
    }

    #[allow(clippy::ptr_arg)]
    fn execute<T: StorageBackend>(
        &self,
        disk: &mut T,
        mem: &GuestMemoryMmap,
    ) -> result::Result<ExecuteType, ExecuteError> {
        disk.check_sector_offset(self.sector, self.data_len.into())
            .map_err(|err| {
                debug!("check_sector_offset {:?}", err);
                ExecuteError::BadRequest(Error::InvalidParam)
            })?;
        disk.seek_sector(self.sector).map_err(|err| {
            debug!("seek_sector {:?}", err);
            ExecuteError::Seek(err)
        })?;

        let (region, addr) = mem.to_region_addr(self.data_addr).unwrap();

        match self.request_type {
            RequestType::In => {
                debug!(
                    "reading {} bytes starting at sector {}",
                    self.data_len, self.sector
                );
                match region.read_from(addr, disk, self.data_len as usize) {
                    Ok(l) => {
                        if disk.is_async() {
                            Ok(ExecuteType::Async(l))
                        } else {
                            Ok(ExecuteType::Sync(l))
                        }
                    }
                    Err(err) => {
                        error!("error reading from disk: {:?}", err);
                        Err(ExecuteError::Read(err))
                    }
                }
            }
            RequestType::Out => {
                debug!(
                    "writing out {} bytes starting on sector {}",
                    self.data_len, self.sector
                );
                match region.write_to(addr, disk, self.data_len as usize) {
                    Ok(l) => {
                        if disk.is_async() {
                            Ok(ExecuteType::Async(l))
                        } else {
                            Ok(ExecuteType::Sync(l))
                        }
                    }
                    Err(err) => {
                        error!("error writing to disk: {:?}", err);
                        Err(ExecuteError::Write(err))
                    }
                }
            }
            RequestType::Flush => {
                debug!("requesting backend to flush out disk buffers");
                match disk.flush() {
                    Ok(_) => Ok(ExecuteType::Sync(0)),
                    Err(err) => {
                        error!("error flushing out buffers: {:?}", err);
                        Err(ExecuteError::Flush(err))
                    }
                }
            }
            RequestType::GetDeviceID => {
                debug!("providing device ID data");
                let image_id = disk.get_image_id();
                if (self.data_len as usize) < image_id.len() {
                    error!("data len smaller than disk_id");
                    return Err(ExecuteError::BadRequest(Error::InvalidParam));
                }
                match region.write_slice(image_id, addr) {
                    Ok(_) => Ok(ExecuteType::Sync(image_id.len())),
                    Err(err) => {
                        error!("error writing device ID to vring address: {:?}", err);
                        Err(ExecuteError::Write(err))
                    }
                }
            }
            RequestType::Unsupported(t) => {
                error!("unsupported request");
                Err(ExecuteError::Unsupported(t))
            }
        }
    }
}

pub struct Vring {
    mem: GuestMemoryMmap,
    queue: Queue,
    call_fd: Option<RawFd>,
    kick_fd: Option<RawFd>,
    err_fd: Option<RawFd>,
    features: u64,
    _started: bool,
    enabled: bool,
    async_requests: HashMap<usize, Request>,
    signalled_used_valid: bool,
    signalled_used: u16,
}

impl Vring {
    fn new(mem: GuestMemoryMmap, queue: Queue) -> Self {
        Vring {
            mem,
            queue,
            call_fd: None,
            kick_fd: None,
            err_fd: None,
            features: 0,
            _started: false,
            enabled: false,
            async_requests: HashMap::new(),
            signalled_used_valid: false,
            signalled_used: 0,
        }
    }

    fn get_queue_mut(&mut self) -> &mut Queue {
        &mut self.queue
    }

    pub fn get_kick_fd(&self) -> RawFd {
        self.kick_fd.unwrap()
    }

    pub fn process_completions<S>(&mut self, backend: &mut S) -> Result<bool>
    where
        S: StorageBackend,
    {
        let mut count = 0;

        loop {
            if self.async_requests.is_empty() {
                debug!("no pending requests");
                break;
            }

            if let Some(cookie) = backend
                .get_completion(false)
                .map_err(|_err| Error::OperationFailedInSlave)?
            {
                let request = self.async_requests.remove(&cookie).unwrap();

                debug!(
                    "got completion with cookie: {}, desc={}",
                    cookie, request.desc_index
                );

                let (region, addr) = self.mem.to_region_addr(request.status_addr).unwrap();
                region.write_obj(VIRTIO_BLK_S_OK, addr).unwrap();

                let used_idx = self
                    .queue
                    .add_used(&self.mem, request.desc_index, request.data_len);

                if self.should_signal_guest(used_idx) {
                    self.signal_guest().unwrap();
                } else {
                    debug!("omitting guest signal");
                }

                count += 1;
            }
        }

        Ok(count != 0)
    }

    pub fn process_queue<S: StorageBackend>(&mut self, backend: &mut S) -> Result<bool> {
        let mut used_desc_heads = [(0, 0); 1024 as usize];
        let mut used_count = 0;
        for avail_desc in self.queue.iter(&self.mem) {
            debug!("got an element in the queue");
            match Request::parse(&avail_desc, &self.mem) {
                Ok(request) => {
                    debug!("element is a valid request");
                    let mut len = 0;
                    match request.execute(backend, &self.mem) {
                        Ok(type_) => match type_ {
                            ExecuteType::Sync(l) => {
                                debug!("executing synchronously: desc={}", request.desc_index);
                                len = l;
                                let (region, addr) =
                                    self.mem.to_region_addr(request.status_addr).unwrap();
                                region.write_obj(VIRTIO_BLK_S_OK, addr).unwrap();
                            }
                            ExecuteType::Async(cookie) => {
                                debug!("executing asynchronously: desc={}", request.desc_index);
                                self.async_requests.insert(cookie, request);
                            }
                        },
                        Err(err) => {
                            error!("failed to execute request: {:?}", err);
                            len = 1; // We need at least 1 byte for the status.
                            let (region, addr) =
                                self.mem.to_region_addr(request.status_addr).unwrap();
                            region.write_obj(err.status(), addr).unwrap();
                        }
                    };
                    if len != 0 {
                        used_desc_heads[used_count] = (avail_desc.index, len);
                        used_count += 1;
                    }
                }
                Err(err) => {
                    error!("failed to parse available descriptor chain: {:?}", err);
                    used_desc_heads[used_count] = (avail_desc.index, 0);
                    used_count += 1;
                }
            }
        }

        for &(desc_index, len) in &used_desc_heads[..used_count] {
            let used_idx = self.queue.add_used(&self.mem, desc_index, len as u32);
            if self.should_signal_guest(used_idx) {
                self.signal_guest().unwrap();
            } else {
                debug!("omitting guest signal");
            }
        }

        if backend.is_async() {
            backend.submit_requests().unwrap();
        }

        Ok(used_count > 0)
    }

    pub fn disable_notifications(&self) {
        if self.features & VhostUserBlkFeatures::EVENT_IDX.bits() != 0 {
            self.queue
                .set_avail_event(&self.mem, self.queue.get_last_avail());
        } else {
            // TODO
            //self.queue.set_used_flags_bit(VRING_USED_F_NO_NOTIFY);
        }
    }

    pub fn enable_notifications(&self) {
        if self.features & VhostUserBlkFeatures::EVENT_IDX.bits() != 0 {
            self.queue
                .set_avail_event(&self.mem, self.queue.get_last_avail());
        } else {
            // TODO
            //self.queue.unset_used_flags_bit(VRING_USED_F_NO_NOTIFY);
        }
    }

    fn vring_need_signal(&mut self, new_idx: u16, old_idx: u16) -> bool {
        let used_event = self.queue.get_used_event(&self.mem);
        debug!("used_event={}", used_event);

        if (new_idx - used_event - 1) < (new_idx - old_idx) {
            true
        } else {
            false
        }
    }

    fn should_signal_guest(&mut self, used_idx: u16) -> bool {
        let valid = self.signalled_used_valid;
        self.signalled_used_valid = true;
        let old = self.signalled_used;
        self.signalled_used = used_idx;
        let new = used_idx;

        return !valid || self.vring_need_signal(new, old);
    }

    fn signal_guest(&mut self) -> Result<()> {
        debug!("signaling guest");
        let signal: u64 = 1;
        let ret = unsafe {
            libc::write(
                self.call_fd.unwrap(),
                &signal as *const u64 as *const libc::c_void,
                mem::size_of::<u64>(),
            )
        };

        if ret <= 0 {
            Err(Error::InvalidParam)
        } else {
            Ok(())
        }
    }
}

impl Drop for Vring {
    fn drop(&mut self) {
        println!("dropping vring");
    }
}

pub struct VhostUserBlk<S: StorageBackend> {
    backend: S,
    main_eventfd: EventFd,
    main_sender: Sender<VubMessage>,
    mem: Option<GuestMemoryMmap>,
    memory_regions: Vec<VhostUserMemoryRegion>,
    num_queues: u16,
    vrings: HashMap<usize, Arc<Mutex<Vring>>>,
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
        num_queues: u16,
    ) -> Self {
        VhostUserBlk {
            backend,
            main_eventfd,
            main_sender,
            mem: None,
            memory_regions: vec![],
            num_queues,
            vrings: HashMap::new(),
            vring_default_enabled: false,
            owned: false,
            features_acked: false,
            acked_features: 0,
            acked_protocol_features: 0,
        }
    }

    pub fn set_backend(&mut self, backend: S) {
        self.backend = backend;
    }

    pub fn get_vring(&self, index: usize) -> Result<Arc<Mutex<Vring>>> {
        let vring = match self.vrings.get(&index) {
            Some(v) => v,
            None => return Err(Error::InvalidParam),
        };

        Ok(vring.clone())
    }

    fn find_region(&self, addr: u64) -> Result<&VhostUserMemoryRegion> {
        for region in &self.memory_regions {
            if addr >= region.userspace_addr && addr <= region.userspace_addr + region.memory_size {
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
        Ok(VhostUserBlkFeatures::all().bits())
    }

    fn set_features(&mut self, features: u64) -> Result<()> {
        if !self.owned {
            return Err(Error::InvalidOperation);
            /*} else if self.features_acked {
            return Err(Error::InvalidOperation);
            } else if (features & VhostUserBlkFeatures::all().bits()) != 0 {
                return Err(Error::InvalidParam);*/
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
            self.acked_features & VhostUserBlkFeatures::PROTOCOL_FEATURES.bits() == 0;
        for (_vring_index, vring_mut) in &mut self.vrings {
            let mut vring = vring_mut.lock().unwrap();
            vring.enabled = vring_enabled;
            vring.features = features;
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
        let mut guest_regions: Vec<GuestRegionMmap> = vec![];

        // Reset the current memory_regions array
        self.memory_regions = vec![];

        for region in regions.iter() {
            self.memory_regions.push(VhostUserMemoryRegion {
                guest_phys_addr: region.guest_phys_addr,
                memory_size: region.memory_size,
                userspace_addr: region.userspace_addr,
                mmap_offset: region.mmap_offset,
            });

            let file = unsafe { File::from_raw_fd(fds[i]) };
            let mmap = MmapRegion::from_fd(
                &file,
                region.memory_size as usize,
                region.mmap_offset as i64,
            )
            .map_err(|_err| Error::OperationFailedInSlave)?;

            guest_regions.push(GuestRegionMmap::new(
                mmap,
                GuestAddress(region.guest_phys_addr),
            ));
            i += 1;
        }

        self.mem = Some(
            GuestMemoryMmap::from_regions(guest_regions)
                .map_err(|_err| Error::OperationFailedInSlave)?,
        );
        Ok(())
    }

    fn get_queue_num(&mut self) -> Result<u64> {
        Ok(self.num_queues as u64)
    }

    fn set_vring_num(&mut self, index: u32, num: u32) -> Result<()> {
        let vring_index: usize = index as usize;
        if let Some(mem) = self.mem.as_ref() {
            if !self.vrings.contains_key(&vring_index) {
                self.vrings.insert(
                    vring_index,
                    Arc::new(Mutex::new(Vring::new(mem.clone(), Queue::new(num as u16)))),
                );
            }
        } else {
            return Err(Error::InvalidOperation);
        }

        let vring_mut = self.vrings.get(&vring_index).unwrap();
        let mut vring = vring_mut.lock().unwrap();
        vring.enabled = self.vring_default_enabled;
        vring.features = self.acked_features;
        let queue = vring.get_queue_mut();
        queue.size = num as u16;

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
        let region = self.find_region(descriptor)?;
        let desc_table = GuestAddress(descriptor - region.userspace_addr);

        let region = self.find_region(used)?;
        let used_ring = GuestAddress(used - region.userspace_addr);

        let region = self.find_region(available)?;
        let avail_ring = GuestAddress(available - region.userspace_addr);

        let vring_index: usize = index as usize;
        let mut vring = match self.vrings.get(&vring_index) {
            Some(v) => v.lock().unwrap(),
            None => return Err(Error::InvalidParam),
        };

        let queue = vring.get_queue_mut();
        queue.desc_table = desc_table;
        queue.used_ring = used_ring;
        queue.avail_ring = avail_ring;

        Ok(())
    }

    fn set_vring_base(&mut self, index: u32, base: u32) -> Result<()> {
        let vring_index: usize = index as usize;
        let mem = match self.mem.as_ref() {
            Some(m) => m,
            None => return Err(Error::InvalidParam),
        };
        let mut vring = match self.vrings.get(&vring_index) {
            Some(v) => v.lock().unwrap(),
            None => return Err(Error::InvalidParam),
        };

        let queue = vring.get_queue_mut();
        queue.set_last_index(mem, base as u16);

        Ok(())
    }

    fn get_vring_base(&mut self, index: u32) -> Result<VhostUserVringState> {
        let vring_index: usize = index as usize;

        // Follow vhost-user spec and stop the ring
        self.vrings.remove(&vring_index);

        self.main_sender
            .send(VubMessage::DisableVring(DisableVringMsg {
                index: vring_index,
            }))
            .unwrap();
        self.main_eventfd.write(1u64).unwrap();

        // TODO - Should probably wait for confirmation that all vring
        // workers have stopped

        Ok(VhostUserVringState::new(index, 0))
    }

    fn set_vring_kick(&mut self, index: u8, fd: Option<RawFd>) -> Result<()> {
        let vring_index: usize = index as usize;
        let mut vring = match self.vrings.get(&vring_index) {
            Some(v) => v.lock().unwrap(),
            None => return Err(Error::InvalidParam),
        };

        if vring.kick_fd.is_some() {
            // Close file descriptor set by previous operations.
            let _ = nix::unistd::close(vring.kick_fd.unwrap());
        }
        vring.kick_fd = fd;

        if vring.enabled {
            self.main_sender
                .send(VubMessage::EnableVring(EnableVringMsg {
                    index: vring_index,
                    fd: vring.kick_fd.unwrap(),
                }))
                .unwrap();
            self.main_eventfd.write(1u64).unwrap();
        }

        Ok(())
    }

    fn set_vring_call(&mut self, index: u8, fd: Option<RawFd>) -> Result<()> {
        let vring_index: usize = index as usize;
        let mut vring = match self.vrings.get(&vring_index) {
            Some(v) => v.lock().unwrap(),
            None => return Err(Error::InvalidParam),
        };

        if vring.call_fd.is_some() {
            // Close file descriptor set by previous operations.
            let _ = nix::unistd::close(vring.call_fd.unwrap());
        }
        vring.call_fd = fd;

        Ok(())
    }

    fn set_vring_err(&mut self, index: u8, fd: Option<RawFd>) -> Result<()> {
        let vring_index: usize = index as usize;
        let mut vring = match self.vrings.get(&vring_index) {
            Some(v) => v.lock().unwrap(),
            None => return Err(Error::InvalidParam),
        };

        if vring.err_fd.is_some() {
            // Close file descriptor set by previous operations.
            let _ = nix::unistd::close(vring.err_fd.unwrap());
        }
        vring.err_fd = fd;

        Ok(())
    }

    fn set_vring_enable(&mut self, index: u32, enable: bool) -> Result<()> {
        let vring_index: usize = index as usize;
        // This request should be handled only when VHOST_USER_F_PROTOCOL_FEATURES
        // has been negotiated.
        if self.acked_features & VhostUserBlkFeatures::PROTOCOL_FEATURES.bits() == 0 {
            return Err(Error::InvalidOperation);
        }
        let mut vring = match self.vrings.get_mut(&vring_index) {
            Some(v) => v.lock().unwrap(),
            None => return Err(Error::InvalidParam),
        };

        // Slave must not pass data to/from the backend until ring is
        // enabled by VHOST_USER_SET_VRING_ENABLE with parameter 1,
        // or after it has been disabled by VHOST_USER_SET_VRING_ENABLE
        // with parameter 0.
        vring.enabled = enable;

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

        let mut config: virtio_blk_config = self.backend.get_config().clone();
        config.num_queues = self.num_queues;

        let buf = unsafe {
            slice::from_raw_parts(
                &config as *const virtio_blk_config as *const _,
                mem::size_of::<virtio_blk_config>(),
            )
        };

        Ok(buf.to_vec())
    }

    fn set_config(&mut self, _buf: &[u8], offset: u32, _flags: VhostUserConfigFlags) -> Result<()> {
        if self.acked_features & VhostUserProtocolFeatures::CONFIG.bits() == 0 {
            return Err(Error::InvalidOperation);
        } else if offset < VHOST_USER_CONFIG_OFFSET || offset >= VHOST_USER_CONFIG_SIZE {
            return Err(Error::InvalidParam);
        }
        // TODO - Implement wce change
        Ok(())
    }
}
