# Changelog

AtticARC uses [Semantic Versioning](https://semver.org). Until 1.0, the formats your data lives in
(the archive layout and its manifest, the catalogue database, `.atticarcdep` reference files) may
still change; 1.0 will mark them stable.

## 0.9.0 (2026-11-10): first MVP

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

### Look up
- **Catalogue**: search every file in every archive, browse an archive's contents, extract files,
  follow references, put replaced files back. Reads in the background, so a slow or sleeping disk
  never freezes the window. **Rebuild from archives** recreates the catalogue from the manifests
  inside the archives, with progress.

### Robustness
- Archives are known by what they hold, not their file name: one renamed in Explorer is found
  again and relinked, and reference files naming its old name still resolve.
- `atticarc-cli soak` stress test (passed 4 hours with no leaks); `atticarc-cli scan --hash`
  confirms duplicate candidates by BLAKE3 and can write a CSV.

Released under the MIT licence. Pull requests #1 to #22.
