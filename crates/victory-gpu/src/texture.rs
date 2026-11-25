//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of each page at every
//! submission that samples it. Only the subresources whose memory holds a
//! changed page are detiled again (and, when watched, only the changed
//! pages read again).
//!
//! shortcut: a changed subresource is detiled whole, not just its blocks in
//! the pages written, and every subresource is still copied into the host
//! texture. Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    buffer: ID3D12Resource,
    /// Its bytes.
    #[cfg(feature = "diagnostics")]
    size: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice.
    mips: u32,
    /// Per subresource, the byte ranges of the memory it is detiled from:
    /// worked out when a check first needs them (a first detile does not).
    extents: std::sync::OnceLock<Vec<Vec<Range<usize>>>>,
    memory: *const u8,
    last: Mutex<Last>,
}

/// What a texture's staging buffer holds: the hash of each page of the
/// memory detiled (memory not watched), or the write-watch asking it was
/// detiled after, with the memory as then read.
#[derive(Default)]
struct Last {
    hashes: Vec<blake3::Hash>,
    /// Room for the hashes of this check, so a check does not allocate.
    scratch: Vec<blake3::Hash>,
    asked: u64,
    read: Vec<u8>,
}

/// Whether any of `changed` overlaps any of `extents` (both sorted).
fn overlaps(extents: &[Range<usize>], changed: &[Range<usize>]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < extents.len() && j < changed.len() {
        let (e, c) = (&extents[i], &changed[j]);
        if e.start < c.end && c.start < e.end {
            return true;
        }
        if e.end <= c.end {
            i += 1;
        } else {
            j += 1;
        }
    }
    false
}

/// The parts of `[0, len)` in which `a` and `b`, hashes of its `page`-byte
/// parts, differ; `None` when they do not hash the same parts.
fn changed_pages(
    a: &[blake3::Hash],
    b: &[blake3::Hash],
    page: usize,
    len: usize,
) -> Option<Vec<Range<usize>>> {
    (a.len() == b.len()).then(|| {
        (0..a.len())
            .filter(|&i| a[i] != b[i])
            .map(|i| i * page..((i + 1) * page).min(len))
            .collect()
    })
}

/// `[start, start + len)`'s byte ranges, relative to `start`, of the
/// `page`-byte pages numbered `pages` (in order), with touching ranges
/// merged: a run of written pages is one read of the memory.
fn page_ranges(
    pages: impl Iterator<Item = usize>,
    page: usize,
    start: usize,
    len: usize,
) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for p in pages {
        let end = ((p + 1) * page).min(start + len);
        let r = (p * page).max(start) - start..end.saturating_sub(start);
        if r.start >= r.end {
            continue;
        }
        match ranges.last_mut() {
            // Also merges a page listed twice.
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => ranges.push(r),
        }
    }
    ranges
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    set.get_or_insert_with(Default::default)
        .insert(resource as usize);
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written || !list.first_use(resource as usize) {
        return;
    }
    let Some(staging) = staging(resource, texture) else {
        return;
    };
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    // The staging buffer lives as long as `staging`, which the deferred
    // refresh below holds until the list is reset.
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (staging.buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    // Once per submission however many of its lists sample the texture.
    list.defer_texture(staging);
}

