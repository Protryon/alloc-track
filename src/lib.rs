#![doc = include_str!("../README.md")]

use dashmap::DashMap;
#[allow(unused_imports)]
use std::collections::HashMap;
use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    collections::BTreeMap,
    fmt,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

#[cfg(feature = "backtrace")]
mod backtrace_support;
#[cfg(feature = "backtrace")]
use backtrace_support::*;
#[cfg(feature = "backtrace")]
pub use backtrace_support::{BacktraceMetric, BacktraceReport, HashedBacktrace};

/// next thread id incrementor
static THREAD_ID_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// On linux you can check your system by running `cat /proc/sys/kernel/threads-max`
/// It's almost certain that this limit will be hit in some strange corner cases.
const MAX_THREADS: usize = 1024;

#[derive(Clone, Copy, Debug)]
struct PointerData {
    alloc_thread_id: usize,
    #[cfg(feature = "backtrace")]
    trace_hash: u64,
}

lazy_static::lazy_static! {
    /// pointer -> data
    static ref PTR_MAP: DashMap<usize, PointerData> = DashMap::new();
    // backtrace -> current allocation size
    #[cfg(feature = "backtrace")]
    static ref TRACE_MAP: DashMap<u64, TraceInfo> = DashMap::new();
}

/// Representation of globally-accessible TLS
struct ThreadStore {
    #[allow(dead_code)]
    tid: AtomicU32,
    alloc: AtomicUsize,
    free: [AtomicUsize; MAX_THREADS],
}

/// A layout compatible representation of `ThreadStore` that is `Copy`.
/// Layout of `AtomicX` is guaranteed to be identical to `X`
/// Used to initialize arrays.
#[allow(dead_code)]
#[derive(Clone, Copy)]
struct ThreadStoreLocal {
    tid: u32,
    alloc: usize,
    free: [usize; MAX_THREADS],
}

/// Custom psuedo-TLS implementation that allows safe global introspection
static THREAD_STORE: [ThreadStore; MAX_THREADS] = unsafe {
    std::mem::transmute(
        [ThreadStoreLocal {
            tid: 0,
            alloc: 0,
            free: [0usize; MAX_THREADS],
        }; MAX_THREADS],
    )
};

thread_local! {
    static THREAD_ID: usize = THREAD_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    /// Used to avoid recursive alloc/dealloc calls for interior allocation
    static IN_ALLOC: Cell<bool> = Cell::new(false);
}

fn enter_alloc<T>(func: impl FnOnce() -> T) -> T {
    let current_value = IN_ALLOC.with(|x| x.get());
    IN_ALLOC.with(|x| x.set(true));
    let output = func();
    IN_ALLOC.with(|x| x.set(current_value));
    output
}

#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub enum BacktraceMode {
    #[default]
    /// Report no backtraces
    None,
    #[cfg(feature = "backtrace")]
    /// Report backtraces with unuseful entries removed (i.e. alloc_track, allocator internals)
    Short,
    /// Report the full backtrace
    #[cfg(feature = "backtrace")]
    Full,
}

/// Global memory allocator wrapper that can track per-thread and per-backtrace memory usage.
pub struct AllocTrack<T: GlobalAlloc> {
    inner: T,
    backtrace: BacktraceMode,
}

impl<T: GlobalAlloc> AllocTrack<T> {
    pub const fn new(inner: T, backtrace: BacktraceMode) -> Self {
        Self { inner, backtrace }
    }
}
#[cfg(all(unix, feature = "fs"))]
#[inline(always)]
unsafe fn get_sys_tid() -> u32 {
    libc::syscall(libc::SYS_gettid) as u32
}

#[cfg(all(windows, feature = "fs"))]
#[inline(always)]
unsafe fn get_sys_tid() -> u32 {
    windows::Win32::System::Threading::GetCurrentThreadId()
}

