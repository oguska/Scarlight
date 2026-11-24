//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and the
//! title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline on a repeat-demand priority queue and
//! waits for it within a budget per frame, so the cheap ones (menus,
//! one-off passes such as a colour-grading bake) are there at once as before;
//! pipelines demanded again by recurring draws/dispatches jump ahead of
//! single-hit entries (with non-mesh tie-breaking and bounded anti-starvation
//! fairness) and ahead of speculative early pre-warms; past the budget a
//! draw or dispatch whose pipeline is not ready is dropped, as an
//! untranslated one is.
//!
//! shortcut: the budget (half a second of waiting per title frame) is a
//! guess; a pass the title runs only once and whose pipeline takes longer
//! is lost.

use std::collections::{BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

const UNQUEUED: u8 = 0;
const EARLY: u8 = 1;
const DEMAND: u8 = 2;
/// Highest priority bucket (`hits >= 256`); caps duplicate heap pushes per item.
const MAX_TIER: u8 = 10;
/// Maximum consecutive higher-tier (`tier >= 2`) pops before serving one oldest
/// waiting demand entry.
const FAIR_STRIDE: u32 = 16;

fn demand_tier(hits: u32) -> u8 {
    match hits {
        0 => 0,
        1..=3 => hits as u8,
        n => (33 - n.leading_zeros()).min(u32::from(MAX_TIER)) as u8,
    }
}

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    non_mesh: bool,
    lane: AtomicU8,
    hits: AtomicU32,
    tier_queued: AtomicU8,
    counted: AtomicBool,
    in_fair: AtomicBool,
    started: AtomicBool,
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
        let non_mesh = !shaders.iter().any(|s| s.stage == "mesh");
        Self::with_non_mesh(shaders, graphics, non_mesh)
    }

    fn with_non_mesh(
        shaders: Vec<Arc<Shader>>,
        graphics: Option<Box<GraphicsState>>,
        non_mesh: bool,
    ) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        Pending {
            shaders,
            graphics,
            non_mesh,
            lane: AtomicU8::new(UNQUEUED),
            hits: AtomicU32::new(0),
            tier_queued: AtomicU8::new(0),
            counted: AtomicBool::new(false),
            in_fair: AtomicBool::new(false),
            started: AtomicBool::new(false),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    fn is_queued(&self) -> bool {
        self.lane.load(Ordering::Relaxed) != UNQUEUED
    }

    fn claim(&self) -> bool {
        !self.started.swap(true, Ordering::AcqRel)
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

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
const BUDGET: Duration = Duration::from_millis(500);

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use, promotes queued early pipelines, and upgrades
/// priority on repeat demand.
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

/// Queues `p` for the workers on the demand or early lane (and promotes
/// unstarted early pipelines to demand without double-counting `WAITING`).
fn start(p: &Arc<Pending>, is_demand: bool) {
    enqueue_on(p, is_demand, &WAITING, &PEAK, work_queue());
}

fn bump_hit(hits: &AtomicU32) -> u32 {
    match hits.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)) {
        Ok(prev) => prev + 1,
        Err(max) => max,
    }
}

fn enqueue_on(
    p: &Arc<Pending>,
    is_demand: bool,
    waiting: &AtomicU64,
    peak: &AtomicU64,
    queue: &WorkQueue,
) {
    if !is_demand {
        if p.lane
            .compare_exchange(UNQUEUED, EARLY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            queue.push_early(p.clone(), waiting, peak);
        }
        return;
    }

    if p.started.load(Ordering::Acquire) {
        return;
    }
    let _ = p
        .lane
        .compare_exchange(UNQUEUED, DEMAND, Ordering::AcqRel, Ordering::Acquire)
        .or_else(|_| {
            p.lane
                .compare_exchange(EARLY, DEMAND, Ordering::AcqRel, Ordering::Acquire)
        });
    if p.started.load(Ordering::Acquire) {
        return;
    }
    let hits = bump_hit(&p.hits);
    let tier = demand_tier(hits);
    let prev = p.tier_queued.fetch_max(tier, Ordering::AcqRel);
    if prev < tier {
        queue.push_demand(p.clone(), tier, waiting, peak);
    }
}

