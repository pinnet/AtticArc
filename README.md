# AtticARC

Archive, catalogue and de-duplicate large folder collections, to free up disk space without losing
anything that matters.

AtticARC packs each folder into its own ordinary `.7z` archive (readable with plain 7-Zip), checks
every file in the archive against hashes taken while compressing, and records everything in a
searchable catalogue that travels with the archives. Only then, after a second full check, does it
delete the original folder. It finds duplicate files across archives and, if you choose, replaces
later copies with small reference files pointing at the original.

Windows is the main target (a GUI and a command-line tool); the code is plain Rust and builds on
other platforms too.

## Features

- **Verified archives**: every file hashed (BLAKE3 and SHA-256) as it is compressed; each archive
  decoded and checked before it is catalogued.
- **Searchable catalogue** (SQLite) beside the archives, rebuildable from the archives alone: each
  carries its own manifest.
- **Safe clean-up**: originals are deleted in two passes: check every ticked folder (every file
  hashed, the archive decoded), then delete only those that passed and haven't changed since.
- **Duplicates**: found across all archives by hash and shown with the space they take. Copies inside
  archives are listed, not removed (taking one out rewrites the whole archive, which takes hours).
  Copies inside one folder can be stored once as it is archived (**Store identical files in a folder
  once**, below).
- **Exclusions**: leave junk out (discard) or keep files apart (set aside) by rule.
- **Free-space check**: a folder that can't fit is refused before anything is written.
- **Rename-proof**: archives and archived folders renamed in Explorer are found again by what they
  hold.

## Install

