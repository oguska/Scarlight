//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed.

use std::collections::HashMap;
use std::sync::Mutex;

use victory_shader::emit::{RawWindow, StrideEntry};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{VIEW_TAG, View, view_at};
use crate::host::host;

pub(crate) struct HostHeap {
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic` more.
    count: u32,
    /// Dynamic slots past `count` (none in sampler heaps): [`DYNAMIC`]
    /// where the host takes them, else [`MIN_DYNAMIC`].
    dynamic: u32,
    sampler: bool,
    /// The record (view index and generation) each slot was last made from.
    made: Mutex<Vec<u64>>,
    /// The host resource each slot views, held while the slot does: shaders
    /// index heaps directly, so a slot the title has not rewritten since it
    /// released the resource may still be read, and a freed host resource
    /// read there is a GPU page fault (the console reads stale memory).
    kept: Mutex<Vec<Option<ID3D12Resource>>>,
    /// The copy of just its view's bytes each slot reads instead of its
    /// buffer's copy, held while the slot does (the map only finds it):
    /// dropping it would leave the descriptor reading a copy nothing
    /// refreshes. `None` for a slot that reads its buffer's copy.
    kept_copy: Mutex<Vec<Option<std::sync::Arc<crate::shadow::Shadow>>>>,
    /// Where the search for free dynamic slots starts, and which are taken
    /// (by lists not yet reset).
    next: Mutex<(u32, Vec<bool>)>,
    /// Per slot, what shaders need to read a buffer whose stride shader
    /// analysis could not recover statically ([`StrideEntry`]).
    strides: Option<StrideTable>,
    /// The first of the [`ALIASES`] slots after the dynamic ones (view
    /// heaps), and which slots have a raw view there.
    alias_base: u32,
    aliases: Mutex<Aliases>,
}

/// The raw views of the bytes structured and typed buffer views read, for
/// shaders that read such a buffer without knowing its stride: they take a
/// raw handle, which reads a raw view only (over the title's own view the
/// host GPUs disagree: an RTX 5070 reads other dwords than a Radeon). The
/// runtime stride table names the slot.
struct Aliases {
    /// The pool slot of each heap slot that has a raw view.
    of: HashMap<u32, u32>,
    /// Pool slots given back.
    free: Vec<u32>,
    /// Pool slots from here on were never taken.
    next: u32,
}

/// Slots past the dynamic ones in a view heap for [`Aliases`]: one per
/// structured or typed buffer view in the heap at a time.
/// shortcut: a heap holding more such views at once leaves the rest without
/// a raw view (they read zeros through the unknown-stride path, with a
/// warning); the count is a guess, not a measured need.
const ALIASES: u32 = 1 << 16;

/// The pool's first slots: null raw views (a shader view, an unordered
/// access one) for views that cannot have a raw view of their own.
const NULL_ALIASES: u32 = 2;

struct StrideTable {
    _resource: ID3D12Resource,
    ptr: usize,
    gpu: u64,
    len: u32,
}

impl StrideTable {
    fn new(device: &ID3D12Device, len: u32) -> Option<Self> {
        let bytes = (u64::from(len) * u64::from(StrideEntry::BYTES)).max(8);
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut resource: Option<ID3D12Resource> = None;
        let mut p = std::ptr::null_mut();
        // SAFETY: plain committed upload buffer creation and persistent CPU map.
        let gpu = unsafe {
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_UPLOAD,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut resource,
                )
                .ok()?;
            let res = resource.as_ref()?;
            res.Map(0, None, Some(&mut p)).ok()?;
            std::ptr::write_bytes(p.cast::<u8>(), 0, bytes as usize);
            res.GetGPUVirtualAddress()
        };
        Some(Self {
            _resource: resource?,
            ptr: p as usize,
            gpu,
            len,
        })
    }
}

/// Host heaps' GPU ranges (start, bytes, bytes per slot, sampler), for
/// device-removed reports: a shader indexing a heap past its end faults.
static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

/// For device-removed reports: where host GPU address `va` lies against
/// the host descriptor heaps.
pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!(
                "{:#x} past its end (slot {})",
                va - start - bytes,
                (va - start) / step
            )
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2; tier 3 hardware may take more.
const PORTABLE_VIEWS: u32 = 1_000_000;

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw). A list holds the slots it
/// took until it is reset (its submissions have run) or released.
/// 2^16 ran out seconds into the world (every world run on an RX 9070 XT,
/// each a device removal shortly after; #59).
const DYNAMIC: u32 = 1 << 18;

/// The dynamic slots of a heap whose host would not take [`DYNAMIC`] more
/// (a capped heap, NVIDIA): the title's slots keep their room first.
const MIN_DYNAMIC: u32 = 1 << 16;

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        let aliases = if sampler { 0 } else { ALIASES };
        // The larger dynamic range only where the host takes it on top of
        // every title slot; else the smaller one, and the cap after that.
        let (heap, count, extra) = if sampler {
            match make(count) {
                Ok(heap) => (heap, count, 0),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else if let Ok(heap) = make(count + DYNAMIC + aliases) {
            (heap, count, DYNAMIC)
        } else {
            match make(count + MIN_DYNAMIC + aliases) {
                Ok(heap) => (heap, count, MIN_DYNAMIC),
                Err(e) if count + MIN_DYNAMIC + aliases > PORTABLE_VIEWS => {
                    // shortcut: descriptors past the host's limit are left
                    // out; a title that reaches them needs remapping.
                    tracing::warn!(error = %e, count, "host view heap capped at {PORTABLE_VIEWS}");
                    (
                        make(PORTABLE_VIEWS).ok()?,
                        PORTABLE_VIEWS - MIN_DYNAMIC - aliases,
                        MIN_DYNAMIC,
                    )
                }
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        };
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        // SAFETY: a live shader-visible heap.
        let gpu = unsafe { heap.GetGPUDescriptorHandleForHeapStart().ptr };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(count + extra + aliases) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let strides = if sampler {
            None
        } else {
            StrideTable::new(&host.device, count + extra)
        };
        let h = HostHeap {
            heap,
            cpu,
            step,
            count,
            dynamic: extra,
            sampler,
            made: Mutex::new(vec![0; count as usize]),
            kept: Mutex::new(vec![None; (count + extra) as usize]),
            kept_copy: Mutex::new(vec![None; (count + extra) as usize]),
            next: Mutex::new((0, vec![false; extra as usize])),
            strides,
            alias_base: count + extra,
            aliases: Mutex::new(Aliases {
                of: HashMap::new(),
                free: Vec::new(),
                next: NULL_ALIASES,
            }),
        };
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot starts
        // null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, count + extra + aliases) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        if !sampler {
            null_buffer_view(h.cpu(h.alias_base));
            null_raw_uav(h.cpu(h.alias_base + 1));
        }
        tracing::debug!(
            ms = started.elapsed().as_millis(),
            "host descriptor heap made null"
        );
        Some(h)
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: BLOCK,
                        ..Default::default()
                    })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                null_sampler(start);
            } else {
                null_view(start);
            }
            let mut filled = 1;
            while filled < BLOCK {
                let n = filled.min(BLOCK - filled);
                host.device
                    .CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            let mut done = 0;
            while done < total {
                let n = BLOCK.min(total - done);
                host.device
                    .CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                done += n;
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    fn null_sampler(&self, index: u32) {
        null_sampler(self.cpu(index));
    }
}

/// A plain point sampler at `at`.
fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

impl HostHeap {
    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release`]: the first's index.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (next, taken) = &mut *guard;
        let n = n as usize;
        // Search from `next` on, then from the start.
        let starts = (*next as usize..self.dynamic as usize).chain(0..*next as usize);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts {
            // A run cannot wrap past the end.
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        let Some(first) = found else {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(n, "no free dynamic descriptor slots: a table is not made")
            });
            return None;
        };
        taken[first..first + n].fill(true);
        *next = ((first + n) % self.dynamic as usize) as u32;
        Some(self.count + first as u32)
    }

    /// The title slots this heap has host slots for; fewer than the title's
    /// when the host caps its size.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned).
    ///
    /// [`dynamic`]: HostHeap::dynamic
    pub(crate) fn release(&self, first: u32, n: u32) {
        self.release_with(first, n, Self::forget_view);
    }

    pub(super) fn release_with(&self, first: u32, n: u32, mut clear: impl FnMut(&Self, u32)) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        // Clear the retiring list's aliases before another list can lease
        // these slots and publish its own stride entries.
        for index in first..first.saturating_add(n).min(self.alias_base) {
            clear(self, index);
            // Drop the retained resource and own-bytes copy too, so a
            // released slot does not pin a large allocation until reuse.
            self.keep(index, None, None);
        }
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let end = (at + n).min(self.dynamic) as usize;
        if let Some(slots) = guard.1.get_mut(at as usize..end) {
            slots.fill(false);
        }
    }

    /// Makes slot `index` (a dynamic one) from `view`; a null view if the
    /// host cannot.
    pub(crate) fn make_at(&self, index: u32, view: &View) {
        let kept = make(view, self.cpu(index));
        if kept.is_none() {
            null_for(view, self.cpu(index));
        }
        self.note_view(index, view, kept.as_ref());
        match kept {
            Some(Kept::Nothing) => self.keep(index, None, None),
            Some(Kept::Resource { resource, copy }) => self.keep(index, Some(resource), copy),
            None => self.keep(index, None, None),
        }
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.cpu(index));
        }
        self.keep(index, None, None);
        self.forget_view(index);
    }

    /// Slot `index` now views `resource` (or nothing of the host's) and, if
    /// it reads a copy of just its view's bytes, keeps `copy` alive.
    fn keep(
        &self,
        index: u32,
        resource: Option<ID3D12Resource>,
        copy: Option<std::sync::Arc<crate::shadow::Shadow>>,
    ) {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = kept.get_mut(index as usize) {
            *slot = resource;
        }
        drop(kept);
        let mut held = self.kept_copy.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = held.get_mut(index as usize) {
            *slot = copy;
        }
    }

    /// The copy of just its bytes that slot `index` reads (a buffer view off
    /// its host copy's elements), as it was made: draws refresh and write
    /// back this one while the slot reads it, whatever the buffer registry
    /// says of that memory since.
    pub(crate) fn copy_of(&self, index: u32) -> Option<std::sync::Arc<crate::shadow::Shadow>> {
        let held = self.kept_copy.lock().unwrap_or_else(|e| e.into_inner());
        held.get(index as usize)?.clone()
    }

    fn set_entry(&self, index: u32, entry: StrideEntry) {
        if let Some(t) = &self.strides
            && index < t.len
        {
            // SAFETY: `t.ptr` points to a live mapped UPLOAD buffer of
            // `t.len` entries of two dwords.
            unsafe {
                (t.ptr as *mut [u32; 2])
                    .add(index as usize)
                    .write(entry.words());
            }
        }
    }

    /// GPU virtual address of the runtime stride table: a [`StrideEntry`]
    /// per slot.
    pub(crate) fn stride_table_gpu(&self) -> Option<u64> {
        self.strides.as_ref().map(|t| t.gpu)
    }

    /// Makes slot `index` (a dynamic one) a constant buffer view of host
    /// memory (the list's own, which it holds).
    pub(crate) fn constant_at(&self, index: u32, gpu: u64, size: u32) {
        self.keep(index, None, None);
        self.forget_view(index);
        let Some(host) = host() else { return };
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: size.next_multiple_of(256),
        };
        // SAFETY: a slot of our heap; the memory is the caller's.
        unsafe {
            host.device
                .CreateConstantBufferView(Some(&desc), self.cpu(index))
        };
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view of `len` bytes
    /// of `buffer` from byte `offset` (a multiple of 16): a root SRV with a
    /// size, whose reads past it return zeros instead of reaching whatever
    /// the host placed after the buffer.
    pub(crate) fn raw_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&desc), self.cpu(index))
        };
        self.keep(index, Some(buffer.clone()), None);
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view with unordered
    /// access of `len` bytes of `buffer` from byte `offset` (a multiple of
    /// 16): a root UAV with a size, whose writes past it are dropped
    /// instead of reaching whatever the host placed after the buffer.
    pub(crate) fn raw_uav_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = raw_uav_desc(offset / 4, elements);
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device.CreateUnorderedAccessView(
                buffer,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.cpu(index),
            )
        };
        self.keep(index, Some(buffer.clone()), None);
    }

    /// Makes slot `index` a raw buffer view that reads zeros.
    pub(crate) fn null_raw_at(&self, index: u32) {
        null_buffer_view(self.cpu(index));
        self.keep(index, None, None);
    }

    /// Makes slot `index` a raw buffer view with unordered access of no
    /// buffer: reads return zeros and writes are dropped.
    pub(crate) fn null_raw_uav_at(&self, index: u32) {
        let Some(host) = host() else { return };
        let desc = raw_uav_desc(0, 1);
        // SAFETY: a slot of our heap; a null resource is allowed with a desc.
        unsafe {
            host.device.CreateUnorderedAccessView(
                None::<&ID3D12Resource>,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.cpu(index),
            )
        };
        self.keep(index, None, None);
    }

    /// Whether this is a sampler heap.
    pub(crate) fn is_sampler(&self) -> bool {
        self.sampler
    }

    /// The shader-visible handle of slot `index`.
    pub(crate) fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        // SAFETY: a live shader-visible heap.
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart().ptr };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start + u64::from(index) * u64::from(self.step),
        }
    }

    fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there. Buffer
    /// views are made only for draws (`for_draw`), which also copy their
    /// data: a host copy of a buffer is made on first use.
    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        if index >= self.count {
            // Diagnostic (NVIDIA heap cap): the highest title slot past the
            // host heap seen so far.
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, std::sync::atomic::Ordering::Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
        let Some(view) = view else {
            // Not a view of ours (never written, or of a resource the
            // title released): nothing to read.
            if made[index as usize] != 0 {
                made[index as usize] = 0;
                self.make_null(index);
            }
            return None;
        };
        if made[index as usize] != key {
            let later = !for_draw && view.is_buffer();
            made[index as usize] = if later { 0 } else { key };
            let kept = if later {
                None
            } else {
                make(&view, self.cpu(index))
            };
            match &kept {
                Some(Kept::Nothing) => self.keep(index, None, None),
                Some(Kept::Resource { resource, copy }) => {
                    self.keep(index, Some(resource.clone()), copy.clone());
                }
                None => {
                    if !self.sampler {
                        null_for(&view, self.cpu(index));
                    }
                    self.keep(index, None, None);
                }
            }
            self.note_view(index, &view, kept.as_ref());
        }
        Some(view)
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = *self
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)?;
        if key == 0 {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = crate::shadow::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }
}

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A view description from its words, with the format as the host has it.
fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (address, _, unordered, words) = buffer_parts(view)?;
    let (from, bytes, _) = extent(address, unordered, words);
    Some((from, bytes))
}

