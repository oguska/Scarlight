//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and the
//! title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! shortcut: the budget (half a second of waiting per title frame,
//! `VICTORY_PIPELINE_WAIT_MS`) is a guess; a pass the title runs only once
//! and whose pipeline takes longer is lost.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queued: AtomicBool,
    urgent: AtomicBool,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw has used it yet.
    key: String,
    used: AtomicBool,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        Pending {
            shaders,
            graphics,
            queued: AtomicBool::new(false),
            urgent: AtomicBool::new(false),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
        }
    }

    fn make(&self) {
        self.made
            .get_or_init(|| HostPipeline::new(&self.shaders, self.graphics.as_deref()));
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

struct WorkQueue {
    urgent: VecDeque<Arc<Pending>>,
    background: VecDeque<Arc<Pending>>,
}

impl WorkQueue {
    fn enqueue(&mut self, p: Arc<Pending>, urgent: bool) {
        if urgent {
            self.urgent.push_back(p);
        } else {
            self.background.push_back(p);
        }
    }

    fn promote(&mut self, p: &Arc<Pending>) -> bool {
        if let Some(pos) = self.background.iter().position(|q| Arc::ptr_eq(q, p))
            && let Some(item) = self.background.remove(pos)
        {
            self.urgent.push_back(item);
            return true;
        }
        false
    }

    fn pop(&mut self) -> Option<Arc<Pending>> {
        self.urgent.pop_front().or_else(|| self.background.pop_front())
    }
}

static WORK: (Mutex<WorkQueue>, Condvar) = (
    Mutex::new(WorkQueue {
        urgent: VecDeque::new(),
        background: VecDeque::new(),
    }),
    Condvar::new(),
);

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads: 500 ms unless
/// `VICTORY_PIPELINE_WAIT_MS` says otherwise (0: never wait, a draw whose
/// pipeline is not made yet is dropped until it is; one-off passes such as
/// a colour-grading bake may then be lost).
fn budget() -> Duration {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    Duration::from_millis(*MS.get_or_init(|| {
        std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500)
    }))
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    if p.made.get().is_none() {
        start(p, true);
        wait(p, crate::frame::current());
    }
    let made = p.made.get()?.as_ref();
    if made.is_some() && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    made
}

/// Queues `p` for the workers, once; urgent draws jump ahead of early
/// pre-warming work and promote already-queued background entries.
fn start(p: &Arc<Pending>, urgent: bool) {
    if urgent {
        p.urgent.store(true, Ordering::Release);
    }
    if !p.queued.swap(true, Ordering::AcqRel) {
        PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        if ensure_workers() {
            let (lock, ready) = &WORK;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            let urgent = urgent || p.urgent.load(Ordering::Acquire);
            q.enqueue(p.clone(), urgent);
            ready.notify_one();
        } else {
            p.make();
            WAITING.fetch_sub(1, Ordering::Relaxed);
        }
    } else if urgent && p.made.get().is_none() {
        let (lock, ready) = &WORK;
        if lock.lock().unwrap_or_else(|e| e.into_inner()).promote(p) {
            ready.notify_one();
        }
    }
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, false);
    }
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static WAITED_US: AtomicU64 = AtomicU64::new(0);
    if FRAME.swap(frame, Ordering::Relaxed) != frame {
        WAITED_US.store(0, Ordering::Relaxed);
    }
    let left = budget().saturating_sub(Duration::from_micros(WAITED_US.load(Ordering::Relaxed)));
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    WAITED_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// Starts the pipeline workers (half the logical processors, 2 to 8) once.
fn ensure_workers() -> bool {
    static WORKERS: OnceLock<bool> = OnceLock::new();
    *WORKERS.get_or_init(|| {
        let workers = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
        let mut started = 0;
        for i in 0..workers {
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(move || {
                    // Below the title's own threads (loading, recording).
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    loop {
                        let p = {
                            let (lock, ready) = &WORK;
                            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                if let Some(p) = q.pop() {
                                    break p;
                                }
                                q = ready.wait(q).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        p.make();
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            match spawned {
                Ok(_) => started += 1,
                Err(e) => tracing::warn!(error = %e, "no pipeline worker"),
            }
        }
        started > 0
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert!(p.queued.load(Ordering::Relaxed));
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn urgent_and_promoted_pipelines_run_before_background() {
        let bg1 = Arc::new(Pending::new(Vec::new(), None));
        let bg2 = Arc::new(Pending::new(Vec::new(), None));
        let urg = Arc::new(Pending::new(Vec::new(), None));
        let mut q = WorkQueue {
            urgent: VecDeque::new(),
            background: VecDeque::new(),
        };
        q.enqueue(bg1.clone(), false);
        q.enqueue(bg2.clone(), false);
        q.enqueue(urg.clone(), true);
        assert!(q.promote(&bg2));
        assert!(!q.promote(&bg2));
        assert!(Arc::ptr_eq(&q.pop().unwrap(), &urg));
        assert!(Arc::ptr_eq(&q.pop().unwrap(), &bg2));
        assert!(Arc::ptr_eq(&q.pop().unwrap(), &bg1));
        assert!(q.pop().is_none());
    }
}