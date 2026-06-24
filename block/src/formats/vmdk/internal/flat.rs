// Copyright © 2021 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::io::{self};

use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::Arc;

use crate::formats::vmdk::internal::descriptor::VmdkDescriptor;

const VMDK_SECTOR_SIZE: u64 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtentAccess {
    /// "RW": readable and writable.
    ReadWrite,
    /// "RDONLY": readable only; writes must be rejected.
    ReadOnly,
    /// "NOACCESS": cannot be accessed; reads and writes must be rejected.
    NoAccess,
}

/// One flat VMDK extent: the (optionally) open data file, its declared access
/// mode, and the virtual-disk byte range `[virtual_start, virtual_start +
/// length)` that it backs.
///
/// `twoGbMaxExtentFlat` images concatenate several of these to form the full
/// virtual disk; `monolithicFlat` images have exactly one.
#[derive(Debug)]
pub(crate) struct VmdkExtent {
    /// Open handle to this extent's data file. `None` for `NoAccess` extents,
    /// which are never opened because they cannot be accessed.
    pub file: Option<File>,
    /// Access mode declared for this extent in the descriptor.
    pub access: ExtentAccess,
    /// First virtual-disk offset (in bytes) backed by this extent.
    pub virtual_start: u64,
    /// Length (in bytes) of the virtual-disk range backed by this extent.
    pub length: u64,
}

#[derive(Debug)]
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
    // All opened data extents, in virtual-disk order. These are the files that
    // actually store guest disk contents (the descriptor only holds metadata).
    // Shared via `Arc` so per-queue clones are cheap and observe the same open
    // files, and so the I/O worker can keep them alive independently.
    extents: Arc<Vec<VmdkExtent>>,
    size: u64,
}

impl FlatVmdk {
    pub fn new(file: File, path: &std::path::Path) -> io::Result<Self> {
        let descriptor = VmdkDescriptor::new(&file, path)?;

        if descriptor.extents_list.extents.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VMDK descriptor lists no extents",
            ));
        }

        // Open every data extent and record the virtual-disk byte range it
        // backs. Extents are concatenated in descriptor order: extent N starts
        // where extent N-1 ended. This cumulative layout is exactly what the
        // extent-aware worker uses to translate a guest offset into the right
        // (extent, file offset) pair.
        let mut extents: Vec<VmdkExtent> =
            Vec::with_capacity(descriptor.extents_list.extents.len());
        let mut virtual_start: u64 = 0;
        for extent in &descriptor.extents_list.extents {
            let length = extent.size_in_sectors * VMDK_SECTOR_SIZE;
            let extent_path = Path::new(&descriptor.base_path).join(&extent.filename);

            // Open the backing file using exactly the access declared for this
            // extent on its descriptor line (the `access` field of
            // `VmdkExtentHeader`). The VMDK spec defines three values:
            //   "RW"       -> read + write
            //   "RDONLY"   -> read only
            //   "NOACCESS" -> not accessible; do not open the file at all
            let (access, extent_file) = match extent.access.as_str() {
                "RW" => {
                    let f = OpenOptions::new().read(true).write(true).open(&extent_path)?;
                    (ExtentAccess::ReadWrite, Some(f))
                }
                "RDONLY" => {
                    let f = OpenOptions::new().read(true).open(&extent_path)?;
                    (ExtentAccess::ReadOnly, Some(f))
                }
                "NOACCESS" => (ExtentAccess::NoAccess, None),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unsupported VMDK extent access mode '{other}'"),
                    ));
                }
            };

            extents.push(VmdkExtent {
                file: extent_file,
                access,
                virtual_start,
                length,
            });
            virtual_start += length;
        }

        // The virtual disk size is the end offset of the last extent.
        let total_disk_size = virtual_start;

        Ok(Self {
            descriptor: Arc::new(descriptor),
            // The `file` handed in by the caller is the descriptor file; retain
            // it so its fd remains valid for advisory locking via `AsRawFd`.
            descriptor_file: Arc::new(file),
            extents: Arc::new(extents),
            size: total_disk_size,
        })
    }

    pub fn virtual_block_size(&self) -> u64 {
        self.size
    }

    /// Shared handle to the opened data extents, used to build the I/O worker.
    ///
    /// Cloning the `Arc` keeps every extent file alive for as long as the
    /// worker exists, independent of this `FlatVmdk`.
    pub fn extents(&self) -> Arc<Vec<VmdkExtent>> {
        Arc::clone(&self.extents)
    }

    /// Host allocation size: the sum of every opened extent file's size.
    /// `NoAccess` extents contribute 0 to the total.
    pub fn physical_block_size(&self) -> u64 {
        self.extents
            .iter()
            .map(|extent| {
                extent
                    .file
                    .as_ref()
                    .and_then(|f| f.metadata().ok())
                    .map(|m| m.len())
                    .unwrap_or(0)
            })
            .sum()
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

// Cloning a `FlatVmdk` produces an independent data-plane handle (one per
// virtio queue worker) that shares the original's reference-counted state.
//
// Both fields are `Arc`, so a clone only bumps reference counts -- the parsed
// descriptor metadata is never deep-copied, and every clone observes the *same*
// open descriptor file (hence the same fd for advisory locking) without
// dup'ing the file descriptor.
impl Clone for FlatVmdk {
    fn clone(&self) -> Self {
        Self {
            descriptor: Arc::clone(&self.descriptor),
            descriptor_file: Arc::clone(&self.descriptor_file),
            extents: Arc::clone(&self.extents),
            size: self.size,
        }
    }
}
