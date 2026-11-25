//! `pe-bind-linux` — offline feasibility probe for a Wine-free Linux host.
//!
//! No game run, no Wine, nothing written outside our own address space:
//!
//! 1. reserve the image at the VA it asks for with `mmap(MAP_FIXED_NOREPLACE)`,
//!    copy the headers and sections in, and apply every `IMAGE_REL_BASED_DIR64`
//!    relocation with the real load delta (the whole "map the title where it
//!    wants to live" question, answered rather than argued);
//! 2. walk the import directory and bind each entry into its IAT slot against
//!    three tables — our HLE as it ships today, "a libc/pthread/mmap/futex
//!    call does the same thing", and "nothing on Linux does this, it is the
//!    Windows ABI" — and print the counts per DLL and per bucket;
//! 3. scan the executable sections for `gs:`-relative reads (the title's TEB),
//!    which is what a Linux host has to answer with `arch_prctl`.
//!
//! Usage: `pe-bind-linux --image <path> [--hle <tsv>] [--verbose]`
//!
//! The image is opened read-only and never written.

use std::collections::BTreeMap;
use std::path::PathBuf;

use victory_pe::{ImageInfo, ImportSymbol, layout};

mod buckets;
mod gsscan;

use buckets::{Bucket, Classifier};

const PROT_NONE: i32 = 0;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
const MAP_PRIVATE: i32 = 0x02;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_FIXED_NOREPLACE: i32 = 0x10_0000;
const PAGE: usize = 4096;

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

