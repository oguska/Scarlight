//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`victory-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection. Blocks found
//! unmapped are not asked about again until it maps them: view memory
//! comes from placeholders, which only the runtime's mapping fills.
//! shortcut: so memory committed otherwise into such a block (a range
//! reaching from a view into a plain reservation) is not seen.
//!
//! shortcut: writes into physical pages through another view (xmem's
//! `write_physical`, DirectStorage's page destinations) are not seen at
//! the guarded addresses; they go to the texture pool, whose textures are
//! hashed, not guarded. A system call writing into a guarded block fails
//! (none is known to).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use crate::writeback::Done;

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

struct Journal {
    /// Numbers the askings; 0 is "never".
    asked: u64,
    /// Page number → the asking that last found it written.
    written: BTreeMap<usize, u64>,
    /// Room for the kernel's answer.
    found: Vec<usize>,
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    asked: 0,
    written: BTreeMap::new(),
    found: Vec::new(),
});

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    // Guarded blocks all still clean: nothing was written there, and the
    // answer is the journal's. Asking the kernel would fail (views are not
    // write-watched) and `guard`'s query walks the view's pages; for a big
    // clean view both cost more than the copy they decide. Blocks are
    // pages, so `first..last` are the blocks. `JOURNAL` is taken before
    // `GUARDS` here as in `guard`; nothing takes them the other way round.
    if guarded && first < last && guards().clean.range(first..last).count() == last - first {
        j.asked += 1;
        return Some((
            j.asked,
            j.written
                .range(first..last)
                .map(|(&p, &w)| (p, w))
                .collect(),
        ));
    }
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    #[cfg(test)]
    tests::KERNEL_ASKED.with(|n| n.set(n.get() + 1));
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            j.found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    let guarded = if failed {
        count = 0;
        if !guarded {
            return None;
        }
        guard(first * PAGE, last * PAGE)?
    } else {
        Vec::new()
    };
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
    }
    j.asked += 1;
    let now = j.asked;
    let pages = j.found[..count].iter().map(|&at| at / PAGE).chain(guarded);
    for page in pages.collect::<Vec<_>>() {
        j.written.insert(page, now);
    }
    Some((
        now,
        j.written
            .range(first..last)
            .map(|(&p, &w)| (p, w))
            .collect(),
    ))
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;
// `writes` reads blocks as pages.
const _: () = assert!(BLOCK == PAGE);

struct Guards {
    /// Blocks we made read-only.
    protected: BTreeSet<usize>,
    /// Of those, the ones not written since a copy took them.
    clean: BTreeSet<usize>,
    /// Blocks found unmapped (not committed) when a copy took them: the
    /// copy holds their zeros until the runtime maps them
    /// ([`title_memory_remapped`]). Without this, every asking found them
    /// "written" again (they cannot be made read-only) and the copy read
    /// them page by page through the kernel's failing reads, hundreds of
    /// MB per world frame.
    absent: BTreeSet<usize>,
    /// Blocks found to be in a view when an asking began there, until the
    /// runtime remaps them. Asking the system each time (`VirtualQuery`)
    /// cost 100-230 ms per world frame, more than the rest of the guard.
    views: BTreeSet<usize>,
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    clean: BTreeSet::new(),
    absent: BTreeSet::new(),
    views: BTreeSet::new(),
});

fn guards() -> std::sync::MutexGuard<'static, Guards> {
    GUARDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    install();
    let mut g = guards();
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // Asked under the lock: a remap in between would leave a stale view.
    if !g.views.contains(&first) {
        if !view(start) {
            return None;
        }
        g.views.insert(first);
    }
    let mut written = Vec::new();
    let mut run: Option<usize> = None;
    for block in first..=last {
        let open = block < last && !g.clean.contains(&block) && !g.absent.contains(&block);
        if open {
            written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            if g.protected.contains(&block) {
                // Written through the runtime, still read-only.
                g.clean.insert(block);
            } else {
                run.get_or_insert(block);
                continue;
            }
        }
        if let Some(from) = run.take() {
            protect(&mut g, from, block);
        }
    }
    Some(written)
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, leaving out any that fail, and
/// noting those not committed (unmapped) absent.
fn protect(g: &mut Guards, from: usize, to: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title view memory; only its protection changes.
        unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        }
    };
    let note = |g: &mut Guards, from: usize, to: usize| {
        g.protected.extend(from..to);
        g.clean.extend(from..to);
    };
    if set(from, to) {
        note(g, from, to);
        return;
    }
    for block in from..to {
        if set(block, block + 1) {
            note(g, block, block + 1);
        } else if unmapped(block * BLOCK) {
            g.absent.insert(block);
        }
    }
}

