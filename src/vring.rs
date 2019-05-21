// Copyright (C) 2019 Red Hat, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::mem;
use std::num::Wrapping;
use std::os::unix::io::RawFd;
use std::result;
use std::slice;
use std::sync::atomic::{fence, Ordering};

use crate::backend::StorageBackend;
use log::{debug, error};
use vhostuser_rs::{Error, Result};
use virtio_bindings::bindings::virtio_blk::*;
use virtio_bindings::bindings::virtio_ring::*;

#[derive(Debug)]
enum ExecuteError {
    BadRequest(Error),
    Flush(io::Error),
    Read(io::Error),
    Seek(io::Error),
    Write(io::Error),
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

fn request_type(mmap_addr: u64, desc_addr: u64) -> result::Result<RequestType, Error> {
    let addr = mmap_addr + desc_addr;
    let type_ = unsafe { &*(addr as *const u32) };
    match *type_ {
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

fn sector(mmap_addr: u64, desc_addr: u64) -> result::Result<u64, Error> {
    let addr = mmap_addr + desc_addr + 8;
    let sector = unsafe { &*(addr as *const u64) };
    Ok(*sector)
}

#[derive(Debug)]
struct Request {
    request_type: RequestType,
    sector: u64,
    data_addr: u64,
    data_len: u32,
    status_addr: u64,
}

impl Request {
    #[allow(clippy::ptr_arg)]
    fn execute<T: StorageBackend>(&self, disk: &mut T) -> result::Result<u32, ExecuteError> {
        disk.check_sector_offset(self.sector, self.data_len.into())
            .map_err(|err| {
                debug!("check_sector_offset {:?}", err);
                ExecuteError::BadRequest(Error::InvalidParam)
            })?;
        disk.seek_sector(self.sector).map_err(|err| {
            debug!("seek_sector {:?}", err);
            ExecuteError::Seek(err)
        })?;

        let data_buf =
            unsafe { slice::from_raw_parts_mut(self.data_addr as *mut u8, self.data_len as usize) };

        match self.request_type {
            RequestType::In => {
                debug!(
                    "reading {} bytes starting at sector {}",
                    self.data_len, self.sector
                );
                match disk.read_exact(data_buf) {
                    Ok(_) => return Ok(self.data_len),
                    Err(err) => {
                        error!("error reading from disk: {:?}", err);
                        return Err(ExecuteError::Read(err));
                    }
                }
            }
            RequestType::Out => {
                debug!(
                    "writing out {} bytes starting on sector {}",
                    self.data_len, self.sector
                );
                match disk.write_all(data_buf) {
                    Ok(_) => (),
                    Err(err) => {
                        error!("error writing to disk: {:?}", err);
                        return Err(ExecuteError::Write(err));
                    }
                }
            }
            RequestType::Flush => {
                debug!("requesting backend to flush out disk buffers");
                match disk.flush() {
                    Ok(_) => return Ok(0),
                    Err(err) => {
                        error!("error flushing out buffers: {:?}", err);
                        return Err(ExecuteError::Flush(err));
                    }
                }
            }
            RequestType::GetDeviceID => {
                debug!("providing device ID data");
                /*
                let image_id = disk.get_image_id();
                if (self.data_len as usize) < image_id.len() {
                    error!("data len smaller than disk_id");
                    return Err(ExecuteError::BadRequest(Error::InvalidParam));
                }
                match region.write_slice(image_id, addr) {
                    Ok(_) => (),
                    Err(err) => {
                        error!("error writing device ID to vring address: {:?}", err);
                        return Err(ExecuteError::Write(err));
                    }
                }
                 */
            }
            RequestType::Unsupported(t) => {
                error!("unsupported request");
                return Err(ExecuteError::Unsupported(t));
            }
        };
        Ok(0)
    }
}

pub struct VringMmapRegion {
    mmap_addr: u64,
    size: u64,
}

impl VringMmapRegion {
    pub fn new(mmap_addr: u64, size: u64) -> Self {
        VringMmapRegion { mmap_addr, size }
    }
}

pub struct VringMemTable {
    desc_region: VringMmapRegion,
    desc_addr: u64,
    used_region: VringMmapRegion,
    used_addr: u64,
    avail_region: VringMmapRegion,
    avail_addr: u64,
}

pub enum VringFd {
    Kick,
    Call,
    Error,
}

pub struct Vring<S: StorageBackend> {
    backend: S,

    mem_table: Option<VringMemTable>,
    shadow_avail_idx: u16,
    last_avail_idx: Wrapping<u16>,
    next_used: Wrapping<u16>,
    size: u16,

    call_fd: Option<RawFd>,
    kick_fd: Option<RawFd>,
    err_fd: Option<RawFd>,
    started: bool,
    enabled: bool,
}

impl<S: StorageBackend> Vring<S> {
    pub fn new(backend: S) -> Self {
        Vring {
            backend,
            mem_table: None,
            shadow_avail_idx: 0,
            last_avail_idx: Wrapping(0),
            next_used: Wrapping(0),
            size: 0,
            call_fd: None,
            kick_fd: None,
            err_fd: None,
            started: false,
            enabled: false,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_started(&mut self, started: bool) {
        self.started = started;
    }

    pub fn set_size(&mut self, size: u16) {
        self.size = size;
    }

    pub fn set_mem_table(
        &mut self,
        desc_region: VringMmapRegion,
        desc_addr: u64,
        used_region: VringMmapRegion,
        used_addr: u64,
        avail_region: VringMmapRegion,
        avail_addr: u64,
    ) -> Result<()> {
        // TODO - check offsets
        self.mem_table = Some(VringMemTable {
            desc_region,
            desc_addr,
            used_region,
            used_addr,
            avail_region,
            avail_addr,
        });

        let used = unsafe { &mut *(used_addr as *mut vring_used) };
        self.next_used = Wrapping(used.idx);
        Ok(())
    }

    pub fn set_base_index(&mut self, base_index: u16) {
        self.shadow_avail_idx = base_index;
        self.last_avail_idx = Wrapping(base_index);
    }

    pub fn get_base_index(&self) -> u16 {
        self.last_avail_idx.0
    }

    pub fn set_fd(&mut self, new_fd: Option<RawFd>, type_: VringFd) {
        let old_fd = match type_ {
            VringFd::Kick => self.kick_fd,
            VringFd::Call => self.call_fd,
            VringFd::Error => self.err_fd,
        };

        if old_fd.is_some() {
            // Close file descriptor set by previous operations.
            let _ = nix::unistd::close(old_fd.unwrap());
        }

        match type_ {
            VringFd::Kick => self.kick_fd = new_fd,
            VringFd::Call => self.call_fd = new_fd,
            VringFd::Error => self.err_fd = new_fd,
        };
    }

    pub fn get_fd(&self, type_: VringFd) -> Option<RawFd> {
        match type_ {
            VringFd::Kick => self.kick_fd,
            VringFd::Call => self.call_fd,
            VringFd::Error => self.err_fd,
        }
    }

    fn get_avail_idx(&mut self) -> u16 {
        let mem_table = self.mem_table.as_ref().unwrap();
        let avail = unsafe { &*(mem_table.avail_addr as *const vring_avail) };
        self.shadow_avail_idx = avail.idx;
        self.shadow_avail_idx
    }

    fn is_empty(&mut self) -> bool {
        if self.shadow_avail_idx != self.last_avail_idx.0 {
            return false;
        }

        self.get_avail_idx() == self.last_avail_idx.0
    }

    fn get_head_desc_idx(&self, idx: u16) -> u16 {
        let mem_table = self.mem_table.as_ref().unwrap();
        let head_offset = (2 * (idx % self.size)) as u64;
        let head_addr = mem_table.avail_addr + 4 + head_offset;
        let head_desc_idx = unsafe { &*(head_addr as *const u16) };

        if *head_desc_idx > self.size {
            panic!("bogus head descriptor index");
        }

        *head_desc_idx
    }

    fn get_desc(&self, idx: u16) -> &vring_desc {
        // TODO - check that descriptor address is within the region
        let mem_table = self.mem_table.as_ref().unwrap();
        unsafe { &*((mem_table.desc_addr + 16 * (idx as u64)) as *const vring_desc) }
    }

    fn get_next_request(&mut self) -> Result<(Request, u16)> {
        let mem_table = self.mem_table.as_ref().unwrap();
        let head_desc_idx = self.get_head_desc_idx(self.last_avail_idx.0);
        self.last_avail_idx += Wrapping(1);
        let head_desc = self.get_desc(head_desc_idx).clone();

        let mut req = Request {
            request_type: request_type(mem_table.desc_region.mmap_addr, head_desc.addr).unwrap(),
            sector: sector(mem_table.desc_region.mmap_addr, head_desc.addr).unwrap(),
            data_addr: 0,
            data_len: 0,
            status_addr: 0,
        };

        debug!("req={:?}", req);

        let data_desc;
        let status_desc;
        let desc = self.get_desc(head_desc.next);

        if desc.flags as u32 & VRING_DESC_F_NEXT == 0 {
            status_desc = desc;
            if req.request_type != RequestType::Flush {
                error!("request without data descriptor");
                return Err(Error::OperationFailedInSlave);
            }
        } else {
            data_desc = desc;
            status_desc = self.get_desc(data_desc.next);

            req.data_addr = mem_table.desc_region.mmap_addr + data_desc.addr;
            req.data_len = data_desc.len;
        }

        req.status_addr = mem_table.desc_region.mmap_addr + status_desc.addr;
        Ok((req, head_desc_idx))
    }

    fn add_used(&mut self, desc_index: u16, len: u32) {
        debug!(
            "add_used: idx={} len={} next={}",
            desc_index, len, self.next_used.0
        );
        let mem_table = self.mem_table.as_ref().unwrap();
        let next_used = self.next_used.0 % self.size;
        let mut addr = mem_table.used_addr + 4 + (next_used as u64) * 8;

        let used_elem_idx = unsafe { &mut *(addr as *mut u32) };
        *used_elem_idx = desc_index as u32;
        addr += 4;
        let used_elem_len = unsafe { &mut *(addr as *mut u32) };
        *used_elem_len = len;

        self.next_used += Wrapping(1);

        fence(Ordering::Release);

        let addr = mem_table.used_addr + 2;
        let used_idx = unsafe { &mut *(addr as *mut u16) };
        *used_idx = self.next_used.0;
    }

    pub fn process_queue(&mut self) -> Result<bool> {
        let mut used_desc_heads = [(0, 0); 1024 as usize];
        let mut used_count = 0;

        loop {
            if self.is_empty() {
                debug!("emtpy queue");
                break;
            }

            debug!("got an element in the queue");
            let len;
            match self.get_next_request() {
                Ok((request, index)) => {
                    debug!("element is a valid request");

                    let status = match request.execute(&mut self.backend) {
                        Ok(l) => {
                            len = l;
                            VIRTIO_BLK_S_OK
                        }
                        Err(err) => {
                            error!("failed to execute request: {:?}", err);
                            len = 1; // We need at least 1 byte for the status.
                            err.status()
                        }
                    };
                    let status_mem = unsafe { &mut *(request.status_addr as *mut u32) };
                    *status_mem = status;
                    //let (region, addr) = self.mem.to_region_addr(request.status_addr).unwrap();
                    //region.write_obj(status, addr).unwrap();
                    used_desc_heads[used_count] = (index, len);
                    used_count += 1;
                }
                Err(err) => {
                    panic!("failed to parse available descriptor chain: {:?}", err);
                }
            }
        }

        for &(desc_index, len) in &used_desc_heads[..used_count] {
            self.add_used(desc_index, len);
        }

        Ok(used_count > 0)
    }

    pub fn signal_guest(&mut self) -> Result<()> {
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
