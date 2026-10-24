//! Attic core: everything the CLI and GUI share.
//!
//! The pipeline, in order:
//! 1. [`scan`]: walk a source folder and record path, size and modified time (fast, no file reads).
//! 2. Size collisions from the scan pick out the only files that *can* be duplicates.
//! 3. Hash those candidates (BLAKE3 planned) to confirm duplicates.
//! 4. Archive each top-level folder to its own 7z (engine chosen by benchmark).
//! 5. Store everything in a SQLite catalogue kept on the destination drive.
//!
//! Only step 1–2 exist so far; the rest arrive once the benchmark settles the engine
//! and dependencies can be added.

pub mod scan;
