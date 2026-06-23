// Copyright © 2026 Microsoft Corporation
//
// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::{cmp, io};

use vmm_sys_util::eventfd::EventFd;

use crate::AlignedFile;
use crate::async_io::{AsyncIo, AsyncIoCompletion, AsyncIoError, AsyncIoOperation, AsyncIoResult};
use crate::formats::vmdk::internal::flat::{ExtentAccess, VmdkExtent};

/// Synchronous, extent-aware I/O worker for flat VMDK images.
///
/// Maps each guest request to one or more backing extents and performs the I/O
/// with blocking `preadv`/`pwritev`. A request that stays within a single
/// extent -- always true for `monolithicFlat`, and the common case for
/// `twoGbMaxExtentFlat` -- takes a zero-copy fast path. Only a request that
/// straddles an extent boundary is split into per-extent segments, each copied
/// through a temporary buffer.
///
/// When the image was opened with `direct=on`, the extents are `O_DIRECT` and
/// the guest buffers/offsets are not guaranteed to be block-aligned. In that
/// case the transfer is routed through [`AlignedFile`], which bounces the I/O
/// through an aligned buffer (read-modify-write for sub-block writes); the
/// zero-copy vectored fast path is used only for buffered (non-`O_DIRECT`)
/// extents.
///
/// TO-DO: async backends (io_uring/AIO) submit one fd + one offset per
/// operation and cannot express a single request spanning two extent files.
pub struct FlatVmdkSync {
    // Opened extents in virtual-disk order. Held here so the fds stay valid for
    // the worker's lifetime, independent of the originating `FlatVmdk`.
    extents: Arc<Vec<VmdkExtent>>,
    // Total virtual disk size; requests beyond this are rejected.
    size: u64,
    eventfd: EventFd,
    completion_list: VecDeque<AsyncIoCompletion>,
}

impl FlatVmdkSync {
    pub fn new(extents: Arc<Vec<VmdkExtent>>, size: u64) -> io::Result<Self> {
        Ok(FlatVmdkSync {
            extents,
            size,
            eventfd: EventFd::new(libc::EFD_NONBLOCK)?,
            completion_list: VecDeque::new(),
        })
    }

    // Returns the extent containing virtual `offset`, or `None` if out of range.
    fn extent_at(&self, offset: u64) -> Option<&VmdkExtent> {
        self.extents
            .iter()
            .find(|e| offset >= e.virtual_start && offset < e.virtual_start + e.length)
    }

