// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead};
use std::path::Path;

const VMDK_DESCRIPTOR_HEADER: &str = "# Disk DescriptorFile";
const VMDK_DESCRIPTOR_EXTENTS: &str = "# Extent description";
const VMDK_DESCRIPTOR_DDB: &str = "# The Disk Data Base";
const VMDK_DESCRIPTOR_DDB_2: &str = "#DDB";

// enum describing the different types of VMDK disk formats
// TO-DO: Currently only supports flat disk types. Other types can be added later.
#[derive(Debug, Default)]
pub enum VMDKDiskType {
    #[default]
    CreateTypeUnsupported,
    MonolithicFlat,
    TwoGbMaxExtentFlat,
}

// VMDK text descriptor extent line.
// Each line describes one extent.
// The format of the line looks like:
// <Access> <Size in sectors> <Type of extent> <filename>
// ex: RW 2097152 FLAT "disk-s001.vmdk"
#[derive(Debug, Default)]
pub struct VmdkExtentHeader {
    pub access: String,
    pub size_in_sectors: u64,
    pub extent_type: String,
    pub filename: String,
}

// VMDK text descriptor
// - Header
// - Extents List
// - Disk Database
#[derive(Debug, Default)]
pub struct VmdkDescriptor {
    pub base_path: String,
    pub header: VmdkDescriptorHeader,
    pub extents_list: VmdkDescriptorExtents,
    // TO-DO: Remove the unused warning
    // For now, we are not using ddb information,
    // but it is part of the descriptor file and we are parsing it.
    pub ddb: VmdkDescriptorDdb,
}

// VMDK text descriptor header
#[derive(Debug, Default)]
pub struct VmdkDescriptorHeader {
    pub version: u32,
    pub cid: u32,
    pub parent_cid: u32,
    pub create_type: VMDKDiskType,
    pub parent_filename_hint: String,
}

// VMDK text descriptor extents list
#[derive(Debug, Default)]
pub struct VmdkDescriptorExtents {
    pub extents: Vec<VmdkExtentHeader>,
}

// VMDK text descriptor disk database
// Each entry is a key:value pair
#[derive(Debug, Default)]
pub struct VmdkDescriptorDdb {
    pub entries: HashMap<String, String>,
}

// TO-DO: Current implementation targets descriptor text as a separate file.
// This is not always the case, as the descriptor can be embedded.
impl VmdkDescriptor {
    pub fn new(file: &File, path: &Path) -> io::Result<Self> {
        // Retrieve base path of the file
        let base_path = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput,
                "Cannot retrieve parent directory of the file"))?
            .to_string_lossy()
            .to_string();

        // Retrieve the metadata of the passed file
        let metadata = file.metadata()?;
        // Check if the file is empty or invalid
        // Also, a valid descriptor files should be at least 4 bytes long
        if metadata.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid VMDK descriptor file: file is empty or too small",
            ));
        }

        // Read the file contents.
        // Descriptor file format is a text file with sections separated by lines starting with '#'.
        // The section line starts with '# ' followed by values for that section in subsequent lines.
        // Read the opened file line by line and parse the sections accordingly.
        let mut reader = io::BufReader::new(file);
        let (desc_header,last_line) = parse_header(&mut reader)?;
        let desc_extents_ddb = parse_extents_and_ddb(&mut reader, &last_line)?;

        Ok(Self {
            base_path,
            header: desc_header,
            extents_list: desc_extents_ddb.0,
            ddb: desc_extents_ddb.1,
        })
    }

}

