# Changelog

AtticARC uses [Semantic Versioning](https://semver.org). Until 1.0, the formats your data lives in
(the archive layout and its manifest, the catalogue database, `.atticarcdep` reference files) may
still change; 1.0 will mark them stable.

## Unreleased

### Clean up: seconds, not hours
- A folder's check is now done when it is archived: every file is hashed as it is compressed and the
  archive decoded against those hashes, and if the folder's NTFS change-journal snapshot is the same
  before and after, a passed check is saved then. Such folders can be deleted straight away.
- Before each delete: the folder's snapshot, the archive's size and modified time, and the archive's
  header (every recorded file listed, same size). Seconds, instead of hashing the folder and decoding
  the archive again (on a USB HDD, per 4 TB: about 59 hours with both on one disk).
- An archive last decoded more than 30 days ago is decoded again before its folder is deleted.
- **Check archives for damage** (`atticarc-cli cleanup --scrub` / `--scrub-all`): decodes archives
  again to catch damage since they were made, reading only the archives. Each archive's last decode is
  recorded.
- The full check (**1. Check**) stays for folders without a check: archived before this, on drives
  without a change journal (exFAT), or changed while archiving.
- Folders in the protected list show as **protected · never deleted** (a lock, no tick box; the
  command-line tool says "protected"), instead of as a failed check, and are never checked.
- Policy from measurements: sevenz-bench `verify` and `faults` (D:\Dev\WIP\sevenz-bench).

### Same name, different content
- A folder whose archive name is taken (a folder with the same name archived before, or the same
  folder archived again) gets `Name (2).7z`, `Name (3).7z`… and a catalogue entry of its own, instead
  of a time-stamped name.

### Duplicates: show only, and stored once within a folder
- Copies inside archives are listed with the space they take, never removed: **Replace duplicates…**
  is switched off, since each archive rewrite took hours.
- **Store identical files in a folder once** (New job, off by default; `atticarc-cli archive
  --identical-once` / `--every-copy`): when a folder is archived, files of 64 KiB or more with the
  same size and BLAKE3 hash are compressed once, and each later copy goes into the same archive as a
  small `.atticarcdep` reference file to the first. Only files sharing a size are read beforehand.
  The folder on disk isn't changed, and its check before a delete still compares every copy with its
  hash. The Catalogue's **Extract the original to…** and **Put back** bring a copy back.

## 0.9.0 (2026-11-11): first MVP

Archives folders to verified 7-Zip archives with a searchable catalogue, finds and replaces
duplicates, and cleans up the archived folders safely. Tested on live data: 56 folders, 416,320
files, every archive verified, and the catalogue rebuilt identically from the archives alone.

### The work, in order (sidebar)
- **New job**: pick a source and destination (with **Recent** menus), exclusions, compression
  level, threads and protected folders. Archives each subfolder to a `.7z`, verified after writing.
  Refuses a folder before writing anything if the destination can't hold it (worst case, nothing
  compresses), and warns up front if a whole selection may not fit. Folder dialogs are modal and
  titled SOURCE / DESTINATION; overlapping source and destination are refused.
- **Progress**: throughput, time left, each folder's state, and the log below it with adjustable
  text size.
- **Duplicates**: every later copy of a file, archive by archive, oldest first. A size limit
  (200 MB to start) filters the page. Replacing is opt-in: nothing is ticked until you pick. Ticked
  copies become small `.atticarcdep` references to their original, after every original passes
  its hash check (with a dry-run preview).
- **Excluded**: files the exclusions set aside or discarded; put chosen ones back.
- **Clean up**: archived folders still on disk, as a tree with tick boxes. Two passes: check every
  ticked folder (files hashed, archive decoded, nothing deleted), then delete only those that
  passed, each only if unchanged since its check. Finds folders renamed since they were archived.
  Checks and ticks are saved, so a check can stop and carry on in a later session, and deleting
  can wait for another day. On NTFS, "unchanged" uses the change journal, which catches a change
  even if its timestamps are set back. The run stays in view while it works, and **Show log**
  lists each folder and how it went (also written to `atticarc.log`).

### Look up
- **Catalogue**: search every file in every archive, browse an archive's contents, extract files,
  follow references, put replaced files back. Reads in the background, so a slow or sleeping disk
  never freezes the window. **Rebuild from archives** recreates the catalogue from the manifests
  inside the archives, with progress.

### Robustness
- Archives are known by what they hold, not their file name: one renamed in Explorer is found
  again and relinked, and reference files naming its old name still resolve.
- The window recovers after displays sleep or change mode (no waiting on vsync, DirectX 12 on
  Windows); **Refresh window (F5)** and a **Compatible graphics** (OpenGL) setting as fallbacks.
  A log file at `%APPDATA%\atticarc\atticarc.log` records the renderer and any warnings.
- A page left mid-read no longer holds the others' buttons disabled.
- `atticarc-cli cleanup` does the same two passes from the command line.
- `atticarc-cli soak` stress test (passed 4 hours with no leaks); `atticarc-cli scan --hash`
  confirms duplicate candidates by BLAKE3 and can write a CSV.

Released under the MIT licence. Pull requests #1 to #27.