    // Validates that every extent the request touches permits the operation.
    // Rejects any I/O to a `NoAccess` extent, and rejects writes to a
    // `ReadOnly` extent, per the access mode declared in the descriptor.
    fn check_access(&self, start: u64, total: u64, is_read: bool) -> io::Result<()> {
        let end = start + total;
        let mut cur = start;
        while cur < end {
            let extent = self.extent_at(cur).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "offset outside any VMDK extent")
            })?;
            match extent.access {
                ExtentAccess::NoAccess => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("VMDK extent at offset {cur} is NOACCESS; request rejected"),
                    ));
                }
                ExtentAccess::ReadOnly if !is_read => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("write to read-only VMDK extent at offset {cur} rejected"),
                    ));
                }
                _ => {}
            }
            cur = extent.virtual_start + extent.length;
        }
        Ok(())
    }

    // Reads or writes a single contiguous segment of one extent through the
    // extent's `AlignedFile`. The transfer is staged in a temporary buffer and
    // issued via `AlignedFile::{read_at,write_at}`, which under `O_DIRECT`
    // bounce it through an aligned buffer (read-modify-write for sub-block
    // writes) and pass straight through for buffered extents.
    //
    // `read_at`/`write_at` follow the positioned-I/O contract and may transfer
    // *fewer* bytes than requested (a short `pread`/`pwrite`), so the transfer
    // is looped until `seg_len` bytes are moved. Stopping early would leave the
    // tail of the guest buffer unfilled on reads, or silently drop the tail on
    // writes -- either of which corrupts the guest's view of the disk. The loop
    // stops early only on a genuine EOF / no-progress (`Ok(0)`); the returned
    // count is the number of bytes actually transferred (equal to `seg_len`
    // except at end-of-file).
    fn segment_io(
        file: &AlignedFile,
        file_offset: u64,
        op: &mut AsyncIoOperation,
        buf_start: usize,
        seg_len: usize,
        is_read: bool,
    ) -> io::Result<usize> {
        let mut buf = vec![0u8; seg_len];
        let mut done = 0usize;
        if is_read {
            while done < seg_len {
                match file.read_at(&mut buf[done..], file_offset + done as u64) {
                    Ok(0) => break, // EOF: nothing more to read
                    Ok(n) => done += n,
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            op.write_bytes_at(buf_start, &buf[..done])?;
            Ok(done)
        } else {
            op.read_bytes_at(buf_start, &mut buf)?;
            while done < seg_len {
                match file.write_at(&buf[done..], file_offset + done as u64) {
                    Ok(0) => break, // no progress: avoid spinning forever
                    Ok(n) => done += n,
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(done)
        }
    }

    // Single-extent path: the whole request lives in `extent`.
    //
    // For buffered extents (alignment 0, i.e. `direct=off`) the original guest
    // iovecs are submitted with a single vectored `preadv`/`pwritev` (no
    // intermediate copy). For `O_DIRECT` extents the guest buffers/offsets may
    // not be block-aligned, so the transfer is routed through `segment_io`,
    // which bounces it through an aligned buffer.
    fn single_extent_io(
        &self,
        extent: &VmdkExtent,
        op: &mut AsyncIoOperation,
    ) -> io::Result<usize> {
        let file = extent.file.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "VMDK extent is not accessible",
            )
        })?;
        let file_offset = extent.file_base_offset + (op.offset() as u64 - extent.virtual_start);
        let is_read = op.is_read();

        // O_DIRECT: the guest buffers/offsets may be unaligned, so bounce the
        // transfer through the aligned buffer rather than submitting the raw
        // iovecs (which the kernel would reject with EINVAL).
        if file.alignment() != 0 {
            let seg_len = op.total_len();
            return Self::segment_io(file, file_offset, op, 0, seg_len, is_read);
        }

        // Buffered (no O_DIRECT): zero-copy vectored fast path.
        let iovecs = op.iovecs();
        let fd = file.as_raw_fd();
        let file_offset = file_offset as libc::off_t;

        // SAFETY: the iovec buffers are owned by `op` and remain valid for the
        // duration of this call.
        let res = unsafe {
            if is_read {
                libc::preadv(
                    fd,
                    iovecs.as_ptr(),
                    iovecs.len() as libc::c_int,
                    file_offset,
                )
            } else {
                libc::pwritev(
                    fd,
                    iovecs.as_ptr(),
                    iovecs.len() as libc::c_int,
                    file_offset,
                )
            }
        };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(res as usize)
    }

    // Slow path: the request straddles >= 2 extents. Walk every extent it
    // covers, copying each segment through `segment_io` at the correct file
    // offset, and report the total number of bytes transferred.
    fn spanning_io(&self, op: &mut AsyncIoOperation) -> io::Result<usize> {
        let start = op.offset() as u64;
        let total = op.total_len() as u64;
        let is_read = op.is_read();

        let mut done: u64 = 0;
        while done < total {
            let cur = start + done;
            let extent = self.extent_at(cur).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "offset outside any VMDK extent")
            })?;
            let extent_end = extent.virtual_start + extent.length;
            // Bytes handled in this extent before reaching its boundary.
            let seg_len = cmp::min(total - done, extent_end - cur) as usize;
            let file = extent.file.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "VMDK extent is not accessible",
                )
            })?;
            let file_offset = extent.file_base_offset + (cur - extent.virtual_start);

            let n = Self::segment_io(file, file_offset, op, done as usize, seg_len, is_read)?;
            done += n as u64;
            if n < seg_len {
                break; // short read/write
            }
        }

        // TO-DO: For short read/short write, we break out of loop
        // and return the number of bytes processed so far.
        // Should we return an error instead? Or is it okay to return the number of bytes processed so far?
        Ok(done as usize)
    }
}

