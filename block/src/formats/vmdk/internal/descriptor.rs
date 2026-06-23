// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead};
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::AlignedFile;

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
// <Access> <Size in sectors> <Type of extent> <filename> [offset in sectors]
// ex: RW 2097152 FLAT "disk-s001.vmdk" 0
//
// The optional trailing offset is the starting sector *within the backing
// file* for this extent. It is non-zero when a single file larger than one
// extent (e.g. a >2GB file in twoGbMaxExtentFlat) is described by several
// extent lines that all reference the same file at growing offsets.
#[derive(Debug, Default)]
pub struct VmdkExtentHeader {
    pub access: String,
    pub size_in_sectors: u64,
    pub extent_type: String,
    pub filename: String,
    pub offset_in_sectors: u64,
}

// VMDK text descriptor
// - Header
// - Extents List
// - Disk Database
#[derive(Debug, Default)]
pub struct VmdkDescriptor {
    pub base_path: String,
    // TO-DO: Remove the unused warning
    // For now, we are not reading the parsed header back from the
    // descriptor, but it is part of the descriptor file and we parse it.
    #[allow(dead_code)]
    pub header: VmdkDescriptorHeader,
    pub extents_list: VmdkDescriptorExtents,
    // TO-DO: Remove the unused warning
    // For now, we are not using ddb information,
    // but it is part of the descriptor file and we are parsing it.
    #[allow(dead_code)]
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
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Cannot retrieve parent directory of the file",
                )
            })?
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
        //
        // Wrap the descriptor fd in an AlignedFile so a descriptor smaller than
        // (or unaligned against) the device block size can still be read when
        // the file is opened with O_DIRECT on a 4K-sector backing store;
        // otherwise such short reads fail with EINVAL.
        let mut reader = read_descriptor(file)?;
        let (desc_header, last_line) = parse_header(&mut reader)?;
        let desc_extents_ddb = parse_extents_and_ddb(&mut reader, &last_line)?;

        Ok(Self {
            base_path,
            header: desc_header,
            extents_list: desc_extents_ddb.0,
            ddb: desc_extents_ddb.1,
        })
    }
}

// Read the whole descriptor file into memory through an `AlignedFile` and
// return it as an in-memory `Cursor` so the line-oriented parsers can consume
// it via `BufRead`.
//
// `AlignedFile` only exposes positioned, alignment-aware reads (`read_at`),
// bouncing unaligned/short reads through an aligned buffer. That lets a small
// descriptor be read even when the backing file was opened O_DIRECT on a
// 4K-sector device. Reads are anchored at offset 0, so any OS file offset left
// behind by earlier image-type probes is irrelevant and no rewind is required.
fn read_descriptor(file: &File) -> io::Result<io::Cursor<Vec<u8>>> {
    let aligned = AlignedFile::new(file.try_clone()?, true);
    let len = file.metadata()?.len() as usize;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < buf.len() {
        match aligned.read_at(&mut buf[filled..], filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(io::Cursor::new(buf))
}

pub(crate) fn parse_header<R: BufRead>(
    reader: &mut R,
) -> io::Result<(VmdkDescriptorHeader, String)> {
    let mut header_line = String::new();
    reader.read_line(&mut header_line)?;

    // Sanity- Check if this is not a descriptor file but actual disk data.
    // VMDK specs mention embedded descriptor file which are part of the headers of the disk data.
    // This implementation currently does not support parsing of embedded descriptor files.
    // `read_line` keeps the trailing newline, so trim it before comparing.
    if header_line.trim_end() != VMDK_DESCRIPTOR_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Not a VMDK descriptor file: missing header: {header_line}"),
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
                    // Tools such as qemu-img quote the value, e.g.
                    // createType="monolithicFlat". Strip surrounding quotes
                    // before matching so real-world descriptors are recognized.
                    header.create_type = match parts[1].trim_matches('"') {
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
    last_comment_line: &str,
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
        // Tools such as qemu-img separate the extent list, the DDB marker and
        // the DDB entries with blank lines. Skip any blank/whitespace-only
        // line so these real-world descriptors are tolerated.
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with("#") {
            if line == VMDK_DESCRIPTOR_DDB || line == VMDK_DESCRIPTOR_DDB_2 {
                in_extents_section = false;
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Received unexpected comment line, expected DDB section comment line",
            ));
        }
        if in_extents_section {
            // Parse the extent line
            let parts: Vec<&str> = line.split_whitespace().collect();
            // The 5th field (offset) is optional. When present it is the
            // starting sector of this extent *within its backing file*; when
            // omitted it defaults to 0. Honoring it is required for images
            // where several extent lines point at the same file at growing
            // offsets (e.g. a >2GB file split under twoGbMaxExtentFlat).
            if parts.len() == 4 || parts.len() == 5 {
                let extent = VmdkExtentHeader {
                    access: parts[0].to_string(),
                    size_in_sectors: parts[1].parse().unwrap_or(0),
                    extent_type: parts[2].to_string(),
                    filename: parts[3].trim_matches('"').to_string(),
                    offset_in_sectors: parts.get(4).map_or(0, |o| o.parse().unwrap_or(0)),
                };
                extents.extents.push(extent);
            } else {
                // Signal malformed extent line, bail
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Malformed VMDK extent line",
                ));
            }
        } else {
            // Parse the ddb entry line
            let parts: Vec<&str> = line.split('=').map(|s| s.trim()).collect();
            if parts.len() == 2 {
                ddb.entries
                    .insert(parts[0].to_string(), parts[1].to_string());
            }
        }
    }

    Ok((extents, ddb))
}

