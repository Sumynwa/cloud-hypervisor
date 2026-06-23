// Copyright © 2021 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io::{self};

use std::os::unix::io::{AsRawFd, RawFd};

#[derive(Debug)]
pub struct FlatVmdk {
    file: File,
    size: u64,
}

impl FlatVmdk {
    pub fn new(file: File) -> io::Result<Self> {
        Ok(Self {
            file,
            size: 0, // TO-DO: Implement a proper check for flat VMDK files
        })
    }
}

impl AsRawFd for FlatVmdk {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl Clone for FlatVmdk {
    fn clone(&self) -> Self {
        Self {
            file: self.file.try_clone().expect("FlatVmdk cloning failed"),
            size: self.size,
        }
    }
}