fn uav_element_size(desc: &D3D12_UNORDERED_ACCESS_VIEW_DESC) -> Option<u64> {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        Some(u64::from(buffer.StructureByteStride))
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        Some(4)
    } else {
        crate::resource::element(desc.Format.0 as u32).map(|(bytes, _)| bytes)
    }
}

impl HostHeap {
    /// Slot `index` was just made from `view` (`made`: what [`make`] gave):
    /// writes its runtime stride table entry, and for a structured or typed
    /// buffer view makes the raw view of the same bytes the entry names.
    fn note_view(&self, index: u32, view: &View, made: Option<&Kept>) {
        if self.strides.is_none() {
            return;
        }
        if made.is_none() {
            // A null view: nothing to read through any handle.
            return self.forget_view(index);
        }
        let stride = view_stride(view);
        let entry = match viewed_bytes(view, made) {
            // Not a buffer view, or a raw one: the slot's own view.
            Viewed::Own => {
                self.unalias(index);
                StrideEntry {
                    stride,
                    ..StrideEntry::default()
                }
            }
            Viewed::Bytes {
                buffer,
                start,
                len,
                unordered,
            } => {
                let (raw_slot, bias) = self.alias(index, &buffer, start, len, unordered);
                StrideEntry {
                    stride,
                    raw_slot: Some(raw_slot),
                    bias,
                }
            }
            Viewed::Unknown => {
                once!("a buffer view whose bytes in its host copy are not found has no raw view");
                self.unalias(index);
                StrideEntry {
                    stride,
                    raw_slot: Some(self.null_alias(view_is_unordered(view))),
                    bias: 0,
                }
            }
        };
        self.set_entry(index, entry);
    }

