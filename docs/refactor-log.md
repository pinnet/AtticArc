# Refactor log

What each phase of [refactor-brief.md](refactor-brief.md) did, what was checked and where, and
what was put off. Newest last.

**Where the checks ran.** The run worked in a Linux workspace with no network for builds: core
and CLI were built, tested, formatted and linted there (rustc 1.97.0, vendored crates). The GUI
(egui) can't be built there, and nothing was run on Windows or macOS by the run itself. So after
each phase "green" below means *core and CLI on Linux*; the Windows build and the GUI are checked
by Danny on WinX, and all three systems by CI once `docs/ci/check.yml` is moved to
`.github/workflows/`.

## Phase 0: safety net (commit `fae4bee`)

- `docs/behaviour.md`: every rule, with the test that holds it.
- `tests/workflow.rs` (3 tests): the whole workflow through the public API; links; no room.
- `tests/fixtures.rs` (3 tests) and `tests/fixtures/dest-2026-11`: a destination made at
  `1767fdd` (plain; set aside and discarded; stored once; archived again, with its superseded
  archive; the catalogue). It must keep opening, verifying, checking, scrubbing, extracting and
  rebuilding, and the same folders archived now must give the same manifests.
- The fixture was made on Linux, so its catalogue holds Linux paths and no archive-time checks
  (those need NTFS). Worth adding a second fixture made on WinX (`cargo test -p atticarc-core
  --test fixtures -- --ignored make_the_fixture`, after changing the folder name in
  `common::fixture_dir`) so NTFS-made catalogues are covered too. **Deferred to Danny.**
- Tests: 60 unit + 6 integration + 1 soak, all passing (Linux).

## Phase 1: tooling and three-system CI

- `rustfmt.toml` (`max_width = 140`, `use_small_heuristics = "Max"`: the closest to how the code
  was written) and one format pass over every crate, GUI included (rustfmt needs no build).
- Clippy clean with `-D warnings` for core and CLI: three small fixes (two nested `if`s joined,
  one type alias), no change in behaviour.
- `docs/ci/check.yml`: fmt, clippy, tests and a release build on windows-latest, macos-latest
  and ubuntu-latest. **Needs moving to `.github/workflows/check.yml`.**
- **Deferred:** clippy on the GUI. It couldn't be run here, so CI shows its warnings without
  failing; once they are cleared on WinX, change that step to `-- -D warnings`.
- **Not yet known:** whether the GUI builds on macOS and Linux as it stands. The first CI run
  will say; phase 3 is where it is made to.
- Tests: unchanged, all passing (Linux).

## Phase 2: the big files split

- `archive.rs` (3,200 lines) is now `archive/`: `manifest`, `progress`, `walk`, `write`,
  `again`, `dedupe`, `verify`, `extract`, `identity`, `rewrite`, `check`, `util`, with the tests
  in `archive/tests.rs`. `archive/mod.rs` keeps the settings and re-exports everything, so every
  `archive::…` path the CLI, GUI and tests use is unchanged.
- `catalog.rs` is now `catalog/`: `schema` (where the catalogue is, its tables, added columns,
  `connect`), `records` (what it holds and hands back) and `mod.rs` (the queries).
- Moves only: a script compared every line before and after; the only differences are
  visibility (`pub(super)` where modules now share a private item), each module's own `use`
  lines, and rustfmt re-wrapping two lines. The same 52 public items in `archive`, and the same
  in `catalog`.
- Tests: unchanged, all passing (Linux). Clippy clean (core, CLI).
- Soak: 5 minutes on the release CLI built from this phase (Linux, 0.3 GB of generated files);
  result recorded under "Soak runs" below.

## Phase 3: the platform layer

- New `crates/atticarc-core/src/platform/`: `mod.rs` (the questions), `windows.rs`, `macos.rs`,
  `linux.rs`, with `unix.rs` shared by the last two. It answers: free space, change marker,
  settings folder, hiding, setting times, the system's name, whether paths ignore case.