pub(crate) fn parse_header<R: BufRead>(
    reader: &mut R
) -> io::Result<(VmdkDescriptorHeader, String)> {
    let mut header_line = String::new();
    reader.read_line(&mut header_line)?;

    // Sanity- Check if this is not a descriptor file but actual disk data.
    // VMDK specs mention embedded descriptor file which are part of the headers of the disk data.
    // This implementation currently does not support parsing of embedded descriptor files.
    if header_line != VMDK_DESCRIPTOR_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Not a VMDK descriptor file: missing header",
        ));
    }

    let mut header = VmdkDescriptorHeader::default();
    let mut last_comment_line = String::new();

    for line in reader.lines() {
        let line = line?;
        if line.starts_with("#") {
            // Reached the end of the header section
            last_comment_line = line;
            break;
        }
        // Parse the key-value pairs in the header section
        let parts: Vec<&str> = line.split('=').map(|s| s.trim()).collect();
        if parts.len() == 2 {
            match parts[0] {
                "version" => header.version = parts[1].parse().unwrap_or(0),
                "CID" => header.cid = u32::from_str_radix(parts[1], 16).unwrap_or(0),
                "parentCID" => header.parent_cid = u32::from_str_radix(parts[1], 16).unwrap_or(0),
                "createType" => {
                    header.create_type = match parts[1] {
                        "monolithicFlat" => VMDKDiskType::MonolithicFlat,
                        "twoGbMaxExtentFlat" => VMDKDiskType::TwoGbMaxExtentFlat,
                        _ => VMDKDiskType::CreateTypeUnsupported,
                    }
                }
                "parentFileNameHint" => header.parent_filename_hint = parts[1].to_string(),
                _ => {}
            }
        }
    }

    Ok((header, last_comment_line))
}

pub(crate) fn parse_extents_and_ddb<R: BufRead>(
    reader: &mut R,
    last_comment_line: &str
) -> io::Result<(VmdkDescriptorExtents, VmdkDescriptorDdb)> {
    // Read the last comment line to determine if we are in the extents section or the ddb section
    let mut extents = VmdkDescriptorExtents::default();
    let mut ddb = VmdkDescriptorDdb::default();

    if last_comment_line != VMDK_DESCRIPTOR_EXTENTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Received NON Extents section comment line, expected Extents section comment line",
        ));
    }

    // Parse both the extents and ddb sections
    let mut in_extents_section = true;
    for line in reader.lines() {
        let line = line?;
        if line.starts_with("#") {
            if line == VMDK_DESCRIPTOR_DDB ||
               line == VMDK_DESCRIPTOR_DDB_2 {
                in_extents_section = false;
                continue;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Received unexpected comment line, expected DDB section comment line",
                ));
            }
        }
        if in_extents_section {
            // Parse the extent line
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                let extent = VmdkExtentHeader {
                    access: parts[0].to_string(),
                    size_in_sectors: parts[1].parse().unwrap_or(0),
                    extent_type: parts[2].to_string(),
                    filename: parts[3].trim_matches('"').to_string(),
                };
                extents.extents.push(extent);
            }
        } else {
            // Parse the ddb entry line
            let parts: Vec<&str> = line.split('=').map(|s| s.trim()).collect();
            if parts.len() == 2 {
                ddb.entries.insert(parts[0].to_string(), parts[1].to_string());
            }
        }
    }

    Ok((extents, ddb))
}

// The VMDK support currently only supports flat disk types.
// This function checks if
// createType = MonolithicFlat | TwoGbMaxExtentFlat
// extent type = "FLAT"
// For any other combination, the function returns false.
pub fn is_flat_vmdk(f: &mut File) -> io::Result<bool> {
    // constuct a VmdkDescriptor from the file
    // TO-DO: don't handle the descriptor file path here.
    let descriptor = VmdkDescriptor::new(f, Path::new(""))?;

    // Only supports flat disk types for now. Other types can be added later.
    match descriptor.header.create_type {
        VMDKDiskType::MonolithicFlat | VMDKDiskType::TwoGbMaxExtentFlat => {},
        _ => {
            return Ok(false)
        }
    }
    // Only supports flat extent types for now. Other types can be added later.
    for extent in &descriptor.extents_list.extents {
        if extent.extent_type != "FLAT" {
            return Ok(false)
        }
    }

    Ok(true)
} 