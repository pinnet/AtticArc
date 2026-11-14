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
