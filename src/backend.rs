// Copyright (C) 2019 Red Hat, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::{Read, Result, Seek, Write};

use virtio_bindings::bindings::virtio_blk::virtio_blk_config;

pub trait StorageBackend: Read + Seek + Write {
    fn get_config(&self) -> &virtio_blk_config;

    fn get_sectors(&self) -> u64;

    fn get_image_id(&self) -> &Vec<u8>;

    fn is_sync(&self) -> bool;

    fn get_last_cookie(&self) -> u32;

    fn get_completion(&mut self, wait: bool) -> Result<Option<u32>>;

    fn check_sector_offset(&self, sector: u64, len: u64) -> Result<()>;

    fn seek_sector(&mut self, sector: u64) -> Result<u64>;
}