    /// Slot `index`'s runtime stride table entry, as shaders read it.
    #[cfg(test)]
    pub(crate) fn entry_words(&self, index: u32) -> [u32; 2] {
        let t = self.strides.as_ref().expect("a view heap's table");
        assert!(index < t.len);
        // SAFETY: as in `set_entry`.
        unsafe { (t.ptr as *const [u32; 2]).add(index as usize).read() }
    }

    /// The first slot of the raw view pool.
    #[cfg(test)]
    pub(crate) fn alias_base(&self) -> u32 {
        self.alias_base
    }

    /// Slot `index` holds no view shaders read through the runtime stride
    /// table: its entry is cleared and its raw view given back.
    pub(super) fn forget_view(&self, index: u32) {
        if self.strides.is_none() {
            return;
        }
        self.unalias(index);
        self.set_entry(index, StrideEntry::default());
    }

    /// The slot of the null raw view standing in for a raw view the host
    /// cannot make: reads give zeros, writes are dropped.
    fn null_alias(&self, unordered: bool) -> u32 {
        self.alias_base + u32::from(unordered)
    }

    /// Gives back the raw view slot of `index`, if it has one.
    fn unalias(&self, index: u32) {
        let mut a = self.aliases.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = a.of.remove(&index) {
            a.free.push(slot);
        }
    }

