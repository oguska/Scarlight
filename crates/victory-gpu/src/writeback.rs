//! A write-back worker per command queue: what shaders wrote (the copy-back
//! into title memory) and the bottom-of-pipe writes are done on a thread of
//! their own, after the submission's fence, so the submitting thread can
//! prepare the title's next submission meanwhile. Off by default:
//! `VICTORY_ASYNC_WRITEBACK=1` (idea from ivall/xcl#562).
//!
//! Correctness rests on three things:
//! - one queue, one worker, jobs in submission order: a job's copy-back runs
//!   before its bottom-of-pipe writes, and before the next job's, exactly as
//!   the submitting thread did them inline;
//! - the blocks a job puts back are held inaccessible to the title until it
//!   has ([`crate::watch::back_begin`]): a title thread that reads or writes
//!   one takes the access fault and waits for the job, so it cannot see the
//!   bytes the GPU replaced;
//! - a later submission's refresh and snapshot of the same host copy wait
//!   for the range (`Shadow::pending`, tagged with the submission it belongs
//!   to) instead of copying title memory over bytes still to be put back.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::queue::Written;
use crate::shadow::Shadow;

thread_local! {
    /// Whether this thread is the write-back worker.
    static IS_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this thread is the write-back worker (it must not wait for its
/// own job when it faults on a block the job holds).
pub(crate) fn this_is_worker() -> bool {
    IS_WORKER.with(|w| w.get())
}

/// Whether the write-back runs on its own thread (`VICTORY_ASYNC_WRITEBACK=1`).
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_ASYNC_WRITEBACK").as_deref() == Ok("1"))
}

/// A submission's work still to be done after its fence: what shaders wrote
/// goes back into title memory, then the bottom-of-pipe writes tell the
/// title the GPU is done.
pub(crate) struct Job {
    /// Per host copy: `[address, address + len)` to put back, and the
    /// submission it belongs to.
    pub(crate) copies: Vec<(Arc<Shadow>, usize, usize, u64)>,
    /// The submission's bottom-of-pipe writes, after the copy-back.
    pub(crate) writes: Vec<(usize, Written, usize)>,
    /// Blocks [`crate::watch::back_begin`] made inaccessible for the job.
    pub(crate) blocks: Vec<usize>,
    done: Arc<Done>,
}

impl Job {
    pub(crate) fn new() -> Job {
        Job {
            copies: Vec::new(),
            writes: Vec::new(),
            blocks: Vec::new(),
            done: Arc::new(Done::default()),
        }
    }

    /// The flag a title thread waits on before it touches a block of the job.
    pub(crate) fn done(&self) -> Arc<Done> {
        self.done.clone()
    }
}

/// Whether a job has finished, for threads that faulted on its blocks.
#[derive(Default)]
pub(crate) struct Done {
    finished: Mutex<bool>,
    ready: Condvar,
}

impl Done {
    pub(crate) fn finish(&self) {
        *self.finished.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ready.notify_all();
    }

    /// Waits until the job's copy-back and writes are done.
    pub(crate) fn wait(&self) {
        let mut finished = self.finished.lock().unwrap_or_else(|e| e.into_inner());
        while !*finished {
            finished = self.ready.wait(finished).unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct Worker {
    jobs: mpsc::Sender<Job>,
}

/// The queue's worker, started on first use.
fn worker() -> &'static Worker {
    static WORKER: OnceLock<Worker> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (jobs, work) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("victory-gpu-writeback".into())
            .spawn(move || {
                IS_WORKER.with(|w| w.set(true));
                for job in work {
                    #[cfg(feature = "diagnostics")]
                    let started = std::time::Instant::now();
                    // The blocks are inaccessible to the title until the job
                    // has put the GPU's bytes back (and written its fences).
                    for (shadow, address, len, submission) in &job.copies {
                        let written = shadow.copy_back(*address, *len, *submission);
                        crate::host::COPY_BACK[1].fetch_add(
                            written as u64,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                    for (at, what, bytes) in &job.writes {
                        // The job's blocks are the job's to write: opened
                        // first, or a CPU write there would fault into the
                        // handler and wait for this very job.
                        crate::watch::back_open(*at, *bytes);
                        what.write(*at as *mut u8, *bytes);
                    }
                    crate::watch::back_release(&job.blocks);
                    job.done.finish();
                    #[cfg(feature = "diagnostics")]
                    {
                        crate::copystat::add(crate::copystat::C::AsyncJobs, 1);
                        crate::copystat::add(
                            crate::copystat::C::AsyncUs,
                            started.elapsed().as_micros() as u64,
                        );
                    }
                }
            })
            .expect("write-back worker");
        Worker { jobs }
    })
}

/// Queues `job` for the worker; the caller must have called
/// [`crate::watch::back_begin`] for its copy-back ranges first.
pub(crate) fn submit(job: Job) {
    if worker().jobs.send(job).is_err() {
        // The worker is gone (only at process exit): nothing to wait for.
    }
}
