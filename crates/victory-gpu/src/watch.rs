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
//! ([`title_memory_remapped`]), which resets their protection.
//!
//! shortcut: writes into physical pages through another view (xmem's
//! `write_physical`, DirectStorage's page destinations) are not seen at
//! the guarded addresses; they go to the texture pool, whose textures are
//! hashed, not guarded. A system call writing into a guarded block fails
//! (none is known to).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

/// Whether every watch page of `[start, end)` is in one committed private
/// region. A failed query for a subrange then cannot have crossed a region
/// boundary or encountered an unmapped page elsewhere in the shadow.
pub(crate) fn private_region_covers(
    start: usize,
    end: usize,
    observed: Option<MemoryRegion>,
) -> bool {
    let start = start / PAGE * PAGE;
    let Some(end) = end.checked_next_multiple_of(PAGE) else {
        return false;
    };
    observed
        .filter(|region| region.contains(start))
        .or_else(|| memory_region(start))
        .is_some_and(|region| region.committed_private && region.end >= end)
}

#[derive(Clone, Copy)]
pub(crate) struct MemoryRegion {
    start: usize,
    pub(crate) end: usize,
    pub(crate) committed_private: bool,
    pub(crate) reserved: bool,
    mapped: bool,
}

impl MemoryRegion {
    pub(crate) fn contains(self, address: usize) -> bool {
        (self.start..self.end).contains(&address)
    }
}

/// The forward extent of the current mapping with the same attributes.
pub(crate) fn memory_region(start: usize) -> Option<MemoryRegion> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // SAFETY: a query of the virtual address, without reading its memory.
    unsafe {
        let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
        if VirtualQuery(start as *const _, &mut info, size_of_val(&info)) == 0 {
            return None;
        }
        let low = info.BaseAddress as usize;
        let end = low.checked_add(info.RegionSize)?;
        (low <= start && end > start).then_some(MemoryRegion {
            start: low,
            end,
            committed_private: info.State == MEM_COMMIT && info.Type == MEM_PRIVATE,
            reserved: info.State == MEM_RESERVE,
            mapped: info.Type == MEM_MAPPED,
        })
    }
}

static MAPPING_REVISION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
pub(crate) struct Allocation {
    pub(crate) start: usize,
    pub(crate) end: usize,
    private: bool,
    revision: u64,
}

impl Allocation {
    pub(crate) fn is_live(self) -> bool {
        self.revision == MAPPING_REVISION.load(Ordering::Acquire)
    }

    fn contains(self, first: usize, end: usize) -> bool {
        self.start <= first && end <= self.end && self.is_live()
    }

    pub(crate) fn is_private_span(self, first: usize, end: usize) -> bool {
        self.private && self.contains(first / PAGE * PAGE, end.div_ceil(PAGE) * PAGE)
    }
}

/// A Windows reservation extent, independent of individual page commitment
/// and protection. Resolve this optionally so older Win32 providers still
/// use the non-reset GetWriteWatch preflight.
pub(crate) fn allocation(address: usize) -> Option<Allocation> {
    use std::{ffi::c_void, sync::OnceLock};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::{
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Memory::{
            MemoryRegionInfo, WIN32_MEMORY_INFORMATION_CLASS, WIN32_MEMORY_REGION_INFORMATION,
        },
        Threading::GetCurrentProcess,
    };
    use windows_sys::core::BOOL;

    type Query = unsafe extern "system" fn(
        HANDLE,
        *const c_void,
        WIN32_MEMORY_INFORMATION_CLASS,
        *mut c_void,
        usize,
        *mut usize,
    ) -> BOOL;
    static QUERY: OnceLock<Option<Query>> = OnceLock::new();
    let query = (*QUERY.get_or_init(|| {
        // SAFETY: the strings are terminated and we look up the documented
        // Win32 function in an already loaded system module.
        unsafe {
            let library = GetModuleHandleW(windows::core::w!("kernelbase.dll").as_ptr());
            if library.is_null() {
                return None;
            }
            GetProcAddress(library, c"QueryVirtualMemoryInformation".as_ptr().cast())
                .map(|entry| std::mem::transmute::<_, Query>(entry))
        }
    }))?;
    let revision = MAPPING_REVISION.load(Ordering::Acquire);
    let mut info = WIN32_MEMORY_REGION_INFORMATION::default();
    // SAFETY: MemoryRegionInfo writes this sized structure for our process.
    unsafe {
        if query(
            GetCurrentProcess(),
            address as *const _,
            MemoryRegionInfo,
            (&raw mut info).cast(),
            size_of_val(&info),
            std::ptr::null_mut(),
        ) == 0
        {
            return None;
        }
        let start = info.AllocationBase as usize;
        let end = start.checked_add(info.RegionSize)?;
        // The Private bit is the first field of the Win32 region union.
        // Wine leaves the union's flags zero for every region; there the
        // allocation type comes from VirtualQuery instead.
        let mut private = info.Anonymous.Anonymous._bitfield & 1 != 0;
        if info.Anonymous.Anonymous._bitfield == 0 {
            use windows_sys::Win32::System::Memory::{
                MEM_PRIVATE, MEMORY_BASIC_INFORMATION, VirtualQuery,
            };
            let mut basic = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: a query about an address; nothing is dereferenced.
            let n = VirtualQuery(address as *const _, &mut basic, size_of_val(&basic));
            private = n != 0 && basic.Type == MEM_PRIVATE;
        }
        (start <= address && address < end).then_some(Allocation {
            start,
            end,
            private,
            revision,
        })
    }
}