/// Whether `at` is in a view (of a section).
fn view(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery};
    #[cfg(test)]
    tests::QUERIED.with(|n| n.set(n.get() + 1));
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.Type == MEM_MAPPED
}

/// Whether the whole block at `at` is uncommitted (a placeholder between
/// views). The kernel's answer covers only the region starting there: one
/// whose tail is committed is not absent, since its pages would fault
/// while nothing guards them.
fn unmapped(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State != MEM_COMMIT && info.RegionSize >= BLOCK
}

/// Blocks a write-back job is putting the GPU's bytes back into
/// (`VICTORY_ASYNC_WRITEBACK`): their protection while it does, and the
/// job. A title thread that reads or writes one of them faults, waits for
/// the job and retries, so it never sees the bytes the GPU replaced.
static BACK: Mutex<BTreeMap<usize, (u32, Arc<Done>)>> = Mutex::new(BTreeMap::new());

fn back() -> std::sync::MutexGuard<'static, BTreeMap<usize, (u32, Arc<Done>)>> {
    BACK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Makes the blocks of `[at, at + len)` inaccessible until `done` finishes
/// (a title access waits for it), returning the blocks taken. Committed
/// blocks only; one an earlier job still holds is left to that job.
pub(crate) fn back_begin(done: &Arc<Done>, at: usize, len: usize) -> Vec<usize> {
    use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, VirtualProtect};
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    let mut taken = Vec::new();
    let mut back = back();
    for block in first..last {
        if let Some(&(old, _)) = back.get(&block) {
            // An earlier job still holds it: the hold passes to this job, so
            // it stays inaccessible until this one has written too. The
            // earlier job's release will not lift it (see `back_release`).
            back.insert(block, (old, done.clone()));
            taken.push(block);
            continue;
        }
        let mut old = 0;
        // SAFETY: title memory; only its protection changes.
        let ok = unsafe {
            VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_NOACCESS, &mut old)
        } != 0;
        if ok {
            back.insert(block, (old, done.clone()));
            taken.push(block);
        }
    }
    taken
}

/// After the job: gives the blocks back the protection they had. Only the
/// blocks it still holds: one a later job took ([`back_begin`]) stays
/// inaccessible until that job is done, and that job restores it.
pub(crate) fn back_release(blocks: &[usize], done: &Arc<Done>) {
    use windows_sys::Win32::System::Memory::VirtualProtect;
    let mut back = back();
    for &block in blocks {
        if !back
            .get(&block)
            .is_some_and(|(_, d)| std::sync::Arc::ptr_eq(d, done))
        {
            continue;
        }
        if let Some((old, _)) = back.remove(&block) {
            let mut was = 0;
            // SAFETY: a block this job made inaccessible.
            unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, old, &mut was) };
        }
    }
}

/// The job, if any, whose write-back holds block `at / BLOCK`: a thread
/// that faulted there waits for it.
pub(crate) fn back_pending(at: usize) -> Option<Arc<Done>> {
    back().get(&(at / BLOCK)).map(|(_, done)| done.clone())
}