    /// Makes the raw view of bytes `[start, start + len)` of `buffer` for
    /// slot `index` (in the slot it already has for one, or a free one): the
    /// heap slot it is in, and the bytes from its start to `start`.
    ///
    /// The raw view ends with the view's last element, so reads past it
    /// give zeros and writes are dropped, as through the view itself.
    /// shortcut: it ends on the dword holding the last byte and starts up
    /// to 12 bytes before the first (raw views start on 16 bytes); only an
    /// offset wrapping below the view's start reaches those bytes.
    fn alias(
        &self,
        index: u32,
        buffer: &ID3D12Resource,
        start: u64,
        len: u64,
        unordered: bool,
    ) -> (u32, u32) {
        let Some(window) = RawWindow::covering(start, len) else {
            // shortcut: a view starting off a dword of its host copy (an
            // element size that is not a multiple of 4) has no raw view; a
            // raw load could not address its bytes either.
            once!(start, len, "a buffer view off the dwords of its host copy has no raw view");
            self.unalias(index);
            return (self.null_alias(unordered), 0);
        };
        let slot = {
            let mut a = self.aliases.lock().unwrap_or_else(|e| e.into_inner());
            let slot = match a.of.get(&index) {
                Some(&slot) => Some(slot),
                None => a.free.pop().or_else(|| {
                    (a.next < ALIASES).then(|| {
                        a.next += 1;
                        a.next - 1
                    })
                }),
            };
            let Some(slot) = slot else {
                once!(
                    "no free raw view slots: a buffer read without a known stride reads zeros"
                );
                return (self.null_alias(unordered), 0);
            };
            a.of.insert(index, slot);
            self.alias_base + slot
        };
        let Some(host) = host() else {
            return (self.null_alias(unordered), 0);
        };
        // SAFETY: a live buffer holding the range (the slot's own view was
        // just made on it) and a slot of our heap.
        unsafe {
            if unordered {
                let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            FirstElement: window.first,
                            NumElements: window.dwords,
                            Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                            ..Default::default()
                        },
                    },
                };
                host.device.CreateUnorderedAccessView(
                    buffer,
                    None::<&ID3D12Resource>,
                    Some(&desc),
                    self.cpu(slot),
                );
            } else {
                let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_SRV {
                            FirstElement: window.first,
                            NumElements: window.dwords,
                            StructureByteStride: 0,
                            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                        },
                    },
                };
                host.device
                    .CreateShaderResourceView(buffer, Some(&desc), self.cpu(slot));
            }
        }
        (slot, window.bias)
    }
}

/// What a view the host made reads, for its raw view.
enum Viewed {
    /// Not a buffer, or through a raw view already.
    Own,
    /// These bytes of this host buffer (the view's own), read or also
    /// written.
    Bytes {
        buffer: ID3D12Resource,
        start: u64,
        len: u64,
        unordered: bool,
    },
    /// A structured or typed buffer view whose host buffer is not the one
    /// the bytes are found in now.
    Unknown,
}

/// Whether `view` is for unordered access.
fn view_is_unordered(view: &View) -> bool {
    matches!(view, View::Unordered(..) | View::Placed(true, ..))
}

/// The bytes of its host copy the structured or typed buffer view `view`
/// reads: as [`make`] places it (`kept`: the resource it made the view on),
/// from the first element's byte in the copy for the view's length.
fn viewed_bytes(view: &View, kept: Option<&Kept>) -> Viewed {
    use windows::core::Interface;
    const BUFFER: u32 = 1;
    // The title buffer's address and length, and the view's description.
    let (base, len, words, unordered): (u64, u64, &[u32; 10], bool) = match view {
        View::Shader(r, Some(desc)) | View::Unordered(r, _, Some(desc)) => {
            let r = *r as crate::object::This;
            if crate::resource::host_resource(r).is_some() {
                return Viewed::Own;
            }
            let Some((data, len)) = crate::resource::buffer_bytes(r) else {
                return Viewed::Own;
            };
            (data as u64, len, desc, view_is_unordered(view))
        }
        View::Placed(unordered, resource, desc) if resource[0] == BUFFER => {
            let Ok(words) = desc[..10].try_into() else {
                return Viewed::Own;
            };
            (
                u64::from(desc[10]) | u64::from(desc[11]) << 32,
                u64::from(resource[4]) | u64::from(resource[5]) << 32,
                words,
                *unordered,
            )
        }
        _ => return Viewed::Own,
    };
    // (first element, elements, bytes per element), none for a raw view.
    let elements = if unordered {
        let desc = uav_desc(words);
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        (b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 == 0)
            .then(|| (b.FirstElement, b.NumElements, uav_element_size(&desc).unwrap_or(0)))
    } else {
        let desc = srv_desc(words);
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        (b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 == 0)
            .then(|| (b.FirstElement, b.NumElements, element_size(&desc)))
    };
    let Some((first, count, element)) = elements else {
        return Viewed::Own;
    };
    let Some(kept) = kept.and_then(Kept::resource) else {
        return Viewed::Unknown;
    };
    if element == 0 {
        return Viewed::Unknown;
    }
    let Some(shadow) = crate::shadow::find(base, len) else {
        return Viewed::Unknown;
    };
    if shadow.buffer.as_raw() != kept.as_raw() {
        // A view that starts off its copy's elements reads a copy of just
        // its bytes, which starts at the view.
        if !off_element(view) {
            return Viewed::Unknown;
        }
        return Viewed::Bytes {
            buffer: kept.clone(),
            start: 0,
            len: u64::from(count) * element,
            unordered,
        };
    }
    Viewed::Bytes {
        buffer: shadow.buffer.clone(),
        start: shadow.offset(base) + first * element,
        len: u64::from(count) * element,
        unordered,
    }
}

