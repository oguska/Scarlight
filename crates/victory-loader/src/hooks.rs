//! Compatibility hooks placed in the title build being run.
//!
//! What to patch and where comes from that build's profile
//! (`victory_core::profile`), which the user keeps outside the repository;
//! this module knows how to place a byte-checked patch, not what any one
//! title needs. Nothing is applied unless `VICTORY_HOOKS` is set and the
//! profile for the mapped image lists hooks.

use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Environment variable that turns the profile's hooks on.
pub(crate) const HOOKS_ENV: &str = "VICTORY_HOOKS";
/// What the same switch was called before profiles carried the patches.
pub(crate) const LEGACY_HOOKS_ENV: &str = "VICTORY_NO_WATCHDOG";

/// Whether the user asked for the profile's hooks to be placed.
pub(crate) fn enabled() -> bool {
    [HOOKS_ENV, LEGACY_HOOKS_ENV]
        .iter()
        .any(|k| std::env::var_os(k).is_some())
}

/// Applies the hooks the profile for `hash` lists. Returns how many applied.
///
/// # Safety
/// `base` is the mapped image whose original BLAKE3 is `hash`, and no title
/// code is running yet.
pub(crate) unsafe fn apply_profile(base: usize, hash: &blake3::Hash) -> usize {
    let Some(profile) = victory_core::profile::current() else {
        tracing::debug!("no title profile: no hooks to place");
        return 0;
    };
    if !profile.matches(&hash.to_hex()) {
        return 0;
    }
    let mut applied = 0;
    for hook in &profile.hooks {
        let (Ok(expect), Ok(new)) = (hook.expected(), hook.replacement()) else {
            tracing::warn!(what = hook.what, "hook skipped: its bytes do not decode");
            continue;
        };
        // SAFETY: forwarded from this function's contract.
        if unsafe { apply(base, hook.rva as usize, &expect, &new, &hook.what) } {
            applied += 1;
        }
    }
    applied
}

/// Writes `new` over `expect` at `base + rva` if the bytes there are the
/// ones the profile says they are.
///
/// # Safety
/// See [`apply_profile`].
unsafe fn apply(base: usize, rva: usize, expect: &[u8], new: &[u8], what: &str) -> bool {
    if expect.is_empty() || new.len() != expect.len() {
        tracing::warn!(what, "hook skipped: no bytes, or the two lengths differ");
        return false;
    }
    let at = (base + rva) as *mut u8;
    // SAFETY: the offset is inside the mapped .text of this exact build.
    let now = unsafe { std::slice::from_raw_parts(at, expect.len()) };
    if now != expect {
        tracing::warn!(what, "hook skipped: the bytes are not the expected ones");
        return false;
    }
    let mut old = 0;
    // SAFETY: changing the protection of those bytes only, restored below.
    if unsafe { VirtualProtect(at as _, new.len(), PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
        return false;
    }
    // SAFETY: writable now; the bytes are the ones checked above.
    unsafe {
        std::ptr::copy_nonoverlapping(new.as_ptr(), at, new.len());
        VirtualProtect(at as _, new.len(), old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), at as _, new.len());
    }
    tracing::info!(what, "hook applied");
    true
}
