// Copyright (C) 2019 Red Hat, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::fs::OpenOptions;
use std::io::{Error, ErrorKind, Read, Result, Seek, SeekFrom, Write};
use std::num::Wrapping;
use std::os::linux::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};

use crate::backend::StorageBackend;
use io_uring::UringQueue;
use virtio_bindings::bindings::virtio_blk::{virtio_blk_config, VIRTIO_BLK_ID_BYTES};

const SECTOR_SHIFT: u8 = 9;
const SECTOR_SIZE: u64 = (0x01 as u64) << SECTOR_SHIFT;
const BLK_SIZE: u32 = 512;

pub fn build_device_id(image: &File) -> Result<String> {
    let blk_metadata = image.metadata()?;
    // This is how kvmtool does it.
    let device_id = format!(
        "{}{}{}",
        blk_metadata.st_dev(),
        blk_metadata.st_rdev(),
        blk_metadata.st_ino()
    )
    .to_owned();
    Ok(device_id)
}

pub struct StorageBackendRawAsync {
    queue: UringQueue,
    image: File,
    image_id: Vec<u8>,
    position: u64,
    next_cookie: Wrapping<usize>,
    config: virtio_blk_config,
}

impl StorageBackendRawAsync {
    pub fn new(
        image_path: &str,
        eventfd: RawFd,
        rdonly: bool,
        flags: i32,
    ) -> Result<StorageBackendRawAsync> {
        let mut options = OpenOptions::new();
        options.read(true);
        if !rdonly {
            options.write(true);
        }
        if flags != 0 {
            options.custom_flags(flags);
        }
        let mut image = options.open(image_path)?;

        let mut queue = UringQueue::new(128).unwrap();
        queue.register_eventfd(eventfd).unwrap();

        let mut config = virtio_blk_config::default();
        config.capacity = (image.seek(SeekFrom::End(0)).unwrap() as u64) / SECTOR_SIZE;
        config.blk_size = BLK_SIZE;
        config.size_max = 65535;
        config.seg_max = 128 - 2;
        config.min_io_size = 1;
        config.opt_io_size = 1;
        config.num_queues = 1;

        let image_id_str = build_device_id(&image)?;
        let image_id_bytes = image_id_str.as_bytes();
        let mut image_id_len = image_id_bytes.len();
        if image_id_len > VIRTIO_BLK_ID_BYTES as usize {
            image_id_len = VIRTIO_BLK_ID_BYTES as usize;
        }
        let mut image_id = vec![0; VIRTIO_BLK_ID_BYTES as usize];
        image_id[..image_id_len].copy_from_slice(&image_id_bytes[..image_id_len]);

        Ok(StorageBackendRawAsync {
            queue,
            image,
            image_id,
            position: 0u64,
            next_cookie: Wrapping(0),
            config,
        })
    }
}

impl Read for StorageBackendRawAsync {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let cookie = self.next_cookie.0;
        self.queue
            .prepare_read(
                self.image.as_raw_fd(),
                buf,
                self.position as i64,
                cookie as u64,
            )
            .unwrap();
        self.next_cookie += Wrapping(1);
        Ok(cookie)
    }
}

impl Seek for StorageBackendRawAsync {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        match pos {
            SeekFrom::Start(offset) => self.position = offset as u64,
            SeekFrom::Current(offset) => self.position += offset as u64,
            SeekFrom::End(offset) => {
                self.position = (self.config.capacity << SECTOR_SHIFT) + (offset as u64)
            }
        }
        Ok(self.position)
    }
}

impl Write for StorageBackendRawAsync {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let cookie = self.next_cookie.0;
        self.queue
            .prepare_write(
                self.image.as_raw_fd(),
                buf,
                self.position as i64,
                cookie as u64,
            )
            .unwrap();
        self.next_cookie += Wrapping(1);
        Ok(cookie)
    }

    fn flush(&mut self) -> Result<()> {
        self.image.flush()
    }
}

/*
impl Clone for StorageBackendRawAsync {
    fn clone(&self) -> Self {
        StorageBackendRawAsync {
            queue: None,
            image: self.image.try_clone().unwrap(),
            image_id: self.image_id.clone(),
            position: self.position,
            last_cookie: Wrapping(0),
            config: self.config.clone(),
        }
    }
}
*/

impl StorageBackend for StorageBackendRawAsync {
    fn get_config(&self) -> &virtio_blk_config {
        &self.config
    }

    fn get_sectors(&self) -> u64 {
        self.config.capacity
    }

    fn get_image_id(&self) -> &Vec<u8> {
        &self.image_id
    }

    fn is_async(&self) -> bool {
        true
    }

    fn submit_requests(&mut self) -> Result<()> {
        match self.queue.submit_requests() {
            Ok(_) => Ok(()),
            Err(io_uring::Error::IOError(e)) => Err(e),
            Err(e) => panic!("Can't submit requests: {:?}", e),
        }
    }

    fn get_completion(&mut self, wait: bool) -> Result<Option<usize>> {
        match self.queue.get_completion(wait) {
            Ok(c) => match c {
                Some(c) => Ok(Some(c as usize)),
                None => Ok(None),
            },
            Err(io_uring::Error::IOError(e)) => Err(e),
            Err(e) => panic!("Can't grab completion: {:?}", e),
        }
    }

    fn check_sector_offset(&self, sector: u64, len: u64) -> Result<()> {
        let mut top = len / SECTOR_SIZE;
        if len % SECTOR_SIZE != 0 {
            top += 1;
        }

        top = top.checked_add(sector).unwrap();
        if top > self.config.capacity {
            Err(Error::new(
                ErrorKind::InvalidInput,
                "offset beyond image end",
            ))
        } else {
            Ok(())
        }
    }

    fn seek_sector(&mut self, sector: u64) -> Result<u64> {
        if sector >= self.config.capacity {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "sector beyond image end",
            ));
        }

        self.seek(SeekFrom::Start(sector << SECTOR_SHIFT))
    }
}