/// Element byte stride of `view` for the runtime stride table (`0` for
/// non-buffer views or descriptors without a known element size).
fn view_stride(view: &View) -> u32 {
    let bytes = match view {
        View::Shader(_, Some(desc)) => element_size(&srv_desc(desc)),
        View::Placed(false, resource, desc) if resource[0] == 1 => desc[..10]
            .try_into()
            .ok()
            .map_or(0, |w: &[u32; 10]| element_size(&srv_desc(w))),
        View::Unordered(_, _, Some(desc)) => uav_element_size(&uav_desc(desc)).unwrap_or(0),
        View::Placed(true, resource, desc) if resource[0] == 1 => desc[..10]
            .try_into()
            .ok()
            .and_then(|w: &[u32; 10]| uav_element_size(&uav_desc(w)))
            .unwrap_or(0),
        _ => 0,
    };
    u32::try_from(bytes).unwrap_or(0)
}

/// A buffer view as [`make`] takes it: where its resource's memory starts and
/// how long it is (its host copy is found by these), whether it is unordered
/// access, and its descriptor's words.
fn buffer_parts(view: &View) -> Option<(u64, u64, bool, &[u32; 10])> {
    let registered = |r: usize| {
        crate::resource::buffer_bytes(r as crate::object::This)
            .map(|(data, len)| (data as u64, len))
    };
    match view {
        View::Shader(r, Some(desc)) => registered(*r).map(|(at, len)| (at, len, false, desc)),
        View::Unordered(r, _, Some(desc)) => registered(*r).map(|(at, len)| (at, len, true, desc)),
        View::Placed(unordered, resource, desc) if resource[0] == 1 => Some((
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            // The resource description's width, after the alignment.
            u64::from(resource[4]) | u64::from(resource[5]) << 32,
            *unordered,
            desc[..10].try_into().ok()?,
        )),
        _ => None,
    }
}

/// A buffer view's own range in the resource at `address` and the bytes of
/// one of its elements (0 for a format of unknown size).
fn extent(address: u64, unordered: bool, words: &[u32; 10]) -> (u64, u64, u64) {
    if unordered {
        unordered_extent(address, &uav_desc(words))
    } else {
        shader_extent(address, &srv_desc(words))
    }
}

