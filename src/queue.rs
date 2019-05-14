// Copyright (C) 2019 Red Hat, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::cmp::min;
use std::num::Wrapping;
use std::sync::atomic::{fence, Ordering};

use vm_memory::{
    Address, ByteValued, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion,
};

pub(super) const VIRTQ_DESC_F_NEXT: u16 = 0x1;
pub(super) const VIRTQ_DESC_F_WRITE: u16 = 0x2;

// GuestMemory::read_obj_from_addr() will be used to fetch the descriptor,
// which has an explicit constraint that the entire descriptor doesn't
// cross the page boundary. Otherwise the descriptor may be splitted into
// two mmap regions which causes failure of GuestMemory::read_obj_from_addr().
//
// The Virtio Spec 1.0 defines the alignment of VirtIO descriptor is 16 bytes,
// which fulfills the explicit constraint of GuestMemory::read_obj_from_addr().

/// A virtio descriptor constraints with C representive.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Descriptor {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

unsafe impl ByteValued for Descriptor {}

/// A virtio descriptor chain.
pub struct DescriptorChain<'a> {
    mem: &'a GuestMemoryMmap,
    desc_table: GuestAddress,
    queue_size: u16,
    ttl: u16, // used to prevent infinite chain cycles

    /// Index into the descriptor table
    pub index: u16,

    /// Guest physical address of device specific data
    pub addr: GuestAddress,

    /// Length of device specific data
    pub len: u32,

    /// Includes next, write, and indirect bits
    pub flags: u16,

    /// Index into the descriptor table of the next descriptor if flags has
    /// the next bit set
    pub next: u16,
}

impl<'a> DescriptorChain<'a> {
    fn checked_new(
        mem: &GuestMemoryMmap,
        desc_table: GuestAddress,
        queue_size: u16,
        index: u16,
    ) -> Option<DescriptorChain> {
        if index >= queue_size {
            return None;
        }

        let (region, addr) = mem.to_region_addr(desc_table).unwrap();
        let desc_head = match region.checked_offset(addr, (index as usize) * 16) {
            Some(a) => a,
            None => panic!("invalid desc_head!"),
        };
        region.checked_offset(desc_head, 16)?;

        let desc: Descriptor = region.read_obj(desc_head).unwrap();

        let chain = DescriptorChain {
            mem,
            desc_table,
            queue_size,
            ttl: queue_size,
            index,
            addr: GuestAddress(desc.addr),
            len: desc.len,
            flags: desc.flags,
            next: desc.next,
        };

        if chain.is_valid() {
            Some(chain)
        } else {
            None
        }
    }

    fn is_valid(&self) -> bool {
        !(self
            .mem
            .checked_offset(self.addr, self.len as usize)
            .is_none()
            || (self.has_next() && self.next >= self.queue_size))
    }

    /// Gets if this descriptor chain has another descriptor chain linked after it.
    pub fn has_next(&self) -> bool {
        self.flags & VIRTQ_DESC_F_NEXT != 0 && self.ttl > 1
    }

    /// If the driver designated this as a write only descriptor.
    ///
    /// If this is false, this descriptor is read only.
    /// Write only means the the emulated device can write and the driver can read.
    pub fn is_write_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }

    /// Gets the next descriptor in this descriptor chain, if there is one.
    ///
    /// Note that this is distinct from the next descriptor chain returned by `AvailIter`, which is
    /// the head of the next _available_ descriptor chain.
    pub fn next_descriptor(&self) -> Option<DescriptorChain<'a>> {
        if self.has_next() {
            DescriptorChain::checked_new(self.mem, self.desc_table, self.queue_size, self.next).map(
                |mut c| {
                    c.ttl = self.ttl - 1;
                    c
                },
            )
        } else {
            None
        }
    }
}

/// Consuming iterator over all available descriptor chain heads in the queue.
pub struct AvailIter<'a, 'b> {
    mem: &'a GuestMemoryMmap,
    desc_table: GuestAddress,
    avail_ring: GuestAddress,
    next_index: Wrapping<u16>,
    last_index: Wrapping<u16>,
    queue_size: u16,
    next_avail: &'b mut Wrapping<u16>,
}

impl<'a, 'b> AvailIter<'a, 'b> {
    pub fn new(mem: &'a GuestMemoryMmap, q_next_avail: &'b mut Wrapping<u16>) -> AvailIter<'a, 'b> {
        AvailIter {
            mem,
            desc_table: GuestAddress(0),
            avail_ring: GuestAddress(0),
            next_index: Wrapping(0),
            last_index: Wrapping(0),
            queue_size: 0,
            next_avail: q_next_avail,
        }
    }
}

impl<'a, 'b> Iterator for AvailIter<'a, 'b> {
    type Item = DescriptorChain<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_index == self.last_index {
            return None;
        }

        let (region, addr) = self.mem.to_region_addr(self.avail_ring).unwrap();
        let offset = (4 + (self.next_index.0 % self.queue_size) * 2) as usize;
        let avail_addr = match region.checked_offset(addr, offset) {
            Some(a) => a,
            None => panic!("invalid avail_addr!"),
        };

        let desc_index: u16 = region.read_obj(avail_addr).unwrap();

        self.next_index += Wrapping(1);

        let ret =
            DescriptorChain::checked_new(self.mem, self.desc_table, self.queue_size, desc_index);
        if ret.is_some() {
            *self.next_avail += Wrapping(1);
        }
        ret
    }
}

#[derive(Clone, Default)]
/// A virtio queue's parameters.
pub struct Queue {
    /// The maximal size in elements offered by the device
    max_size: u16,

    /// The queue size in elements the driver selected
    pub size: u16,

    /// Inidcates if the queue is finished with configuration
    pub ready: bool,

