# A Linux test machine on WinX (Hyper-V)

For trying AtticARC (and Plugin²) on a real Ubuntu desktop before a merge or a release, as
the three builds must stay the same ([refactor-brief.md](refactor-brief.md), "The outcome").
CI builds and tests on Linux too, but only a desktop shows whether the GUI works there.

## Once: make the VM

In PowerShell opened with **Run as administrator**, in `D:\Dev\WIP\AtticArc`:

```
powershell -ExecutionPolicy Bypass -File scripts\new-test-vm.ps1
```

It needs Hyper-V on (Windows Pro; the script says how if it isn't). It downloads the newest
Ubuntu 24.04 Desktop image to `D:\Dev\VMs\iso` (about 6 GB, checked against Ubuntu's
checksums), makes the VM *AtticARC Linux test* (4 processors, 8 GB of memory, a 60 GB disk at
`D:\Dev\VMs\vms`, Secure Boot set for Linux, the Default Switch) and opens its window. Install
Ubuntu with the default choices, then take a checkpoint called *Fresh Ubuntu* (Hyper-V Manager →
right-click the VM → Checkpoint), so a clean machine is always one click away.

Other folder or size: `-Folder E:\VMs`, `-MemoryBytes 12GB`, `-Processors 6`.

## Each time: build and test

In the VM, open Terminal:

```
curl -fsSL https://raw.githubusercontent.com/pinnet/AtticArc/main/scripts/linux-test.sh | bash
```

That installs the build tools and GUI libraries (asks for your password once), Rust, clones
AtticARC into `~/dev/AtticArc` (or pulls it if it's there), runs every test and makes a release
build. A branch: `... | bash -s refactor-plugins`. Plugin² too: `... | bash -s main --plugin2`.

Then open the GUI: `~/dev/AtticArc/target/release/atticarc`. Hyper-V gives the VM no real
graphics card: if the window is blank or doesn't open, use
`~/dev/AtticArc/target/release/atticarc --compatible-graphics` (OpenGL in software; slower to
draw, fine for testing).

## What to try on Linux

- Archive a small folder, then Clean up: it should already say "✓ passed … · change time" (on
  Linux the archive-time check uses status change times, where Windows uses the NTFS journal)
  and delete in one step.
- Settings: colour, background, dark mode; restart and see them kept (`~/.config/atticarc`).
- Folder dialogs open and choose a folder.
