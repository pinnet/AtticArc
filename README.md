# Attic

Archive, catalogue and de-duplicate large folder collections, to free up disk space without losing anything that matters.

Private successor to `pinnet/compressed-subfolders-7z`, rewritten in Rust.

## What it will do

1. **Scan** a source folder, recording path, size and date (fast, no file reads).
2. **Hash** only files whose size matches another, to confirm duplicates.
3. **Archive** each top-level folder to its own `.7z` on the destination drive, then verify it.
4. **Catalogue** everything in a SQLite file kept on the destination drive, so the catalogue travels with the archives.
5. **Review duplicates** in a GUI and remove copies only after confirming the list. Every removal is logged with the path of the kept copy.

## Decisions so far

- **KISS**: every file is stored whole in ordinary `.7z` archives, readable with plain 7-Zip. No delta or diff storage.
- **Space over speed**: runs can take as long as they need. Higher compression levels and fewer threads (less RAM) are fine.
- **Hash**: BLAKE3 by default, MD5 as an option for compatibility with the older tool.
- **Database**: embedded SQLite, no separate server.
- **EXE files**: extract version info (publisher, product, version), signature and header details into the catalogue. Older versions are kept, since rollbacks happen and some files are no longer available online.
- **Deployment**: one USB source disk and one USB destination disk (4 TB each).

## Open items

- [ ] Pick the 7z engine (sevenz-turbo, zesven, sevenz-rust2 or the 7-Zip binary) from the real-data benchmark.
- [ ] Pick a GUI framework.
- [ ] Macrium Reflect image reader: mount `.mrimg` files read-only and catalogue their contents to prune the NAS image collection.

## Layout

| Crate | Purpose |
|---|---|
| `crates/attic-core` | Scanning, hashing, archiving and catalogue logic |
| `crates/attic-cli` | `attic` command-line tool |
| `crates/attic-gui` | Desktop GUI (placeholder) |

## Try it

```
cargo run --release -p attic-cli -- scan D:\7ztest
```

Lists files and the largest groups of possible duplicates, without reading file contents.