    /// Guest physical address of the descriptor table
    pub desc_table: GuestAddress,

    /// Guest physical address of the available ring
    pub avail_ring: GuestAddress,

    /// Guest physical address of the used ring
    pub used_ring: GuestAddress,

    next_avail: Wrapping<u16>,
    next_used: Wrapping<u16>,
}

impl Queue {
    /// Constructs an empty virtio queue with the given `max_size`.
    pub fn new(max_size: u16) -> Queue {
        Queue {
            max_size,
            size: 0,
            ready: false,
            desc_table: GuestAddress(0),
            avail_ring: GuestAddress(0),
            used_ring: GuestAddress(0),
            next_avail: Wrapping(0),
            next_used: Wrapping(0),
        }
    }

    pub fn get_max_size(&self) -> u16 {
        self.max_size
    }

    /// Return the actual size of the queue, as the driver may not set up a
    /// queue as big as the device allows.
    pub fn actual_size(&self) -> u16 {
        min(self.size, self.max_size)
    }

    pub fn is_valid(&self, mem: &GuestMemoryMmap) -> bool {
        let queue_size = self.actual_size() as u64;
        let desc_table = self.desc_table;
        let desc_table_size = 16 * queue_size;
        let avail_ring = self.avail_ring;
        let avail_ring_size = 6 + 2 * queue_size;
        let used_ring = self.used_ring;
        let used_ring_size = 6 + 8 * queue_size;
        if !self.ready {
            //error!("attempt to use virtio queue that is not marked ready");
            false
        } else if self.size > self.max_size || self.size == 0 || (self.size & (self.size - 1)) != 0
        {
            //error!("virtio queue with invalid size: {}", self.size);
            false
        } else if desc_table
            .checked_add(desc_table_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            //error!(
            //    "virtio queue descriptor table goes out of bounds: start:0x{:08x} size:0x{:08x}",
            //    desc_table.raw_value(),
            //    desc_table_size
            //);
            false
        } else if avail_ring
            .checked_add(avail_ring_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            //error!(
            //    "virtio queue available ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
            //    avail_ring.raw_value(),
            //    avail_ring_size
            //);
            false
        } else if used_ring
            .checked_add(used_ring_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            //error!(
            //    "virtio queue used ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
            //    used_ring.raw_value(),
            //    used_ring_size
            //);
            false
        } else if desc_table.raw_value() & 0xf != 0 {
            //error!("virtio queue descriptor table breaks alignment contraints");
            false
        } else if avail_ring.raw_value() & 0x1 != 0 {
            //error!("virtio queue available ring breaks alignment contraints");
            false
        } else if used_ring.raw_value() & 0x3 != 0 {
            //error!("virtio queue used ring breaks alignment contraints");
            false
        } else {
            true
        }
    }

    /// A consuming iterator over all available descriptor chain heads offered by the driver.
    pub fn iter<'a, 'b>(&'b mut self, mem: &'a GuestMemoryMmap) -> AvailIter<'a, 'b> {
        let queue_size = self.actual_size();
        let avail_ring = self.avail_ring;

        let (region, addr) = mem.to_region_addr(avail_ring).unwrap();
        let index_addr = match region.checked_offset(addr, 2) {
            Some(ret) => ret,
            None => panic!("invalid avail_ring!"),
        };
        let last_index: u16 = region.read_obj(index_addr).unwrap();

        AvailIter {
            mem,
            desc_table: self.desc_table,
            avail_ring,
            next_index: self.next_avail,
            last_index: Wrapping(last_index),
            queue_size,
            next_avail: &mut self.next_avail,
        }
    }

    pub fn set_last_index(&mut self, mem: &GuestMemoryMmap, value: u16) {
        let (region, addr) = mem.to_region_addr(self.avail_ring).unwrap();
        let index_addr = match region.checked_offset(addr, 2) {
            Some(ret) => ret,
            None => panic!("Invalid offset"),
        };
        region.write_obj(value, index_addr).unwrap();
    }

    pub fn get_last_index(&self, mem: &GuestMemoryMmap) -> u16 {
        let (region, addr) = mem.to_region_addr(self.avail_ring).unwrap();
        let index_addr = match region.checked_offset(addr, 2) {
            Some(ret) => ret,
            None => panic!("Invalid offset"),
        };
        let last_index: u16 = region.read_obj(index_addr).unwrap();

        last_index
    }

    /// Puts an available descriptor head into the used ring for use by the guest.
    pub fn add_used(&mut self, mem: &GuestMemoryMmap, desc_index: u16, len: u32) {
        if desc_index >= self.actual_size() {
            //error!(
            //    "attempted to add out of bounds descriptor to used ring: {}",
            //    desc_index
            //);
            return;
        }

        let used_ring = self.used_ring;
        let next_used = (self.next_used.0 % self.actual_size()) as u64;
        let used_elem = used_ring.unchecked_add(4 + next_used * 8);
        let (_region, used_ring) = mem.to_region_addr(used_ring).unwrap();
        let (region, used_elem) = mem.to_region_addr(used_elem).unwrap();

        // These writes can't fail as we are guaranteed to be within the descriptor ring.
        region.write_obj(desc_index as u32, used_elem).unwrap();
        region.write_obj(len, used_elem.unchecked_add(4)).unwrap();

        self.next_used += Wrapping(1);

        // This fence ensures all descriptor writes are visible before the index update is.
        fence(Ordering::Release);

        region
            .write_obj(self.next_used.0, used_ring.unchecked_add(2))
            .unwrap();
    }

    /// Goes back one position in the available descriptor chain offered by the driver.
    /// Rust does not support bidirectional iterators. This is the only way to revert the effect
    /// of an iterator increment on the queue.
    pub fn go_to_previous_position(&mut self) {
        self.next_avail -= Wrapping(1);
    }
}