unsafe impl<T: GlobalAlloc> GlobalAlloc for AllocTrack<T> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if IN_ALLOC.with(|x| x.get()) {
            return self.inner.alloc(layout);
        }
        enter_alloc(|| {
            let size = layout.size();
            let ptr = self.inner.alloc(layout);
            let tid = THREAD_ID.with(|x| *x);
            assert!(
                tid < MAX_THREADS,
                "Thread ID {tid} is greater than the maximum number of threads {MAX_THREADS}"
            );
            #[cfg(feature = "fs")]
            if THREAD_STORE[tid].tid.load(Ordering::Relaxed) == 0 {
                let os_tid = get_sys_tid();
                THREAD_STORE[tid].tid.store(os_tid, Ordering::Relaxed);
            }
            THREAD_STORE[tid].alloc.fetch_add(size, Ordering::Relaxed);
            #[cfg(feature = "backtrace")]
            let trace = HashedBacktrace::capture(self.backtrace);
            PTR_MAP.insert(
                ptr as usize,
                PointerData {
                    alloc_thread_id: tid,
                    #[cfg(feature = "backtrace")]
                    trace_hash: trace.hash(),
                },
            );
            #[cfg(feature = "backtrace")]
            if !matches!(self.backtrace, BacktraceMode::None) {
                let mut trace_info = TRACE_MAP.entry(trace.hash()).or_insert_with(|| TraceInfo {
                    backtrace: trace,
                    allocated: 0,
                    freed: 0,
                    mode: self.backtrace,
                    allocations: 0,
                });
                trace_info.allocated += size as u64;
                trace_info.allocations += 1;
            }
            ptr
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if IN_ALLOC.with(|x| x.get()) {
            self.inner.dealloc(ptr, layout);
            return;
        }
        enter_alloc(|| {
            let size = layout.size();
            let (_, target) = PTR_MAP.remove(&(ptr as usize)).expect("double free");
            #[cfg(feature = "backtrace")]
            if !matches!(self.backtrace, BacktraceMode::None) {
                if let Some(mut info) = TRACE_MAP.get_mut(&target.trace_hash) {
                    info.freed += size as u64;
                }
            }
            self.inner.dealloc(ptr, layout);
            let tid = THREAD_ID.with(|x| *x);
            THREAD_STORE[tid].free[target.alloc_thread_id].fetch_add(size, Ordering::SeqCst);
        });
    }
}

/// Size display helper
pub struct Size(pub u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 1024 {
            write!(f, "{} B", self.0)
        } else if self.0 < 1024 * 1024 {
            write!(f, "{} KB", self.0 / 1024)
        } else {
            write!(f, "{} MB", self.0 / 1024 / 1024)
        }
    }
}

/// Size display helper
pub struct SizeF64(pub f64);

impl fmt::Display for SizeF64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 1024.0 {
            write!(f, "{:.01} B", self.0)
        } else if self.0 < 1024.0 * 1024.0 {
            write!(f, "{:.01} KB", self.0 / 1024.0)
        } else {
            write!(f, "{:.01} MB", self.0 / 1024.0 / 1024.0)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ThreadMetric {
    /// Total bytes allocated in this thread
    pub total_alloc: u64,
    /// Total bytes freed in this thread
    pub total_did_free: u64,
    /// Total bytes allocated in this thread that have been freed
    pub total_freed: u64,
    /// Total bytes allocated in this thread that are not freed
    pub current_used: u64,
    /// Total bytes allocated in this thread that have been freed by the given thread
    pub freed_by_others: BTreeMap<String, u64>,
}

impl fmt::Display for ThreadMetric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "total_alloc: {}", Size(self.total_alloc))?;
        writeln!(f, "total_did_free: {}", Size(self.total_did_free))?;
        writeln!(f, "total_freed: {}", Size(self.total_freed))?;
        writeln!(f, "current_used: {}", Size(self.current_used))?;
        for (name, size) in &self.freed_by_others {
            writeln!(f, "freed by {}: {}", name, Size(*size))?;
        }
        Ok(())
    }
}

/// A comprehensive report of all thread allocation metrics
pub struct ThreadReport(pub BTreeMap<String, ThreadMetric>);

impl fmt::Display for ThreadReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, metric) in &self.0 {
            writeln!(f, "{name}:\n{metric}\n")?;
        }
        Ok(())
    }
}

/// Generate a memory usage report for backtraces, if enabled
#[cfg(feature = "backtrace")]
pub fn backtrace_report(
    filter: impl Fn(&crate::backtrace::Backtrace, &BacktraceMetric) -> bool,
) -> BacktraceReport {
    IN_ALLOC.with(|x| x.set(true));
    let mut out = vec![];
    for mut entry in TRACE_MAP.iter_mut() {
        let metric = BacktraceMetric {
            allocated: entry.allocated,
            freed: entry.freed,
            mode: entry.mode,
            allocations: entry.allocations,
        };
        if !filter(entry.backtrace.inner(), &metric) {
            continue;
        }
        entry.backtrace.inner_mut().resolve();
        out.push((entry.backtrace.clone(), metric));
    }
    out.sort_by_key(|x| x.1.allocated.saturating_sub(x.1.freed) as i64);
    IN_ALLOC.with(|x| x.set(false));
    let out2 = out.clone();
    IN_ALLOC.with(|x| x.set(true));
    drop(out);
    IN_ALLOC.with(|x| x.set(false));
    BacktraceReport(out2)
}