Download `atticarc.exe` (the GUI) and `atticarc-cli.exe` from the
[latest release](https://github.com/pinnet/AtticArc/releases/latest). No installer; run them from
anywhere.

Or build from source with Rust (stable, edition 2024):

```
cargo build --release
```

The programs are in `target/release/`. `cargo test` runs the tests.

## Status

**0.9.0** is the first release: the full workflow, tested on live data (56 folders, 416,320 files).
Until 1.0, the formats your data lives in (the archive manifest, the catalogue database and the
`.atticarcdep` reference files) may still change. See [CHANGELOG.md](CHANGELOG.md).

## Licence

MIT: see [LICENSE](LICENSE).

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

- [x] Duplicates: replace later copies with `.atticarcdep` references to the oldest copy, and put them back.
- [ ] Archive breakdown screen (pie / treemap of an archive's contents).
- [ ] MD5 as an optional hash for compatibility with the older tool.
- [x] Free-space check before archiving and before rewriting an archive (worst case: nothing compresses).
- [ ] Skip locked files instead of failing the folder.
- [x] Hash the size-collision candidates from `atticarc-cli scan` before archiving (`scan --hash`).
- [ ] EXE version info in the catalogue.
- [ ] Macrium Reflect image reader: mount `.mrimg` files read-only and catalogue their contents to prune the NAS image collection.

## Layout

| Crate | Purpose |
|---|---|
| `crates/atticarc-core` | `scan` (fast metadata scan), `archive` (compress, verify, check-then-delete), `catalog` (SQLite), `settings` |
| `crates/atticarc-cli` | `atticarc-cli` command-line tool |
| `crates/atticarc-gui` | Desktop GUI (`atticarc`) |

## GUI

```
cargo run --release -p atticarc-gui -- D:\Photos
```

Starts on the given source folder (or the last one used).

- **New job**: source and destination, compression level, threads, store mode and protected folders.
  **Store identical files in a folder once** (off by default; `atticarc-cli archive --identical-once`):
  files of 64 KiB or more with exactly the same content in the folder are compressed once, and each
  later copy goes into the archive as a small `<name>.atticarcdep` reference file naming the first
  copy. The folder on disk isn't changed, and its check before a delete still compares every copy
  with its hash. The Catalogue's **Extract the original to…** brings a copy back as a file, and
  **Put back** puts it back into the archive; 7-Zip on its own extracts the reference file.
  Each has a **Recent ▾** menu of the last 8 folders used (a folder that isn't there now, such as on an
  unplugged drive, is greyed out), and the app starts on the last ones used.
  Every subfolder of the source is listed with a tick box; unticked folders grey out and sink to the
  bottom. Folders that already have a verified archive are listed under **Already archived** with an
  **X** that starts the check-then-delete flow. While the folder is being deleted, a window with a
  spinner and the time so far stays up until it's done.
- **Progress**: the folder list, with the log below it. **A−** / **A+** set the log's text size (remembered).
- **New job → Exclusions**: rules for files to leave out of the main archive, each ticked on or off.
  A *wildcard* without `/` matches file names anywhere (`*.lnk`, `Thumbs.db`); `*` stays within a
  folder, `**` crosses folders. A *regex* matches anywhere in the path inside the folder. Case is
  ignored. Each rule either **sets aside** its files, into a verified archive of their own that is
  stored, without compressing it again, inside the main archive at `.atticarc/excluded.7z` (so the
  destination holds one archive per folder), or **discards** them (junk such as shortcuts): they are archived nowhere,
  but the manifest lists them with their hashes. If both kinds match, the file is set aside. An
  unticked `*.lnk` discard rule is there to start from.
- **Excluded**: every archived folder whose exclusions left files out, with what was set aside or
  discarded, and **Delete the folder…**. Deleting checks every file in the folder against the main
  archive, the set-aside archive or the discard list (same hash): the set-aside archive inside is
  saved to a check folder in `.atticarc` as the main archive is decoded, then decoded itself. The
  confirmation lists the discarded files that will be gone for good.
  Tick files under *Files left out* and **Put them back in the archive**: the main archive is unpacked
  into a work folder in `.atticarc`, written again with those files (set-aside ones from the set-aside
  archive inside it, discarded ones from the folder while it still has them), verified, and only then swapped in,
  so it needs free space for the unpacked archive. **Put ticked back, discard the rest…** also moves
  the remaining set-aside files to the discard list and takes the set-aside archive out, so you can
  keep what you want and throw the rest away with the folder.
  Archives made before this keep `<folder>.excluded.7z` beside them, and still work: the check before
  a delete reads it there, and **Move it inside the archive** (or any put back, discard or duplicate
  change) moves it inside, verifies the new archive, and only then deletes the one beside it.
- **Progress**: overall progress, throughput, time left, per-folder status and a log. *Stop after this
  folder* or *Cancel now* (the partial archive is removed).
- **Clean up**: the last step. Every archived folder whose source is still on disk, as a tree by parent
  folder with tick boxes (nothing ticked to start; a parent's box ticks all its folders). Nothing is
  deleted without a passed check. On NTFS the check is done as each folder is archived (every file is
  hashed as it is compressed and the archive decoded against those hashes, and the folder's change
  journal shows it didn't change meanwhile), so those folders can go straight to **2. Delete … that
  passed**. Just before each delete, after a yes, AtticARC makes sure in seconds that the folder still
  matches its snapshot and the archive its size, modified time and header; anything changed is left
  alone. An archive last decoded more than 30 days ago is decoded again first, since after the delete
  it is the only copy. **1. Check** is the full check (every file hashed, the archive decoded), for
  folders without a check: archived before this, on a drive without a change journal, changed while
  archiving, or after a refused delete. **Check archives for damage** decodes again every archive not
  decoded for 30 days (**…all of them** for every one), reading only the archives, to catch damage
  such as a bad sector; it can be stopped and carried on. (Measured on a USB HDD, per 4 TB: the checks
  before a delete take seconds, decoding the archives about 14.5 hours, and the old full check about
  59 hours.) **Stop after this folder**
  ends either pass cleanly. Archived folders no longer where they were are listed apart, with
  **Look for renamed folders**, which finds a folder holding exactly their files among those beside
  them and lists it under its new name (checked like any other before deleting).
  Checks and ticks are saved in the catalogue, so they survive a restart (or a window that froze):
  **1. Check** carries on with the ticked folders not yet passed (**Check all ticked again** redoes
  them all), and a long run can be checked over several sessions and deleted later. On NTFS, the
  snapshot holds every file's change journal number, so any write since the check is caught, even
  one whose timestamps were set back; it needs no administrator rights, and on drives without a
  journal (exFAT) timestamps are used. Folders checked with the journal show "change journal". The same steps run without the window
  with `atticarc-cli cleanup <destination> [--check] [--scrub | --scrub-all] [--delete]`.
- The sidebar runs in the order of the work: New job, Progress, Duplicates, Excluded, Clean up. The
  **Catalogue** sits apart below it, under *Look up*, since it is for finding things, not a step.
- **Catalogue**: open any archive and browse its folders like a file manager (name, type, size and
  date, sortable), or search every file in every archive by name or folder, all without unpacking.
  Select a file to see its original path, hashes and how many copies exist, and extract it.
- **Catalogue → Clear the catalogue…** (for development): forgets every archive in the catalogue after a confirmation. The .7z files and manifests stay on disk.
- **Duplicates**: a tree on the right lists every archive, oldest first. The oldest copy of each file
  (same size and BLAKE3 hash) is the original and always stays; every later copy is a leaf under its
  archive, shown with its full path and the archive and path of its original. The left pane shows the
  details of what is selected. Copies are shown, not removed: the **Replace duplicates…** button is
  switched off for now, because each archive rewrite took hours (five copies, about eight hours). The
  replacing described here stays in the code for a later batched mode. **Replace duplicates…** opens the list of every copy, none ticked: tick
  the copies to replace (or **Tick all**); anything unticked stays as it is. Tick **Only copies of at least … MB** (200 MB to start; type or drag any size) to leave smaller
  copies alone. **Preview (dry run)** decodes every original from its archive and checks
  its hash, changing nothing. **Replace** runs the same check (nothing changes if any original fails),
  then works through the archives oldest first: each is unpacked, every ticked copy is checked against
  its hash and swapped for a small `<name>.atticarcdep` reference file, and the archive is written
  again, verified and swapped in. A `.atticarcdep` file is plain JSON naming the original's archive and
  path, its size and hashes, so it can be read in any text editor. The manifest records each replaced
  file, so the folder still passes the check before a delete, the references survive a catalogue
  rebuild, and an original that references point to can't be removed.
- **Catalogue → a reference file** shows the file it stands for and where the original is:
  **Extract the original to…** follows it (checking the hash), **Show the original** opens it, and
  **Put the file back** rewrites the archive with the real file again, taken from the original.
- **Dark mode / Light mode**: the button at the bottom of the sidebar switches colours; the choice is remembered.

## Output

The destination folder (default: `AtticARC archives\<name>` beside the source, so `D:\Photos` archives to
`D:\AtticARC archives\Photos`) holds:

- `<folder>.7z`: standard 7z, opens in 7-Zip. With store mode on, already-compressed files (zip,
  jpg, mp4, whl, safetensors and similar) go in uncompressed blocks. Each archive also holds
  `.atticarc/manifest.json` (hidden): every file's path, size, times, BLAKE3 and SHA-256, so the
  record travels with the archive. It sits in a small block of its own, so reading it is quick.
- `.atticarc\catalog.db` (hidden folder): SQLite with `archives`, `entries` (full-text index
  `entries_fts`) and `runs`. It can be rebuilt from the archives' manifests (**Rebuild from archives**
  in the Catalogue, or `atticarc-cli rebuild`). A `catalog.db` left loose by earlier versions is
  moved in automatically, and `.7z.manifest.json` files they left beside archives are still read.

Settings are kept in `%APPDATA%\atticarc\settings.json`; settings saved by earlier versions (`attic`,
`foldarc`) are picked up on first run.

## Command line

```
atticarc-cli scan <folder> [--top N]                  Possible duplicates by size, without reading files
        [--hash] [--csv FILE]                         Then confirm them by BLAKE3; list them in a CSV
atticarc-cli archive <folder>... [--out DIR] [--level 5|9]   Archive and verify, exit 0 if all verified
atticarc-cli search <words> [--out DIR]               Search the catalogue
atticarc-cli rebuild [--out DIR]                      Add archives missing from the catalogue
atticarc-cli soak --work DIR [--hours H] [--gb G] [--source FOLDER]   Robustness test (below)
atticarc-cli watch [atticarc|PID] [--every S]         Log the window's memory until it exits
```

### Confirming duplicates before archiving

`scan --hash` hashes only the files that share a size with another file. Files over 64 KiB are
first compared by a hash of their first 64 KiB, and only those still matching are read in full,
so most same-size files that differ cost one small read. It prints the confirmed groups and the
space keeping one copy of each would free; `--csv FILE` writes every confirmed file as
`group,size,blake3,path`. Files that can't be read, or changed since the scan, are reported and
left out rather than stopping the run. Nothing is changed on disk.

### Displays waking with another resolution or scale

With two monitors at different resolutions or scaling, Windows can briefly change a window's
scale when the displays sleep and wake, and the window may come back drawn at the wrong size or
stop responding. Work in the background (archiving, checking, deleting) carries on regardless.

- The renderer doesn't wait for vsync, so it can't be left waiting for ever for a display that is
  asleep or changing mode (the likely cause of the freeze), and on Windows it uses DirectX 12, which
  copes with display changes better (set `WGPU_BACKEND` to choose another).
- AtticARC notices when the display it is on changes and redraws itself at the new size and scale.
- **Refresh window (F5)** in the sidebar does the same on demand.
- If the window still freezes, tick **Compatible graphics** (bottom of the sidebar) and restart:
  AtticARC then draws with OpenGL instead of the default renderer. The same can be chosen for one
  run with `atticarc --compatible-graphics` or `ATTICARC_RENDERER=glow`.
- A log is kept at `%APPDATA%\atticarc\atticarc.log`: the renderer and graphics card used, display
  changes and renderer warnings. Send it along with a report if the window ever freezes.
- If the window is frozen now: Win+Shift+←/→ (move it to the other monitor), Win+↓ then Win+↑,
  or Win+D twice, all make Windows send it a fresh size.

### Renamed archives

An archive is known by what it holds, not its file name: a fingerprint of its creation time and every
file's path, size and BLAKE3 hash, which the catalogue and the manifest inside the archive both give.
If an archive is renamed in Explorer, the Catalogue and Duplicates pages find it again under its new
name when they load (reading only the small manifests of `.7z` files the catalogue doesn't know),
update the catalogue, and keep the old name so reference files that name it still work.
**Rebuild from archives** relinks renamed archives instead of adding them twice, and reports a copy of
an archive under another name instead of cataloguing it again. An archive whose file can't be found
at all is marked as missing.

### Free space

Before a folder is archived, its destination must have room for the archive (and the set-aside
archive) as if nothing compressed, plus 1% and a 256 MB margin; otherwise that folder fails with a
not-enough-space message before anything is written, and the job moves on to the next. Putting
files back and replacing duplicates check the same way for the unpacked archive plus its new copy.
The GUI log and `atticarc-cli archive` also warn up front when the whole selection may not fit.

### Robustness testing

`soak` runs every engine path a long job uses, round after round: archive (with set-aside and
discarded files), the full check before a delete (nothing is deleted), putting a file back and
discarding the rest, removing a duplicate, then clearing and rebuilding the catalogue. It works on
G GB of generated files (large, medium, thousands of small ones in deep folders, odd names), or with
`--source` archives a real folder, which is only read. Everything is written inside `--work`, which
must be empty or a soak's own and may not be a protected folder; each round's archives are removed
before the next.

Every `--every` seconds (default 10) it writes the process's resident and private memory, handles,
threads, phase and free disk space to a CSV in the work folder, and after each round prints the
same. It passes only if every round succeeds and, after two warm-up rounds, private memory, handles
and threads stop climbing (private memory may not grow by more than 64 MB at more than 16 MB an
hour). Exit code 0 means passed.

`watch` samples the window the same way while you run a real job in it, so the GUI's own memory is
measured too; it stops when the window closes and prints the trend.

## Build

```
cargo build --release
cargo test
```
