use dashmap::DashMap;
use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    collections::{BTreeMap, HashMap},
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

const MAX_THREADS: usize = 256;

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
    None,
    #[cfg(feature = "backtrace")]
    Short,
    #[cfg(feature = "backtrace")]
    Full,
}

pub struct AllocTrack<T: GlobalAlloc> {
    inner: T,
    backtrace: BacktraceMode,
}

impl<T: GlobalAlloc> AllocTrack<T> {
    pub const fn new(inner: T, backtrace: BacktraceMode) -> Self {
        Self { inner, backtrace }
    }
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
            if THREAD_STORE[tid].tid.load(Ordering::Relaxed) == 0 {
                let os_tid = libc::syscall(libc::SYS_gettid) as u32;
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
                });
                trace_info.allocated += size as u64;
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
pub fn backtrace_report() -> BacktraceReport {
    IN_ALLOC.with(|x| x.set(true));
    let mut out = vec![];
    for entry in TRACE_MAP.iter() {
        let metric = BacktraceMetric {
            allocated: entry.allocated,
            freed: entry.freed,
            mode: entry.mode,
        };
        let mut backtrace = entry.backtrace.clone();
        backtrace.inner_mut().resolve();
        out.push((backtrace, metric));
    }
    out.sort_by_key(|x| x.1.allocated.saturating_sub(x.1.freed) as i64);
    IN_ALLOC.with(|x| x.set(false));
    let out2 = out.clone();
    IN_ALLOC.with(|x| x.set(true));
    drop(out);
    IN_ALLOC.with(|x| x.set(false));
    BacktraceReport(out2)
}

/// Generate a memory usage report
/// Note that the numbers are not a synchronized snapshot, and have slight timing skew.
pub fn thread_report() -> ThreadReport {
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

    let mut tid_names: HashMap<usize, &String> = HashMap::new();
    for (i, thread) in THREAD_STORE.iter().enumerate() {
        let tid = thread.tid.load(Ordering::Relaxed);
        if tid == 0 {
            continue;
        }
        if let Some(name) = os_tid_names.get(&tid) {
            tid_names.insert(i, name);
        }
    }

    let mut metrics = BTreeMap::new();

    for (i, thread) in THREAD_STORE.iter().enumerate() {
        let name = if let Some(name) = tid_names.get(&i) {
            *name
        } else {
            continue;
        };
        let alloced = thread.alloc.load(Ordering::Relaxed) as u64;
        let metric: &mut ThreadMetric = metrics.entry(name.clone()).or_default();
        metric.total_alloc += alloced;

        let mut total_free: u64 = 0;
        for (j, thread2) in THREAD_STORE.iter().enumerate() {
            let name = if let Some(name) = tid_names.get(&j) {
                *name
            } else {
                continue;
            };
            let freed = thread2.free[i].load(Ordering::Relaxed);
            if freed == 0 {
                continue;
            }
            total_free += freed as u64;
            *metric.freed_by_others.entry(name.clone()).or_default() += freed as u64;
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