#[cfg(all(unix, feature = "fs"))]
fn os_tid_names() -> HashMap<u32, String> {
    let mut os_tid_names: HashMap<u32, String> = HashMap::new();
    for task in procfs::process::Process::myself().unwrap().tasks().unwrap() {
        let task = task.unwrap();
        os_tid_names.insert(
            task.tid as u32,
            std::fs::read_to_string(format!("/proc/{}/task/{}/comm", task.pid, task.tid))
                .unwrap()
                .trim()
                .to_string(),
        );
    }
    os_tid_names
}

#[cfg(all(windows, feature = "fs"))]
fn os_tid_names() -> HashMap<u32, String> {
    use std::alloc::alloc;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        Thread32First, Thread32Next, THREADENTRY32,
    };
    let mut os_tid_names: HashMap<u32, String> = HashMap::new();
    unsafe {
        let process_id = windows::Win32::System::Threading::GetCurrentProcessId();
        let snapshot = windows::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot(
            windows::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPTHREAD,
            0,
        );
        if let Ok(snapshot) = snapshot {
            let mut thread_entry = alloc(Layout::new::<THREADENTRY32>()) as *mut THREADENTRY32;
            (*thread_entry).dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            if Thread32First(snapshot, thread_entry).as_bool() {
                loop {
                    if (*thread_entry).th32OwnerProcessID == process_id {
                        let thread_handle = windows::Win32::System::Threading::OpenThread(
                            windows::Win32::System::Threading::THREAD_QUERY_LIMITED_INFORMATION,
                            false,
                            (*thread_entry).th32ThreadID,
                        );
                        if let Ok(handle) = thread_handle {
                            let result =
                                windows::Win32::System::Threading::GetThreadDescription(handle);
                            if let Ok(str) = result {
                                os_tid_names.insert(
                                    (*thread_entry).th32ThreadID,
                                    str.to_string().unwrap_or("UTF-16 Error".to_string()),
                                );
                            } else {
                                os_tid_names
                                    .insert((*thread_entry).th32ThreadID, "unknown".to_string());
                            }
                            CloseHandle(handle);
                        }
                    }
                    if !Thread32Next(snapshot, thread_entry).as_bool() {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
        }
    }
    os_tid_names
}

/// Generate a memory usage report
/// Note that the numbers are not a synchronized snapshot, and have slight timing skew.
pub fn thread_report() -> ThreadReport {
    #[cfg(feature = "fs")]
    let os_tid_names: HashMap<u32, String> = os_tid_names();
    #[cfg(feature = "fs")]
    let mut tid_names: HashMap<usize, &String> = HashMap::new();
    #[cfg(feature = "fs")]
    let get_tid_name = {
        for (i, thread) in THREAD_STORE.iter().enumerate() {
            let tid = thread.tid.load(Ordering::Relaxed);
            if tid == 0 {
                continue;
            }
            if let Some(name) = os_tid_names.get(&tid) {
                tid_names.insert(i, name);
            }
        }
        |id: usize| tid_names.get(&id).map(|x| &**x)
    };
    #[cfg(not(feature = "fs"))]
    let get_tid_name = { move |id: usize| Some(id.to_string()) };

    let mut metrics = BTreeMap::new();

    for (i, thread) in THREAD_STORE.iter().enumerate() {
        let Some(name) = get_tid_name(i) else {
            continue;
        };
        let alloced = thread.alloc.load(Ordering::Relaxed) as u64;
        let metric: &mut ThreadMetric = metrics.entry(name.into()).or_default();
        metric.total_alloc += alloced;

        let mut total_free: u64 = 0;
        for (j, thread2) in THREAD_STORE.iter().enumerate() {
            let Some(name) = get_tid_name(j) else {
                continue;
            };
            let freed = thread2.free[i].load(Ordering::Relaxed);
            if freed == 0 {
                continue;
            }
            total_free += freed as u64;
            *metric.freed_by_others.entry(name.into()).or_default() += freed as u64;
        }
        metric.total_did_free += total_free;
        metric.total_freed += thread
            .free
            .iter()
            .map(|x| x.load(Ordering::Relaxed) as u64)
            .sum::<u64>();
        metric.current_used += alloced.saturating_sub(total_free);
    }
    ThreadReport(metrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub fn test_os_tid_names() {
        std::thread::Builder::new()
            .name("thread2".to_string())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(100));
            })
            .unwrap();

        let os_tid_names = os_tid_names();
        println!("{:?}", os_tid_names);
    }
}
