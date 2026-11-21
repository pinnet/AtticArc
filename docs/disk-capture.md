# Disk capture: a whole disk stored as a set of files

Design note, 21 Nov 2026. Not started; after the plugin refactor, as a plugin. Uses no other
backup product: open formats and AtticARC's own code only.

## The idea in one paragraph

Capture a whole disk (partition table, every partition, boot areas) as a deduplicated store of
chunk files plus a small index per capture. Many captures share one store, so a repeat capture of
the same machine costs only what changed. The store is just files, so it is catalogued, verified,
scrubbed, rolled out to disk sets and covered by parity like everything else
(`docs/disk-sets.md`).

## How a capture works

1. **Read the device**: Windows `\\.\PhysicalDriveN`, macOS `/dev/rdiskN`, Linux `/dev/sdX`. Needs
   admin rights: done by the small privileged helper (TODO, "Folders that need admin rights"), not
   by running the app elevated. A live system disk is read from a snapshot (VSS on Windows, an
   APFS snapshot on macOS, LVM/btrfs snapshot on Linux where there is one); otherwise the disk
   must be idle or unmounted.
2. **Used blocks only**: for file systems AtticARC understands (NTFS bitmap first; then exFAT,
   FAT, ext4, APFS/HFS+ as plugins), only allocated blocks are read; free space is recorded as
   holes. Unknown file systems and unpartitioned space are read in full (zero runs cost almost
   nothing after chunking).
3. **Content-defined chunking** (FastCDC, MIT crate; average ~1 MiB, min/max set per store): chunk
   edges follow the data, so content that moved on the disk still makes the same chunks.
4. **Store each chunk once**: named by its BLAKE3 hash, compressed with zstd, kept in a fan-out
   folder (`chunks/ab/cd/abcd….zst`), packed into larger pack files if the count gets too high for
   the file system.
5. **The capture's index** (`captures/<name> <date>.atticimg`): disk size and model, partition
   table (GPT/MBR) as read, per partition the file system and its used-block map, then the ordered
   list of chunk hashes and lengths, and a BLAKE3 of the whole disk as read. Small; the catalogue
   holds the same.

## Restoring and using a capture

- **Restore to a disk** (same size or larger), via the helper; verified chunk by chunk.
- **Export** to VHDX (mounts natively on Windows), QCOW2, or raw `.img`, for mounting or for a VM.
- **Later**: read-only mount straight from the store (a virtual disk driver per system), and
  listing the files inside a capture in the Catalogue by reading its file systems.

## Space and safety

- Dedup across captures and across disks in the same store; the catalogue shows per capture
  "new data" vs "shared".
- Removing a capture deletes only chunks no other capture uses (reference counts, checked by a
  full scan before any chunk is deleted).
- Scrub re-hashes chunks in the background, same rules as archive scrub.
- A capture only dedups against other captures, not against folder archives. If only the files
  on an old disk matter, archiving its folders saves more and is easier to browse.

## Tests to write first

- Capture a small virtual disk (a VHDX/raw file with GPT, NTFS and FAT partitions), restore it to
  a file: byte-identical in used areas, zeros in free space; whole-disk hash matches.
- Capture twice with a few files changed: the second adds only the changed chunks.
- Delete one capture of two: the other still restores; no shared chunk lost.
- Exported VHDX mounts on Windows; QCOW2 passes `qemu-img check`.
- A store made on one system restores on the other two.