const CHUNK_PAGES: usize = 64;

struct ChangeChunk {
    latest: u64,
    pages: [u64; CHUNK_PAGES],
}

impl ChangeChunk {
    fn new() -> Self {
        Self {
            latest: 0,
            pages: [0; CHUNK_PAGES],
        }
    }
}

struct Journal {
    /// Numbers watch queries and remap notifications; 0 is "never".
    asked: u64,
    /// Page generations indexed by chunks. A chunk's latest change lets a
    /// reader skip all of its older entries without walking those pages.
    changes: BTreeMap<usize, ChangeChunk>,
    /// Room for the kernel's answer.
    found: Vec<usize>,
}

impl Journal {
    fn record(
        changes: &mut BTreeMap<usize, ChangeChunk>,
        first: usize,
        last: usize,
        generation: u64,
    ) {
        for index in first / CHUNK_PAGES..last.div_ceil(CHUNK_PAGES) {
            let base = index * CHUNK_PAGES;
            let lo = first.saturating_sub(base);
            let hi = (last - base).min(CHUNK_PAGES);
            let chunk = changes.entry(index).or_insert_with(ChangeChunk::new);
            chunk.latest = generation;
            chunk.pages[lo..hi].fill(generation);
        }
    }

    fn since(&self, first: usize, last: usize, after: u64) -> (u64, Vec<(usize, u64)>) {
        let mut pages = Vec::new();
        for (&index, chunk) in self
            .changes
            .range(first / CHUNK_PAGES..last.div_ceil(CHUNK_PAGES))
            .filter(|(_, chunk)| chunk.latest > after)
        {
            let base = index * CHUNK_PAGES;
            let lo = first.saturating_sub(base);
            let hi = (last - base).min(CHUNK_PAGES);
            pages.extend(
                chunk.pages[lo..hi]
                    .iter()
                    .enumerate()
                    .filter(|&(_, &generation)| generation > after)
                    .map(|(offset, &generation)| (base + lo + offset, generation)),
            );
        }
        (self.asked, pages)
    }
}

#[cfg(test)]
thread_local! {
    static KERNEL_WATCH_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    asked: 0,
    changes: BTreeMap::new(),
    found: Vec::new(),
});

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    query(start, end, guarded, false, 0, None)
}

/// Export the first mapping checked for the guard fallback to buffer
/// refreshes, which can reuse it without another VirtualQuery.
pub(crate) fn shadow_writes(
    start: usize,
    end: usize,
    after: u64,
    region: &mut Option<MemoryRegion>,
) -> Option<(u64, Vec<(usize, u64)>)> {
    query(start, end, true, false, after, Some(region))
}

/// One fresh observation for a group of requested ranges. The caller
/// uses it only within this preparation pass. When the reservation has
/// been verified we can skip the non-reset probe across its own pages.
pub(crate) fn grouped_writes(
    start: usize,
    end: usize,
    after: u64,
    allocation: Option<Allocation>,
) -> Option<(u64, Vec<(usize, u64)>)> {
    let first = start / PAGE;
    let last = end.div_ceil(PAGE);
    match allocation.filter(|a| a.contains(first * PAGE, last * PAGE)) {
        Some(a) if a.private => query(start, end, false, false, after, None),
        Some(_) => {
            let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
            j.clean_guarded(first, last, after)
        }
        None => query(start, end, false, true, after, None),
    }
}

