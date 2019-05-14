// Copyright (C) 2019 Red Hat, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::io::RawFd;

pub struct EnableVringMsg {
    pub index: usize,
    pub fd: RawFd,
}

pub struct DisableVringMsg {
    pub index: usize,
}

pub enum VubMessage {
    EnableVring(EnableVringMsg),
    DisableVring(DisableVringMsg),
}
