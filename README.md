# Attic ARC

Archive, catalogue and de-duplicate large folder collections, to free up disk space without losing anything that matters.

Private successor to `pinnet/compressed-subfolders-7z`, rewritten in Rust. The archive,
catalogue and verified-delete engine comes from foldarc, which is now part of Attic ARC.

## What it will do

1. **Scan** a source folder, recording path, size and date (fast, no file reads).
2. **Hash** only files whose size matches another, to confirm duplicates.
3. **Archive** each top-level folder to its own `.7z` on the destination drive, then verify it.
4. **Catalogue** everything in a SQLite file kept on the destination drive, so the catalogue travels with the archives.
5. **Review duplicates** in a GUI and remove copies only after confirming the list. Every removal is logged with the path of the kept copy.

## Decisions so far

- **KISS**: every file is stored whole in ordinary `.7z` archives, readable with plain 7-Zip. No delta or diff storage.
- **Space over speed**: runs can take as long as they need. Higher compression levels and fewer threads (less RAM) are fine.
- **Hash**: BLAKE3 for matching, plus SHA-256 for anything that expects it. MD5 as an option for compatibility with the older tool is still to come.
- **Database**: embedded SQLite, no separate server.
- **EXE files**: extract version info (publisher, product, version), signature and header details into the catalogue. Older versions are kept, since rollbacks happen and some files are no longer available online.
- **Deployment**: one USB source disk and one USB destination disk (4 TB each).
- **7z engine**: sevenz-turbo, LZMA2 level 5 (9 optional), 32 MiB chunks, one archive at a time on all
  threads (thread count is a setting). Picked from the sevenz-bench real-data benchmark: fastest
  compressor on SSD, and level 9 saved under 1%.
- **GUI**: eframe/egui, laid out like the "Archiver GUI mockups" canvas.
- **Verify before trusting**: every file is hashed (BLAKE3 and SHA-256) as it is compressed, and the
  archive is decoded and checked against those hashes before it is catalogued.
- **Delete only after a full check**: a source folder is deleted only after every file in it is
  re-hashed against the catalogue *and* the archive is decoded against the same hashes, with two
  confirmations. Deleting is permanent (no Recycle Bin). Protected folders can never be deleted.

## Open items

- [ ] Duplicates: choose the copy to keep and remove the rest, logging each removal (the screen lists groups, read-only).
- [ ] Archive breakdown screen (pie / treemap of an archive's contents).
- [ ] MD5 as an optional hash for compatibility with the older tool.
- [ ] Skip locked files instead of failing the folder; free-space check before archiving.
- [ ] Hash the size-collision candidates from `attic scan` before archiving.
- [ ] EXE version info in the catalogue.
- [ ] Macrium Reflect image reader: mount `.mrimg` files read-only and catalogue their contents to prune the NAS image collection.

## Layout

| Crate | Purpose |
|---|---|
| `crates/attic-core` | `scan` (fast metadata scan), `archive` (compress, verify, check-then-delete), `catalog` (SQLite), `settings` |
| `crates/attic-cli` | `attic` command-line tool |
| `crates/attic-gui` | Desktop GUI (`attic-gui`) |

## GUI

```
cargo run --release -p attic-gui -- D:\Photos
```

Starts on the given source folder (or the last one used).

- **New job**: source and destination, compression level, threads, store mode and protected folders.
  Every subfolder of the source is listed with a tick box; unticked folders grey out and sink to the
  bottom. Folders that already have a verified archive are listed under **Already archived** with an
  **X** that starts the check-then-delete flow.
- **Progress**: overall progress, throughput, time left, per-folder status and a log. *Stop after this
  folder* or *Cancel now* (the partial archive is removed).
- **Catalogue**: search every file in every archive by name or folder, without unpacking; see its
  original path, hashes and how many copies exist, and extract a single file.
- **Catalogue → Clear the catalogue…** (for development): forgets every archive in the catalogue after a confirmation. The .7z files and manifests stay on disk.
- **Duplicates**: files with the same size and BLAKE3 hash across all archives, largest saving first.
- **Dark mode / Light mode**: the button at the bottom of the sidebar switches colours; the choice is remembered.

## Output

The destination folder (default: `Attic ARC archives\<name>` beside the source, so `D:\Photos` archives to
`D:\Attic ARC archives\Photos`) holds:

- `<folder>.7z`: standard 7z, opens in 7-Zip. With store mode on, already-compressed files (zip,
  jpg, mp4, whl, safetensors and similar) go in uncompressed blocks.
- `<folder>.7z.manifest.json`: every file's path, size, times, BLAKE3 and SHA-256.
- `catalog.db`: SQLite with `archives`, `entries` (full-text index `entries_fts`) and `runs`.

Settings are kept in `%APPDATA%\attic\settings.json`; settings saved by foldarc are picked up on
first run.

## Command line

```
attic scan <folder> [--top N]                  Possible duplicates by size, without reading files
attic archive <folder>... [--out DIR] [--level 5|9]   Archive and verify, exit 0 if all verified
attic search <words> [--out DIR]               Search the catalogue
```

## Build

```
cargo build --release
cargo test
```