/// Asks for the written pages of `[start, end)`, one question per allocation:
/// the kernel refuses a question that crosses allocations (under Wine it
/// always does), which left whole buffers unwatched and copied again every
/// submission. A part that cannot be answered leaves the whole answer
/// unknown, as before.
fn query(
    start: usize,
    end: usize,
    guarded: bool,
    preflight: bool,
    after: u64,
    mut first_region: Option<&mut Option<MemoryRegion>>,
) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let parts = page_parts(first, last);
    match parts.as_slice() {
        [] | [_] => ask_span(first, last, guarded, preflight, after, first_region),
        parts => {
            // Every part is asked (and its dirty bits cleared) before the
            // answer is merged, so no page is reported twice or lost.
            let mut now = 0;
            let mut pages = Vec::new();
            for (i, &(from, to)) in parts.iter().enumerate() {
                let region = if i == 0 { first_region.take() } else { None };
                let (asked, found) = ask_span(from, to, guarded, preflight, after, region)?;
                now = now.max(asked);
                pages.extend(found);
            }
            pages.sort_unstable_by_key(|&(page, _)| page);
            Some((now, pages))
        }
    }
}

/// The page ranges of `[first, last)`, cut at mapping region boundaries (a
/// region is inside one allocation): each is asked on its own.
fn page_parts(first: usize, last: usize) -> Vec<(usize, usize)> {
    let mut parts = Vec::new();
    let mut page = first;
    while page < last {
        let end = memory_region(page * PAGE).map_or(last, |r| r.end / PAGE);
        let end = end.max(page + 1).min(last);
        parts.push((page, end));
        page = end;
    }
    parts
}

/// One question for the whole of `[first, last)` (pages), which must lie in
/// one allocation.
fn ask_span(
    first: usize,
    last: usize,
    guarded: bool,
    preflight: bool,
    after: u64,
    first_region: Option<&mut Option<MemoryRegion>>,
) -> Option<(u64, Vec<(usize, u64)>)> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    if guarded && let Some(answer) = j.clean_guarded(first, last, after) {
        return Some(answer);
    }
    let capacity = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(capacity.max(j.found.len()), 0);
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let mut ask = |flags| {
        #[cfg(test)]
        KERNEL_WATCH_CALLS.with(|calls| calls.set(calls.get() + 1));
        let mut count = capacity;
        let mut granularity = 0;
        // SAFETY: the answer buffer holds `capacity` entries.
        let result = unsafe {
            GetWriteWatch(
                flags,
                (first * PAGE) as *const _,
                (last - first) * PAGE,
                j.found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        (result == 0).then_some(count)
    };
    // Flags zero tests the full span without clearing its dirty bits. The
    // second call collects and resets in one operation, including writes
    // that arrived after the probe. The journal lock serializes our askers.
    if preflight && ask(0).is_none() {
        return None;
    }
    let (count, guarded) = match ask(WRITE_WATCH_FLAG_RESET) {
        Some(count) => (count, Vec::new()),
        None if guarded => {
            // A positive section-view lookup remains valid until a runtime
            // mapping change. Dirty guards need not rewalk the view.
            if !guards().views.contains(&first) {
                let region = memory_region(first * PAGE)?;
                if let Some(out) = first_region {
                    *out = Some(region);
                }
                if !region.mapped || region.reserved {
                    return None;
                }
                guards().views.insert(first);
            }
            (0, guard_blocks(first * PAGE, last * PAGE))
        }
        None => return None,
    };
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
    }
    j.asked += 1;
    let now = j.asked;
    let Journal { changes, found, .. } = j;
    for page in found[..count].iter().map(|&at| at / PAGE).chain(guarded) {
        Journal::record(changes, page, page + 1, now);
    }
    Some(j.since(first, last, after))
}