/// [`extent`] of a shader view.
fn shader_extent(address: u64, desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> (u64, u64, u64) {
    let (from, bytes) = view_range_of(address, desc);
    (from, bytes, element_size(desc))
}

/// [`extent`] of an unordered access view.
fn unordered_extent(address: u64, desc: &D3D12_UNORDERED_ACCESS_VIEW_DESC) -> (u64, u64, u64) {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    (
        address.saturating_add(buffer.FirstElement * element),
        u64::from(buffer.NumElements) * element,
        element,
    )
}

/// Whether [`make`] gave this buffer view a copy of just its bytes, because it
/// starts off an element of its buffer's copy: the copy `readable` brings up
/// to date for it, and the one its writes go back from.
pub(crate) fn off_element(view: &View) -> bool {
    let Some((address, len, unordered, words)) = buffer_parts(view) else {
        return false;
    };
    let (from, bytes, element) = extent(address, unordered, words);
    crate::shadow::find(address, len)
        .is_some_and(|buffer| matches!(plan(bytes, element, buffer.offset(from)), Copy::OwnBytes))
}

/// What a slot made by [`make`] views keeps alive.
pub(crate) enum Kept {
    /// Made, and keeps nothing alive (a sampler slot).
    Nothing,
    /// Made, viewing `resource`; `copy` is the copy of just the view's bytes
    /// it reads instead of its buffer's copy, if it made one.
    Resource {
        resource: ID3D12Resource,
        copy: Option<std::sync::Arc<crate::shadow::Shadow>>,
    },
}

impl Kept {
    /// The resource the view reads (`None` for a sampler).
    fn resource(&self) -> Option<&ID3D12Resource> {
        match self {
            Kept::Nothing => None,
            Kept::Resource { resource, .. } => Some(resource),
        }
    }
}

/// The title memory a buffer shader view reads, from where its descriptor puts
/// it in the resource at `address`: its first element and the bytes its
/// elements take.
fn view_range_of(address: u64, desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> (u64, u64) {
    let element = element_size(desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    (
        address.saturating_add(buffer.FirstElement * element),
        u64::from(buffer.NumElements) * element,
    )
}

/// The host copy a buffer view of the resource at `address` (`len` bytes)
/// uses, the element its start is, and whether that copy holds only the
/// view's bytes. `extent` is the view's range and element size.
///
/// [`Copy::OwnBytes`] is for a view of memory the title placed inside another
/// object (`View::Placed`: methods 0x230 and 0x238), whose start need not be a
/// multiple of its stride. Its copy is a host copy like any other, so its own
/// refresh brings it up to date from title memory, what shaders write in it
/// goes back there after the submission, and a shader sees what the rest of
/// the pipeline sees.
fn view_copy(
    address: u64,
    len: u64,
    (from, bytes, element): (u64, u64, u64),
) -> Result<(std::sync::Arc<crate::shadow::Shadow>, u64, bool), &'static str> {
    // The view's own range is also the key `aligned` caches its copy by (the
    // one `buffer_range` gives `readable`).
    let buffer = crate::shadow::find(address, len).ok_or("no host copy of its buffer")?;
    match plan(bytes, element, buffer.offset(from)) {
        Copy::Element { first } => Ok((buffer, first, false)),
        Copy::OwnBytes => {
            let copy = crate::shadow::aligned(from, bytes).ok_or("no copy of the view")?;
            Ok((copy, 0, true))
        }
        Copy::None { why } => Err(why),
    }
}

/// What a buffer shader view reads.
enum Copy {
    /// The buffer's copy, at this element.
    Element { first: u64 },
    /// A copy of just the view's bytes, starting at the view.
    OwnBytes,
    /// Nothing, for the reason given.
    None { why: &'static str },
}

/// Where a buffer shader view's bytes come from: the buffer's copy when the
/// view starts on one of its elements (`element` bytes each, at `offset` in
/// the copy), else a copy of the view's own bytes.
fn plan(len: u64, element: u64, offset: u64) -> Copy {
    // Elements of a format whose size is not known: left null, as before.
    if element == 0 {
        return Copy::None {
            why: "elements of unknown size",
        };
    }
    if offset.is_multiple_of(element) {
        return Copy::Element {
            first: offset / element,
        };
    }
    if len == 0 {
        return Copy::None {
            why: "a view of no bytes",
        };
    }
    Copy::OwnBytes
}

/// Strides from `VICTORY_NULL_VIEW_STRIDES`: decimal or `0x` hex, comma
/// separated; anything else is named and skipped.
#[cfg(feature = "diagnostics")]
fn parsed_strides(value: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match part
            .strip_prefix("0x")
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .or_else(|| part.parse().ok())
        {
            Some(n) => out.push(n),
            None => tracing::warn!(
                part,
                "VICTORY_NULL_VIEW_STRIDES expects strides in decimal or 0x hex"
            ),
        }
    }
    out
}

/// With `VICTORY_NULL_VIEW_STRIDES` (diagnostics): buffer views of these
/// strides are left null, to confirm in the game which ones a pass needs.
/// Decimal or `0x` hex strides, comma separated.
fn nulled_stride(stride: u32) -> bool {
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = stride;
        false
    }
    #[cfg(feature = "diagnostics")]
    {
        if stride == 0 {
            return false;
        }
        static NULLED: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
        let nulled = NULLED.get_or_init(|| {
            parsed_strides(&std::env::var("VICTORY_NULL_VIEW_STRIDES").unwrap_or_default())
        });
        nulled.contains(&stride)
    }
}

/// The elements a buffer view takes: the `wanted` it names (0: all that fit)
/// cut to the `left` bytes of the host copy from the view's own start, of
/// `element` bytes each. Counted from the view's start, not the buffer's or
/// the copy's: the offset of a placed view into its copy is already gone.
fn bounded_elements(wanted: u32, left: u64, element: u64) -> u32 {
    let fit = u32::try_from(left / element.max(1)).unwrap_or(u32::MAX);
    if wanted == 0 { fit } else { wanted.min(fit) }
}

fn buffer_srv(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    placed: bool,
) -> Option<Kept> {
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let stride = unsafe { desc.Anonymous.Buffer }.StructureByteStride;
    if nulled_stride(stride) {
        once!(
            stride,
            "a buffer shader view left null (VICTORY_NULL_VIEW_STRIDES)"
        );
        return None;
    }
    let extent = shader_extent(address, &desc);
    let (shadow, first, own) = view_copy(address, len, extent)
        .map_err(|why| {
            once!(
                view = kind(placed),
                address = format_args!("{address:#x}"),
                len,
                element,
                stride,
                why,
                "a buffer shader view left null"
            );
        })
        .ok()?;
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    // `first` is the element the view's own start is from the copy's start,
    // so it replaces the title's first element rather than adding to it: on a
    // copy of just the view's bytes that is 0, since the copy starts there.
    buffer.FirstElement = if own { 0 } else { first };
    // Bounded to the host copy: reads past it return zeros instead of
    // reaching whatever the host placed after the buffer.
    buffer.NumElements = bounded_elements(buffer.NumElements, shadow.left(extent.0), element);
    if buffer.NumElements == 0 {
        return None;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
    };
    Some(Kept::Resource {
        resource: shadow.buffer.clone(),
        copy: own.then(|| shadow.clone()),
    })
}

/// How a buffer view was placed, as its warning names it: a registered
/// resource's views start at the resource and so on an element; the title
/// places a view of its own memory, which names its own start and can land off
/// an element.
fn kind(placed: bool) -> &'static str {
    if placed { "placed" } else { "registered" }
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; none if the copy cannot be
/// written by shaders (upload memory). What shaders write goes back to title
/// memory after the submission (`readable` declares the view's range).
fn buffer_uav(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    placed: bool,
) -> Option<Kept> {
    let extent = unordered_extent(address, &desc);
    let (shadow, first, own) = view_copy(address, len, extent)
        .map_err(|why| {
            once!(
                view = kind(placed),
                address = format_args!("{address:#x}"),
                len,
                element = extent.2,
                why,
                "an unordered buffer view left null"
            );
        })
        .ok()?;
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return None;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    // As for a shader view: the element the view starts at in the copy.
    buffer.FirstElement = if own { 0 } else { first };
    buffer.CounterOffsetInBytes = 0;
    // Bounded to the host copy, as for a shader view.
    buffer.NumElements = bounded_elements(buffer.NumElements, shadow.left(extent.0), extent.2);
    if buffer.NumElements == 0 {
        return None;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device.CreateUnorderedAccessView(
            &shadow.buffer,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
    Some(Kept::Resource {
        resource: shadow.buffer.clone(),
        copy: own.then(|| shadow.clone()),
    })
}

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in another format than a
/// typed resource's. A view of a typed texture in another format of its
/// family takes the texture's format.
/// shortcut: such a view (sRGB of UNORM, say) reads without its
/// conversion; cast with castable formats if colors come out wrong.
fn fits(texture: &ID3D12Resource, mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    if family(view) != family(own) {
        once!(view, own, "view format of another family");
        return false;
    }
    if let Some(format) = format
        && !typeless
    {
        *format = d.Format;
    }
    true
}

/// Makes the host descriptor for `view` at `at`: the host resource it
/// views, or `None` if it cannot.
fn make(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    let host = host()?;
    let device = &host.device;
    match view {
        &View::Constant(address, size) => {
            let shadow = crate::shadow::find(address, u64::from(size))?;
            let gpu = shadow.gpu(address);
            if gpu % 256 != 0 {
                once!(
                    address = format_args!("{address:#x}"),
                    "constant buffer not 256-byte aligned on the host"
                );
                return None;
            }
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: gpu,
                SizeInBytes: size.next_multiple_of(256),
            };
            // SAFETY: a slot of our heap; the range lies in the host copy.
            unsafe { device.CreateConstantBufferView(Some(&desc), at) };
            Some(Kept::Resource {
                resource: shadow.buffer.clone(),
                copy: None,
            })
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::object::This;
            if let Some(texture) = crate::resource::host_resource(resource) {
                let mut desc = desc.as_ref().map(srv_desc);
                if !fits(texture, desc.as_mut().map(|d| &mut d.Format), false) {
                    return None;
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                Some(Kept::Resource {
                    resource: texture.clone(),
                    copy: None,
                })
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                // A registered resource's copy starts at the resource, so its
                // views always start on an element.
                buffer_srv(data as u64, len, srv_desc(desc), at, false)
            } else {
                None
            }
        }
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::object::This;
            let Some(texture) = crate::resource::host_resource(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        buffer_uav(data as u64, len, uav_desc(desc), at, false)
                    }
                    _ => None,
                };
            };
            let mut desc = desc.as_ref().map(uav_desc);
            if !fits(texture, desc.as_mut().map(|d| &mut d.Format), true) {
                return None;
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            Some(Kept::Resource {
                resource: texture.clone(),
                copy: None,
            })
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(Kept::Nothing)
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words);
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(Kept::Nothing)
        }
        View::Placed(unordered, resource, desc) => {
            if let Some(view) = placed_texture(view) {
                return make(&view, at);
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                // shortcut: unordered access to a texture the title placed
                // itself is not mirrored; give it a resource as shader
                // views of such textures get when a dispatch writes one.
                once!(
                    dimension = resource[0],
                    "placed unordered texture view not mirrored"
                );
                return None;
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            // The view names its own start: it can land off an element.
            if *unordered {
                buffer_uav(address, len, uav_desc(words), at, true)
            } else {
                buffer_srv(address, len, srv_desc(words), at, true)
            }
        }
    }
}

/// A shader view of a texture the title placed in memory itself, as a view
/// of the resource made for that memory (on first use).
pub(crate) fn placed_texture(view: &View) -> Option<View> {
    const BUFFER: u32 = 1;
    let View::Placed(false, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let texture = crate::resource::texture_at(address, resource)?;
    Some(View::Shader(texture as usize, desc[..10].try_into().ok()))
}

/// A null view of the kind `view` is, for a slot whose view the host
/// cannot make: unordered access (to a buffer or a 2D texture) or a shader
/// view. Writes to it are dropped, reads give zeros.
fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let is_buffer = |r: &usize| crate::resource::buffer_bytes(*r as crate::object::This).is_some();
    // Unordered views: whether a buffer, and the title's view dimension.
    let (buffer, dimension) = match view {
        View::Unordered(r, _, desc) => (
            is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
            desc.map(|d| d[1]),
        ),
        View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
        // Shader views of buffers read nothing through a null buffer view:
        // a null 2D view where the shader expects a buffer is a mismatch
        // the hardware does not survive in general (GPU-based validation
        // reports it).
        View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
            return null_buffer_view(at);
        }
        View::Placed(false, resource, _) if resource[0] == 1 => return null_buffer_view(at),
        // Textures: a null view of the title's dimension (1D, arrays, 3D,
        // cubes), not always 2D.
        View::Shader(_, Some(desc)) => return null_like(desc, at),
        View::Placed(false, _, desc) => {
            return null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
        }
        View::Constant(..) => {
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC::default();
            // SAFETY: a slot of our heap; a zero location makes a null view.
            return unsafe { host.device.CreateConstantBufferView(Some(&desc), at) };
        }
        _ => return null_view(at),
    };
    let desc = if buffer {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        }
    } else {
        null_uav_desc(dimension)
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

/// A null texture UAV of the title's view dimension (1D, 1D or 2D arrays,
/// 3D), 2D if it names none: a 2D null view where the shader writes a 3D
/// texture is a mismatch GPU-based validation reports (the world's wind
/// field, CS_CopyWindFieldBeforeUpdateWithSmooth, 2026-11-24).
fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
        Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        _ => {}
    }
    desc
}

