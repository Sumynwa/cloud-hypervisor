// Copyright 2026 The Cloud Hypervisor Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Sync/async I/O workers for flat VMDK images.
//!
//! Thin wrappers around the raw workers that clamp I/O to the
//! virtual disk size.

#[cfg(feature = "io_uring")]
pub(crate) mod sync;
