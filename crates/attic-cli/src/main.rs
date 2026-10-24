use std::path::PathBuf;
use std::process::ExitCode;

use attic_core::scan;

const USAGE: &str = "\
Attic – archive, catalogue and de-duplicate folders

Usage:
  attic scan <folder> [--top N]   List files and show possible duplicates (by size).
                                  Reads no file contents, so it is fast.
  attic help                      Show this message.";

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
