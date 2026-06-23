// Copyright 2026 The Cloud Hypervisor Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! VMDK format parsing and data structures.
//!
//! Contains the descriptor text parser and the low level flat VMDK
//! block backend.

pub(crate) mod descriptor;
pub(crate) mod flat;