/// At the title's creation of a pipeline: starts it at once on the lower-priority
/// lane if an earlier run drew with it (during loading, so the world's first
/// frames find it without delaying live draws).
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
    let left = BUDGET.saturating_sub(Duration::from_micros(WAITED_US.load(Ordering::Relaxed)));
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

struct WorkItem {
    tier: u8,
    non_mesh: bool,
    seq: u64,
    pending: Arc<Pending>,
}

impl Ord for WorkItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.tier
            .cmp(&other.tier)
            .then_with(|| self.non_mesh.cmp(&other.non_mesh))
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for WorkItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for WorkItem {}

impl PartialEq for WorkItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

struct Queues {
    next_seq: u64,
    high_streak: u32,
    demand: BinaryHeap<WorkItem>,
    fair_fifo: VecDeque<Arc<Pending>>,
    early: VecDeque<Arc<Pending>>,
}

impl Queues {
    const fn new() -> Self {
        Self {
            next_seq: 0,
            high_streak: 0,
            demand: BinaryHeap::new(),
            fair_fifo: VecDeque::new(),
            early: VecDeque::new(),
        }
    }

    /// Pops the highest-priority unclaimed pipeline: repeat-demanded entries
    /// (with non-mesh, oldest-queued tie-break) ahead of 1-hit demand entries,
    /// interleaving an oldest waiting demand entry after `FAIR_STRIDE` high-tier
    /// pops so 1-hit work cannot permanently starve, and draining `early` last.
    fn pop_claimed(&mut self) -> Option<Arc<Pending>> {
        while self
            .fair_fifo
            .front()
            .is_some_and(|p| p.started.load(Ordering::Acquire))
        {
            self.fair_fifo.pop_front();
        }
        if self.high_streak >= FAIR_STRIDE && !self.fair_fifo.is_empty() {
            while let Some(p) = self.fair_fifo.pop_front() {
                if p.claim() {
                    self.high_streak = 0;
                    return Some(p);
                }
            }
        }
        while let Some(item) = self.demand.pop() {
            if item.pending.claim() {
                if item.tier >= 2 {
                    self.high_streak += 1;
                } else {
                    self.high_streak = 0;
                }
                return Some(item.pending);
            }
        }
        self.high_streak = 0;
        while let Some(p) = self.early.pop_front() {
            if p.claim() {
                return Some(p);
            }
        }
        None
    }
}

struct WorkQueue {
    state: Mutex<Queues>,
    ready: Condvar,
    workers_alive: AtomicBool,
}

impl WorkQueue {
    fn new(workers_alive: bool) -> Self {
        Self {
            state: Mutex::new(Queues::new()),
            ready: Condvar::new(),
            workers_alive: AtomicBool::new(workers_alive),
        }
    }