impl Staging {
    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let (read, _unchanged) = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        let us = started.elapsed().as_micros() as u64;
        preparing[2].fetch_add(us, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
        #[cfg(feature = "diagnostics")]
        crate::copystat::kind(crate::copystat::Kind::Texture, read, us);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::TexChecks, 1);
            if _unchanged {
                add(C::TexIdle, 1);
                add(C::TexIdleBytes, self.layout.size);
            }
        }
    }

    /// Returns the bytes of title memory read, and whether the check found
    /// the texture unchanged (a clean watch or equal hashes: not a texture
    /// whose memory is unavailable).
    fn refresh_now(&self) -> (usize, bool) {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        if let Some((now, written)) = crate::watch::writes(start, start + size, false) {
            let before = last.asked;
            if before != 0 && written.iter().all(|&(_, w)| w <= before) {
                return (0, true);
            }
            last.asked = now;
            // The staging buffer now holds bytes the page hashes do not
            // describe: a later hash check must detile every subresource,
            // not only those of the pages that differ from the old hashes.
            last.hashes.clear();
            #[cfg(feature = "diagnostics")]
            let started = std::time::Instant::now();
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            if before == 0 || last.read.len() != size {
                last.read.resize(size, 0);
                crate::host::copy_mapped(last.read.as_mut_ptr(), self.memory, size, false);
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::TexWatched, size as u64);
                    add(C::TexReadUs, started.elapsed().as_micros() as u64);
                }
                self.detile(&last.read, None);
                return (size, false);
            }
            // The rest of the memory is as last read: read again only the
            // pages written since, and detile what they hold.
            let pages = written
                .iter()
                .filter(|&&(_, w)| w > before)
                .map(|&(p, _)| p);
            let changed = page_ranges(pages, crate::watch::PAGE, start, size);
            let mut read = 0;
            for r in &changed {
                // SAFETY: `r` lies inside both the read copy and the
                // texture's memory, each `size` bytes.
                let (to, from) = unsafe {
                    (
                        last.read.as_mut_ptr().add(r.start),
                        self.memory.add(r.start),
                    )
                };
                crate::host::copy_mapped(to, from, r.len(), false);
                read += r.len();
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(
                crate::copystat::C::TexReadUs,
                started.elapsed().as_micros() as u64,
            );
            self.detile(&last.read, Some(&changed));
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, read as u64);
            return (read, false);
        }
        if !committed(self.memory, size) {
            #[cfg(feature = "diagnostics")]
            tracing::debug!(
                memory = ?self.memory,
                size,
                desc = ?self.layout.desc,
                "a sampled texture's memory is not committed"
            );
            return (0, false);
        }
        // SAFETY: committed memory of the texture's layout size.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let page = crate::watch::PAGE;
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        // The hashes of the check before are in `hashes`: fill `scratch` and
        // swap, so a check allocates nothing.
        last.scratch.clear();
        last.scratch.extend(tiled.chunks(page).map(blake3::hash));
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::TexHashUs,
            started.elapsed().as_micros() as u64,
        );
        if last.hashes == last.scratch {
            return (size, true);
        }
        let changed = changed_pages(&last.hashes, &last.scratch, page, size);
        self.detile(tiled, changed.as_deref());
        std::mem::swap(&mut last.hashes, &mut last.scratch);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        (size, false)
    }

    /// Each subresource's ranges of the memory, made on first use: the
    /// walk that works them out is a detile's worth of work per texture, so
    /// a texture whose contents never change again never pays it.
    fn extents(&self) -> &[Vec<Range<usize>>] {
        self.extents.get_or_init(|| {
            let mips = self.mips.max(1);
            let whole: Vec<Range<usize>> = std::iter::once(0..self.layout.size as usize).collect();
            let one = |i: usize| match self.layout.extents(i as u32 % mips, i as u32 / mips) {
                Ok(e) => e
                    .into_iter()
                    .map(|r| r.start as usize..r.end as usize)
                    .collect(),
                // Detiling it fails too; any change "touches" it.
                Err(_) => whole.clone(),
            };
            (0..self.footprints.len()).map(one).collect()
        })
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer: the
    /// subresources whose memory overlaps `changed` (sorted byte ranges),
    /// or all of them.
    fn detile(&self, tiled: &[u8], changed: Option<&[Range<usize>]>) {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        if tiled[..tiled.len().min(4096)].iter().all(|&b| b == 0) {
            // A sample, not the whole memory: this runs per detile, and a
            // full scan would be counted as texture time.
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory starts all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        #[cfg(feature = "diagnostics")]
        let mut done = 0u32;
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            if changed.is_some_and(|c| !overlaps(&self.extents()[i], c)) {
                continue;
            }
            #[cfg(feature = "diagnostics")]
            {
                done += 1;
            }
            let (mip, slice) = (i as u32 % self.mips, i as u32 / self.mips);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = pitch * rows as usize * f.Depth as usize;
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if let Err(e) =
                self.layout
                    .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::TexDetileUs, started.elapsed().as_micros() as u64);
            add(C::TexSubres, u64::from(done));
            add(C::TexSubresAll, self.footprints.len() as u64);
        }
    }
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize)) {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (buffer, data) = host.upload_buffer(total)?;
    crate::host::name(&buffer, || {
        format!(
            "staging of {resource:?} {}x{}x{} f{}",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0
        )
    });
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    let s = Arc::new(Staging {
        buffer,
        #[cfg(feature = "diagnostics")]
        size: total,
        data,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        extents: std::sync::OnceLock::new(),
        memory,
        last: Mutex::default(),
    });
    #[cfg(feature = "diagnostics")]
    {
        crate::copystat::add(crate::copystat::C::TexStaged, 1);
        crate::copystat::add(crate::copystat::C::TexStagedBytes, total);
    }
    map.insert(resource as usize, s.clone());
    Some(s)
}