/// A buffer view that reads zeros (raw, as translated shaders mostly read
/// buffers).
/// A raw buffer view with unordered access of `elements` dwords from dword
/// `first`.
fn raw_uav_desc(first: u64, elements: u32) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_UAV {
                FirstElement: first,
                NumElements: elements,
                StructureByteStride: 0,
                CounterOffsetInBytes: 0,
                Flags: D3D12_BUFFER_UAV_FLAG_RAW,
            },
        },
    }
}

fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 1,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A raw unordered access view of a buffer that drops writes and reads
/// zeros.
fn null_raw_uav(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_UAV {
                NumElements: 1,
                Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

/// A texture view that reads zeros with the dimension (and mip and array
/// counts) of the title's description `words`; 2D if it names none.
fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
    if !TEXTURES.contains(&words[1]) {
        return null_view(at);
    }
    let Some(host) = host() else { return };
    // One mip, one slice or cube, plane 0: fields a null view still checks.
    let mut w = [0u32; 10];
    w[1] = words[1];
    match words[1] {
        6 => {}
        7 => w[5] = 1,
        3 | 5 | 10 => (w[5], w[7]) = (1, 1),
        _ => w[5] = 1,
    }
    let mut desc = srv_desc(&w);
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A view that reads zeros, for slots whose view the host cannot make.
/// shortcut: slots with no view at all (kind unknown) get this null 2D
/// view; null buffer views there did not stop the world's low-address
/// page faults (2026-11-24, run w10), so they are not the cause.
fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A null texture UAV keeps the title's dimension (3D for the world's
    /// wind field), and is 2D when the title names none.
    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        // Multisampled UAVs need a cap; they stay 2D.
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    /// A null 3D UAV made on the real device leaves it alive.
    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    /// A view on an element of its buffer's copy reads that copy; one off its
    /// elements reads a copy of just its bytes, which starts at the view.
    #[test]
    fn a_view_on_its_copy_reads_it_and_one_off_it_gets_its_own() {
        // 56-byte elements at offset 0: the first and third elements.
        assert!(matches!(
            plan(56, 56, 0),
            Copy::Element { first } if first == 0
        ));
        assert!(matches!(
            plan(56, 56, 112),
            Copy::Element { first } if first == 2
        ));
        // The hit-detection SRV: 56-byte elements, 48 bytes in (offset % 56).
        assert!(matches!(plan(3136, 56, 48), Copy::OwnBytes));
        // PreSkinSimNodes: 336-byte elements, 32 bytes in.
        assert!(matches!(plan(18_289_152, 336, 32), Copy::OwnBytes));
        // A raw view (4-byte elements) off by 2 bytes.
        assert!(matches!(plan(64, 4, 2), Copy::OwnBytes));
        // Elements of a format the host has no size for: as before, nothing.
        assert!(matches!(plan(64, 0, 0), Copy::None { .. }));
        // A view of no bytes has nothing to copy.
        assert!(matches!(plan(0, 56, 48), Copy::None { .. }));
    }

    /// An unordered view off an element of its buffer's copy writes a copy of
    /// just its bytes, which a shader view of the same bytes shares; one on
    /// an element writes the buffer's copy. The device takes all of them.
    #[test]
    fn an_unordered_view_off_an_element_writes_its_own_copy() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        // A title buffer the views' memory lies in (its copy is made on the
        // host; title memory is not read).
        let (address, len) = (0x7d00_0000_0000_u64, 0x1_0000_u64);
        crate::shadow::register(usize::MAX - 2, address, len);
        // Memory placed `start` bytes into it, of four 56-byte elements.
        let placed = |unordered: bool, start: u64| {
            let mut resource = [0u32; 14];
            resource[0] = 1; // a buffer
            resource[4] = 4 * 56;
            let mut desc = [0u32; 16];
            desc[1] = 1; // buffer dimension
            if unordered {
                // NumElements, StructureByteStride
                (desc[4], desc[5]) = (4, 56);
            } else {
                desc[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
                (desc[6], desc[7]) = (4, 56);
            }
            let at = address + start;
            (desc[10], desc[11]) = (at as u32, (at >> 32) as u32);
            View::Placed(unordered, resource, desc)
        };
        let made = |index: usize| heap.kept.lock().unwrap()[index].is_some();
        let copy = |index: usize| heap.kept_copy.lock().unwrap()[index].clone();
        heap.make_at(0, &placed(true, 112));
        assert!(
            made(0) && copy(0).is_none(),
            "on an element: the buffer's copy"
        );
        assert!(!off_element(&placed(true, 112)));
        // The hit-detection offset: 48 bytes in.
        heap.make_at(1, &placed(true, 48));
        heap.make_at(2, &placed(false, 48));
        let uav = copy(1).expect("an unordered view off an element: its own copy");
        let srv = copy(2).expect("a shader view off an element: its own copy");
        assert!(
            std::sync::Arc::ptr_eq(&uav, &srv),
            "one copy of the same bytes"
        );
        assert!(off_element(&placed(true, 48)) && off_element(&placed(false, 48)));
        // SAFETY: a plain device query.
        let removed = unsafe { host.device.GetDeviceRemovedReason() };
        assert!(removed.is_ok(), "device removed: {removed:?}");
        crate::shadow::unregister(usize::MAX - 2, address);
    }

    /// A nulled-stride list parses decimal and hex, and ignores junk.
    #[cfg(feature = "diagnostics")]
    #[test]
    fn nulled_strides_parse_decimal_and_hex() {
        assert_eq!(parsed_strides("56, 336,0x20"), vec![56, 336, 32]);
        assert_eq!(parsed_strides(" nope , 56"), vec![56]);
        assert!(parsed_strides("").is_empty());
    }

    /// A view's range is its own elements where its descriptor puts them, and
    /// `buffer_range` (what `readable` refreshes) gives the same one, so the
    /// copy `buffer_srv` or `buffer_uav` makes is found again for the refresh.
    #[test]
    fn a_views_range_is_the_one_its_copy_is_keyed_by() {
        // Whatever the words mean, both paths read them the same way: the
        // copy's key and the range `readable` refreshes cannot diverge.
        for words in [
            [0u32, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0u32, 4, 2, 0, 1, 56, 0, 0, 0, 0],
            [0u32, 4, 0, 0, 64, 336, 0, 0, 0, 0],
        ] {
            let mut resource = [0u32; 14];
            resource[0] = 1; // a buffer
            let mut placed = [0u32; 16];
            placed[..10].copy_from_slice(&words);
            placed[10] = 0x1000;
            let range = |(from, bytes, _): (u64, u64, u64)| Some((from, bytes));
            assert_eq!(
                buffer_range(&View::Placed(false, resource, placed)),
                range(shader_extent(0x1000, &srv_desc(&words)))
            );
            assert_eq!(
                buffer_range(&View::Placed(true, resource, placed)),
                range(unordered_extent(0x1000, &uav_desc(&words)))
            );
        }
    }

    /// A placed view in the middle of a copy keeps its elements: the bytes
    /// left are counted from the view's own start, not taken from the
    /// placement offset a second time.
    #[test]
    fn a_view_off_the_start_of_its_copy_keeps_its_elements() {
        // A 1120-byte parent, a four-element view of stride 56 at +560: the
        // bytes left from the view are 560, ten elements.
        assert_eq!(bounded_elements(4, 1120 - 560, 56), 4);
        // Cut to the copy's end.
        assert_eq!(bounded_elements(20, 1120 - 560, 56), 10);
        // No count: all that fit.
        assert_eq!(bounded_elements(0, 1120 - 560, 56), 10);
        // Nothing left, and elements of no size, take none and all.
        assert_eq!(bounded_elements(4, 0, 56), 0);
        assert_eq!(bounded_elements(4, 560, 0), 4);
    }
}
