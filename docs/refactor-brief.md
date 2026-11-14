# Refactor brief: AtticARC into a plugin scheme

A brief for one long, unattended run (Claude Fable) that restructures AtticARC around small
plugin contracts **without changing what it does**. Read this whole file first, then
`TODO.md` sections 0.5 and 1 (1.1 to 1.8), which this brief puts in order. Written 14 Nov 2026,
at commit `8672aa4` on `fast-cleanup` plus the uncommitted Settings / detailed log / Archive
again work (commit that first: see "Before starting").

## The goal in one paragraph

Today the app works and has been settled on live data, but most of the logic sits in one
3,200-line `archive.rs` and the GUI pages each repeat their own background-loading code. The
refactor splits the core into modules, puts the parts most likely to be replaced behind narrow
traits (progress, archive backend, per-file streams, metadata extractors, catalogue access),
wires them in one registry, moves everything system-specific behind one platform module, and
builds the same app for Windows, macOS and Linux (see "The outcome"). Success means: every existing test and the new
behaviour tests pass at every step, archives made before the refactor still open, check, scrub,
rebuild and delete exactly as before, and a later change of compressor or a new metadata plugin
touches one module and the registry, nothing else.

## The outcome: three builds, one AtticARC

The run ends with **three separate but identical builds**: Windows (x64), macOS (Apple
Silicon, a signed `.app` like Plugin²'s) and Linux (x64). "Identical" means:

1. **The same features everywhere.** Every screen, setting, command and check exists on all
   three; nothing is "Windows only" to the user. Where the system differs underneath (change
   detection, free space, file streams), a platform module gives the same answer by its own
   means.
2. **Interchangeable.** An archive made on any of the three opens, lists, verifies, checks,
   scrubs, extracts and rebuilds the catalogue on the other two. A destination folder can be
   carried between them on a drive and used as it is. Paths in the catalogue are kept as written
   by the system that made them and shown as such; nothing assumes `\` or drive letters.
3. **Metadata streams for every file, on every system.** Each file's extra data is kept in the
   archive and put back on extract wherever the system can hold it: macOS extended attributes
   (Finder tags, the old colour label, resource forks), Linux extended attributes
   (`user.xdg.tags` and other `user.*`), Windows NTFS alternate streams (where a Mac's tags land
   on a Windows share). A stream another system can't hold is kept in the archive, shown in the
   Catalogue, and put back when the file reaches a system that can. Tags are decoded into one
   shared form (name + Finder colour) so they look the same on all three.
4. **Similar speeds.** The same work takes about the same time on comparable hardware: same
   algorithms, same thread use, no slow path on one system. Measured by one benchmark command on
   all three (see "Checking the work").
5. **Settings per module.** Each plugin (archive backend, each stream plugin, each metadata
   extractor, progress views) offers its own options, shown in the Settings window in a section
   of its own, with an on/off where it makes sense. The same settings exist on all three systems;
   one that a system can't use is shown greyed with the reason, never hidden.

## Ground rules (not negotiable)

1. **No change in behaviour.** Not in the GUI, the CLI output that scripts might read, the
   archive layout, the manifest, the catalogue schema, or `.atticarcdep` files. Where a rule is
   unclear, the current code is the specification. If something looks like a bug, write it in
   `TODO.md` under "Found during the refactor" and leave it.
2. **Green at every step.** Each step ends with `cargo fmt --check`, `cargo clippy --workspace
   --all-targets -- -D warnings` and `cargo test --workspace` passing **on Windows (WinX)**. A
   step that can't pass is reverted, not patched over.
3. **Formats are frozen.** Archives made by 0.9.0 and by `8672aa4` must keep working: a fixture
   made in phase 0 is opened, verified, checked, scrubbed and rebuilt by the tests in every later
   phase. New manifest fields are optional and skipped when empty, so an unchanged folder gives a
   byte-identical manifest.
4. **Only inside `D:\Dev\WIP\AtticArc`.** Warn before writing anything outside `D:\Dev`.
   Generated test data goes under the repo's `target\` or a temp folder that is removed.
5. **`.github` is protected** from Claude's tools: write workflow files to `docs/ci/` and say they
   need moving to `.github/workflows/`.
6. **Commits:** one per step, on a branch `refactor-plugins` from `fast-cleanup`. Author
   `Danny Arnold <pinnet@users.noreply.github.com>`, message ending with the session's
   `Co-Authored-By` and `Claude-Session` lines. **Never push.**
7. **No new dependencies** unless a step can't be done without one; say which and why in the
   commit. Never bump `sevenz-turbo` (0.29.3, needs rustc 1.97.1) or `rusqlite` (0.41) as part of
   this.
8. **Keep it plain.** Plain English in UI strings and docs, as now. No speculative generality:
   only the seams listed below become traits; constants and core models stay concrete.

## Where things are now

| File | Lines | Holds |
|---|---:|---|
| `core/src/archive.rs` | 3,210 | scan, write (sevenz-turbo), verify, manifest, rewrite (put back, replace duplicates), dedupe within a folder, fingerprints and relink, check before delete, snapshots, delete, archive again, scrub, `Progress` |
| `core/src/catalog.rs` | 1,296 | SQLite schema, migrations (`add_column`), every query |
| `core/src/cleanup.rs` | 492 | Clean up policy: candidates, check, delete gate, scrub list |
| `core/src/scan.rs` | 388 | the fast duplicate scan for `atticarc-cli scan` |
| `core/src/{settings,exclude,journal,disk}.rs` | 675 | saved settings, exclusion rules, NTFS journal, free space |
| `gui/src/{cleanup,duplicates,catalogue}.rs` | 2,509 | three pages, each with its own background loader and spinner |
| `gui/src/{jobs,new_job,progress,main}.rs` | 1,788 | batch archiving, New job, Progress, sidebar, Settings window |
| `cli/src/{main,soak,mem}.rs` | 1,446 | commands, the soak test, memory watcher |

60 core tests today, plus the soak test (`atticarc-cli soak`).

## Target shape

```
atticarc-core
  plugin/            contracts (traits) and the registry; no implementations
    progress.rs      Progress events and sinks
    backend.rs       ArchiveBackend
    streams.rs       FileStreams
    metadata.rs      MetadataExtractor
    store.rs         CatalogStore (only what the GUI and CLI need)
    registry.rs      Plugins: built once at start, passed down; no globals
  backend/sevenz/    the sevenz-turbo ArchiveBackend (all sevenz-turbo use lives here)
  streams/           macos-xattr, linux-xattr, ntfs-streams, and the tags decoder
  platform/          windows.rs, macos.rs, linux.rs: one per system, chosen at build time
  archive/           scan, plan, write (via backend), verify, manifest, rewrite, dedupe,
                     identity (fingerprints, relink, renamed folders), again
  cleanup/           check before delete, snapshots, delete gate, scrub
  catalog/           schema + migrations apart from queries; implements CatalogStore
  limits.rs          named constants (TODO 1.2)
atticarc-gui
  loader.rs          one Loader<T> (background read, spinner, result) for every page
  views/progress/    progress bar, log, "Now:" line as ProgressSink views
  settings_window.rs one section per module, drawn from each plugin's SettingSpec
atticarc-cli         unchanged commands, using the registry
```

## The contracts

Each is small. Sketches only; the run settles the exact signatures, keeping them this narrow.

1. **Progress** (first: it is the pattern for the rest).
   `trait ProgressSink: Send + Sync { fn event(&self, e: &Event); fn cancelled(&self) -> bool; }`
   with `Event::{Phase { name, total }, File { verb, rel, size }, Bytes(u64), Line(String),
   Finished(Result<(), String>)}`. Views: the GUI's bar, "Now:" line and detailed log; the CLI
   ticker. The current `Arc<Progress>` becomes one sink that keeps today's fields (so the GUI
   changes one page at a time) until every caller has moved.
2. **ArchiveBackend.** What AtticARC needs from a compressor, no more:
   `write(plan, sink) -> Written` (files in order with their blocks and methods, the inner
   archive, the manifest last in its own block), `verify(archive, expected, sink)`,
   `index(archive) -> Vec<IndexEntry>` (names and sizes, header only), `extract(archive, rel,
   dest)`, `read_entry(archive, rel) -> bytes` (the manifest). First and only implementation:
   sevenz-turbo, moved as it is. Add **contract tests** any backend must pass (round trip,
   Unicode names, empty files and folders, >4 GB block split by constant, cancel mid-write
   leaves no `.7z`), run against the sevenz backend.
3. **FileStreams** (TODO 1.8, agreed Nov 2026): `read(file) -> Vec<Stream { name, bytes }>` at
   archive time, `restore(file, &[Stream])` at extract time, plus which systems it reads and
   restores on. Small streams go in the file's manifest entry, big ones as hidden entries under
   `.atticarc/streams/<path>/<name>`, hashed and verified like files. Plugins: `macos-xattr`,
   `linux-xattr`, `ntfs-streams`, and a `tags` decoder on top (name + Finder colour). Stream
   names are stored in one portable form (`xattr:<name>` or `ntfs:<name>`), so each system knows
   which it can put back. A file whose streams change counts as changed for the check before a
   delete. With every stream plugin off, manifests are byte-identical to today.
4. **MetadataExtractor**: `fn extract(file) -> Option<Metadata>` for the catalogue (EXE version
   info, Macrium `.mrimg` later). Contract and registry slot only; no extractor yet.
5. **CatalogStore**: the queries the GUI and CLI call, as a trait the SQLite catalogue
   implements, so pages can be tested against an in-memory store. Keep it to what is used.
6. **Platform** (not a plugin, one implementation per system, chosen at build time):
   `free_bytes`, `change_marker(file)` (NTFS journal number on Windows, `ctime` on macOS and
   Linux, see TODO 1.9), `config_dir`, `delete` (read-only files), `open_in_viewer`, and the raw
   stream access the stream plugins use. The rest of the code never asks which system it is on.

**Every plugin also describes its settings**: `fn settings(&self) -> Vec<SettingSpec>` (key,
label, help, kind: on/off, choice, number with range, folder list; default; which systems it
applies to). Values are kept in the settings file under `plugins.<id>`. The Settings window
draws a section per module from these specs, so a new plugin brings its own options without
touching the window. Each archive's manifest records which plugins (and versions) and which
non-default options made it, so an archive says how it was made wherever it is opened.

**Not pluggable:** hashing (BLAKE3 + SHA-256), the manifest format, the delete policy, exclusion
rules, constants.

The **registry** (`Plugins`) is built in one place, `Plugins::default_for_this_system()`, and
handed to whatever needs it (with `Settings` or a small `Context`). Plugins never look each
other up.

## Phases

Each phase is one or more commits; each ends green (ground rule 2) and with a short entry in
`docs/refactor-log.md`: what changed, test counts, anything deferred.

| # | Phase | Done when |
|---|---|---|
| 0 | **Safety net** (TODO 0.5): `docs/behaviour.md`, one line per rule; a test for each rule missing one; an end-to-end test (archive → duplicates → excluded → clean up check, restart, delete → rebuild → compare); a **fixture** of archives made now (plain, set-aside inside, discarded, stored once, superseded) kept under `crates/atticarc-core/tests/fixtures/`, with a test that opens, verifies, checks, scrubs and rebuilds them | rules listed, all tests green, fixture checked in |
| 1 | **Tooling and three-system CI** (TODO 1.1, 1.9): `rustfmt.toml`, one `cargo fmt`, clippy clean; `docs/ci/check.yml` running fmt, clippy and test on windows-latest, macos-latest and ubuntu-latest | core and CLI tests green on all three in CI (GUI built on all three) |
| 2 | **Split the big files** (TODO 1.4): `archive.rs` and `catalog.rs` into the modules above, as pure moves (code moved, `pub(crate)` added, nothing rewritten) | diff is moves only; tests unchanged and green |
| 3 | **Platform layer**: the Windows-only code (journal, free space, delete rules, config folder, Explorer/Notepad calls) behind `platform`, with macOS and Linux implementations; paths kept portable | the whole test suite passes on all three, journal-dependent tests replaced by `change_marker` tests that run everywhere |
| 4 | **Constants and duplication** (TODO 1.2, 1.3): `limits.rs`; `Loader<T>` and one spinner for the GUI pages; one plural helper; "busy, repaint me" as a trait | three pages on `Loader<T>`; no magic numbers left in core |
| 5 | **Progress contract**: sinks and views; every producer emits events | GUI and CLI show exactly what they did before (compare the detailed log of the same folder before/after) |
| 6 | **ArchiveBackend**: contract, sevenz backend, contract tests; every write, verify, index and extract goes through it | no `sevenz_turbo::` outside `backend/sevenz/`; contract tests green on all three |
| 7 | **Registry and module settings**: `Plugins`, `SettingSpec`, settings under `plugins.<id>`, the Settings window drawing a section per module, plugins and options recorded in the manifest | today's options (level, threads, store mode, stored once, detailed log) come from their modules; old settings files load |
| 8 | **FileStreams and tags**: contract, the three stream plugins, the `tags` decoder, manifest field, restore on extract, streams in the check before a delete, Catalogue shows tags as coloured dots | the **interchange test** (below) passes: streams made on each system come back on the systems that can hold them |
| 9 | **MetadataExtractor, CatalogStore**: contracts and registry slots; pages tested against an in-memory store | no SQL outside `catalog/` |
| 10 | **Fluent API where touched** (TODO 1.5): `Archive::folder(src).to(out).run(&sink)`, `ArchiveId`, `Bytes` | callers read like the UI; no behaviour change |
| 11 | **Release builds for three systems**: `docs/ci/release.yml` building and attaching all three on a tag (Windows zip, macOS `.app` in a signed, notarised `.dmg` as Plugin² does, Linux tar.gz); `atticarc-cli bench` (below) | a tag produces three working downloads |
| 12 | **Docs** (TODO 1.7): `DESIGN.md`, `docs/architecture.md` (with a diagram), `docs/formats.md` (archive layout, manifest with streams and plugin records, schema, `.atticarcdep`: what 1.0 freezes), `docs/platforms.md` (what each system does underneath, and which streams it can hold), rustdoc on public items | `cargo doc` clean; TODO 1.x items ticked |

### Stop after phase 3

Phase 3 is the first point where the same code passes the same tests on all three systems.
**Stop there**, before any plugin work, and hand over:

- `docs/refactor-log.md` up to date: phases 0 to 3, test counts per system, anything deferred.
- The CI run (fmt, clippy, tests on windows-latest, macos-latest, ubuntu-latest) green, and the
  workflow files in `docs/ci/` named for moving to `.github/workflows/`.
- A first `atticarc-cli bench`-style timing on each system if it is cheap to get; otherwise
  note that it comes in phase 11.
- A short list of what phase 4 onwards will touch, and any change to this plan the first four
  phases showed is needed.

Danny then builds and tries it on WinX (and the M1), commits or asks for fixes, and starts the
second run with *"Read docs/refactor-brief.md and carry on from phase 4."*

## Checking the work

- Every step: fmt, clippy, `cargo test --workspace` on WinX; from phase 1, on all three
  systems in CI as well.
- **Interchange test** (CI, from phase 3, with streams from phase 8): each system makes the same
  generated folder tree into an archive (with tags and extended attributes on the systems that
  have them), uploads it, and each system then lists, verifies, checks against its own copy of
  the tree, scrubs, extracts and rebuilds all three. Streams must come back where the system can
  hold them and be listed where it can't.
- **Speed parity**: `atticarc-cli bench` archives, verifies and checks a generated tree (a few
  GB, mixed compressible and incompressible files) with fixed settings and prints MB/s per stage.
  Run it on WinX, the M1 MacBook and Linux (CI and a Linux VM on Proxmox), and keep the results in
  `docs/benchmarks.md`. Any stage more than 25% slower on one system than its hardware explains
  is a bug to look into; say which in the refactor log.
- Phases 2, 5 and the end: `atticarc-cli soak --work D:\Dev\WIP\AtticArc\target\soak --minutes 10`.
- Phase 4 and the end: the same test folder archived before and after with **Detailed: every
  file** on; the logs must match apart from times.
- The end, by hand on WinX (Danny): New job → archive two folders, one with an exclusion → ↻
  Archive again → Duplicates → Excluded → Clean up check and delete one → Catalogue search →
  Settings colour and dark mode → restart and see ticks and checks kept.
- A separate review pass at the end (an agent that didn't write the code) compares
  `docs/behaviour.md` with the final code.

## Known traps

- **The GUI only builds on Windows here.** A Linux container can build and test core and CLI
  with vendored crates (`--offline --ignore-rust-version`, the GUI left out of the workspace
  members) but not egui; every GUI change needs a WinX build, or the three-system CI from
  phase 1.
- **Extended attributes and NTFS streams can't all be tested on one machine.** The interchange
  test in CI is the proof; on WinX, tags written by a Mac over SMB are the real-world check
  (TODO 2, "check on WinX first").
- **Paths:** a catalogue made on Windows holds `D:\...` paths; opened on a Mac it must still
  list, search and extract, just not find the source folder (shown as "not on this system").
- **Windows locks a running exe**: close AtticARC before `cargo build`, or the link step fails
  and the old exe stays.
- **The archive-time check and journal snapshots only run on NTFS**; tests that need them skip
  elsewhere (`journal_here`). Run the full suite on WinX.
- Many files have **CRLF** line endings; keep each file's own.
- `D:\7ztest` and `W:\7ztest` are protected test folders with real data: read only.
- **Superseded archives** and `.atticarcdep` originals resolve through `archive_aliases`; moving
  code around must keep that lookup.

## Out of scope

AtticARC's own colour labels set by hand in the Catalogue (TODO 2; the tags read from files are
in scope), a Rust compressor (the backend contract makes it possible later), the versions folder
(TODO 2), UI changes beyond what moving code and the per-module settings need, and the rest of
"Features still open".

## Before starting (for Danny)

1. Build and try the uncommitted work (Settings window, detailed log, Archive again), then commit
   it on `fast-cleanup`.
2. Merge `fast-cleanup` into `main` (or say to branch from `fast-cleanup`).
3. Start the run with: *"Read docs/refactor-brief.md and carry it out, phase by phase."* It
   stops after phase 3 (see "Stop after phase 3"); the second run carries on from phase 4.