    fn make_inline(p: &Pending, waiting: &AtomicU64, peak: &AtomicU64) {
        if p.claim() {
            if !p.counted.swap(true, Ordering::Relaxed) {
                peak.fetch_max(waiting.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
            }
            p.make();
            if waiting.fetch_sub(1, Ordering::Relaxed) == 1 {
                drained();
            }
        }
    }

    fn push_demand(&self, p: Arc<Pending>, tier: u8, waiting: &AtomicU64, peak: &AtomicU64) {
        if !self.workers_alive.load(Ordering::Acquire) {
            Self::make_inline(&p, waiting, peak);
            return;
        }
        let mut queues = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if p.started.load(Ordering::Acquire) {
            return;
        }
        if !p.counted.swap(true, Ordering::Relaxed) {
            peak.fetch_max(waiting.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        let seq = queues.next_seq;
        queues.next_seq = queues.next_seq.wrapping_add(1);
        if queues.demand.is_empty() && tier <= 1 {
            queues.high_streak = 0;
        }
        if !p.in_fair.swap(true, Ordering::Relaxed) {
            queues.fair_fifo.push_back(p.clone());
        }
        queues.demand.push(WorkItem {
            tier,
            non_mesh: p.non_mesh,
            seq,
            pending: p,
        });
        drop(queues);
        self.ready.notify_one();
    }

    fn push_early(&self, p: Arc<Pending>, waiting: &AtomicU64, peak: &AtomicU64) {
        if !self.workers_alive.load(Ordering::Acquire) {
            Self::make_inline(&p, waiting, peak);
            return;
        }
        let mut queues = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if p.started.load(Ordering::Acquire) {
            return;
        }
        if !p.counted.swap(true, Ordering::Relaxed) {
            peak.fetch_max(waiting.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        queues.early.push_back(p);
        drop(queues);
        self.ready.notify_one();
    }

    fn recv(&self) -> Arc<Pending> {
        let mut queues = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(p) = queues.pop_claimed() {
                return p;
            }
            queues = self.ready.wait(queues).unwrap_or_else(|e| e.into_inner());
        }
    }

    #[cfg(test)]
    fn try_pop(&self) -> Option<Arc<Pending>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_claimed()
    }
}

/// Shared priority queue and workers (half the logical processors, 2 to 8),
/// started on first use.
fn work_queue() -> &'static WorkQueue {
    static QUEUE: OnceLock<Arc<WorkQueue>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let queue = Arc::new(WorkQueue::new(false));
        let workers = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
        let mut alive = false;
        for i in 0..workers {
            let worker_queue = queue.clone();
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
                        let p = worker_queue.recv();
                        p.make();
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            match spawned {
                Ok(_) => alive = true,
                Err(e) => tracing::warn!(error = %e, "no pipeline worker"),
            }
        }
        queue.workers_alive.store(alive, Ordering::Release);
        queue
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
        assert!(p.is_queued());
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn priority_queue_runs_ready_and_promoted_before_early() {
        let q = WorkQueue::new(true);
        let waiting = AtomicU64::new(0);
        let peak = AtomicU64::new(0);

        let e1 = Arc::new(Pending::new(Vec::new(), None));
        let e2 = Arc::new(Pending::new(Vec::new(), None));
        let e3 = Arc::new(Pending::new(Vec::new(), None));
        let d1 = Arc::new(Pending::new(Vec::new(), None));

        enqueue_on(&e1, false, &waiting, &peak, &q);
        enqueue_on(&e2, false, &waiting, &peak, &q);
        enqueue_on(&e3, false, &waiting, &peak, &q);
        assert_eq!(waiting.load(Ordering::Relaxed), 3);

        // Promoting an unstarted early pipeline pushes it onto demand without double-counting.
        enqueue_on(&e2, true, &waiting, &peak, &q);
        assert_eq!(waiting.load(Ordering::Relaxed), 3);

        // A direct demand pipeline queues ahead of remaining early pipelines too.
        enqueue_on(&d1, true, &waiting, &peak, &q);
        assert_eq!(waiting.load(Ordering::Relaxed), 4);
        assert_eq!(peak.load(Ordering::Relaxed), 4);

        let pop_ptr = || q.try_pop().map(|p| Arc::as_ptr(&p));
        assert_eq!(pop_ptr(), Some(Arc::as_ptr(&e2)));
        assert_eq!(pop_ptr(), Some(Arc::as_ptr(&d1)));
        assert_eq!(pop_ptr(), Some(Arc::as_ptr(&e1)));
        assert_eq!(pop_ptr(), Some(Arc::as_ptr(&e3)));
        assert_eq!(pop_ptr(), None, "promoted duplicate in early lane must be skipped");

        // Re-requesting an already claimed pipeline does not re-queue it.
        enqueue_on(&e2, true, &waiting, &peak, &q);
        assert_eq!(waiting.load(Ordering::Relaxed), 4);
        assert!(q.try_pop().is_none());
    }

    #[test]
    fn repeated_demand_jumps_ahead_of_hundreds_of_single_hit_pipelines() {
        let q = WorkQueue::new(true);
        let waiting = AtomicU64::new(0);
        let peak = AtomicU64::new(0);

        let one_hit: Vec<_> = (0..500)
            .map(|i| Arc::new(Pending::with_non_mesh(Vec::new(), None, i % 2 == 0)))
            .collect();
        for p in &one_hit {
            enqueue_on(p, true, &waiting, &peak, &q);
        }
        let hot = Arc::new(Pending::with_non_mesh(Vec::new(), None, false));
        let hotter = Arc::new(Pending::with_non_mesh(Vec::new(), None, false));
        enqueue_on(&hot, true, &waiting, &peak, &q);
        enqueue_on(&hot, true, &waiting, &peak, &q);
        for _ in 0..4 {
            enqueue_on(&hotter, true, &waiting, &peak, &q);
        }
        assert_eq!(waiting.load(Ordering::Relaxed), 502);

        assert_eq!(q.try_pop().map(|p| Arc::as_ptr(&p)), Some(Arc::as_ptr(&hotter)));
        assert_eq!(q.try_pop().map(|p| Arc::as_ptr(&p)), Some(Arc::as_ptr(&hot)));

        let mut drained_single = 0usize;
        while let Some(p) = q.try_pop() {
            assert_ne!(Arc::as_ptr(&p), Arc::as_ptr(&hot));
            assert_ne!(Arc::as_ptr(&p), Arc::as_ptr(&hotter));
            drained_single += 1;
        }
        assert_eq!(drained_single, 500, "every 1-hit pipeline must still complete");
    }

    #[test]
    fn tie_breaks_favor_non_mesh_and_fair_stride_prevents_starvation() {
        let q = WorkQueue::new(true);
        let waiting = AtomicU64::new(0);
        let peak = AtomicU64::new(0);

        let mesh_a = Arc::new(Pending::with_non_mesh(Vec::new(), None, false));
        let mesh_b = Arc::new(Pending::with_non_mesh(Vec::new(), None, false));
        let compute_a = Arc::new(Pending::with_non_mesh(Vec::new(), None, true));
        let pixel_b = Arc::new(Pending::with_non_mesh(Vec::new(), None, true));

        enqueue_on(&mesh_a, true, &waiting, &peak, &q);
        enqueue_on(&mesh_b, true, &waiting, &peak, &q);
        enqueue_on(&compute_a, true, &waiting, &peak, &q);
        enqueue_on(&pixel_b, true, &waiting, &peak, &q);

        assert_eq!(q.try_pop().map(|p| Arc::as_ptr(&p)), Some(Arc::as_ptr(&compute_a)));
        assert_eq!(q.try_pop().map(|p| Arc::as_ptr(&p)), Some(Arc::as_ptr(&pixel_b)));
        assert_eq!(q.try_pop().map(|p| Arc::as_ptr(&p)), Some(Arc::as_ptr(&mesh_a)));
        assert_eq!(q.try_pop().map(|p| Arc::as_ptr(&p)), Some(Arc::as_ptr(&mesh_b)));
        assert!(q.try_pop().is_none());

        // Anti-starvation: after `FAIR_STRIDE` consecutive high-tier pops, one oldest
        // 1-hit item is served instead of starving forever under continuous repeat demand.
        let old_one_hit = Arc::new(Pending::with_non_mesh(Vec::new(), None, false));
        enqueue_on(&old_one_hit, true, &waiting, &peak, &q);
        let hot: Vec<_> = (0..=FAIR_STRIDE as usize)
            .map(|_| {
                let p = Arc::new(Pending::with_non_mesh(Vec::new(), None, true));
                enqueue_on(&p, true, &waiting, &peak, &q);
                enqueue_on(&p, true, &waiting, &peak, &q);
                p
            })
            .collect();

        for _ in 0..FAIR_STRIDE {
            let popped = q.try_pop().expect("high-tier pop");
            assert_ne!(Arc::as_ptr(&popped), Arc::as_ptr(&old_one_hit));
        }
        assert_eq!(
            q.try_pop().map(|p| Arc::as_ptr(&p)),
            Some(Arc::as_ptr(&old_one_hit)),
            "oldest 1-hit pipeline must make forward progress after FAIR_STRIDE high pops"
        );
        assert_eq!(
            q.try_pop().map(|p| Arc::as_ptr(&p)),
            Some(Arc::as_ptr(&hot[FAIR_STRIDE as usize]))
        );
        assert!(q.try_pop().is_none());
    }
}
