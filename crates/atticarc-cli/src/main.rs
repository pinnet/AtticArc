mod mem;
mod soak;

/// mimalloc hands freed memory back to the system, so memory stays flat
/// over hours of archiving; the system allocator kept growing in the soak
/// test as many threads freed and reused large buffers.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use atticarc_core::archive::{self, Progress};
use atticarc_core::catalog::Catalog;
use atticarc_core::{scan, settings};

const USAGE: &str = "\
AtticARC – archive, catalogue and de-duplicate folders

Usage:
  atticarc-cli scan <folder> [--top N]   List files and show possible duplicates (by size).
                                  Reads no file contents, so it is fast.
  atticarc-cli archive <folder>...       Archive each folder to its own .7z and verify it,
                                  using the settings saved by the GUI.
        [--out DIR] [--level 5|9]  Override the output folder or level.
  atticarc-cli search <words> [--out DIR] Search the catalogue in the output folder.
  atticarc-cli rebuild [--out DIR]       Add the archives in the output folder that the
                                  catalogue is missing, from their manifests.
  atticarc-cli soak --work DIR           Robustness test: archive, check, put back, de-duplicate
        [--hours H | --minutes M]  and rebuild in a loop (default 60 minutes), logging
        [--rounds N] [--gb G]      memory, handles and threads to a CSV in DIR. Uses G GB
        [--source FOLDER]          of generated files (default 1), or archives FOLDER
        [--every S]                (only read) each round. DIR must be empty or a soak's.
                                  Fails if a round fails or memory keeps climbing.
  atticarc-cli watch [atticarc|PID]      Log the window's memory, handles and threads every
        [--every S] [--csv FILE]   S seconds (default 10) until it exits.
  atticarc-cli help                      Show this message.

Exit code 0 means every folder was archived and verified.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("scan") => match parse_scan(&args[1..]) {
            Ok((folder, top)) => run_scan(folder, top),
            Err(msg) => {
                eprintln!("{msg}\n\n{USAGE}");
                ExitCode::from(2)
            }
        },
        Some("archive") => match parse_archive(&args[1..]) {
            Ok(a) => run_archive(a),
            Err(msg) => {
                eprintln!("{msg}\n\n{USAGE}");
                ExitCode::from(2)
            }
        },
        Some("search") => match parse_search(&args[1..]) {
            Ok((words, out)) => run_search(&words, out),
            Err(msg) => {
                eprintln!("{msg}\n\n{USAGE}");
                ExitCode::from(2)
            }
        },
        Some("rebuild") => match args.get(1).map(String::as_str) {
            None => run_rebuild(None),
            Some("--out") if args.len() == 3 => run_rebuild(Some(PathBuf::from(&args[2]))),
            _ => {
                eprintln!("rebuild takes only --out <folder>\n\n{USAGE}");
                ExitCode::from(2)
            }
        },
        Some("soak") => match soak::parse_soak(&args[1..]) {
            Ok(a) => soak::run_soak(a),
            Err(msg) => {
                eprintln!("{msg}\n\n{USAGE}");
                ExitCode::from(2)
            }
        },
        Some("watch") => soak::run_watch(&args[1..]),
        Some("help" | "-h" | "--help") | None => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("Unknown command '{other}'.\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn parse_scan(args: &[String]) -> Result<(PathBuf, usize), String> {
    let mut folder = None;
    let mut top = 10;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--top" {
            let n = it.next().ok_or("--top needs a number")?;
            top = n.parse().map_err(|_| format!("'{n}' is not a number"))?;
        } else if folder.is_none() {
            folder = Some(PathBuf::from(a));
        } else {
            return Err(format!("Unexpected argument '{a}'"));
        }
    }
    let folder = folder.ok_or("scan needs a folder")?;
    if !folder.is_dir() {
        return Err(format!("'{}' is not a folder", folder.display()));
    }
    Ok((folder, top))
}

fn run_scan(folder: PathBuf, top: usize) -> ExitCode {
    let started = std::time::Instant::now();
    let r = scan::scan(&folder);
    let groups = r.size_collisions();
    let candidate_files: usize = groups.iter().map(|g| g.files.len()).sum();
    let candidate_bytes: u64 = groups.iter().map(|g| g.size * g.files.len() as u64).sum();
    let max_waste: u64 = groups.iter().map(|g| g.potential_waste()).sum();

    println!("Scanned {} in {:.1?}", folder.display(), started.elapsed());
    println!("  Files:            {}", r.files.len());
    println!("  Total size:       {}", human(r.total_bytes()));
    println!("  Unreadable items: {}", r.errors.len());
    println!();
    println!("Possible duplicates (same size, not yet hashed):");
    println!("  {} files in {} size groups, {} to hash", candidate_files, groups.len(), human(candidate_bytes));
    println!("  Up to {} could be freed if all turn out identical", human(max_waste));

    if !groups.is_empty() && top > 0 {
        println!();
        println!("Largest groups by potential saving:");
        for g in groups.iter().take(top) {
            println!("  {} × {} (up to {} saved)", g.files.len(), human(g.size), human(g.potential_waste()));
            for f in g.files.iter().take(3) {
                println!("      {}", f.rel_path.display());
            }
            if g.files.len() > 3 {
                println!("      … and {} more", g.files.len() - 3);
            }
        }
    }
    for e in r.errors.iter().take(5) {
        eprintln!("warning: {}: {}", e.path.display(), e.error);
    }
    ExitCode::SUCCESS
}

