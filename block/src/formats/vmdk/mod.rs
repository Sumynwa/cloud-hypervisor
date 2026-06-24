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
use crate::disk_file;

// VMDK uses a synchronous, extent-aware worker (see `worker::sync`) that maps
// each request to the backing extent(s). This supports both single-extent
// `monolithicFlat` and multi-extent `twoGbMaxExtentFlat` images.
//
// io_uring/AIO remain disabled for VMDK: their one-fd + one-offset submission
// model cannot express a single request that spans two extent files, so a
// `twoGbMaxExtentFlat` boundary-crossing request has no direct kernel-async
// representation. The synchronous worker handles that by splitting the request
// across extents in user space.
#[derive(Debug)]
pub struct VmdkDisk {
    inner: FlatVmdk,
    use_async_io: bool,
}

impl VmdkDisk {
    pub fn new(
        file: std::fs::File,
        path: &std::path::Path,
        enable_async_io: bool,
    ) -> Result<Self, crate::error::BlockError> {
        let inner = FlatVmdk::new(file, path)?;
        Ok(VmdkDisk {
            inner,
            use_async_io: enable_async_io,
        })
    }
}

// Not implementing `BlockBackend` for VmdkDisk.
// The `DiskFile` trait is now used instead, which provides the
// necessary functionality for disk operations. 
impl disk_file::DiskSize for VmdkDisk {
    fn logical_size(&self) -> BlockResult<u64> {
        Ok(self.inner.virtual_block_size())
    }
}

impl disk_file::PhysicalSize for VmdkDisk {
    fn physical_size(&self) -> BlockResult<u64> {
        Ok(self.inner.physical_block_size())
    }
}

// Expose the backing fd for advisory image locking only (not data I/O).
//
// For VMDK this resolves to the *descriptor* file's fd. The descriptor
// enumerates every extent, so locking it guards the whole image regardless of
// whether the layout is single-extent (`monolithicFlat`) or multi-extent
// (`twoGbMaxExtentFlat`). See `FlatVmdk`'s `AsRawFd` impl for the rationale.
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
            DiskFileError::ResizeError(io::Error::other("resize not supported for flat VMDK")),
        )
        .with_op(ErrorOp::Resize))
    }
}

impl disk_file::DiskFile for VmdkDisk {}

impl disk_file::AsyncDiskFile for VmdkDisk {
    fn try_clone(&self) -> BlockResult<Box<dyn disk_file::AsyncDiskFile>> {
        Ok(Box::new(VmdkDisk {
            inner: self.inner.clone(),
            use_async_io: self.use_async_io,
        }))
    }

    fn create_async_io(&self, ring_depth: u32) -> BlockResult<Box<dyn AsyncIo>> {
        // VMDK provides a synchronous, extent-aware worker, so the io_uring ring
        // depth is unused here.
        let _ = ring_depth;

        // The extent-aware worker maps each request to the backing extent(s), so
        // it handles both single-extent `monolithicFlat` and multi-extent
        // `twoGbMaxExtentFlat` images. It is bounded by the virtual disk size so
        // out-of-range requests are rejected.
        Ok(Box::new(
            FlatVmdkSync::new(self.inner.extents(), self.inner.virtual_block_size()).map_err(
                |e| {
                    BlockError::new(BlockErrorKind::Io, DiskFileError::NewAsyncIo(e))
                        .with_op(ErrorOp::Open)
                },
            )?,
        ))
    }
}