impl AsyncIo for FlatVmdkSync {
    fn notifier(&self) -> &EventFd {
        &self.eventfd
    }

    fn submit_data_operation(&mut self, mut op: AsyncIoOperation) -> AsyncIoResult<()> {
        let start = op.offset() as u64;
        let total = op.total_len() as u64;
        let is_read = op.is_read();

        // Bounds check against the virtual disk size (overflow-safe: `start`
        // is checked before subtracting it from `size`).
        if start > self.size || total > self.size - start {
            let error = io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "VMDK request [{start}, {}) exceeds virtual size {}",
                    start + total,
                    self.size
                ),
            );
            return Err(if is_read {
                AsyncIoError::ReadVectored(error)
            } else {
                AsyncIoError::WriteVectored(error)
            });
        }

        // Reject the request up front if any extent it touches forbids it:
        // NOACCESS extents reject all I/O, RDONLY extents reject writes.
        if total != 0
            && let Err(error) = self.check_access(start, total, is_read)
        {
            return Err(if is_read {
                AsyncIoError::ReadVectored(error)
            } else {
                AsyncIoError::WriteVectored(error)
            });
        }

        let result = if total == 0 {
            Ok(0)
        } else if let Some(extent) = self.extent_at(start) {
            if start + total <= extent.virtual_start + extent.length {
                // Entire request fits in one extent -> single-extent path
                // (zero-copy when buffered, aligned bounce under O_DIRECT).
                self.single_extent_io(extent, &mut op)
            } else {
                // Request crosses an extent boundary -> segmented copy path.
                self.spanning_io(&mut op)
            }
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "offset outside any VMDK extent",
            ))
        };

        let bytes = result.map_err(|e| {
            if is_read {
                AsyncIoError::ReadVectored(e)
            } else {
                AsyncIoError::WriteVectored(e)
            }
        })?;

        self.completion_list
            .push_back(AsyncIoCompletion::from_operation(op, bytes as i32));
        self.eventfd.write(1).unwrap();
        Ok(())
    }

    fn fsync(&mut self, user_data: Option<u64>) -> AsyncIoResult<()> {
        // Flush every extent: a single guest flush must durably persist data
        // that may have been written across multiple extent files.
        for extent in self.extents.iter() {
            // Skip NoAccess extents, which have no open file.
            if let Some(file) = extent.file.as_ref() {
                // SAFETY: FFI call with a valid fd owned by the extent.
                let res = unsafe { libc::fsync(file.as_raw_fd()) };
                if res < 0 {
                    return Err(AsyncIoError::Fsync(io::Error::last_os_error()));
                }
            }
        }

        if let Some(user_data) = user_data {
            self.completion_list
                .push_back(AsyncIoCompletion::new(user_data, 0, None));
            self.eventfd.write(1).unwrap();
        }

        Ok(())
    }

    fn next_completed_request(&mut self) -> Option<AsyncIoCompletion> {
        self.completion_list.pop_front()
    }

    fn punch_hole(&mut self, _offset: u64, _length: u64, _user_data: u64) -> AsyncIoResult<()> {
        // Flat VMDK is not sparse-capable (see `SparseCapable` impl), so this
        // should never be negotiated by the guest.
        Err(AsyncIoError::PunchHole(io::Error::other(
            "punch_hole not supported for flat VMDK",
        )))
    }

    fn write_zeroes(&mut self, _offset: u64, _length: u64, _user_data: u64) -> AsyncIoResult<()> {
        Err(AsyncIoError::WriteZeroes(io::Error::other(
            "write_zeroes not supported for flat VMDK",
        )))
    }
}
