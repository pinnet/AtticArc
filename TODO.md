# TODO

Where AtticARC stands and what comes next, written at 0.9.0 (10 Nov 2026) so work can pick up
cold. Each item is meant to be one small PR, tested on Windows (`cargo test`, then a real run)
before the next.

## 0. Finish the 0.9.0 release

- [ ] Make the repository public (licence, README and a history check were done in #22).
- [ ] Tag `v0.9.0` on the merge of #22 and create the GitHub release, notes from `CHANGELOG.md`.
- [ ] Attach the Windows programs, from `D:\Dev\WIP\AtticArc` after `cargo build --release`:
      `gh release upload v0.9.0 target\release\atticarc.exe target\release\atticarc-cli.exe`

## 0.1 Fixes before more use (agreed Nov 2026)

- [x] **Clean up: stop checking twice.** Today the check before deleting hashes the whole folder and
      decodes the whole archive again, though both were done when archiving (one run: still going
      ~18 hours later). Measured with sevenz-bench `verify` / `faults` (Nov 2026,
      D:\Dev\WIP\sevenz-bench, verify_table.txt and faults_table.txt), USB HDD, per 4 TB:
      fingerprint + index + change-journal snapshot: **seconds**; archive decode: 14.5 h;
      folder re-hash: 14.8 h; today's full check (both on one disk): 59 h.
      What each catches: the change-journal snapshot catches every folder change the full re-hash
      does (even a byte changed with its timestamp put back), no false alarms; only decoding the
      archive catches silent damage inside it (bit rot, a byte flipped with time put back).
  - [x] At archive time: take the change-journal snapshot (before and after hashing, so the
        hashes belong to an unchanging folder) and record the archive's size, modified time and
        "decoded OK" date. The decode already happens at archive time.
  - [x] Before deleting: archive fingerprint + archive index + folder snapshot. All match =
        delete (seconds). Anything different = refuse, offer the full check.
  - [x] Drop the folder re-hash from the delete check altogether: the snapshot covers it.
  - [x] Archive scrub: decode archives again in the background (one disk at a time, cancellable,
        resumable), recording the date. Before deleting, if the archive's last decode is older
        than a setting (e.g. 30 days), scrub it first. Catches bit rot without making every delete
        wait.
  - [x] Volumes without a change journal (exFAT, FAT32, network): the metadata snapshot misses
        "byte changed, time put back"; there, no archive-time check is saved, so the full check is
        needed.
  - [ ] The scrub age (30 days) as a setting.
- [x] **Same name, different content.** A folder with the same name as an archived one but
      different files is renamed with a number ("Name (2)", "Name (3)"…) and archived as its own
      entry. Test: archive a folder, swap in different content under the same name, archive
      again: it comes out as "Name (2)" and both archives are intact.
- [x] **Duplicates: remove only within folders.** Removing a duplicate inside an archive means
      rewriting the archive (five took about eight hours). Duplicates inside archives are only
      listed, with the space they waste; never removed. Done as "Replace duplicates…" switched off.
  - [x] Duplicates *within a folder*, at archive time: **Store identical files in a folder once**
        (New job; `atticarc-cli archive --identical-once`). Files of 64 KiB or more with the same
        size and BLAKE3 hash are archived once; later copies go in as `.atticarcdep` reference files
        to the first, in the same archive. The folder on disk is untouched. Off by default.
  - [ ] Try it on a real folder of songs on WinX, then decide whether it should be on by default.

## 0.5 Pin down the logic first (before refactoring)

The logic was settled by testing on live data (PRs #1 to #25). Before restructuring anything,
write down what "correct" means, so the refactor can be proved not to change it:

- [ ] `docs/behaviour.md`: every rule the code keeps, one line each, e.g. "a folder is deleted only
      if its saved check passed and its snapshot is unchanged"; "nothing is ticked to start"; "a
      folder that doesn't fit is refused before anything is written"; "an archive is matched by
      content, not name"; "checks and ticks survive a restart".
- [ ] A test for each rule that doesn't have one yet (most do: archive, verify, free space,
      rebuild, rename, relink, duplicates, clean up, snapshots, journal).
- [ ] An end-to-end test of the whole workflow on a generated folder tree: archive → duplicates
      → excluded → clean up (check, restart, delete) → rebuild the catalogue → compare.
- [ ] Then refactor (section 1): every step must leave all of these passing.

## 1. Refactor before 1.0

Goal: code that reads naturally ("fluent"), with named constants, written design rules, and
documentation. No change in behaviour: the existing tests prove each step.

### 1.1 Design rules and tooling
- [ ] `DESIGN.md` with the rules the code already follows:
  - Never block the window on disk: every catalogue or disk read in the GUI runs in the background
    with a spinner.
  - Nothing is deleted without a full check (files hashed against the catalogue, archive decoded).
  - Anything destructive is opt-in: nothing ticked to start; a confirmation that lists what goes.
  - Archives and archived folders are known by what they hold, not their names.
  - The catalogue can always be rebuilt from the archives (each carries its manifest).
  - Plain English in the UI; say what will happen and what was done.
- [ ] `rustfmt.toml` and one `cargo fmt` pass. The code doesn't match default rustfmt today.
- [ ] GitHub Action on every PR: `cargo fmt --check`, `cargo clippy -- -D warnings`
      (consider `-W clippy::pedantic`), `cargo test` on Windows.

### 1.2 Constants instead of scattered numbers (about 45 today)
- [ ] `core::limits`: free-space margin (256 MB), worst-case overhead (1%), hash prefix (64 KiB),
      default duplicate size limit (200 MB), recent folders kept (8), log text size range (9–24 pt).
- [ ] `gui::style` (extend `theme.rs`): text sizes (12.5, 13, 16, 26), spacing, row heights.
- [ ] `gui::timing`: repaint intervals (100/150/200 ms), dialog click pause (500 ms).

### 1.3 Remove duplication
- [ ] One `Loader<T>` for "read in the background, show a spinner, take the result": now copied in
      Catalogue, Duplicates and Clean up (and the "a disk that was asleep…" text three times).
- [ ] One `loading_spinner(ui, text)` widget.
- [ ] One plural helper, `count(n, "file")`, for `files()`, `folders()`, `copies()`, `archives()`.
- [ ] One way for pages to report "busy, repaint me" (a small trait) instead of the list in `main.rs`.

### 1.4 Split the big files
- [ ] `archive.rs` (about 2,700 lines) into modules: `scan`, `write`, `verify`, `rewrite`
      (put back / replace), `dedupe`, `identity` (fingerprints, relink, renamed folders), `cleanup`
      (check before delete, snapshots), `manifest`.
- [ ] `catalog.rs` (about 1,100 lines): schema and migrations apart from queries.

### 1.5 Fluent code
- [ ] Builders for long argument lists, e.g.
      `Check::folder(&src).against(&archive).threads(32).run(&progress)?`,
      `Archive::folder(src).to(out).level(5).run(&progress)?`.
- [ ] Meaningful types instead of bare ones: `ArchiveId`, `Fingerprint`, `Bytes(u64)` with
      `.human()`, so arguments can't be swapped and sizes format themselves.
- [ ] Names that read like the UI: `folder.passed_check()`, `archive.is_missing()`.

### 1.6 Unused code and dependencies
- [ ] `cargo clippy` and `cargo machete`; remove what is unused.
- [ ] Hide or remove development-only bits before 1.0 (e.g. **Clear the catalogue…**).

### 1.7 Documentation
- [ ] Rustdoc on every public item, with short examples; `#![warn(missing_docs)]` in each crate;
      `cargo doc` builds clean.
- [ ] `docs/architecture.md`: how core, CLI and GUI fit together, with a diagram.
- [ ] `docs/formats.md`: the archive layout and manifest, the catalogue schema, `.atticarcdep`
      files. These are what 1.0 freezes.
- [ ] `docs/guide/`: a short page per screen (New job, Progress, Duplicates, Excluded, Clean up,
      Catalogue).

### 1.8 Plugin-based structure

Goal: each section of the app is a plugin behind a small trait, so any part can be replaced
later without touching the rest. First up, progress: one progress plugin used everywhere in the
GUI. Do this alongside 1.3–1.5 (it absorbs the `Loader<T>` and "busy, repaint me" items).

- **Three roles, kept apart:**
  - *Contract*: a small, stable trait in `core`, e.g. `Progress { begin(task, total),
    report(done, message), end(result) }`. Nothing else in the app knows a concrete type.
  - *Producers*: archive, verify, hash, duplicate scan and clean up only emit through the trait.
    They never know what is shown.
  - *Views*: progress bar, ETA/throughput label, taskbar badge, log line, later an MCP status
    feed. Each is its own implementation; several can listen to one task.
- **Reuse:** the same view plugin appears wherever a job runs; each instance just subscribes to a
  different task.
- **One registry** wires plugins together at startup (config or simple registration). Plugins
  never look each other up directly.
- **Events over direct calls:** jobs push progress events onto a channel/bus, so the GUI can
  attach and detach while a long background job keeps running (fits "leave it running").
- **Small contracts:** keep each trait narrow so a replacement only implements a few methods.
- **Seams only where change is likely** (don't make constants or core models pluggable):
  - [ ] Progress / feedback (first, as the pattern for the rest).
  - [ ] Archive backend: 7-Zip now; a Rust compression routine later.
  - [ ] Metadata extractors: EXE version info, Macrium `.mrimg` reader (section 2).
  - [ ] Storage / catalogue access.
  - [ ] AI review over MCP (section 2) as a plugin on the same contracts.
- [ ] Write the contracts and the registry into `docs/architecture.md` and `DESIGN.md`.
- Every step must leave the section 0.5 tests passing.

## 1.9 Other operating systems (after the refactor)

The core is plain Rust and mostly portable; the Windows-only parts sit behind a small platform
layer after the refactor, one implementation per system:

| Piece | Windows (now) | macOS | Linux |
|---|---|---|---|
| Free space | `GetDiskFreeSpaceExW` | `statvfs` | `statvfs` |
| Change detection in snapshots | NTFS change journal numbers | file status change time (`ctime`) | `ctime` |
| Renderer | wgpu on DirectX 12, no vsync; OpenGL fallback | wgpu on Metal | wgpu on Vulkan; OpenGL fallback |
| Folder dialogs, modal | `rfd` with the window as parent | same | same |
| Settings and log | `%APPDATA%\atticarc` | `~/Library/Application Support/atticarc` | `~/.config/atticarc` |
| Delete permission, locked files | Windows rules | Unix permissions | Unix permissions |

- On macOS and Linux, `ctime` changes with every write or metadata change and, unlike the
  modified time, can't be set back by ordinary tools, so it protects a snapshot as well as the
  NTFS journal does.
- [ ] Platform layer: `platform::{free_bytes, change_marker(file), config_dir}`.
- [ ] CI matrix (Windows, macOS, Linux) building and testing every PR.
- [ ] Release builds per system attached to each GitHub release (`.exe`; a macOS app bundle for
      the M1 MacBook; a Linux binary).

## 2. Features still open

- [ ] Skip locked files instead of failing the whole folder.
- [ ] MD5 as an optional hash, for compatibility with the old tool.
- [ ] EXE version info (publisher, product, version, signature) in the catalogue.
- [ ] Archive breakdown screen (pie or treemap of what an archive holds).
- [ ] Macrium `.mrimg` reader: catalogue the image collection on the NAS to find repeated data.
- [ ] Tree view of an archive's contents in the Catalogue browser (Clean up has the tree; the
      Catalogue browser was left as it is).
- [ ] AI review over MCP: an `atticarc-mcp` server with read-only catalogue tools (list, summarise,
      search, file info) and one `suggest` tool that records keep/rescue/drop suggestions for a new
      **Review** page; you approve, and anything deleted still goes through the usual check.
      Metadata only by default. Rust with `rmcp`; Claude Desktop or a local LLM on Proxmox as client.

## 3. Before calling it 1.0

- [ ] Steps 1.1–1.8 done.
- [ ] Use 0.9.x on real data for a while with no format changes needed.
- [ ] Freeze the formats in `docs/formats.md`; from then on, a change that can't read older
      archives or catalogues is a 2.0.

## Notes from the 0.9.0 testing

- Live run: 56 folders, 858.6 GB, about 64 MB/s with source and destination on different disks
  (31 MB/s on one disk). 4-hour soak test passed with no leaks.
- Catalogue rebuilt from the archives matched the original exactly (416,320 files); only the run
  log is not kept in the archives.
- Copying many small Amiga files with Explorer froze (likely Amiga Forever's Explorer extension);
  robocopy works. Add Defender exclusions for big moves.