/// Returns true when `prefix` begins with the flat VMDK descriptor header
/// (`# Disk DescriptorFile`).
///
/// Image-type detection calls this on the first block it already read for
/// magic-sniffing and only invokes the full [`is_flat_vmdk`] parse when it
/// returns true. This is a *content* check, not a filename check: a flat VMDK
/// names both its tiny text descriptor and its large `-flat.vmdk` data extent
/// with the `.vmdk` extension, so the extension cannot tell them apart — only
/// the descriptor carries this header. Gating on it keeps detection from
/// reading a whole non-descriptor file (a raw/OS disk, or a multi-gigabyte
/// flat extent) into memory just to reject it.
pub fn has_descriptor_header(prefix: &[u8]) -> bool {
    prefix.starts_with(VMDK_DESCRIPTOR_HEADER.as_bytes())
}

// The VMDK support currently only supports flat disk types.
// This function checks if
// createType = MonolithicFlat | TwoGbMaxExtentFlat
// extent type = "FLAT"
// For any other combination, the function returns false.
pub fn is_flat_vmdk(f: &mut File) -> io::Result<bool> {
    // Wrap the fd in an AlignedFile so a descriptor smaller than (or unaligned
    // against) the device block size can still be read under O_DIRECT on a
    // 4K-sector backing store, where a short read would otherwise fail with
    // EINVAL. AlignedFile uses positioned (pread) reads anchored at offset 0,
    // so it is unaffected by the OS file offset that earlier image-type probes
    // (e.g. is_fixed_vhd, which seeks to the end of the file) leave behind — no
    // rewind is required.
    let mut reader = read_descriptor(f)?;

    // A missing/non-matching header (or otherwise malformed descriptor) just
    // means this is not a flat VMDK. Return Ok(false) so detection falls
    // through to the other formats instead of aborting with a fatal error.
    let (header, last_line) = match parse_header(&mut reader) {
        Ok(parsed) => parsed,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => return Ok(false),
        Err(e) => return Err(e),
    };

    // Only supports flat disk types for now. Other types can be added later.
    match header.create_type {
        VMDKDiskType::MonolithicFlat | VMDKDiskType::TwoGbMaxExtentFlat => {}
        _ => return Ok(false),
    }

    let extents = match parse_extents_and_ddb(&mut reader, &last_line) {
        Ok((extents, _ddb)) => extents,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => return Ok(false),
        Err(e) => return Err(e),
    };

    // Only supports flat extent types for now. Other types can be added later.
    for extent in &extents.extents {
        if extent.extent_type != "FLAT" {
            return Ok(false);
        }
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom};

    use super::*;

    // `&[u8]` implements `BufRead`, so the parsers can be driven straight from
    // byte literals without temp files.

    fn parse_hdr(input: &[u8]) -> io::Result<(VmdkDescriptorHeader, String)> {
        let mut reader = input;
        parse_header(&mut reader)
    }

    fn parse_body(
        last_comment: &str,
        body: &[u8],
    ) -> io::Result<(VmdkDescriptorExtents, VmdkDescriptorDdb)> {
        let mut reader = body;
        parse_extents_and_ddb(&mut reader, last_comment)
    }

    // Full two-stage parse, exactly as `VmdkDescriptor::new` chains it.
    fn parse_full(
        input: &[u8],
    ) -> io::Result<(
        VmdkDescriptorHeader,
        VmdkDescriptorExtents,
        VmdkDescriptorDdb,
    )> {
        let mut reader = input;
        let (header, last) = parse_header(&mut reader)?;
        let (extents, ddb) = parse_extents_and_ddb(&mut reader, &last)?;
        Ok((header, extents, ddb))
    }

    // ---- VmdkDescriptor::new: shared-cursor regression ----

    #[test]
    fn new_rewinds_shared_file_cursor() {
        use std::io::Write;

        use vmm_sys_util::tempfile::TempFile;

        // A minimal but complete monolithicFlat descriptor.
        let descriptor_text: &[u8] = b"# Disk DescriptorFile\n\
                                       version=1\n\
                                       createType=monolithicFlat\n\
                                       # Extent description\n\
                                       RW 2097152 FLAT \"disk-flat.vmdk\"\n\
                                       # The Disk Data Base\n\
                                       ddb.adapterType = \"ide\"\n";

        // Write the descriptor to a temp file and rewind to the start.
        let tmp = TempFile::new().unwrap();
        let mut file: &File = tmp.as_file();
        file.write_all(descriptor_text).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();

        // Advance the shared OS file offset using the parser helper, exactly
        // as the image-type detection path (is_flat_vmdk) does. `&File` is
        // `Copy`, so the BufReader operates on the same underlying fd/offset.
        {
            let mut reader = io::BufReader::new(file);
            parse_header(&mut reader).unwrap();
        }
        // The cursor is now past the descriptor header, not at the start.
        assert_ne!(file.stream_position().unwrap(), 0);

        // Constructing the descriptor from the SAME file must still succeed,
        // because `VmdkDescriptor::new` rewinds before parsing.
        let descriptor = VmdkDescriptor::new(file, tmp.as_path()).unwrap();
        assert_eq!(descriptor.extents_list.extents.len(), 1);
        assert_eq!(
            descriptor.extents_list.extents[0].filename,
            "disk-flat.vmdk"
        );
    }

    // ---- parse_extents_and_ddb: valid inputs ----

    #[test]
    fn single_flat_extent_with_ddb() {
        let body: &[u8] = b"RW 2097152 FLAT \"disk-flat.vmdk\"\n\
                            # The Disk Data Base\n\
                            ddb.adapterType = \"ide\"\n\
                            ddb.geometry.sectors = \"63\"\n";

        let (extents, ddb) = parse_body("# Extent description", body).unwrap();

        assert_eq!(extents.extents.len(), 1);
        let e = &extents.extents[0];
        assert_eq!(e.access, "RW");
        assert_eq!(e.size_in_sectors, 2_097_152);
        assert_eq!(e.extent_type, "FLAT");
        assert_eq!(e.filename, "disk-flat.vmdk");

        assert_eq!(
            ddb.entries.get("ddb.adapterType").map(String::as_str),
            Some("\"ide\"")
        );
        assert_eq!(
            ddb.entries.get("ddb.geometry.sectors").map(String::as_str),
            Some("\"63\"")
        );
    }

    #[test]
    fn multiple_extents_two_gb_max() {
        let body: &[u8] = b"RW 4192256 FLAT \"disk-s001.vmdk\"\n\
                            RW 4192256 FLAT \"disk-s002.vmdk\"\n\
                            RW 2097152 FLAT \"disk-s003.vmdk\"\n\
                            # The Disk Data Base\n\
                            ddb.adapterType = \"lsilogic\"\n";

        let (extents, _ddb) = parse_body("# Extent description", body).unwrap();

        assert_eq!(extents.extents.len(), 3);
        assert_eq!(extents.extents[0].filename, "disk-s001.vmdk");
        assert_eq!(extents.extents[2].filename, "disk-s003.vmdk");
        assert!(extents.extents.iter().all(|e| e.extent_type == "FLAT"));
    }

    #[test]
    fn extent_line_with_optional_offset_field() {
        // 5-field form: <access> <sectors> <type> <file> <offset>
        let body: &[u8] = b"RW 2097152 FLAT \"disk-flat.vmdk\" 0\n";

        let (extents, _ddb) = parse_body("# Extent description", body).unwrap();

        assert_eq!(extents.extents.len(), 1);
        assert_eq!(extents.extents[0].filename, "disk-flat.vmdk");
    }

    #[test]
    fn extent_access_modes_are_preserved() {
        let body: &[u8] = b"RDONLY 2097152 FLAT \"ro.vmdk\"\n\
                            NOACCESS 1048576 FLAT \"noaccess.vmdk\"\n";

        let (extents, _ddb) = parse_body("# Extent description", body).unwrap();

        assert_eq!(extents.extents.len(), 2);
        assert_eq!(extents.extents[0].access, "RDONLY");
        assert_eq!(extents.extents[1].access, "NOACCESS");
    }

    // ---- parse_extents_and_ddb: invalid inputs ----

    #[test]
    fn rejects_wrong_leading_comment() {
        let body: &[u8] = b"RW 2097152 FLAT \"disk-flat.vmdk\"\n";
        // Must be told we are at "# Extent description"; anything else is an error.
        parse_body("# The Disk Data Base", body).unwrap_err();
    }

    #[test]
    fn rejects_malformed_extent_line() {
        // Only three fields -> malformed.
        let body: &[u8] = b"RW 2097152 FLAT\n";
        parse_body("# Extent description", body).unwrap_err();
    }

    #[test]
    fn rejects_unexpected_comment_in_body() {
        let body: &[u8] = b"RW 2097152 FLAT \"disk-flat.vmdk\"\n\
                            # Some other comment\n";
        parse_body("# Extent description", body).unwrap_err();
    }

    #[test]
    fn skips_blank_line_inside_extent_section() {
        // Tools such as qemu-img emit a blank line between the last extent
        // line and the "# The Disk Data Base" marker. Such blank lines must
        // be tolerated (skipped) rather than treated as malformed extents.
        let body: &[u8] = b"RW 2097152 FLAT \"disk-flat.vmdk\"\n\
                            \n\
                            # The Disk Data Base\n";
        let (extents, _ddb) = parse_body("# Extent description", body).unwrap();
        assert_eq!(extents.extents.len(), 1);
        assert_eq!(extents.extents[0].filename, "disk-flat.vmdk");
    }

    // ---- parse_header ----

    #[test]
    fn rejects_missing_descriptor_header() {
        let input: &[u8] = b"NOT_A_DESCRIPTOR\nversion=1\n";
        parse_hdr(input).unwrap_err();
    }

    #[test]
    fn parses_header_fields() {
        let input: &[u8] = b"# Disk DescriptorFile\n\
                             version=1\n\
                             CID=fffffffe\n\
                             parentCID=ffffffff\n\
                             createType=monolithicFlat\n\
                             # Extent description\n";

        let (header, last) = parse_hdr(input).unwrap();

        assert_eq!(header.version, 1);
        assert_eq!(header.cid, 0xffff_fffe);
        assert_eq!(header.parent_cid, 0xffff_ffff);
        assert!(matches!(header.create_type, VMDKDiskType::MonolithicFlat));
        assert_eq!(last, "# Extent description");
    }

    #[test]
    fn parses_quoted_create_type() {
        // qemu-img and other tools quote the createType value; the parser
        // must strip the quotes before matching.
        let input: &[u8] = b"# Disk DescriptorFile\n\
                             version=1\n\
                             createType=\"twoGbMaxExtentFlat\"\n\
                             # Extent description\n";

        let (header, _last) = parse_hdr(input).unwrap();
        assert!(matches!(
            header.create_type,
            VMDKDiskType::TwoGbMaxExtentFlat
        ));
    }

    // ---- end-to-end ----

    #[test]
    fn full_monolithic_flat_descriptor() {
        let input: &[u8] = b"# Disk DescriptorFile\n\
                             version=1\n\
                             createType=monolithicFlat\n\
                             # Extent description\n\
                             RW 2097152 FLAT \"disk-flat.vmdk\"\n\
                             # The Disk Data Base\n\
                             ddb.adapterType = \"ide\"\n";

        let (header, extents, ddb) = parse_full(input).unwrap();

        assert!(matches!(header.create_type, VMDKDiskType::MonolithicFlat));
        assert_eq!(extents.extents.len(), 1);
        assert_eq!(extents.extents[0].access, "RW");
        assert_eq!(
            ddb.entries.get("ddb.adapterType").map(String::as_str),
            Some("\"ide\"")
        );
    }

    #[test]
    fn full_two_gb_max_extent_flat_descriptor() {
        let input: &[u8] = b"# Disk DescriptorFile\n\
                             version=1\n\
                             createType=twoGbMaxExtentFlat\n\
                             # Extent description\n\
                             RW 4192256 FLAT \"disk-s001.vmdk\"\n\
                             RW 4192256 FLAT \"disk-s002.vmdk\"\n\
                             # The Disk Data Base\n";

        let (header, extents, _ddb) = parse_full(input).unwrap();

        assert!(matches!(
            header.create_type,
            VMDKDiskType::TwoGbMaxExtentFlat
        ));
        assert_eq!(extents.extents.len(), 2);
    }

    #[test]
    fn full_qemu_style_descriptor() {
        // Mirrors a real qemu-img monolithicFlat descriptor: quoted
        // createType, blank lines separating sections, 5-field extent lines
        // with a trailing offset, and the "#DDB" marker form.
        let input: &[u8] = b"# Disk DescriptorFile\n\
                             version=1\n\
                             CID=eb2295a4\n\
                             parentCID=ffffffff\n\
                             createType=\"monolithicFlat\"\n\
                             \n\
                             # Extent description\n\
                             RW 6291456 FLAT \"t-flat.vmdk\" 0\n\
                             \n\
                             # The Disk Data Base\n\
                             #DDB\n\
                             \n\
                             ddb.virtualHWVersion = \"4\"\n\
                             ddb.adapterType = \"ide\"\n";

        let (header, extents, ddb) = parse_full(input).unwrap();

        assert!(matches!(header.create_type, VMDKDiskType::MonolithicFlat));
        assert_eq!(extents.extents.len(), 1);
        assert_eq!(extents.extents[0].access, "RW");
        assert_eq!(extents.extents[0].size_in_sectors, 6_291_456);
        assert_eq!(extents.extents[0].extent_type, "FLAT");
        assert_eq!(extents.extents[0].filename, "t-flat.vmdk");
        assert_eq!(
            ddb.entries.get("ddb.adapterType").map(String::as_str),
            Some("\"ide\"")
        );
    }
}
