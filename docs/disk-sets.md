# Disk sets: rolling archives out to drives, with optional parity

Design note, 21 Nov 2026. Not started; for after the plugin refactor (`docs/refactor-brief.md`),
built as plugins from the start. Nothing here changes the archive format.

## The idea in one paragraph

Cheap cold storage: take the finished archives in a destination and roll them out onto a set of
ordinary hard disks of a size the user picks (500 GB, 1 TB, 4 TB, or any size). Optionally add
parity disks so the set survives any *m* disks being lost or damaged. The user chooses both the
rollout and the parity, and each is a plugin that can be swapped without touching the other.
No disk-image format and no other backup product is involved: a disk in the set is a plain file
system with archives on it.

## What a disk set is

- **Data disks**: exFAT by default (Windows, macOS and Linux read and write it; no 4 GB file
  limit), NTFS as an option. Each holds whole archives (or volumes of a split one), copied as they
  are. Readable with nothing but 7-Zip.
- **Parity disks** (0 to *m*): recovery data for the data disks. Never needed while the data disks
  are healthy.
- **On every disk**: `ATTICARC-SET.json` with the set's id and label, this disk's number
  ("Disk 3 of 7", "Parity 1 of 2"), the archives on it with size and BLAKE3/MD5, and a copy of the
  whole set's map (which archive is on which disk). Any one disk tells you where everything is.
- **In the catalogue**: the same map, so a search says which disk to fetch.

## Rollout (plugin: `SetPlanner`)

Decides which archive goes on which disk. Settings:

- **Disk size**: presets (500 GB, 1 TB, 2 TB, 4 TB) or custom; plan from the real capacity
  (a "1 TB" disk is 10^12 bytes, about 931 GiB), keeping back a margin (default 3%) for file-system
  overhead and `ATTICARC-SET.json`.
- **Packing strategy** (swappable):
  - *First-fit decreasing* (default): largest archives first, each into the first disk with room.
    Fast, usually close to the fewest disks.
  - *Keep together*: archives from the same source folder or label kept on one disk where
    possible, so a restore needs fewer disks.
  - *In order*: archives in catalogue order (by name or date), filling disk after disk; easiest
    for a person to browse.
- **Archives bigger than a disk**: split into 7-Zip volumes (`.7z.001`, `.002`…) placed on
  consecutive disks; the map records each part.
- **Plan first, write after**: the plan (disk count, fill per disk, parity overhead) is shown
  before anything is written, and the user can change size, strategy or parity and re-plan.

## Writing (plugin: `SetWriter`)

Copies one disk at a time (the user is asked for the next disk), verifies each file after
copying, writes `ATTICARC-SET.json`, and records the disk in the catalogue. Reuses the existing
verify-once rule: record size and modified time; full decode only on request or by scrub.
Resumable: a set can be written over days.

## Parity (plugin: `ParityEngine`, swappable)

Erasure coding across the disks of a set: *k* data disks + *m* parity disks survive the loss or
damage of **any** *m* of them. Overhead is m / (k + m) (8 + 2 = 20%, survives two).

- **Layout**: each data disk is treated as one run of fixed-size blocks (default 1 MiB) in the
  order the plan places its files; block *n* of every data disk is a stripe, and each stripe gets
  *m* parity blocks (Reed–Solomon). A short disk is padded with zeros, which are not stored.
- **Per-block checksums** (BLAKE3) kept in `ATTICARC-SET.json` and the catalogue. Parity says
  something is wrong; the checksums say which block, so bit rot is repaired as well as a whole
  missing disk.
- **Each parity disk is at least as large as the largest data disk** (same-size disks in a set
  keep this simple).
- **One disk at a time**: Reed–Solomon is linear, so parity is built by reading the data disks one
  after another and adding each into running totals, and a rebuild reads the surviving disks one
  at a time with scratch space the size of the missing disk. No need for every drive plugged in
  at once.
- **Write once**: sets are not changed after writing, so there is no parity update cost. Adding
  archives later means a new set (or re-planning a set from scratch).

Engines the user can pick:

| Engine | Notes |
|---|---|
| **None** | Plain disks only. |
| **Native Reed–Solomon** (default when parity is on) | In `atticarc-core`, a Rust Reed–Solomon erasure crate with an MIT-compatible licence; one-disk-at-a-time build and rebuild; same on all three systems. |
| **PAR2** | Per-disk recovery files (e.g. 5–10%) for bit rot on a single disk; can be combined with the above. `par2cmdline` is GPL: run as a separate program, never linked. Slow at terabyte scale, so per-disk only. |
| **SnapRAID** | Mature file-level parity over ordinary file systems, up to six parity disks. GPL: run as a separate program. Expects disks attached together; AtticARC would write its config and drive it. |

## Interchangeable choices

The point of the plugins: rollout and parity are independent. Any `SetPlanner` strategy works with
any `ParityEngine`, the Settings window has a section for each (per the refactor's "settings per
module"), and a new strategy or engine is one module plus a registry entry. A set records which
planner and engine made it, so it can be checked and rebuilt later even if the defaults change.

## Commands (sketch)

- `atticarc-cli set plan --size 1TB --strategy ffd --parity rs:2` shows the plan.
- `atticarc-cli set write <plan>`, disk by disk.
- `atticarc-cli set verify <disk>`: checksums of one disk, or all disks in turn.
- `atticarc-cli set rebuild --missing 3`: rebuild disk 3 onto a new disk from the others.
- The GUI does the same from a "Disk sets" page.

## Tests to write first

- Packing: every archive placed once; no disk over its usable size; split archives reassemble.
- Parity: for every combination of up to *m* missing disks in small sets (e.g. 4 + 2 with tiny
  "disks" as folders), rebuild is byte-identical; a flipped byte is found by checksum and repaired.
- One-at-a-time build gives the same parity as all-at-once.
- A set written on one system verifies and rebuilds on the other two.

## Open questions

- Block size: 1 MiB default, or larger for fewer checksums on big disks? Benchmark like
  sevenz-bench.
- Should a set's map also go in a small file beside each archive in the destination, so the
  destination knows where its archives were rolled out to?
- Delete from the destination after a set is written and verified? (Probably only with parity, or
  with two sets.)