impl Journal {
    /// Every protected page in this interval is already tracked by our
    /// guards. Writes, runtime write preparation, and remaps invalidate
    /// that proof. Lock order is journal then guards, as on the slow path.
    fn clean_guarded(
        &mut self,
        first: usize,
        last: usize,
        after: u64,
    ) -> Option<(u64, Vec<(usize, u64)>)> {
        if first == last || guards().clean.range(first..last).count() != last - first {
            return None;
        }
        self.asked += 1;
        Some(self.since(first, last, after))
    }
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;
const _: () = assert!(BLOCK == PAGE);

struct Guards {
    /// Blocks we made read-only.
    protected: BTreeSet<usize>,
    /// Of those, the ones not written since a copy took them.
    clean: BTreeSet<usize>,
    /// Starting blocks positively identified as section views. Remap and
    /// allocation notices invalidate these observations.
    views: BTreeSet<usize>,
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    clean: BTreeSet::new(),
    views: BTreeSet::new(),
});

fn guards() -> std::sync::MutexGuard<'static, Guards> {
    GUARDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
#[cfg(test)]
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    memory_region(start)
        .filter(|region| region.mapped)
        .map(|_| guard_blocks(start, end))
}

fn guard_blocks(start: usize, end: usize) -> Vec<usize> {
    install();
    let mut g = guards();
    let mut written = Vec::new();
    let mut run: Option<usize> = None;
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    for block in first..=last {
        let open = block < last && !g.clean.contains(&block);
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
    written
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, leaving out any that fail (unmapped).
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
        }
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded.
fn open(g: &mut Guards, start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
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
    open(&mut guards(), at, at + len);
}

/// The runtime mapped or unmapped title memory `[at, at + len)`.
/// The old guards and shadow copies describe the previous mapping.
pub fn title_memory_remapped(at: usize, len: usize) {
    let Some(end) = at.checked_add(len) else {
        return;
    };
    if at == end {
        return;
    }
    // Queries may hold both locks in this order.
    let mut journal = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut g = guards();
        let from = at / BLOCK;
        let to = end.div_ceil(BLOCK);
        forget_views(&mut g, from, to);
        let blocks: Vec<usize> = g.protected.range(from..to).copied().collect();
        for block in blocks {
            g.protected.remove(&block);
            g.clean.remove(&block);
        }
    }
    record_mapping_change(&mut journal, at, end);
}

/// A private title allocation changed. Write watch can stay clean across a
/// decommit and recommit, so existing shadows need a new journal generation.
pub fn title_memory_allocation_changed(at: usize, len: usize) {
    if let Some(end) = at.checked_add(len)
        && at < end
    {
        let mut journal = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        // A private allocation notice can also cover a former guarded view.
        // Keep the handler's ownership of any remaining protection, but
        // require another observation before reusing its clean state.
        {
            let mut g = guards();
            let first = at / BLOCK;
            let last = end.div_ceil(BLOCK);
            forget_views(&mut g, first, last);
            while let Some(block) = g.clean.range(first..last).next().copied() {
                g.clean.remove(&block);
            }
        }
        record_mapping_change(&mut journal, at, end);
    }
}

fn forget_views(guards: &mut Guards, first: usize, last: usize) {
    while let Some(block) = guards.views.range(first..last).next().copied() {
        guards.views.remove(&block);
    }
}

