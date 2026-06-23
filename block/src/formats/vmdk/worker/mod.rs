// Copyright 2026 The Cloud Hypervisor Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Sync/async I/O workers for flat VMDK images.
//!
//! Thin wrappers around the raw workers that clamp I/O to the
//! virtual disk size.

// The synchronous worker must always be available; it does not depend on
// io_uring (it wraps the raw crate's blocking `RawSync`).
pub(crate) mod sync;
