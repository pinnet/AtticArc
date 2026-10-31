//! How much memory, how many handles and how many threads a process is
//! using, for the soak test and `watch`.

use std::time::Instant;

#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    /// Physical memory in use (the working set on Windows, RSS on Linux).
    pub resident: u64,
    /// Memory the process has committed for itself, which is what grows
    /// with a leak (private bytes on Windows, anonymous resident memory on Linux).
    pub private: u64,
    /// Open handles on Windows, open file descriptors on Linux.
    pub handles: u64,
    pub threads: u64,
}

/// This process.
pub fn own() -> Option<Sample> {
    imp::sample(None)
}

/// Another process by id.
pub fn of(pid: u32) -> Option<Sample> {
    imp::sample(Some(pid))
}

/// Ids of running processes whose program name is `name` (case ignored,
/// `.exe` optional).
pub fn find(name: &str) -> Vec<u32> {
    let want = name.trim_end_matches(".exe").to_lowercase();
    imp::processes().into_iter().filter(|(_, n)| n.to_lowercase().trim_end_matches(".exe") == want).map(|(p, _)| p).collect()
}

/// A series of samples and whether it keeps climbing.
#[derive(Default)]
pub struct Trend {
    /// (seconds since start, value)
    points: Vec<(f64, f64)>,
}

impl Trend {
    pub fn push(&mut self, since: Instant, v: u64) {
        self.points.push((since.elapsed().as_secs_f64(), v as f64));
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// The value at point `i`, or the last one if there are fewer.
    pub fn at(&self, i: usize) -> f64 {
        self.points.get(i).or(self.points.last()).map_or(0.0, |p| p.1)
    }

    pub fn first(&self) -> f64 {
        self.points.first().map_or(0.0, |p| p.1)
    }

    pub fn last(&self) -> f64 {
        self.points.last().map_or(0.0, |p| p.1)
    }

    pub fn max(&self) -> f64 {
        self.points.iter().map(|p| p.1).fold(0.0, f64::max)
    }

    /// Least-squares growth per hour, over the points after the first
    /// `skip` (warm-up: caches and pools fill early on).
    pub fn per_hour(&self, skip: usize) -> f64 {
        let p = &self.points[skip.min(self.points.len())..];
        if p.len() < 3 {
            return 0.0;
        }
        let n = p.len() as f64;
        let mx = p.iter().map(|x| x.0).sum::<f64>() / n;
        let my = p.iter().map(|x| x.1).sum::<f64>() / n;
        let sxx: f64 = p.iter().map(|x| (x.0 - mx).powi(2)).sum();
        if sxx == 0.0 {
            return 0.0;
        }
        let sxy: f64 = p.iter().map(|x| (x.0 - mx) * (x.1 - my)).sum();
        sxy / sxx * 3600.0
    }
}

#[cfg(windows)]
mod imp {
    use super::Sample;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub fn sample(pid: Option<u32>) -> Option<Sample> {
        // SAFETY: plain Win32 calls on a handle we own and close, with
        // out-structures sized as the calls require.
        unsafe {
            let (h, own): (HANDLE, bool) = match pid {
                None => (GetCurrentProcess(), true),
                Some(p) => (OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, p), false),
            };
            if h.is_null() {
                return None;
            }
            let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
            c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            let ok = GetProcessMemoryInfo(h, &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb) != 0;
            let mut handles = 0u32;
            GetProcessHandleCount(h, &mut handles);
            if !own {
                CloseHandle(h);
            }
            if !ok {
                return None;
            }
            let id = pid.unwrap_or_else(|| GetCurrentProcessId());
            let threads = processes_with_threads().into_iter().find(|p| p.0 == id).map_or(0, |p| p.2);
            Some(Sample {
                resident: c.WorkingSetSize as u64,
                private: c.PrivateUsage as u64,
                handles: handles as u64,
                threads,
            })
        }
    }

    pub fn processes() -> Vec<(u32, String)> {
        processes_with_threads().into_iter().map(|(p, n, _)| (p, n)).collect()
    }

    fn processes_with_threads() -> Vec<(u32, String, u64)> {
        let mut out = Vec::new();
        // SAFETY: the snapshot handle is closed below; the entry's size is set.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snap, &mut e) != 0;
            while more {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                out.push((e.th32ProcessID, String::from_utf16_lossy(&e.szExeFile[..len]), e.cntThreads as u64));
                more = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        out
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Sample;

    pub fn sample(pid: Option<u32>) -> Option<Sample> {
        let dir = match pid {
            None => "/proc/self".to_string(),
            Some(p) => format!("/proc/{p}"),
        };
        let status = std::fs::read_to_string(format!("{dir}/status")).ok()?;
        let kb = |key: &str| {
            status
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let handles = std::fs::read_dir(format!("{dir}/fd")).map(|d| d.count() as u64).unwrap_or(0);
        Some(Sample { resident: kb("VmRSS:") * 1024, private: kb("RssAnon:") * 1024, handles, threads: kb("Threads:") })
    }

    pub fn processes() -> Vec<(u32, String)> {
        std::fs::read_dir("/proc")
            .map(|d| {
                d.flatten()
                    .filter_map(|e| {
                        let pid = e.file_name().to_str()?.parse().ok()?;
                        let name = std::fs::read_to_string(e.path().join("comm")).ok()?;
                        Some((pid, name.trim().to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