struct ArchiveArgs {
    folders: Vec<PathBuf>,
    out: Option<PathBuf>,
    level: Option<u32>,
}

fn parse_archive(args: &[String]) -> Result<ArchiveArgs, String> {
    let mut a = ArchiveArgs { folders: Vec::new(), out: None, level: None };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => a.out = Some(PathBuf::from(it.next().ok_or("--out needs a folder")?)),
            "--level" => {
                let n = it.next().ok_or("--level needs 5 or 9")?;
                match n.as_str() {
                    "5" => a.level = Some(5),
                    "9" => a.level = Some(9),
                    _ => return Err(format!("level must be 5 or 9, not '{n}'")),
                }
            }
            f => {
                let p = PathBuf::from(f);
                if !p.is_dir() {
                    return Err(format!("'{f}' is not a folder"));
                }
                a.folders.push(p);
            }
        }
    }
    if a.folders.is_empty() {
        return Err("archive needs at least one folder".into());
    }
    Ok(a)
}

fn run_archive(a: ArchiveArgs) -> ExitCode {
    let saved = settings::load();
    let mut failed = 0;
    for folder in &a.folders {
        let source_dir = std::fs::canonicalize(folder)
            .ok()
            .and_then(|p| p.parent().map(settings::strip_verbatim))
            .unwrap_or_default();
        let mut s = saved.settings_for(&source_dir);
        if let Some(out) = &a.out {
            s.out_dir = out.clone();
        }
        if let Some(level) = a.level {
            s.level = level;
        }
        println!("Archiving {} to {} ({}, {} threads)", folder.display(), s.out_dir.display(), s.engine_label(), s.threads);
        let started = std::time::Instant::now();
        match archive::archive_folder(folder, &s, &Arc::new(Progress::default())) {
            Ok(o) => {
                println!(
                    "  verified: {} files, {} → {} in {:.1?}",
                    o.record.file_count,
                    human(o.record.input_bytes),
                    human(o.record.archive_bytes),
                    started.elapsed()
                );
                if !o.skipped_links.is_empty() {
                    println!("  links not followed: {}", o.skipped_links.join(", "));
                }
                let names = |v: &[atticarc_core::catalog::EntryRecord]| {
                    v.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>().join(", ")
                };
                if !o.excluded.set_aside.is_empty() {
                    println!("  set aside inside the archive ({}): {}", archive::EXCLUDED_ENTRY, names(&o.excluded.set_aside));
                }
                if !o.excluded.discarded.is_empty() {
                    println!("  discarded: {}", names(&o.excluded.discarded));
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("  failed: {e:#}");
            }
        }
    }
    if failed == 0 { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

fn parse_search(args: &[String]) -> Result<(String, Option<PathBuf>), String> {
    let mut words = Vec::new();
    let mut out = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--out" {
            out = Some(PathBuf::from(it.next().ok_or("--out needs a folder")?));
        } else {
            words.push(a.as_str());
        }
    }
    if words.is_empty() {
        return Err("search needs some words".into());
    }
    Ok((words.join(" "), out))
}

fn run_rebuild(out: Option<PathBuf>) -> ExitCode {
    let Some(out) = out.or(settings::load().out_dir) else {
        eprintln!("No output folder saved yet; pass --out <folder>.");
        return ExitCode::from(2);
    };
    match archive::rebuild_catalog(&out) {
        Ok(r) => {
            println!("{} added, {} already in the catalogue", r.added, r.already_there);
            for f in &r.failed {
                println!("  not added: {f}");
            }
            if r.failed.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) }
        }
        Err(e) => {
            eprintln!("Rebuild failed: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run_search(words: &str, out: Option<PathBuf>) -> ExitCode {
    let saved = settings::load();
    let Some(out) = out.or(saved.out_dir) else {
        eprintln!("No output folder saved yet; pass --out <folder>.");
        return ExitCode::from(2);
    };
    let found = Catalog::open_existing(&out).and_then(|c| c.search(words, None, 200));
    match found {
        Ok(found) => {
            for f in &found {
                println!("{:>10}  {} › {}", human(f.size), f.archive_name, f.rel_path);
            }
            println!("{} matches", found.len());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::from(1)
        }
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}
