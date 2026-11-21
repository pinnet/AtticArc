//! `atticarc-cli compare <source> <copy>`: is the copy complete? See `atticarc_core::compare`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use atticarc_core::compare::{self, Comparison, Options, Problem, Side};

/// Lines shown per list unless `--all`.
const SHOWN: usize = 20;

pub struct CompareArgs {
    source: PathBuf,
    copy: PathBuf,
    options: Options,
    all: bool,
}

pub fn parse(args: &[String]) -> Result<CompareArgs, String> {
    let mut folders = Vec::new();
    let mut options = Options::default();
    let mut all = false;
    for a in args {
        match a.as_str() {
            "--quick" => options.quick = true,
            "--strict" => options.strict = true,
            "--all" => all = true,
            s if s.starts_with("--") => return Err(format!("compare doesn't know '{s}'")),
            _ => folders.push(PathBuf::from(a)),
        }
    }
    let [source, copy]: [PathBuf; 2] = folders.try_into().map_err(|_| "compare needs two folders: <source> <copy>".to_string())?;
    Ok(CompareArgs { source, copy, options, all })
}

/// Exit code 0: the copy is complete. 1: it isn't. 2: the folders couldn't be compared.
pub fn run(a: CompareArgs) -> ExitCode {
    let how = if a.options.quick { "sizes only (--quick)" } else { "every file hashed on both sides" };
    println!("Comparing {}\n     with {}\n  {how}\n", a.source.display(), a.copy.display());
    let started = Instant::now();
    let mut last = Instant::now();
    let mut shown = false;
    let result = compare::compare(&a.source, &a.copy, a.options, |read| {
        if last.elapsed() >= Duration::from_secs(1) {
            last = Instant::now();
            shown = true;
            eprint!("\r  {} read…   ", super::human(read));
        }
        true
    });
    if shown {
        eprint!("\r{:30}\r", "");
    }
    match result {
        Ok(c) => {
            report(&c, &a);
            println!("\n{}", verdict(&c, &a, started.elapsed()));
            if c.is_complete(a.options.strict) { ExitCode::SUCCESS } else { ExitCode::from(1) }
        }
        Err(e) => {
            eprintln!("Couldn't compare: {e}");
            ExitCode::from(2)
        }
    }
}

fn report(c: &Comparison, a: &CompareArgs) {
    let show = |title: &str, lines: Vec<String>| {
        if lines.is_empty() {
            return;
        }
        println!("{title} ({}):", lines.len());
        let n = if a.all { lines.len() } else { SHOWN };
        for l in lines.iter().take(n) {
            println!("  {l}");
        }
        if lines.len() > n {
            println!("  … and {} more (--all lists every one)", lines.len() - n);
        }
        println!();
    };
    let p = |path: &Path| path.display().to_string();
    show(
        "NOT COPIED, or not the same",
        c.problems
            .iter()
            .map(|f| {
                let why = match &f.problem {
                    Problem::Missing => "missing from the copy".to_string(),
                    Problem::NotAFile => "the copy has a folder there, not a file".to_string(),
                    Problem::SizeDiffers { source, copy } => format!("size differs: {source} bytes, copy {copy}"),
                    Problem::ContentDiffers => "contents differ (same size)".to_string(),
                    Problem::Unreadable { side: Side::Source, error } => format!("couldn't read the source: {error}"),
                    Problem::Unreadable { side: Side::Copy, error } => format!("couldn't read the copy: {error}"),
                };
                format!("{}  — {why}", p(&f.path))
            })
            .collect(),
    );
    let (extra_title, empty_title) = if a.options.strict {
        ("ONLY IN THE COPY (fails --strict)", "EMPTY FOLDERS NOT IN THE COPY (fail --strict)")
    } else {
        ("Only in the copy (fine for rclone copy)", "Empty folders not in the copy (rclone skips these unless --create-empty-src-dirs)")
    };
    show(extra_title, c.extra.iter().map(|x| p(x)).collect());
    show("Modified times differ (contents match)", c.times_differ.iter().map(|x| p(x)).collect());
    show(empty_title, c.empty_folders_missing.iter().map(|x| p(x)).collect());
    show("Links in the source, not compared", c.links_skipped.iter().map(|x| p(x)).collect());
}

fn verdict(c: &Comparison, a: &CompareArgs, took: Duration) -> String {
    let matched = format!("{}, {}", count(c.files_matched, "file"), super::human(c.bytes_matched));
    let proof = if a.options.quick { "match by size (contents not read)" } else { "match byte for byte" };
    let time = match took.as_secs() {
        0 => "under a second".to_string(),
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min {} s", s / 60, s % 60),
        s => format!("{} h {} min", s / 3600, s % 3600 / 60),
    };
    if c.stopped {
        format!("STOPPED part way: not checked. {matched} matched so far.")
    } else if c.is_complete(a.options.strict) {
        format!("COMPLETE: {matched} {proof}. ({time})")
    } else if !c.problems.is_empty() {
        format!("INCOMPLETE: {}; {matched} {proof}. ({time})", count(c.problems.len() as u64, "problem"))
    } else {
        let (extra, empty) = (c.extra.len(), c.empty_folders_missing.len());
        format!("NOT IDENTICAL (--strict): {extra} only in the copy, {empty} empty folders missing. {matched} {proof}. ({time})")
    }
}

fn count(n: u64, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}
