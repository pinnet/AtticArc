//! `soak`: runs the archiving engine round after round for as long as asked,
//! sampling this process's memory, handles and threads, and fails if any
//! round fails or if they keep climbing once warmed up.
//!
//! `watch`: samples another process (the window, while it runs a real job)
//! the same way until it exits.
//!
//! Everything is written inside the work folder, which must be empty or one
//! a soak made before; a source folder given with --source is only read.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use atticarc_core::archive::{self, Progress};
use atticarc_core::catalog::Catalog;
use atticarc_core::exclude::{Action, Rule};
use atticarc_core::settings;

use crate::human;
use crate::mem::{self, Trend};

/// Marks a work folder as the soak's own, so it may empty it.
const MARKER: &str = "atticarc-soak.txt";

pub struct SoakArgs {
    pub work: PathBuf,
    pub source: Option<PathBuf>,
    pub minutes: Option<u64>,
    pub rounds: Option<u64>,
    pub gb: f64,
    pub every: u64,
}

pub fn parse_soak(args: &[String]) -> Result<SoakArgs, String> {
    let mut a = SoakArgs { work: PathBuf::new(), source: None, minutes: None, rounds: None, gb: 1.0, every: 10 };
    let mut work = None;
    let mut it = args.iter();
    let num = |v: Option<&String>, what: &str| -> Result<f64, String> {
        let v = v.ok_or(format!("{what} needs a number"))?;
        v.parse::<f64>().ok().filter(|n| *n > 0.0).ok_or(format!("'{v}' is not a number above 0"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--work" => work = Some(PathBuf::from(it.next().ok_or("--work needs a folder")?)),
            "--source" => a.source = Some(PathBuf::from(it.next().ok_or("--source needs a folder")?)),
            "--minutes" => a.minutes = Some(num(it.next(), "--minutes")? as u64),
            "--hours" => a.minutes = Some((num(it.next(), "--hours")? * 60.0) as u64),
            "--rounds" => a.rounds = Some(num(it.next(), "--rounds")? as u64),
            "--gb" => a.gb = num(it.next(), "--gb")?,
            "--every" => a.every = num(it.next(), "--every")? as u64,
            other => return Err(format!("Unexpected argument '{other}'")),
        }
    }
    a.work = work.ok_or("soak needs --work <folder>")?;
    if a.minutes.is_none() && a.rounds.is_none() {
        a.minutes = Some(60);
    }
    if let Some(s) = &a.source
        && !s.is_dir()
    {
        return Err(format!("'{}' is not a folder", s.display()));
    }
    Ok(a)
}

pub fn run_soak(a: SoakArgs) -> ExitCode {
    match soak(&a) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("Soak test stopped: {e:#}");
            ExitCode::from(1)
        }
    }
}

