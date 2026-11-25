//! Counts `gs:`-relative memory operands in mapped code.
//!
//! The console build reads its TEB through `gs:[...]` (x64 Windows keeps the
//! TEB in GS; Linux keeps thread-local storage in FS and leaves GS free, so
//! `arch_prctl(ARCH_SET_GS)` is the host's answer). What matters for the
//! estimate is *how many* such reads there are and what they touch.
//!
//! This is a linear sweep for the segment prefix byte (`0x65`), not a full
//! disassembler: a `0x65` that is really data (a jump-table byte, a constant
//! inside `.text`) can be counted as a false positive. `--verify` in the
//! notes compares the count against `objdump -d` on the same bytes.

use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct Report {
    pub total: u64,
    /// `gs:[disp32]` with no base or index: the classic TEB read.
    pub absolute: u64,
    /// Register-relative forms (`gs:[rax+8]`), which need a base register.
    pub indirect: u64,
    /// Absolute reads whose displacement is inside the TEB itself (< 0x800).
    pub teb_domain: u64,
    pub disp: BTreeMap<i64, u64>,
}

impl Report {
    pub fn merge(&mut self, other: &Report) {
        self.total += other.total;
        self.absolute += other.absolute;
        self.indirect += other.indirect;
        self.teb_domain += other.teb_domain;
        for (d, n) in &other.disp {
            *self.disp.entry(*d).or_default() += n;
        }
    }
}

/// Opcodes that take a ModRM operand and are worth attributing to the prefix
/// we found: MOV r/m,r (88,89,8A,8B), LEA (8D), CMP (38,39,3A,3B), TEST (84,85),
/// ADD/SUB/AND/OR/XOR (01,03,09,0B,21,23,29,2B,31,33), MOVZX/MOVSX (0F B6/B7/BE/BF).
fn takes_modrm(op: u8) -> bool {
    matches!(
        op,
        0x01 | 0x03
            | 0x09
            | 0x0b
            | 0x21
            | 0x23
            | 0x29
            | 0x2b
            | 0x31
            | 0x33
            | 0x38
            | 0x39
            | 0x3a
            | 0x3b
            | 0x84
            | 0x85
            | 0x88
            | 0x89
            | 0x8a
            | 0x8b
            | 0x8d
            | 0xff
    )
}

/// Sweeps `code` for `gs:`-relative operands.
///
/// # Safety
/// `code` must be a readable byte slice (it points into the mapped image).
pub unsafe fn scan(code: &[u8]) -> Report {
    let mut report = Report::default();
    let mut i = 0usize;
    while i < code.len() {
        if code[i] != 0x65 {
            i += 1;
            continue;
        }
        let mut p = i + 1;
        // Optional REX prefix.
        if code.get(p).is_some_and(|b| b & 0xf0 == 0x40) {
            p += 1;
        }
        // Two-byte opcode (`0f b6` etc.): skip the escape byte.
        let mut two_byte = false;
        if code.get(p) == Some(&0x0f) {
            two_byte = true;
            p += 1;
        }
        let Some(&op) = code.get(p) else { break };
        p += 1;
        if two_byte {
            if !matches!(op, 0xb6 | 0xb7 | 0xbe | 0xbf) {
                i += 1;
                continue;
            }
        } else if !takes_modrm(op) {
            i += 1;
            continue;
        }
        let Some(&modrm) = code.get(p) else { break };
        p += 1;
        let md = modrm >> 6;
        let rm = modrm & 7;
        report.total += 1;
        if rm == 4 {
            // SIB follows.
            let Some(&sib) = code.get(p) else { break };
            p += 1;
            let index = (sib >> 3) & 7;
            let base = sib & 7;
            if md == 0 && index == 4 && base == 5 {
                // disp32, no base, no index: `gs:[abs]`.
                if let Some(disp) = read_i32(code, p) {
                    report.absolute += 1;
                    if (0..0x800).contains(&disp) {
                        report.teb_domain += 1;
                    }
                    *report.disp.entry(i64::from(disp)).or_default() += 1;
                }
            } else {
                report.indirect += 1;
            }
        } else if md == 0 && rm == 5 {
            // RIP-relative: not a TEB read in practice, but it is a gs access.
            report.indirect += 1;
        } else if md != 0 || rm != 4 {
            report.indirect += 1;
        }
        i += 1;
    }
    report
}

fn read_i32(code: &[u8], at: usize) -> Option<i32> {
    let end = at.checked_add(4)?;
    Some(i32::from_le_bytes(code.get(at..end)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_the_teb_read_form() {
        // mov rax, gs:[0x58]; mov rcx, gs:[0x30]; mov rdx, gs:[rax+0x8]
        let code = [
            0x65, 0x48, 0x8b, 0x04, 0x25, 0x58, 0x00, 0x00, 0x00, 0x65, 0x48, 0x8b, 0x0c, 0x25,
            0x30, 0x00, 0x00, 0x00, 0x65, 0x48, 0x8b, 0x50, 0x08,
        ];
        // SAFETY: the slice is a valid local array.
        let report = unsafe { scan(&code) };
        assert_eq!(report.total, 3);
        assert_eq!(report.absolute, 2);
        assert_eq!(report.indirect, 1);
        assert_eq!(report.disp.get(&0x58), Some(&1));
    }
}
