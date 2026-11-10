# TODO

Where AtticARC stands and what comes next, written at 0.9.0 (10 Nov 2026) so work can pick up
cold. Each item is meant to be one small PR, tested on Windows (`cargo test`, then a real run)
before the next.

## 0. Finish the 0.9.0 release

- [ ] Make the repository public (licence, README and a history check were done in #22).
- [ ] Tag `v0.9.0` on the merge of #22 and create the GitHub release, notes from `CHANGELOG.md`.
- [ ] Attach the Windows programs, from `D:\Dev\WIP\AtticArc` after `cargo build --release`:
      `gh release upload v0.9.0 target\release\atticarc.exe target\release\atticarc-cli.exe`

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

- [ ] Steps 1.1–1.7 done.
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