fn main() {
    let mut image: Option<PathBuf> = None;
    let mut hle: Option<PathBuf> = None;
    let mut verbose = false;
    let mut max_bytes: Option<usize> = None;
    let mut list_imports = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--image" => image = args.next().map(PathBuf::from),
            "--hle" => hle = args.next().map(PathBuf::from),
            "--verbose" | "-v" => verbose = true,
            "--max-bytes" => max_bytes = args.next().and_then(|v| v.parse().ok()),
            "--list-imports" => list_imports = true,
            "--help" | "-h" => {
                println!("pe-bind-linux --image <title.exe> [--hle <tsv>] [--verbose]");
                return;
            }
            other => {
                eprintln!("unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    let Some(image) = image else {
        eprintln!("pe-bind-linux --image <title.exe> [--hle <tsv>] [--verbose]");
        std::process::exit(2);
    };

    let bytes = match std::fs::read(&image) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {}: {e}", image.display());
            std::process::exit(1);
        }
    };
    let info = match ImageInfo::parse(&bytes) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("cannot parse {}: {e}", image.display());
            std::process::exit(1);
        }
    };

    let hle_path = hle.unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/hle-exports.tsv")
    });
    let classifier = match Classifier::load(&hle_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot load HLE table {}: {e}", hle_path.display());
            std::process::exit(1);
        }
    };
    let mut classifier = classifier;
    // DLLs shipped beside the image that it imports from: their imports are
    // bound from the DLL's own export table, no platform underneath.
    let mut shipped: Vec<(String, std::collections::HashSet<String>)> = Vec::new();
    if let Some(dir) = image.parent() {
        let wanted: Vec<String> = info
            .imports
            .iter()
            .map(|m| m.dll.to_ascii_lowercase())
            .collect();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("dll")) {
                    continue;
                }
                let Some(file) = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase())
                else {
                    continue;
                };
                if !wanted.contains(&file) {
                    continue;
                }
                // MSVC's runtime DLLs travel with the title but they are
                // Windows PE code implementing the Windows C++ ABI: importing
                // from them is the "Windows semantics" bucket, not a title
                // binding we can satisfy by reading an export table.
                if file.starts_with("msvcp")
                    || file.starts_with("vcruntime")
                    || file.starts_with("ucrtbase")
                    || file.starts_with("api-ms-win-crt")
                {
                    println!("  (not counted as a title DLL: {file}, it is the MSVC runtime)");
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else { continue };
                let Ok(parsed) = ImageInfo::parse(&bytes) else { continue };
                let exports = parsed.exports.iter().filter_map(|e| e.name.clone()).collect();
                shipped.push((file, exports));
            }
        }
        shipped.sort_by(|a, b| a.0.cmp(&b.0));
    }
    println!(
        "  {} of the imported DLLs are shipped beside the image and parsed for exports: {}",
        shipped.len(),
        shipped
            .iter()
            .map(|(f, e)| format!("{f} ({} exports)", e.len()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    classifier.set_title_dlls(shipped);

    println!("image {}", image.display());
    println!(
        "  machine amd64, subsystem {}, image base {:#x}, size of image {:#x} ({:.1} MiB)",
        info.subsystem_name(),
        info.image_base,
        info.size_of_image,
        f64::from(info.size_of_image) / (1024.0 * 1024.0)
    );
    println!(
        "  {} sections, {} imports in {} DLLs, {} delay imports, {} TLS callbacks, \
         exception table {}, relocations {}",
        info.sections.len(),
        info.import_count(),
        info.imports.len(),
        info.delay_imports.iter().map(|m| m.entries.len()).sum::<usize>(),
        info.tls.as_ref().map_or(0, |t| t.callback_rvas.len()),
        info.has_exception_table,
        info.has_relocations
    );

    let mut mapped = Mapped::reserve(info.image_base, info.size_of_image as usize);
    println!(
        "  mmap at requested base: {} (got {:#x}, delta {} bytes)",
        if mapped.at_preferred {
            "ok"
        } else {
            "FAILED, fell back to a free address"
        },
        mapped.base,
        mapped.delta(info.image_base)
    );
    mapped.protect(PROT_READ | PROT_WRITE | PROT_EXEC);
    let copied = mapped.copy_image(&info, &bytes);
    println!(
        "  copied {} of {} bytes of section data ({} RWX sections, {} W^X splits needed)",
        copied.bytes,
        info.size_of_image,
        mapped.rwx_sections,
        mapped.wx_sections
    );

    let relocs = mapped.apply_relocations(&info, &bytes);
    let kinds: Vec<String> = relocs
        .kinds
        .iter()
        .map(|(kind, n)| format!("type {kind} x{n}"))
        .collect();
    println!(
        "  relocations: {} DIR64 applied, {} other types ({}), {} pages, {} unreadable entries",
        relocs.dir64,
        relocs.other,
        kinds.join(", "),
        relocs.pages,
        relocs.unreadable
    );

    let mut per_dll: BTreeMap<String, buckets::Row> = BTreeMap::new();
    let mut totals = [0u64; 5];
    let mut written = 0u64;
    let mut out_of_range = 0u64;
    let mut gap_names: Vec<String> = Vec::new();
    let mut native_names: Vec<String> = Vec::new();
    let mut title_matched = 0u64;
    for module in info.imports.iter().chain(info.delay_imports.iter()) {
        for entry in &module.entries {
            let name = match &entry.symbol {
                ImportSymbol::Name { name, .. } => Some(name.clone()),
                ImportSymbol::Ordinal(n) => Some(format!("#{n}")),
            };
            let bucket = classifier.classify(&module.dll, name.as_deref());
            if matches!(bucket, Bucket::Title) {
                title_matched += 1;
            }
            let row = per_dll.entry(module.dll.clone()).or_insert([0; 5]);
            row[bucket as usize] += 1;
            totals[bucket as usize] += 1;
            if list_imports {
                println!(
                    "{}\t{}\t{}",
                    module.dll,
                    name.as_deref().unwrap_or("<ordinal>"),
                    match bucket {
                        Bucket::Hle => "hle",
                        Bucket::Native => "native",
                        Bucket::Windows => "windows",
                        Bucket::Title => "title",
                        Bucket::Gap => "gap",
                    }
                );
            }
            if verbose {
                match bucket {
                    Bucket::Gap => gap_names.push(format!(
                        "{}!{}",
                        module.dll,
                        name.as_deref().unwrap_or("<ordinal>")
                    )),
                    Bucket::Native => native_names.push(format!(
                        "{}!{} -> {}",
                        module.dll,
                        name.as_deref().unwrap_or("<ordinal>"),
                        classifier
                            .native_equivalent(name.as_deref().unwrap_or_default())
                            .unwrap_or("?")
                    )),
                    _ => {}
                }
            }
            // Bind the slot the way a loader would: a non-zero tag so a
            // missing binding is visible as a fault on 0, not a silent skip.
            if mapped.write_slot(&info, entry.iat_slot_rva, bucket as u64 + 1) {
                written += 1;
            } else {
                out_of_range += 1;
            }
        }
    }

    println!();
    println!("imports per DLL and bucket (hle / native / windows / title / gap)");
    println!(
        "{:<42} {:>6} {:>6} {:>7} {:>8} {:>6} {:>5}",
        "dll", "total", "hle", "native", "windows", "title", "gap"
    );
    let mut dlls: Vec<_> = per_dll.iter().collect();
    dlls.sort_by_key(|(_, row)| std::cmp::Reverse(row.iter().sum::<u64>()));
    for (dll, row) in &dlls {
        println!(
            "{:<42} {:>6} {:>6} {:>7} {:>8} {:>6} {:>5}",
            dll,
            row.iter().sum::<u64>(),
            row[0],
            row[1],
            row[2],
            row[3],
            row[4]
        );
    }
    let total: u64 = totals.iter().sum();
    println!(
        "{:<42} {:>6} {:>6} {:>7} {:>8} {:>6} {:>5}",
        "TOTAL", total, totals[0], totals[1], totals[2], totals[3], totals[4]
    );
    println!(
        "  IAT slots written: {written}/{total} ({} outside the image); \
         {title_matched} title-DLL imports bound from their own export tables",
        out_of_range
    );
    if verbose {
        println!("  native mappings:");
        for name in &native_names {
            println!("    {name}");
        }
        println!("  gap entries:");
        for name in &gap_names {
            println!("    {name}");
        }
    }

    println!();
    let mut gs_total = gsscan::Report::default();
    for section in &info.sections {
        if section.characteristics & 0x2000_0000 == 0 {
            continue;
        }
        let start = mapped.base + section.rva as usize;
        let len = section.virtual_size.min(section.file_size) as usize;
        let len = max_bytes.map_or(len, |max| len.min(max));
        // SAFETY: the section lies inside the reservation we just mapped.
        let report = unsafe { gsscan::scan(std::slice::from_raw_parts(start as *const u8, len)) };
        println!(
            "  {:<10} {:#010x} {:>10} bytes  gs refs {:>6} (absolute {:>6}, indirect {:>5})",
            section.name, section.rva, len, report.total, report.absolute, report.indirect
        );
        gs_total.merge(&report);
    }
    println!(
        "  gs: total {} refs; absolute disp32 {} (of which TEB-domain < 0x800: {}); \
         indirect base-relative {}",
        gs_total.total, gs_total.absolute, gs_total.teb_domain, gs_total.indirect
    );
    let mut hist: Vec<_> = gs_total.disp.iter().collect();
    hist.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    let top: Vec<String> = hist
        .iter()
        .take(12)
        .map(|(d, n)| format!("gs:[{d:#x}]x{n}"))
        .collect();
    println!("  top offsets: {}", top.join(" "));
    println!(
        "  image stays mapped for the rest of this process; nothing was written to disk"
    );
}

/// A reservation for the title image. Dropped (unmapped) at process exit.
struct Mapped {
    base: usize,
    len: usize,
    at_preferred: bool,
    rwx_sections: usize,
    wx_sections: usize,
}

impl Mapped {
    fn reserve(preferred: u64, size: usize) -> Self {
        let len = size.div_ceil(PAGE) * PAGE;
        let preferred = usize::try_from(preferred).unwrap_or(0);
        // SAFETY: plain anonymous mapping; the kernel either honours the hint
        // or hands back a different address, which we check below.
        let first = unsafe {
            mmap(
                preferred as *mut u8,
                len,
                PROT_NONE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        if !first.is_null() && first as isize != -1 {
            return Self {
                base: first as usize,
                len,
                at_preferred: true,
                rwx_sections: 0,
                wx_sections: 0,
            };
        }
        // SAFETY: same call with a null hint lets the kernel choose.
        let second = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_NONE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if second.is_null() || second as isize == -1 {
            eprintln!("mmap of {len} bytes failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
        Self {
            base: second as usize,
            len,
            at_preferred: false,
            rwx_sections: 0,
            wx_sections: 0,
        }
    }

    fn delta(&self, preferred: u64) -> i64 {
        self.base as i64 - preferred as i64
    }

    fn protect(&self, prot: i32) {
        // SAFETY: exactly the range we reserved above.
        let rc = unsafe { mprotect(self.base as *mut u8, self.len, prot) };
        if rc != 0 {
            eprintln!("mprotect failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
    }

    /// Copies headers and section contents to their virtual addresses.
    fn copy_image(&mut self, info: &ImageInfo, bytes: &[u8]) -> CopyStats {
        let mut bytes_copied = 0usize;
        let first_rva = info.sections.iter().map(|s| s.rva).min().unwrap_or(0x1000);
        let header_len = (first_rva as usize).min(bytes.len());
        // SAFETY: destination is inside our reservation, source is the file.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base as *mut u8, header_len);
        }
        bytes_copied += header_len;
        for section in &info.sections {
            let len = match section.virtual_size {
                0 => section.file_size,
                vs => vs.min(section.file_size),
            } as usize;
            let src = section.file_offset as usize;
            if len == 0 || src + len > bytes.len() {
                continue;
            }
            if section.rva as usize + len > self.len {
                continue;
            }
            if section.characteristics & 0x8000_0000 != 0 && section.characteristics & 0x2000_0000 != 0 {
                self.rwx_sections += 1;
            }
            if section.characteristics & 0x8000_0000 == 0 && section.characteristics & 0x2000_0000 != 0 {
                self.wx_sections += 1;
            }
            // SAFETY: bounds checked above, both ranges valid and disjoint.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes[src..src + len].as_ptr(),
                    (self.base + section.rva as usize) as *mut u8,
                    len,
                );
            }
            bytes_copied += len;
        }
        CopyStats { bytes: bytes_copied }
    }

    /// Applies every base relocation of the table, relative to the real base.
    fn apply_relocations(&self, info: &ImageInfo, bytes: &[u8]) -> RelocStats {
        let mut stats = RelocStats::default();
        if !info.has_relocations {
            stats.unreadable = u64::MAX;
            return stats;
        }
        let Some(dir) = layout::data_directory_offset(bytes, layout::dir::BASERELOC) else {
            stats.unreadable = u64::MAX;
            return stats;
        };
        let (Some(rva), Some(size)) = (layout::read_u32(bytes, dir), layout::read_u32(bytes, dir + 4))
        else {
            stats.unreadable = u64::MAX;
            return stats;
        };
        let Some(start) = info.rva_to_file_offset(rva) else {
            stats.unreadable = u64::MAX;
            return stats;
        };
        let end = (start as usize + size as usize).min(bytes.len());
        let delta = self.delta(info.image_base);
        let mut at = start as usize;
        while at + 8 <= end {
            let Some(page_rva) = layout::read_u32(bytes, at) else { break };
            let Some(block_size) = layout::read_u32(bytes, at + 4) else { break };
            if block_size < 8 {
                break;
            }
            let count = (block_size as usize - 8) / 2;
            stats.pages += 1;
            for i in 0..count {
                let Some(entry) = layout::read_u32(bytes, at + 8 + i * 2).map(|v| v as u16) else {
                    stats.unreadable += 1;
                    continue;
                };
                let kind = entry >> 12;
                let offset = u64::from(entry & 0x0fff);
                if kind != 10 {
                    stats.other += 1;
                    *stats.kinds.entry(kind).or_default() += 1;
                    continue;
                }
                let target = self.base as u64 + u64::from(page_rva) + offset;
                if target + 8 > (self.base + self.len) as u64 {
                    stats.unreadable += 1;
                    continue;
                }
                // SAFETY: target is inside the mapping (checked). The
                // relocation table is untrusted, so the 8-byte slot is
                // accessed unaligned rather than trusted to be aligned.
                unsafe {
                    let slot = target as *mut u64;
                    let old = std::ptr::read_unaligned(slot);
                    std::ptr::write_unaligned(slot, old.wrapping_add(delta as u64));
                }
                stats.dir64 += 1;
            }
            at += block_size as usize;
        }
        stats
    }

    /// Writes a binding tag into an IAT slot; false if the slot is not mapped.
    fn write_slot(&self, info: &ImageInfo, rva: u32, tag: u64) -> bool {
        let _ = info;
        let target = self.base as u64 + u64::from(rva);
        if target + 8 > (self.base + self.len) as u64 {
            return false;
        }
        // SAFETY: inside the mapping (checked above); the IAT slot is 8-byte
        // aligned in practice, but the address comes from a header field, so
        // it is written unaligned.
        unsafe { std::ptr::write_unaligned(target as *mut u64, tag) };
        true
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: the exact reservation made in `reserve`.
        unsafe {
            munmap(self.base as *mut u8, self.len);
        }
    }
}

struct CopyStats {
    bytes: usize,
}

#[derive(Default)]
struct RelocStats {
    dir64: u64,
    other: u64,
    pages: u64,
    unreadable: u64,
    kinds: BTreeMap<u16, u64>,
}