/// Before a kernel write into title memory `[at, at + len)`: the blocks are
/// given back writable, as [`open`] does for the guarded ones (that write is
/// the title's, so nothing is waiting for the job there any more).
pub(crate) fn back_open(at: usize, len: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    let mut opened = Vec::new();
    {
        let mut back = back();
        for block in first..last {
            if back.remove(&block).is_some() {
                let mut old = 0;
                // SAFETY: a block a write-back job made inaccessible.
                unsafe {
                    VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old)
                };
                opened.push(block);
            }
        }
    }
    // The bytes at `[at, at + len)` are the caller's now. A guarded block in
    // range must not stay recorded as clean: the kernel's write did not fault
    // into `open`, and the next copy would take the block as unchanged and
    // skip it.
    if !opened.is_empty() {
        let mut g = guards();
        for block in opened {
            g.protected.remove(&block);
            g.clean.remove(&block);
        }
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded.
fn open(g: &mut Guards, start: usize, end: usize) -> bool {    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let blocks: Vec<usize> = g
        .protected
        .range(start / BLOCK..end.div_ceil(BLOCK))
        .copied()
        .collect();
    for &block in &blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
        let mut old = 0;
        // SAFETY: a block we made read-only.
        unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old) };
    }
    !blocks.is_empty()
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler): opens its guarded blocks.
/// The runtime calls it (as `kernel_will_write`) before the title's kernel
/// reads into its own memory.
pub fn writing(at: usize, len: usize) {
    // The length is the title's (`fread`'s size times count, saturated).
    // A block a write-back job holds is this write's now.
    back_open(at, len);
    open(&mut guards(), at, at.saturating_add(len));
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new and read-write, so it is no longer guarded (nor known
/// unmapped).
pub fn title_memory_remapped(at: usize, len: usize) {
    let mut g = guards();
    let end = at.saturating_add(len);
    let range = at / BLOCK..end.div_ceil(BLOCK);
    let absent: Vec<usize> = g.absent.range(range.clone()).copied().collect();
    for block in absent {
        g.absent.remove(&block);
    }
    let views: Vec<usize> = g.views.range(range).copied().collect();
    for block in views {
        g.views.remove(&block);
    }
    // Write access comes back before the records go: a block the range
    // only partly covers stays ours, and a write to a page of it we did
    // not touch must not find no record (then it is not handled). Blocks
    // unmapped since are only forgotten: their address may belong to
    // another mapping by now, which is not ours to open.
    let gone: Vec<usize> = g
        .protected
        .range(at / BLOCK..end.div_ceil(BLOCK))
        .copied()
        .filter(|&block| !committed(block * BLOCK))
        .collect();
    for block in gone {
        g.protected.remove(&block);
        g.clean.remove(&block);
    }
    open(&mut g, at, end);
}

/// Whether `at` is committed memory (a block still mapped where we left it).
fn committed(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS || r.NumberParameters < 2 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let is_write = r.ExceptionInformation[0] == WRITE;
        let at = r.ExceptionInformation[1];
        // A write-back job has this block (it is inaccessible to both reads
        // and writes): wait for the GPU's bytes to be put back before the
        // instruction reads or writes them.
        if let Some(done) = back_pending(at) {
            // The worker itself must never wait for its own job: it opens the
            // block and goes on (its write is the one to wait for).
            if crate::writeback::this_is_worker() {
                back_open(at, 1);
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            #[cfg(feature = "diagnostics")]
            let started = std::time::Instant::now();
            done.wait();
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::add(crate::copystat::C::AsyncWaits, 1);
                crate::copystat::add(
                    crate::copystat::C::AsyncWaitUs,
                    started.elapsed().as_micros() as u64,
                );
            }
            // The job released the block (or wrote it, through the kernel).
            // Retry: if a newer job holds it again, the fault comes back here.
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if !is_write {
            // A read of memory that is not ours: as before, someone else's.
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // Held across both checks: another thread may have opened the block
        // since this one faulted, and a copy may have guarded it again since
        // (then it is ours again, not read-only for some other reason).
        let mut g = guards();
        if open(&mut g, at, at + 1) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::GuardFaults, 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if writable(at) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// A write-back job's blocks are inaccessible to the title until the
    /// job released them; a thread that faults there waits for the job.
    #[test]
    fn write_back_blocks_wait_for_their_job() {
        use std::sync::Arc;
        // SAFETY: two blocks of private memory, freed at the end.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * BLOCK,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            ) as usize
        };
        assert_ne!(at, 0);
        let done = Arc::new(Done::default());
        let blocks = back_begin(&done, at, 2 * BLOCK);
        assert_eq!(blocks, [at / BLOCK, at / BLOCK + 1]);
        assert!(!writable(at), "held inaccessible while the job runs");
        assert!(back_pending(at + BLOCK).is_some());
        done.finish();
        back_release(&blocks, &done);
        assert!(writable(at), "given back after the job");
        assert!(back_pending(at).is_none());
        // SAFETY: the memory allocated above.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// A block a job holds passes to the next job that declares it: the
    /// earlier job's release does not give it back while the later job's
    /// copy-back is still queued.
    #[test]
    fn a_held_block_passes_to_the_next_job() {
        // SAFETY: one block of private memory, freed at the end.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                BLOCK,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            ) as usize
        };
        assert_ne!(at, 0);
        let first = Arc::new(Done::default());
        let second = Arc::new(Done::default());
        let taken = back_begin(&first, at, BLOCK);
        assert_eq!(taken, [at / BLOCK]);
        let taken_again = back_begin(&second, at, BLOCK);
        assert_eq!(taken_again, [at / BLOCK], "the later job takes the block");
        first.finish();
        back_release(&taken, &first);
        assert!(!writable(at), "still inaccessible: the later job's bytes are not in");
        second.finish();
        back_release(&taken_again, &second);
        assert!(writable(at), "given back by the job that last held it");
        // SAFETY: the memory allocated above.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    thread_local! {
        /// The kernel askings `writes` made on this thread (each test runs
        /// on its own).
        pub(super) static KERNEL_ASKED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// The view queries `guard` made on this thread (each test runs on
        /// its own).
        pub(super) static QUERIED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn kernel_asked() -> usize {
        KERNEL_ASKED.with(|n| n.get())
    }

    fn queried() -> usize {
        QUERIED.with(|n| n.get())
    }

    /// A pagefile section of `blocks` blocks, mapped read-write.
    fn view(blocks: usize) -> usize {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile};
        // SAFETY: a fresh section; the view is ours.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (blocks * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, blocks * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        view
    }

    /// Unmaps a view from [`view`], first forgetting its pages' journal
    /// entries: the next allocation there is not written.
    fn unmap(view: usize, blocks: usize) {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        let pages = view / PAGE..(view + blocks * BLOCK) / PAGE;
        JOURNAL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .written
            .retain(|p, _| !pages.contains(p));
        // Told first: after the unmap the address may belong to another
        // test's view, which is not ours to open.
        title_memory_remapped(view, blocks * BLOCK);
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
    }

    /// A clean guarded view is answered from the journal, without the
    /// kernel; a write to it (by the title, or announced) is still seen.
    #[test]
    fn clean_guarded_views_are_answered_from_the_journal() {
        let v = view(4);
        let (start, end) = (v, v + 4 * BLOCK);
        let first = v / PAGE;
        let asked = kernel_asked();
        // The first asking guards every block: all are written as far as
        // anyone knows.
        let (taken, all) = writes(start, end, true).expect("a view");
        assert_eq!(kernel_asked(), asked + 1);
        assert_eq!(all, (first..first + 4).map(|p| (p, taken)).collect::<Vec<_>>());
        // Clean since: the kernel is not asked, the journal answers as before.
        for _ in 0..3 {
            let (now, same) = writes(start, end, true).expect("a view");
            assert!(now > taken);
            assert_eq!(same, all);
        }
        let (_, part) = writes(v + BLOCK + 10, v + 2 * BLOCK - 10, true).expect("a view");
        assert_eq!(part, [(first + 1, taken)]);
        assert_eq!(kernel_asked(), asked + 1);

        // The title writes block 2: the handler opens it; the kernel is asked.
        // SAFETY: our view, guarded read-only; the handler gives write access.
        unsafe { ((v + 2 * BLOCK + 8) as *mut u8).write(7) };
        let (written, after) = writes(start, end, true).expect("a view");
        assert_eq!(kernel_asked(), asked + 2);
        assert_eq!(after[2], (first + 2, written));
        assert_eq!(after.iter().filter(|&&(_, w)| w == written).count(), 1);
        // SAFETY: as above, readable.
        assert_eq!(unsafe { ((v + 2 * BLOCK + 8) as *const u8).read() }, 7);
        // Guarded again, and clean.
        let (_, again) = writes(start, end, true).expect("a view");
        assert_eq!(again, after);
        assert_eq!(kernel_asked(), asked + 2);

        // Our own write through the kernel, announced.
        writing(v + 3 * BLOCK, 1);
        let (announced, after) = writes(start, end, true).expect("a view");
        assert_eq!(kernel_asked(), asked + 3);
        assert_eq!(after[3], (first + 3, announced));

        // The runtime remapped block 0.
        title_memory_remapped(v, BLOCK);
        let (remapped, after) = writes(start, end, true).expect("a view");
        assert_eq!(kernel_asked(), asked + 4);
        assert_eq!(after[0], (first, remapped));
        let (_, settled) = writes(start, end, true).expect("a view");
        assert_eq!(settled, after);
        assert_eq!(kernel_asked(), asked + 4);

        // An empty range is not answered from the guards.
        let _ = writes(v, v, true);
        assert_eq!(kernel_asked(), asked + 5);
        unmap(v, 4);
    }

    /// A range only partly guarded and clean asks the kernel.
    #[test]
    fn partly_clean_ranges_ask_the_kernel() {
        let v = view(2);
        let asked = kernel_asked();
        // Only block 0 is guarded.
        let _ = writes(v, v + BLOCK, true).expect("a view");
        assert_eq!(kernel_asked(), asked + 1);
        let (now, both) = writes(v, v + 2 * BLOCK, true).expect("a view");
        assert_eq!(kernel_asked(), asked + 2);
        assert_eq!(both.last(), Some(&(v / PAGE + 1, now)));
        // Both guarded and clean now.
        let _ = writes(v, v + 2 * BLOCK, true).expect("a view");
        assert_eq!(kernel_asked(), asked + 2);
        // Not guarded askers (texture hashes) never take the journal's word.
        assert!(writes(v, v + 2 * BLOCK, false).is_none());
        assert_eq!(kernel_asked(), asked + 3);
        unmap(v, 2);
    }

    /// A view is asked about once per block an asking starts in, again
    /// only after the runtime remapped it; other memory every time.
    #[test]
    fn views_are_queried_once_until_remapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        let at = queried();
        assert!(guard(view, view + 2 * BLOCK).is_some());
        assert!(guard(view + 8, view + BLOCK).is_some());
        assert_eq!(queried(), at + 1);
        // Another start block is asked about once.
        assert!(guard(view + BLOCK, view + 2 * BLOCK).is_some());
        assert!(guard(view + BLOCK, view + 2 * BLOCK).is_some());
        assert_eq!(queried(), at + 2);
        // The runtime remapped the first block: asked again.
        title_memory_remapped(view, BLOCK);
        assert!(guard(view, view + 2 * BLOCK).is_some());
        assert_eq!(queried(), at + 3);
        // Private memory is not a view, however often asked.
        let plain = vec![0u8; 2 * BLOCK];
        let p = plain.as_ptr() as usize;
        assert!(guard(p, p + 16).is_none());
        assert!(guard(p, p + 16).is_none());
        assert_eq!(queried(), at + 5);
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: view as *mut _,
            })
        };
        title_memory_remapped(view, 2 * BLOCK);
    }

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        let (_, before) = writes(start, end, false).expect("watched");
        assert!(before.is_empty());
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        assert_eq!(after, [(p / PAGE + 1, now)]);
        // Another asker after the kernel's bit was cleared still sees it.
        let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(
            !read(&mut file),
            "the kernel does not fault into our handler"
        );
        writing(view + 100, 13);
        assert!(read(&mut file));
        // SAFETY: the bytes just read.
        let landed = unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) };
        assert_eq!(landed, b"from the disk");
        assert_eq!(
            guard(view, view + 2 * BLOCK),
            Some((view / PAGE..view / PAGE + 1).collect())
        );
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
        title_memory_remapped(view, 2 * BLOCK);
    }

    /// A write or mapping that runs past the end of the address space (a
    /// title's `fread` with a huge count) covers up to the end; the sum used
    /// to overflow. Up there no test has guarded blocks.
    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let top = usize::MAX - 16;
        writing(top, PAGE);
        title_memory_remapped(top, usize::MAX);
    }

    /// Blocks of a placeholder between views are reported once, then not
    /// again (they cannot be guarded) until the runtime maps them.
    #[test]
    fn unmapped_blocks_are_reported_once_until_mapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of two; block 0 shows the section's first block, as the
        // runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let first = map(0);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        let second = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// The protection of the block at `at`, as the kernel reports it.
    fn protection(at: usize) -> u32 {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        assert_ne!(n, 0);
        info.Protect
    }

    /// A remap covering one page of a guarded block gives the whole block
    /// write access again: its other pages are not guarded any more, and a
    /// write to one of them must not find no record.
    #[test]
    fn a_partly_remapped_block_is_writable_again() {
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE};
        let v = view(2);
        let pages = |block: usize| (v + block * BLOCK) / PAGE..(v + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..2).flat_map(pages).collect();
        assert_eq!(guard(v, v + 2 * BLOCK), Some(all));
        assert_eq!(protection(v), PAGE_READONLY);
        // The runtime remaps the block's last page and the next block's
        // first: block 0 is only partly covered.
        title_memory_remapped(v + BLOCK - PAGE, 2 * PAGE);
        assert_eq!(protection(v), PAGE_READWRITE, "block 0 is not guarded");
        assert!(!guards().protected.contains(&(v / BLOCK)));
        unmap(v, 2);
    }

    /// A block whose tail is committed is not absent: its committed pages
    /// would fault with nothing guarding them.
    #[test]
    fn a_partly_committed_block_is_not_absent() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
        // SAFETY: a fresh reservation of two blocks; we commit one page in
        // the first one's tail and free the rest.
        let p = unsafe { VirtualAlloc(std::ptr::null(), 2 * BLOCK, MEM_RESERVE, PAGE_READWRITE) }
            as usize;
        assert_ne!(p, 0);
        assert!(unmapped(p), "a fully reserved block is absent");
        // SAFETY: reserved; commits one kernel page of it.
        let tail = unsafe {
            VirtualAlloc(
                (p + BLOCK - PAGE) as *const _,
                PAGE,
                MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert_eq!(tail, p + BLOCK - PAGE);
        assert!(!unmapped(p), "the first block's tail is committed");
        assert!(unmapped(p + BLOCK), "the second block is whole");
        // SAFETY: our reservation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }
}
