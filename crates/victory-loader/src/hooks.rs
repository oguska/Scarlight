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
/// A profile is the user's file, so every hook is checked before it is
/// touched: the bytes must decode, `new` must be as long as `expect`, and
/// `rva..rva+expect.len()` must lie inside the mapped image. Anything else is
/// logged and skipped; a bad profile never faults the process.
///
/// # Safety
/// `base` is the mapped image whose original BLAKE3 is `hash` and whose size
/// is `size`, and no title code is running yet.
pub(crate) unsafe fn apply_profile(base: usize, size: usize, hash: &blake3::Hash) -> usize {
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
        if let Some(why) = unusable(&expect, &new, hook.rva, size) {
            tracing::warn!(what = hook.what, "hook skipped: {why}");
            continue;
        }
        // SAFETY: forwarded from this function's contract, and `unusable`
        // checked that the whole hook lies inside the image.
        if unsafe { apply(base, hook.rva as usize, &expect, &new, &hook.what) } {
            applied += 1;
        }
    }
    applied
}

/// Why a hook cannot be applied to an image of `size` bytes, if it cannot.
fn unusable(expect: &[u8], new: &[u8], rva: u64, size: usize) -> Option<&'static str> {
    if expect.is_empty() {
        return Some("it has no bytes");
    }
    if new.len() != expect.len() {
        return Some("its new bytes differ in length from its expect bytes");
    }
    match usize::try_from(rva) {
        Ok(at) if at.checked_add(expect.len()).is_some_and(|end| end <= size) => None,
        _ => Some("it lies outside the mapped image"),
    }
}

/// Writes `new` over `expect` at `base + rva` if the bytes there are the
/// ones the profile says they are.
///
/// # Safety
/// See [`apply_profile`].
unsafe fn apply(base: usize, rva: usize, expect: &[u8], new: &[u8], what: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::unusable;

    /// An image of 0x1000 bytes with a five-byte hook at 0x100.
    const SIZE: usize = 0x1000;

    #[test]
    fn a_hook_inside_the_image_is_usable() {
        assert_eq!(unusable(&[1, 2, 3], &[4, 5, 6], 0x100, SIZE), None);
        // The last byte of the image is still inside it.
        assert_eq!(unusable(&[1, 2, 3], &[4, 5, 6], 0xffd, SIZE), None);
    }

    #[test]
    fn a_hook_past_the_end_of_the_image_is_refused() {
        assert_eq!(
            unusable(&[1, 2, 3], &[4, 5, 6], 0xffe, SIZE),
            Some("it lies outside the mapped image")
        );
        assert_eq!(
            unusable(&[1], &[4], 0x1000, SIZE),
            Some("it lies outside the mapped image")
        );
    }

    #[test]
    fn an_rva_beyond_the_address_space_is_refused_not_wrapped() {
        assert_eq!(
            unusable(&[1, 2, 3], &[4, 5, 6], u64::MAX, SIZE),
            Some("it lies outside the mapped image")
        );
        // The end would overflow `usize`: refused, not wrapped to a low one.
        assert_eq!(
            unusable(&[1, 2], &[3, 4], u64::MAX - 1, usize::MAX),
            Some("it lies outside the mapped image")
        );
    }

    #[test]
    fn a_hook_whose_new_bytes_differ_in_length_is_refused() {
        assert_eq!(
            unusable(&[1, 2, 3], &[4, 5], 0x100, SIZE),
            Some("its new bytes differ in length from its expect bytes")
        );
        assert_eq!(unusable(&[], &[], 0x100, SIZE), Some("it has no bytes"));
    }
}
