// Copyright © 2026, Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Containment check for untrusted, image-embedded absolute file references.
//!
//! VMDK absolute extents (and, in future, QCOW2 backing files) name host paths
//! taken verbatim from an untrusted disk image. After opening such a reference
//! following symlinks, this verifies the file actually opened resolves inside
//! one of the caller's configured trusted roots.

use std::fs::{File, canonicalize, read_link};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

/// Requires an already-opened file to resolve inside one of `trusted_roots`.
///
/// The check is fd-bound and therefore race-free: it reads the real path of the
/// open fd via `/proc/self/fd` (symlinks and `..` already collapsed by the
/// kernel) and prefix-matches it against the canonicalized roots. It fails
/// closed when the backing file was unlinked (no live path to reason about),
/// when `/proc` is unavailable, or when `trusted_roots` is empty.
pub fn verify_within_trusted_root(file: &File, trusted_roots: &[PathBuf]) -> io::Result<()> {
    // A file unlinked after opening has no live path: /proc/self/fd reports a
    // synthetic "<path> (deleted)" target that could still prefix-match a root,
    // so containment can no longer be established.
    if file.metadata()?.nlink() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "image-embedded absolute path backing file was deleted; cannot verify containment",
        ));
    }

    let proc_path = format!("/proc/self/fd/{}", file.as_raw_fd());
    let resolved = read_link(&proc_path).map_err(|e| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "cannot verify image-embedded absolute path: reading '{proc_path}' failed ({e}); \
                 is /proc mounted?"
            ),
        )
    })?;

    // Canonicalize each root so a symlinked root path (e.g. /var -> /mnt/var)
    // still matches the kernel-resolved path. A root that does not exist
    // canonicalizes with an error and simply never matches.
    let trusted = trusted_roots
        .iter()
        .filter_map(|root| canonicalize(root).ok())
        .any(|root| resolved.starts_with(root));

    if trusted {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "image-embedded absolute path resolves to '{}', outside any trusted root",
                resolved.display()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use vmm_sys_util::tempdir::TempDir;

    use super::*;

    #[test]
    fn accepts_file_under_a_trusted_root() {
        let dir = TempDir::new_with_prefix("/tmp/trusted-root-ok").unwrap();
        let path = dir.as_path().join("blob");
        fs::write(&path, b"data").unwrap();
        let file = File::open(&path).unwrap();

        verify_within_trusted_root(&file, &[PathBuf::from("/tmp")]).unwrap();
    }

    #[test]
    fn rejects_file_outside_every_trusted_root() {
        let dir = TempDir::new_with_prefix("/tmp/trusted-root-out").unwrap();
        let path = dir.as_path().join("blob");
        fs::write(&path, b"data").unwrap();
        let file = File::open(&path).unwrap();

        // The file is under /tmp, not under /var, so it is refused.
        verify_within_trusted_root(&file, &[PathBuf::from("/var")]).unwrap_err();
    }

    #[test]
    fn rejects_when_no_roots_configured() {
        let dir = TempDir::new_with_prefix("/tmp/trusted-root-empty").unwrap();
        let path = dir.as_path().join("blob");
        fs::write(&path, b"data").unwrap();
        let file = File::open(&path).unwrap();

        verify_within_trusted_root(&file, &[]).unwrap_err();
    }

    #[test]
    fn rejects_deleted_file() {
        let dir = TempDir::new_with_prefix("/tmp/trusted-root-deleted").unwrap();
        let path = dir.as_path().join("blob");
        fs::write(&path, b"data").unwrap();
        let file = File::open(&path).unwrap();
        let roots = [PathBuf::from("/tmp")];

        verify_within_trusted_root(&file, &roots).unwrap();
        fs::remove_file(&path).unwrap();
        verify_within_trusted_root(&file, &roots).unwrap_err();
    }
}
