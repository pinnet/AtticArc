# What each system does underneath

AtticARC is meant to be the same program on Windows, macOS and Linux
([refactor-brief.md](refactor-brief.md), "The outcome"). Where the systems differ, the difference
lives in one module, `crates/atticarc-core/src/platform/`, and the rest of the code calls it
without asking which system it is on.

| What | Windows | macOS | Linux |
|---|---|---|---|
| Free space on the destination | `GetDiskFreeSpaceExW` | `df -Pk` | `df -Pk` |
| Change marker (moves with any write, even if the modified time is put back) | NTFS / ReFS change journal number, read per file; none on exFAT, FAT32 and network shares | status change time (`ctime`), nanoseconds | status change time (`ctime`), nanoseconds |
| Folder snapshot prefix | `usn:` (or `meta:` without a journal) | `ctime:` | `ctime:` |
| Settings and log folder | `%APPDATA%\atticarc` | `~/Library/Application Support/atticarc` | `$XDG_CONFIG_HOME/atticarc`, else `~/.config/atticarc` |
| Hiding `.atticarc` | Hidden attribute (`attrib +h`) | the leading dot | the leading dot |
| Times put back on extracted files | modified and created | modified only | modified only |
| Source folders looked up | ignoring case | as written | as written |

## What this means in use

- **Checks at archive time.** A folder's check is saved as it is archived wherever there are
  change markers: NTFS on Windows, and any drive on macOS and Linux. On exFAT or a network share
  on Windows there are none, so those folders need the full check before a delete, as before.
- **A snapshot belongs to the system that took it.** `usn:` and `ctime:` snapshots never match
  each other, so a folder checked on one system and looked at from another must be checked again
  there. Nothing is deleted on the strength of another system's snapshot.
- **A destination can be carried between systems.** The archives are the record. The catalogue
  names archives where they were; opened somewhere else, they are found again by what they hold
  (`relink_missing`), not by path. Source folders named by another system's paths show as not
  being where they were archived from, and are never candidates for clean up there.
- **Status change times** move with every write and every change to a file's details, and
  ordinary tools can't set them back. They tick with the system clock, so two changes within the
  same tick (a few milliseconds) can share one; the snapshot also holds each file's size and
  modified time.

## Not the same yet

Kept here until each is closed (the brief's phases in brackets):

- Free space uses `df` on macOS and Linux rather than `statvfs`: no new dependency was added for
  it. Same answer, one process started per question.
- Created times aren't put back on macOS (it can hold them; Linux can't).
- macOS's default drive ignores case in names, but source folders are looked up as written, as
  on Linux.
- The default protected folders (`C:\7ztest`, `D:\7ztest`, `W:\7ztest`) are Windows paths on
  every system; harmless elsewhere, but they should become per-system defaults (phase 7).
- The CLI's memory watcher (`atticarc-cli watch`, and the soak test's memory columns) reads
  process memory its own way per system; it is a development tool and stays outside `platform`.
- File streams (extended attributes, NTFS streams) and tags: phase 8.
- The GUI has been built only on Windows. It has almost nothing system-specific in it (the
  DirectX choice, and which program opens the log file), so it should build on the other two;
  the first CI run will show.