fn record_mapping_change(journal: &mut Journal, at: usize, end: usize) {
    MAPPING_REVISION.fetch_add(1, Ordering::Release);
    journal.asked += 1;
    Journal::record(
        &mut journal.changes,
        at / PAGE,
        end.div_ceil(PAGE),
        journal.asked,
    );
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
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
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
pub(crate) mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
        VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// A private reservation is reported private, a mapped view is not
    /// (Wine fills no region flags, so this checks the fallback there).
    #[test]
    fn allocation_tells_private_memory_from_views() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            UnmapViewOfFile,
        };
        // SAFETY: a fresh write-watched reservation of ours.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let Some(a) = allocation(p) else {
            // QueryVirtualMemoryInformation is missing: nothing to check.
            return;
        };
        assert!(a.private, "a VirtualAlloc reservation is private");
        assert!(a.is_private_span(p, p + 2 * PAGE));
        // SAFETY: a pagefile section of one block, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                PAGE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, PAGE)
        };
        assert!(!view.Value.is_null());
        assert!(
            !allocation(view.Value as usize).unwrap().private,
            "a mapped view is not"
        );
        // SAFETY: ours.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
            VirtualFree(p as *mut _, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn journal_index_preserves_order_bounds_and_older_observers() {
        let mut journal = Journal {
            asked: 12,
            changes: BTreeMap::new(),
            found: Vec::new(),
        };
        Journal::record(&mut journal.changes, 62, 66, 3);
        Journal::record(&mut journal.changes, 65, 66, 8);
        Journal::record(&mut journal.changes, 128, 130, 12);

        assert_eq!(journal.since(63, 129, 7), (12, vec![(65, 8), (128, 12)]));
        assert_eq!(journal.since(64, 129, 10), (12, vec![(128, 12)]));
        assert_eq!(
            journal.since(63, 66, 0),
            (12, vec![(63, 3), (64, 3), (65, 8)])
        );
        assert_eq!(journal.since(63, 129, 12), (12, Vec::new()));
    }

    #[test]
    fn clean_guarded_observation_sees_later_title_and_runtime_writes() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: this test owns the anonymous section and its view.
        let (section, view) = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (
                section,
                MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK),
            )
        };
        let start = view.Value as usize;
        assert_ne!(start, 0);
        let end = start + 2 * BLOCK;
        let (at, _) = writes(start, end, true).unwrap();
        let native_before = KERNEL_WATCH_CALLS.with(|calls| calls.get());
        let (clean, changes) = shadow_writes(start, end, at, &mut None).unwrap();
        assert!(changes.is_empty());
        assert_eq!(KERNEL_WATCH_CALLS.with(|calls| calls.get()), native_before);

        // SAFETY: a CPU store to our mapped view opens its tracked guard.
        unsafe { view.Value.cast::<u8>().write(7) };
        let mut region = None;
        let (dirty, changes) = shadow_writes(start, end, clean, &mut region).unwrap();
        assert_eq!(changes, [(start / PAGE, dirty)]);
        assert!(
            region.is_none(),
            "the dirty guard still belongs to the known view"
        );
        writing(start + BLOCK, 1);
        let (opened, changes) = shadow_writes(start, end, dirty, &mut None).unwrap();
        assert_eq!(changes, [(start / PAGE + 1, opened)]);

        // SAFETY: unmap and close the test's own view and section.
        unsafe {
            assert_ne!(UnmapViewOfFile(view), 0);
            assert_ne!(CloseHandle(section), 0);
        }
        title_memory_remapped(start, 2 * BLOCK);
        let g = guards();
        assert!(!g.views.contains(&(start / BLOCK)));
        assert!(!g.clean.contains(&(start / BLOCK)));
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
        let (initial, before) = writes(start, end, false).expect("watched");
        // The address can have belonged to an earlier test's reservation.
        // Only this test's write should advance its journal generation.
        assert!(before.iter().all(|&(_, at)| at < initial));
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        let new_writes: Vec<_> = after.into_iter().filter(|&(_, at)| at > initial).collect();
        assert_eq!(new_writes, [(p / PAGE + 1, now)]);
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
        assert_eq!(
            // SAFETY: the bytes just read.
            unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) },
            b"from the disk"
        );
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

    pub(crate) struct Reservation(pub(crate) *mut std::ffi::c_void);

    impl Drop for Reservation {
        fn drop(&mut self) {
            // SAFETY: this is the base of a reservation owned by the test.
            assert_ne!(unsafe { VirtualFree(self.0, 0, MEM_RELEASE) }, 0);
        }
    }

    pub(crate) fn adjacent(watched: usize, unwatched: usize) -> (Reservation, Reservation) {
        adjacent_reservations(watched, unwatched, false)
    }

    pub(crate) fn adjacent_reservations(
        first_len: usize,
        second_len: usize,
        second_watched: bool,
    ) -> (Reservation, Reservation) {
        for _ in 0..16 {
            // SAFETY: these independent test reservations use valid allocation flags.
            unsafe {
                let span = VirtualAlloc(
                    std::ptr::null(),
                    first_len + second_len,
                    MEM_RESERVE,
                    PAGE_NOACCESS,
                );
                assert!(!span.is_null());
                assert_ne!(VirtualFree(span, 0, MEM_RELEASE), 0);
                let first = VirtualAlloc(
                    span,
                    first_len,
                    MEM_COMMIT | MEM_RESERVE | MEM_WRITE_WATCH,
                    PAGE_READWRITE,
                );
                if first.is_null() {
                    continue;
                }
                let first = Reservation(first);
                let second = VirtualAlloc(
                    span.cast::<u8>().wrapping_add(first_len).cast(),
                    second_len,
                    MEM_COMMIT | MEM_RESERVE | if second_watched { MEM_WRITE_WATCH } else { 0 },
                    PAGE_READWRITE,
                );
                if !second.is_null() {
                    return (first, Reservation(second));
                }
            }
        }
        panic!("adjacent test reservations");
    }

    #[test]
    fn grouped_probe_preserves_dirty_pages_in_adjacent_allocations() {
        for second_watched in [false, true] {
            let (first, second) = adjacent_reservations(PAGE, PAGE, second_watched);
            let base = first.0 as usize;
            let next = second.0 as usize;
            let (before, _) = writes(base, next, false).unwrap();
            // SAFETY: each pointer starts its own live reservation.
            unsafe {
                first.0.cast::<u8>().write(1);
                second.0.cast::<u8>().write(2);
            }
            assert!(grouped_writes(base, next + PAGE, 0, None).is_none());
            let (now, first_writes) = writes(base, next, false).unwrap();
            assert!(now > before);
            assert_eq!(first_writes, [(base / PAGE, now)]);
            if second_watched {
                let (now, second_writes) = writes(next, next + PAGE, false).unwrap();
                assert_eq!(second_writes, [(next / PAGE, now)]);
            } else {
                assert!(writes(next, next + PAGE, false).is_none());
            }
        }
    }

    #[test]
    fn unwatched_subrange_does_not_disable_watch_elsewhere_in_shadow() {
        if crate::host::host().is_none() {
            return;
        }
        let watched = 16 << 20;
        let (first, second) = adjacent(watched, PAGE);
        let start = first.0 as usize;
        let len = (watched + PAGE) as u64;
        struct RegisteredBuffer(usize);
        impl Drop for RegisteredBuffer {
            fn drop(&mut self) {
                crate::shadow::unregister(self.0, self.0 as u64);
            }
        }
        crate::shadow::register(start, start as u64, len);
        let _registration = RegisteredBuffer(start);
        // SAFETY: both reservations are live. Populate demand-zero pages
        // before the first refresh, since touching them can trip write watch.
        unsafe {
            first.0.cast::<u8>().write_bytes(0, watched);
            first.0.cast::<u32>().write(13);
            second.0.cast::<u32>().write(27);
        }
        let shadow = crate::shadow::find(start as u64, len).unwrap();
        assert_eq!(shadow.update(start, watched), watched);
        assert_eq!(shadow.update(start, watched), 0);

        // A small view in the next allocation needs the CPU fallback. A large
        // conventional view on the same shadow can still use write watching.
        assert_eq!(shadow.update(second.0 as usize, 4), 4);
        let refreshed: usize = (0..4).map(|_| shadow.update(start, watched)).sum();
        eprintln!(
            "4-byte unwatched refresh, then 4 clean watched requests copied {refreshed} bytes"
        );
        assert_eq!(refreshed, 0);

        // SAFETY: the first allocation remains live.
        unsafe { first.0.cast::<u32>().write(500) };
        assert_eq!(shadow.update(start, watched), PAGE);
        // SAFETY: the persistently mapped shadow owns the refreshed word.
        let mut data = std::ptr::null_mut();
        // SAFETY: the live buffer contains the refreshed word. No GPU work is in flight.
        unsafe {
            shadow.buffer.Map(0, None, Some(&mut data)).unwrap();
            let word = data.cast::<u32>().read_unaligned();
            shadow.buffer.Unmap(0, None);
            assert_eq!(word, 500);
        }

        // The ordinary suffix still needs copying. The watched prefix
        // stays valid, including when the two are requested together.
        assert_eq!(shadow.update(start + watched - 4, 8), 4);
        assert_eq!(shadow.update(start, watched), 0);
        // SAFETY: the last byte of the live watched reservation.
        unsafe { first.0.cast::<u8>().add(watched - 1).write(9) };
        assert_eq!(shadow.update(start + watched - 4, 8), 8);
        assert_eq!(shadow.update(start, watched), PAGE - 4);
        assert_eq!(shadow.update(start, watched), 0);
    }
}