- Moved behind it, unchanged on Windows: free space (`disk.rs`, and the soak test's own copy),
  hiding `.atticarc` (`catalog/schema.rs`), setting times on extracted files
  (`archive/util.rs`), the settings folder (`settings.rs`), lower-casing path keys
  (`catalog/records.rs`). `journal.rs` (the NTFS reader) is untouched; Windows' change marker
  calls it.
- **One widening, on purpose:** folder snapshots on macOS and Linux now use status change times
  (`ctime:` snapshots), so the check saved at archive time happens there too, as on NTFS. On
  Windows snapshots are byte for byte what they were (`usn:` / `meta:`). The words beside a
  check come from the platform ("change journal" on Windows, "change time" elsewhere): one line
  each in the CLI and in the GUI's Clean up page.
- New tests: 3 in `platform`, 2 in `workflow` (a same-size edit with its time put back is
  caught; a folder is ready to delete once archived), 1 in `fixtures` (a destination carried
  elsewhere is relinked by what it holds). The fixture comparison no longer depends on whether a
  drive records created times.
- `docs/platforms.md`: what each system does underneath, and what isn't the same yet.
- Tests: 63 unit + 9 integration + 1 soak, all passing (Linux), four runs in a row. Clippy clean
  (core, CLI). `cargo fmt --check` clean.
- **Not checked by the run:** `platform/windows.rs` and `platform/macos.rs` have never been
  compiled (no Windows or macOS toolchain in the workspace). The Windows file is the old code
  moved, function by function; the macOS file is eight lines over `unix.rs`. WinX and CI will say.

## Workflows on GitHub (14 Nov 2026, evening)

- `docs/ci/check.yml` moved to `.github/workflows/check.yml` (as the handover asked).
- `.github/workflows/mac-signed.yml`, `packaging/macos/Info.plist`,
  `packaging/macos/set-secrets.sh` and `docs/mac-signing.md`: the signed, notarised
  `AtticARC.app` in a `.dmg`, the same workflow as Plugin²'s with AtticARC's names. This is the
  macOS part of phase 11, done early; the Windows and Linux release builds are still phase 11.
- Branch pushed and a draft pull request opened, so `check.yml` runs on all three systems.

## Soak runs

| Phase | Where | Run | Result |
|---|---|---|---|
| 2 | Linux, release CLI | 5 minutes, 0.3 GB generated, 2 rounds | passed (too few rounds to judge memory growth) |
| 3 | Linux, release CLI | 7 rounds, 0.1 GB generated | passed: memory flat (167 MB after warm-up, 167 MB at the end), handles 5, threads 2 |

## Stop after phase 3: handover

The first run stops here, as the brief says. Branch `refactor-plugins`, four commits on top of
`main` (`1767fdd`): phases 0, 1, 2, 3.

**For Danny, on WinX (and the M1):**

1. `cargo test --workspace`, then `cargo build --release --workspace` and a look round the GUI:
   nothing should look or behave differently. Clean up should still say "change journal" beside
   a check made on NTFS.
2. `cargo fmt --all --check` and `cargo clippy -p atticarc-core -p atticarc-cli --all-targets --
   -D warnings` should both be clean. `cargo clippy -p atticarc-gui` will list the GUI's
   warnings (never run before): clear them, or paste them back.
3. Move `docs/ci/check.yml` to `.github/workflows/check.yml` and push the branch: the first run
   shows whether all three systems build and pass. Expect the GUI on macOS and Linux to be the
   part that needs attention, if anything does.
4. Optional: make a second fixture on WinX (see phase 0), so a catalogue made on NTFS, with
   archive-time checks in it, is covered for good.

**No speed readings yet.** `atticarc-cli bench` comes in phase 11; the soak timings above are
from a small Linux container and say nothing about the three systems against each other.

**What phase 4 onwards will touch:**

- Phase 4 (constants, `Loader<T>`): `limits.rs` in core; the GUI's Catalogue, Duplicates and
  Clean up pages. GUI work, so it needs a machine that builds the GUI at every step.
- Phase 5 (progress contract): `archive/progress.rs` and every producer; the GUI's Progress
  page and the CLI's tickers.
- Phase 6 (archive backend): `archive/write.rs`, `verify.rs`, `extract.rs`, `rewrite.rs`,
  `manifest.rs` are where `sevenz_turbo::` is used today.
- Phases 7 to 9 (registry and settings, streams and tags, metadata and store): new modules, the
  manifest, the Settings window.

**Changes to the plan these phases showed are needed:**

- The run can't build the GUI or anything for Windows or macOS. From phase 4 on, most steps
  touch the GUI. Either run the next phases where the GUI builds (Claude Code on WinX itself), or
  get the three-system CI green first and let it be the check after each push. The second is
  the better use of the CI this run wrote.
- Ground rule 2 ("green on WinX at every step") was met only as "green for core and CLI on
  Linux". Each phase is one commit, so a failure on WinX can be traced to its phase.
- `df` was kept for free space on macOS and Linux instead of `statvfs`, to add no dependency.