/// Samples taken in the background, with the peak so far.
struct Sampler {
    stop: Arc<AtomicBool>,
    peak_private: Arc<AtomicU64>,
    peak_resident: Arc<AtomicU64>,
    round: Arc<AtomicU64>,
    phase: Arc<Mutex<String>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    /// Writes a CSV line every `every` seconds: from this process, or from
    /// process `pid` while it runs.
    fn start(csv: &Path, every: u64, pid: Option<u32>, progress: Option<Arc<Progress>>) -> Result<Self> {
        let mut file = std::fs::File::create(csv).with_context(|| format!("can't create {}", csv.display()))?;
        writeln!(file, "time,elapsed_s,round,phase,resident_mb,private_mb,handles,threads,free_disk_gb")?;
        let s = Sampler {
            stop: Arc::new(AtomicBool::new(false)),
            peak_private: Arc::default(),
            peak_resident: Arc::default(),
            round: Arc::default(),
            phase: Arc::default(),
            thread: None,
        };
        let (stop, pp, pr, round, phase) =
            (s.stop.clone(), s.peak_private.clone(), s.peak_resident.clone(), s.round.clone(), s.phase.clone());
        let dir = csv.parent().map(Path::to_path_buf).unwrap_or_default();
        let started = Instant::now();
        let thread = std::thread::spawn(move || {
            let mut next = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                if Instant::now() >= next {
                    next += Duration::from_secs(every);
                    let sample = match pid {
                        Some(p) => mem::of(p),
                        None => mem::own(),
                    };
                    let Some(m) = sample else { break };
                    pp.fetch_max(m.private, Ordering::Relaxed);
                    pr.fetch_max(m.resident, Ordering::Relaxed);
                    let ph = match &progress {
                        Some(p) => p.phase.lock().unwrap().clone(),
                        None => phase.lock().unwrap().clone(),
                    };
                    let _ = writeln!(
                        file,
                        "{},{:.0},{},{},{:.1},{:.1},{},{},{:.1}",
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                        started.elapsed().as_secs_f64(),
                        round.load(Ordering::Relaxed),
                        ph.replace(',', ";"),
                        mb(m.resident),
                        mb(m.private),
                        m.handles,
                        m.threads,
                        free_gb(&dir),
                    );
                    let _ = file.flush();
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        Ok(Sampler { thread: Some(thread), ..s })
    }

    fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn mb(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

/// Free space on the drive holding `dir`, in GB, where it can be found.
fn free_gb(dir: &Path) -> f64 {
    free_bytes(dir).map_or(-1.0, |b| b as f64 / 1e9)
}

#[cfg(windows)]
fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    // SAFETY: a NUL-terminated path and out-pointers to locals.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut())
    };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
fn free_bytes(dir: &Path) -> Option<u64> {
    let out = std::process::Command::new("df").arg("-Pk").arg(dir).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().nth(1)?.split_whitespace().nth(3)?.parse::<u64>().ok().map(|k| k * 1024)
}

fn soak(a: &SoakArgs) -> Result<bool> {
    let saved = settings::load();
    let work = prepare_work(&a.work, &saved.protected)?;
    let data = work.join("data");
    let twin = work.join("twin");
    let out = work.join("out");

    let source = match &a.source {
        Some(s) => {
            let s = std::fs::canonicalize(s)?;
            if work.starts_with(&s) {
                bail!("the work folder is inside {}; choose one elsewhere", s.display());
            }
            println!("Source (read only): {}", s.display());
            s
        }
        None => {
            println!("Making {} of test files in {}…", human((a.gb * 1e9) as u64), data.display());
            make_data(&data, (a.gb * 1e9) as u64)?;
            make_twin(&data, &twin)?;
            data.clone()
        }
    };
    let synthetic = a.source.is_none();
    let s = archive::Settings {
        out_dir: out.clone(),
        level: saved.level,
        store_mode: saved.store_mode,
        threads: saved.threads.count(),
        protected: saved.protected.clone(),
        exclusions: if synthetic {
            vec![
                Rule { pattern: "*.tmp".into(), action: Action::SetAside, ..Rule::default() },
                Rule { pattern: "*.lnk".into(), action: Action::Discard, ..Rule::default() },
            ]
        } else {
            Vec::new()
        },
    };
    let deadline = a.minutes.map(|m| Instant::now() + Duration::from_secs(m * 60));
    let csv = work.join(format!("soak {}.csv", chrono::Local::now().format("%Y-%m-%d %H%M%S")));
    let progress = Arc::new(Progress::default());
    let sampler = Sampler::start(&csv, a.every, None, Some(progress.clone()))?;
    println!(
        "Soak test: {}, {} threads, level {}. Samples every {}s go to {}",
        match (a.rounds, a.minutes) {
            (Some(r), Some(m)) => format!("{r} rounds or {m} minutes"),
            (Some(r), None) => format!("{r} rounds"),
            (None, Some(m)) => format!("{m} minutes"),
            _ => unreachable!(),
        },
        s.threads,
        s.level,
        a.every,
        csv.display()
    );
    println!("{:>5}  {:>8}  {:>11}  {:>11}  {:>7}  {:>7}  checks", "round", "time", "resident", "private", "handles", "threads");

    let started = Instant::now();
    let (mut private, mut resident, mut handles, mut threads) = (Trend::default(), Trend::default(), Trend::default(), Trend::default());
    let mut round = 0u64;
    let mut failure = None;
    loop {
        if a.rounds.is_some_and(|r| round >= r) || deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        round += 1;
        sampler.round.store(round, Ordering::Relaxed);
        let t = Instant::now();
        let result = one_round(&source, &twin, &out, &s, &progress, synthetic);
        // Measured once the round's work is cleaned away, so only what
        // stays behind counts.
        if out.exists() {
            let _ = std::fs::remove_dir_all(&out);
        }
        let m = mem::own().unwrap_or_default();
        for (trend, v) in [(&mut private, m.private), (&mut resident, m.resident), (&mut handles, m.handles), (&mut threads, m.threads)] {
            trend.push(started, v);
        }
        let checks = match &result {
            Ok(c) => c.clone(),
            Err(e) => format!("FAILED: {e:#}"),
        };
        println!(
            "{round:>5}  {:>8}  {:>8.1} MB  {:>8.1} MB  {:>7}  {:>7}  {checks} ({:.0?})",
            clock(started.elapsed()),
            mb(m.resident),
            mb(m.private),
            m.handles,
            m.threads,
            t.elapsed()
        );
        if let Err(e) = result {
            failure = Some(format!("round {round}: {e:#}"));
            break;
        }
    }
    println!();
    println!(
        "Peak during the run: {} private, {} resident",
        human(sampler.peak_private.load(Ordering::Relaxed)),
        human(sampler.peak_resident.load(Ordering::Relaxed))
    );
    sampler.finish();
    if synthetic {
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&twin);
    }

    // The verdict: rounds after the first two (warm-up) must not keep climbing.
    let skip = 2;
    let mut report = String::new();
    let mut pass = failure.is_none();
    if let Some(f) = &failure {
        let _ = writeln!(report, "FAIL  {f}");
    }
    if private.len() < skip + 3 {
        let _ = writeln!(report, "Too few rounds ({round}) to judge growth; run for longer or with --rounds 6 or more.");
    } else {
        let base = private.at(skip);
        let grow = private.last() - base;
        let rate = private.per_hour(skip);
        let leak = grow > 64.0 * 1048576.0 && rate > 16.0 * 1048576.0;
        pass &= !leak;
        let _ = writeln!(
            report,
            "{}  Private memory: {} after warm-up, {} at the end ({:+.1} MB/hour)",
            if leak { "FAIL" } else { "ok  " },
            human(base as u64),
            human(private.last() as u64),
            rate / 1048576.0
        );
        let hgrow = handles.last() - handles.at(skip);
        let hleak = hgrow > 64.0 && handles.per_hour(skip) > 16.0;
        pass &= !hleak;
        let _ = writeln!(
            report,
            "{}  Handles: {:.0} after warm-up, {:.0} at the end",
            if hleak { "FAIL" } else { "ok  " },
            handles.at(skip),
            handles.last()
        );
        let tgrow = threads.last() - threads.at(skip);
        pass &= tgrow <= 4.0;
        let _ = writeln!(
            report,
            "{}  Threads: {:.0} after warm-up, {:.0} at the end",
            if tgrow > 4.0 { "FAIL" } else { "ok  " },
            threads.at(skip),
            threads.last()
        );
        let _ = writeln!(report, "      Resident memory peaked at {} between rounds", human(resident.max() as u64));
    }
    println!("\n{round} rounds in {}:\n{report}", clock(started.elapsed()));
    println!("{}", if pass { "PASSED" } else { "FAILED" });
    println!("Samples: {}", csv.display());
    Ok(pass)
}

/// Every engine path a long job uses, once: archive (with set-aside and
/// discarded files), the full check before a delete (without deleting),
/// putting files back, removing duplicates, and rebuilding the catalogue.
fn one_round(source: &Path, twin: &Path, out: &Path, s: &archive::Settings, p: &Arc<Progress>, synthetic: bool) -> Result<String> {
    let mut done = Vec::new();
    let o = archive::archive_folder(source, s, p)?;
    let main = PathBuf::from(&o.record.archive_path);
    done.push(format!("archived {}", human(o.record.input_bytes)));
    let cat = Catalog::open_existing(out)?;
    let a = cat.archived_folder(source)?.context("the archive isn't in the catalogue")?;
    let entries = cat.entries(a.archive_id)?;
    drop(cat);
    archive::check_before_delete(source, &a.archive_path, &entries, s.threads, p)?;
    done.push("checked".into());
    if synthetic {
        let keep: Vec<String> = o.excluded.set_aside.iter().take(1).map(|e| e.rel_path.clone()).collect();
        let r = archive::restore_excluded(&main, &keep, true, s, p)?;
        if r.put_back != keep.len() || !r.aside_removed {
            bail!("putting files back didn't do what was asked: {r:?}");
        }
        done.push("put back".into());
        archive::archive_folder(twin, s, p)?;
        let cat = Catalog::open_existing(out)?;
        let twin_id = cat.archived_folder(twin)?.context("the twin isn't in the catalogue")?.archive_id;
        let ids: Vec<i64> = cat
            .duplicate_groups(10_000)?
            .iter()
            .flat_map(|g| g.copies.iter().filter(|c| c.archive_id == twin_id).take(1))
            .map(|c| c.entry_id)
            .collect();
        drop(cat);
        if ids.is_empty() {
            bail!("the twin folder has no duplicates");
        }
        let r = archive::remove_duplicates(out, &ids, s, p)?;
        done.push(format!("deduped {}", r.files));
    }
    let mut cat = Catalog::open_existing(out)?;
    let before = cat.archives()?.len();
    cat.clear()?;
    drop(cat);
    let r = archive::rebuild_catalog(out)?;
    if r.added as usize != before || !r.failed.is_empty() {
        bail!("rebuilding found {} of {before} archives: {:?}", r.added, r.failed);
    }
    done.push("rebuilt".into());
    Ok(done.join(", "))
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Checks the work folder is empty or the soak's own, and empties the
/// soak's own leftovers.
fn prepare_work(work: &Path, protected: &[PathBuf]) -> Result<PathBuf> {
    std::fs::create_dir_all(work).with_context(|| format!("can't create {}", work.display()))?;
    let work = std::fs::canonicalize(work)?;
    let key = atticarc_core::catalog::path_key(&work);
    for p in protected {
        let pk = atticarc_core::catalog::path_key(p);
        if key == pk || key.starts_with(&format!("{pk}{}", std::path::MAIN_SEPARATOR)) {
            bail!("{} is a protected folder; choose a work folder elsewhere", work.display());
        }
    }
    let marker = work.join(MARKER);
    let mut others = std::fs::read_dir(&work)?.flatten().map(|e| e.file_name()).filter(|n| n != MARKER).peekable();
    if others.peek().is_some() && !marker.exists() {
        bail!("{} isn't empty; give the soak test an empty folder of its own", work.display());
    }
    for d in ["data", "twin", "out"] {
        let p = work.join(d);
        if p.exists() {
            std::fs::remove_dir_all(&p).with_context(|| format!("can't empty {}", p.display()))?;
        }
    }
    std::fs::write(&marker, "This folder belongs to the AtticARC soak test, which may empty it.\n")?;
    Ok(work)
}

/// A test tree of about `bytes`: one large file, medium random files, many
/// small text files in deep folders, odd names, empty files and folders,
/// a file to set aside and a shortcut to discard.
fn make_data(root: &Path, bytes: u64) -> Result<()> {
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let write_random = |path: &Path, size: u64, next: &mut dyn FnMut() -> u64, text: bool| -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut left = size;
        let mut buf = vec![0u8; 1 << 20];
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            if text {
                for (i, b) in buf[..n].iter_mut().enumerate() {
                    *b = b"the quick brown fox jumps over the lazy dog 0123456789\n"[(i + (next() % 7) as usize) % 55];
                }
            } else {
                for c in buf[..n].chunks_mut(8) {
                    let v = next().to_le_bytes();
                    c.copy_from_slice(&v[..c.len()]);
                }
            }
            f.write_all(&buf[..n])?;
            left -= n as u64;
        }
        Ok(())
    };
    // 40% in one large file, 40% in medium random files, the rest small text.
    write_random(&root.join("big/large file.bin"), bytes * 4 / 10, &mut next, false)?;
    let medium = (bytes * 4 / 10 / (8 << 20)).max(1);
    for i in 0..medium {
        write_random(&root.join(format!("media/clip {i:04}.mp4")), 8 << 20, &mut next, false)?;
    }
    let small = ((bytes / 5) / (40 << 10)).clamp(10, 20_000);
    for i in 0..small {
        let depth = (i % 6) as usize;
        let mut p = root.join("docs");
        for d in 0..depth {
            p = p.join(format!("level {d}"));
        }
        write_random(&p.join(format!("note {i:05}.txt")), 1 + next() % (80 << 10), &mut next, true)?;
    }
    std::fs::create_dir_all(root.join("empty folder"))?;
    std::fs::write(root.join("docs/empty.txt"), b"")?;
    std::fs::write(root.join("docs/caf\u{e9} \u{1f600} \u{65e5}\u{672c}.txt"), "odd name")?;
    std::fs::create_dir_all(root.join("scratch"))?;
    std::fs::write(root.join("scratch/keep me.tmp"), "set aside, then put back")?;
    std::fs::write(root.join("scratch/junk.tmp"), "set aside, then discarded")?;
    std::fs::write(root.join("Desktop.lnk"), "a shortcut")?;
    Ok(())
}

/// A second folder sharing the medium files with `data`, so its archive
/// has duplicates to remove.
fn make_twin(data: &Path, twin: &Path) -> Result<()> {
    std::fs::create_dir_all(twin.join("copied"))?;
    for e in std::fs::read_dir(data.join("media"))?.flatten().take(3) {
        std::fs::copy(e.path(), twin.join("copied").join(e.file_name()))?;
    }
    std::fs::write(twin.join("own.txt"), "only here")?;
    Ok(())
}

pub fn run_watch(args: &[String]) -> ExitCode {
    let mut target = "atticarc".to_string();
    let mut every = 10u64;
    let mut csv = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--every" => match it.next().and_then(|v| v.parse().ok()).filter(|n: &u64| *n > 0) {
                Some(n) => every = n,
                None => {
                    eprintln!("--every needs a number of seconds");
                    return ExitCode::from(2);
                }
            },
            "--csv" => csv = it.next().map(PathBuf::from),
            other => target = other.to_string(),
        }
    }
    let pid = match target.parse::<u32>() {
        Ok(p) => p,
        Err(_) => match mem::find(&target).as_slice() {
            [p] => *p,
            [] => {
                eprintln!("No running process called {target}. Start AtticARC first, or give its process id.");
                return ExitCode::from(1);
            }
            many => {
                eprintln!("{} processes are called {target} ({many:?}); give the process id instead.", many.len());
                return ExitCode::from(1);
            }
        },
    };
    let csv = csv.unwrap_or_else(|| PathBuf::from(format!("watch {} {pid}.csv", chrono::Local::now().format("%Y-%m-%d %H%M%S"))));
    let sampler = match Sampler::start(&csv, every, Some(pid), None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e:#}");
            return ExitCode::from(1);
        }
    };
    println!("Watching process {pid} every {every}s until it exits (Ctrl+C to stop). Samples go to {}", csv.display());
    println!("{:>8}  {:>11}  {:>11}  {:>7}  {:>7}", "time", "resident", "private", "handles", "threads");
    let started = Instant::now();
    let mut private = Trend::default();
    let mut last_print = Instant::now() - Duration::from_secs(60);
    while let Some(m) = mem::of(pid) {
        private.push(started, m.private);
        if last_print.elapsed() >= Duration::from_secs(60) {
            last_print = Instant::now();
            println!(
                "{:>8}  {:>8.1} MB  {:>8.1} MB  {:>7}  {:>7}",
                clock(started.elapsed()),
                mb(m.resident),
                mb(m.private),
                m.handles,
                m.threads
            );
        }
        std::thread::sleep(Duration::from_secs(every));
    }
    sampler.finish();
    println!(
        "Process {pid} has exited after {}. Private memory went from {} to a peak of {}, trend {:+.1} MB/hour after the first minutes.",
        clock(started.elapsed()),
        human(private.first() as u64),
        human(private.max() as u64),
        private.per_hour(6) / 1048576.0
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_round_on_a_small_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let work = prepare_work(&tmp.path().join("work"), &[]).unwrap();
        let (data, twin, out) = (work.join("data"), work.join("twin"), work.join("out"));
        make_data(&data, 8 << 20).unwrap();
        make_twin(&data, &twin).unwrap();
        let s = archive::Settings {
            out_dir: out.clone(),
            level: 5,
            store_mode: true,
            threads: 2,
            protected: vec![],
            exclusions: vec![
                Rule { pattern: "*.tmp".into(), action: Action::SetAside, ..Rule::default() },
                Rule { pattern: "*.lnk".into(), action: Action::Discard, ..Rule::default() },
            ],
        };
        let p = Arc::new(Progress::default());
        for _ in 0..2 {
            let done = one_round(&data, &twin, &out, &s, &p, true).unwrap();
            assert!(done.contains("put back") && done.contains("deduped") && done.contains("rebuilt"), "{done}");
            std::fs::remove_dir_all(&out).unwrap();
        }
        // A folder that isn't the soak's own is refused.
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("precious.txt"), "x").unwrap();
        assert!(prepare_work(&other, &[]).is_err());
        assert!(prepare_work(&tmp.path().join("work2"), &[tmp.path().to_path_buf()]).is_err());
        assert!(mem::own().is_some_and(|m| m.resident > 0));
    }
}