#[cfg(test)]
// Single ranges in brackets are meant: lists of one range.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::{changed_pages, overlaps, page_ranges};

    /// Hashes cleared by a watched detile are "not the same parts": every
    /// subresource is detiled at the next hash check.
    #[test]
    fn cleared_hashes_change_every_subresource() {
        let now = [blake3::hash(b"a"), blake3::hash(b"b")];
        assert_eq!(changed_pages(&[], &now, 4, 8), None);
        assert_eq!(changed_pages(&now, &now, 4, 8), Some(vec![]));
    }

    #[test]
    fn ranges_overlap_only_when_they_share_a_byte() {
        let extents = [10..20, 40..50];
        assert!(overlaps(&extents, &[15..16]));
        assert!(overlaps(&extents, &[0..5, 49..60]));
        assert!(overlaps(&extents, &[0..11]));
        // Touching ends share no byte.
        assert!(!overlaps(&extents, &[20..40]));
        assert!(!overlaps(&extents, &[0..10, 20..40, 50..90]));
        assert!(!overlaps(&extents, &[]));
        assert!(!overlaps(&[], &[0..100]));
        // A long change across several extents, and the reverse.
        assert!(overlaps(&[10..20, 40..50], &[0..100]));
        assert!(overlaps(&[0..100], &[10..20, 40..50]));
    }

    #[test]
    fn hashed_pages_that_differ_are_the_change() {
        let h = |b: u8| blake3::hash(&[b]);
        let (a, b) = ([h(1), h(2), h(3)], [h(1), h(9), h(9)]);
        // The last page is short: the memory ends inside it.
        assert_eq!(
            changed_pages(&a, &b, 64, 150),
            Some(vec![64..128, 128..150])
        );
        assert_eq!(changed_pages(&a, &a, 64, 150), Some(vec![]));
        // Nothing hashed before: no part known to be unchanged.
        assert_eq!(changed_pages(&[], &b, 64, 150), None);
    }

    #[test]
    fn written_pages_become_ranges_of_the_memory() {
        // Memory from 100 for 250 bytes, in 64-byte pages 1..=5.
        let r = page_ranges([1, 3, 5].into_iter(), 64, 100, 250);
        assert_eq!(r, vec![0..28, 92..156, 220..250]);
        // Pages outside the memory give nothing.
        assert!(page_ranges([0, 6].into_iter(), 64, 100, 250).is_empty());
        // A run of written pages is one range (one read of the memory),
        // whether the pages come in order or repeated.
        assert_eq!(
            page_ranges([1, 2, 3].into_iter(), 64, 100, 250),
            vec![0..156]
        );
        assert_eq!(
            page_ranges([1, 1, 3].into_iter(), 64, 100, 250),
            vec![0..28, 92..156]
        );
    }

    /// Timing, not correctness: `victory_gpu.exe texture::tests::the_hash_pass
    /// --ignored --nocapture` prints what a check's hash pass costs.
    #[test]
    #[ignore]
    fn the_hash_pass_costs_this_much() {
        use std::time::Instant;
        let size = 16 << 20;
        let tiled: Vec<u8> = (0..size).map(|i| (i * 31 + i / 251) as u8).collect();
        let page = crate::watch::PAGE;
        let n = 10;
        let t = Instant::now();
        let mut last = 0usize;
        for _ in 0..n {
            last ^= tiled
                .chunks(page)
                .map(|p| blake3::hash(p).as_bytes()[0] as usize)
                .fold(0, |a, b| a ^ b);
        }
        let per = t.elapsed().as_secs_f64() / f64::from(n);
        println!(
            "hash: {} MB in {:.2} ms = {:.2} GB/s ({last})",
            size >> 20,
            per * 1e3,
            size as f64 / 1e9 / per
        );
    }
}
