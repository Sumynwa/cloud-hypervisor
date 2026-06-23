// Copyright © 2021 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io::{self};

use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

use crate::formats::vmdk::internal::descriptor::VmdkDescriptor;

const VMDK_SECTOR_SIZE: u64 = 512;

#[derive(Debug, Clone)]
pub struct FlatVmdk {
    // Parsed VMDK descriptor (shared, read-only metadata). Wrapped in `Arc`
    // so per-virtio-queue clones are cheap pointer bumps rather than deep copies.
    descriptor: Arc<VmdkDescriptor>,
    // Open handle to the VMDK *descriptor* file.
    //
    // We deliberately keep the descriptor file open for the entire lifetime of
    // the disk (rather than the extent/data files) because its file descriptor
    // is what `DiskFd`/`AsRawFd` expose for advisory image locking. Holding the
    // `File` here guarantees the fd stays valid as long as the disk is in use.
    // `Arc` lets `try_clone` share the same underlying open file across queue
    // workers without dup'ing the fd.
    descriptor_file: Arc<File>,
    size: u64,
}

impl FlatVmdk {
    pub fn new(file: File, path: &std::path::Path) -> io::Result<Self> {
        let descriptor = VmdkDescriptor::new(&file, path)?;

        // TO-DO: Check if we should calculate the total VMDK virtual disk size based on the descriptor's extents list.
        let mut total_disk_size: u64 = 0;
        for extent in &descriptor.extents_list.extents {
            total_disk_size += extent.size_in_sectors * VMDK_SECTOR_SIZE;
        }

        Ok(Self {
            descriptor: Arc::new(descriptor),
            // The `file` handed in by the caller is the descriptor file; retain
            // it so its fd remains valid for advisory locking via `AsRawFd`.
            descriptor_file: Arc::new(file),
            size: total_disk_size,
        })
    }

    pub fn virtual_block_size(&self) -> u64 {
        self.size
    }

    // TO-DO: For flat VMDK, should be calculate the actual block size on
    // host based on every extents file size on host
    pub fn physical_block_size(&self) -> u64 {
        let mut total_extents_file_size: u64 = 0;
        // iterate over all the extents
        for extent in &self.descriptor.extents_list.extents {
            let mut extent_path = self.descriptor.base_path.clone();
            extent_path.push_str(&extent.filename);
            let metadata = std::fs::metadata(extent_path).unwrap();
            total_extents_file_size += metadata.len();
        }
        total_extents_file_size
    }
}

// Expose the *descriptor* file's fd as the disk's representative fd.
//
// VMDK splits an image into a text descriptor plus one or more extent (data)
// files. Two flat layouts are supported: `monolithicFlat` (descriptor + a
// single extent) and `twoGbMaxExtentFlat` (descriptor + multiple 2GB extents).
//
// The fd returned here is used solely for advisory whole-image locking (see
// `Block::try_lock_image`/`unlock_image`), never for data-plane I/O. We expose
// the descriptor file's fd rather than an extent's because:
//   1. The descriptor is the single authoritative entry point that enumerates
//      every extent, so locking it protects the whole image -- including the
//      multi-extent `twoGbMaxExtentFlat` layout.
//   2. `DiskFd` can only return one fd; there is no single data fd to lock for
//      multi-extent images.
//   3. Locking one arbitrary extent would leave the remaining extents (and the
//      descriptor itself) unprotected.
impl AsRawFd for FlatVmdk {
    fn as_raw_fd(&self) -> RawFd {
        self.descriptor_file.as_raw_fd()
    }
}
