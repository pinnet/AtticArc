//! AtticARC core: everything the CLI and GUI share.
//!
//! The pipeline, in order:
//! 1. [`scan`]: walk a source folder and record path, size and modified time (fast, no file reads).
//! 2. Size collisions from the scan pick out the only files that *can* be duplicates.
//! 3. [`archive`]: check the destination has room ([`disk`]), then compress each top-level folder to its own .7z (sevenz-turbo, LZMA2),
//!    hashing every file (BLAKE3 and SHA-256) as it is read, then decode the archive and
//!    check every hash before it counts as done. Files matching the [`exclude`] rules
//!    are left out.
//! 4. [`catalog`]: record every archive and file in a SQLite catalogue kept next to the
//!    archives, with a full-text index for searching and hash indexes for duplicates.
//! 5. Delete a source folder only after re-hashing it and the archive against the catalogue.
//!
//! [`settings`] holds what the GUI and CLI remember between runs.

pub mod archive;
pub mod catalog;
pub mod cleanup;
pub mod disk;
pub mod exclude;
pub mod scan;
pub mod settings;
