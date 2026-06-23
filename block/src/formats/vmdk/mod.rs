// Copyright 2026 The Cloud Hypervisor Authors. All rights reserved.
//
// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// SPDX-License-Identifier: Apache-2.0

//! Flat VMDK disk image format.
//!
//! Provides [`VmdkDisk`], the `DiskFile` wrapper for flat VMDK
//! images of types `monolithicFlat` and `twoGbMaxExtentFlat`.

pub(crate) mod internal;
pub(crate) mod worker;

use std::io;
use std::os::unix::io::AsRawFd;

pub use internal::descriptor::is_flat_vmdk;

use self::internal::flat::FlatVmdk;
use self::worker::sync::FlatVmdkSync;

use crate::async_io::{AsyncIo, BorrowedDiskFd, DiskFileError};
use crate::error::{BlockError, BlockErrorKind, BlockResult, ErrorOp};
use crate::{disk_file};

#[derive(Debug)]
#[cfg_attr(not(test), expect(dead_code))]
pub struct VmdkDisk {
    inner: FlatVmdk,
    use_io_uring: bool,
}

impl VmdkDisk {
    pub fn new(file: std::fs::File) -> Result<Self, crate::error::BlockError> {
        let inner = FlatVmdk::new(file)?;
        Ok(VmdkDisk {
            inner,
            use_io_uring: false,
        })
    }
}

impl disk_file::DiskSize for VmdkDisk {
    fn logical_size(&self) -> BlockResult<u64> {
        Ok(0) // TO-DO: Implement a proper check for flat VMDK files
    }
}

impl disk_file::PhysicalSize for VmdkDisk {
    fn physical_size(&self) -> BlockResult<u64> {
        Ok(0) // TO-DO: Implement a proper check for flat VMDK files
    }
}

impl disk_file::DiskFd for VmdkDisk {
    fn fd(&self) -> BorrowedDiskFd<'_> {
        BorrowedDiskFd::new(self.inner.as_raw_fd())
    }
}

impl disk_file::Geometry for VmdkDisk {}

impl disk_file::SparseCapable for VmdkDisk {}

impl disk_file::Resizable for VmdkDisk {
    fn resize(&mut self, _size: u64) -> BlockResult<()> {
        Err(BlockError::new(
            BlockErrorKind::UnsupportedFeature,
            DiskFileError::ResizeError(io::Error::other("resize not supported for fixed VMDK")),
        )
        .with_op(ErrorOp::Resize))
    }
}

impl disk_file::DiskFile for VmdkDisk {}

impl disk_file::AsyncDiskFile for VmdkDisk {
    fn try_clone(&self) -> BlockResult<Box<dyn disk_file::AsyncDiskFile>> {
        Ok(Box::new(VmdkDisk {
            inner: self.inner.clone(),
            use_io_uring: self.use_io_uring,
        }))
    }

    fn create_async_io(&self, ring_depth: u32) -> BlockResult<Box<dyn AsyncIo>> {
        let size: u64 = 0;
        let _ = ring_depth;
        Ok(Box::new(
            FlatVmdkSync::new(self.inner.as_raw_fd(), size).map_err(|e| {
                BlockError::new(BlockErrorKind::Io, DiskFileError::NewAsyncIo(e))
                    .with_op(ErrorOp::Open)
            })?,
        ))
    }
}