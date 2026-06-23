// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs::File;
use std::io::{self};//, Seek, SeekFrom};

// enum describing the different types of VMDK disk formats
// TO-DO: Should this contain the complete list??
#[derive(Debug)]
enum VMDKDiskType {
    MonolithicFlat,
    TwoGbMaxExtentFlat,
    MonolithicSparse,
    VmfsSparse,
    Vmfs,
    TwoGbMaxExtentSparse,
    FullDevice,
    VmfsRaw,
    PartitionedDevice,
    VmfsRawDeviceMap,
    VmfsPassthroughRawDeviceMap,
    StreamOptimized
}

// VMDK text descriptor extent line.
// Each line describes one extent.
// The format of the line looks like:
// <Access> <Size in sectors> <Type of extent> <filename>
// ex: RW 2097152 FLAT "disk-s001.vmdk"
#[derive(Debug)]
pub struct VmdkExtent {
    pub access: String,
    pub size_in_sectors: u64,
    pub extent_type: String,
    pub filename: String,
}

// VMDK text descriptor
// - Header
// - Extents List
// - Disk Database
#[derive(Debug)]
pub struct VmdkDescriptor {
    pub header: VmdkDescriptorHeader,
    pub extents_list: VmdkDescriptorExtents,
    pub ddb: VmdkDescriptorDdb,
}

// VMDK text descriptor header
#[derive(Debug)]
pub struct VmdkDescriptorHeader {
    pub version: u32,
    pub cid: u32,
    pub parent_cid: u32,
    pub create_type: VMDKDiskType,
    pub parent_filename_hint: String,
}

// VMDK text descriptor extents list
#[derive(Debug)]
pub struct VmdkDescriptorExtents {
    pub extents: Vec<VmdkExtent>,
}

// VMDK text descriptor disk database
// Each entry is a key:value pair
#[derive(Debug)]
pub struct VmdkDescriptorDdb {
    pub entries: HashMap<String, String>,
}

#[allow(dead_code)]
impl VmdkDescriptor {

}

pub fn is_flat_vmdk(_f: &mut File) -> io::Result<bool> {
    Ok(true) // TO-DO: Implement a proper check for flat VMDK files
} 