// Copyright (C) 2019 Red Hat, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod backend;
pub mod backend_raw;
pub mod backend_raw_async;
pub mod block;
pub mod eventfd;
pub mod message;
pub mod queue;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
