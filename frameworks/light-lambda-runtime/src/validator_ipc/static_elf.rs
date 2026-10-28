//! Proposed fixed-work admission of a captured static ELF. No execution, paths,
//! allocation, external tools, dynamic loader, or instruction decoding here.
use super::super::controller::Failure;
use std::{fs::File, os::unix::fs::FileExt};

// Admission-parser profile bounds, not measured production memory/CPU bounds.
const MAX_PROGRAM_HEADERS: u16 = 128;
const MAX_DYNAMIC_ENTRIES: u64 = 256;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_GNU_STACK: u32 = 0x6474_e551;

fn bad() -> Failure {
    Failure::AdapterInvariant
}
fn u16_at(b: &[u8], n: usize) -> u16 {
    u16::from_le_bytes(b[n..n + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n..n + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], n: usize) -> u64 {
    u64::from_le_bytes(b[n..n + 8].try_into().unwrap())
}
fn read<const N: usize>(file: &File, length: u64, offset: u64) -> Result<[u8; N], Failure> {
    if offset.checked_add(N as u64).is_none_or(|end| end > length) {
        return Err(bad());
    }
    let mut b = [0; N];
    file.read_exact_at(&mut b, offset)
        .map_err(|_| Failure::NoReady)?;
    Ok(b)
}
fn end(start: u64, size: u64, limit: u64) -> Result<u64, Failure> {
    start
        .checked_add(size)
        .filter(|end| *end <= limit)
        .ok_or_else(bad)
}
fn allowed_dynamic(tag: u64, value: u64) -> bool {
    // Closed vocabulary for the selected static PIE. Unknown tags fail closed;
    // future compiler/artifact formats need review, not an automatic fallback.
    match tag {
        0 => value == 0,
        4 | 5 | 6 | 7 | 8 | 10 | 12 | 13 | 21 | 25 | 26 | 27 | 28 | 0x6fff_fef5 | 0x6fff_fff9 => {
            true
        }
        9 | 11 => value == 24,
        30 => value & !8 == 0, // DF_BIND_NOW; no TEXTREL/other loader mode.
        0x6fff_fffb => value & !(1 | 0x0800_0000) == 0, // NOW | PIE.
        _ => false,            // Includes NEEDED, path, audit, filter and auxiliary tags.
    }
}

pub(super) fn validate(file: &File, length: u64) -> Result<(), Failure> {
    if file.metadata().map_err(|_| Failure::NoReady)?.len() != length {
        return Err(bad());
    }
    let h = read::<64>(file, length, 0)?;
    if h[..7] != [0x7f, b'E', b'L', b'F', 2, 1, 1]
        || !matches!(u16_at(&h, 16), 2 | 3)
        || u16_at(&h, 18) != 62
        || u32_at(&h, 20) != 1
        || u16_at(&h, 52) != 64
        || u16_at(&h, 54) != 56
    {
        return Err(bad());
    }
    let count = u16_at(&h, 56);
    let offset = u64_at(&h, 32);
    let entry = u64_at(&h, 24);
    if count == 0 || count > MAX_PROGRAM_HEADERS || offset < 64 {
        return Err(bad());
    }
    end(offset, u64::from(count) * 56, length)?;
    let mut dynamic = None;
    let mut executable_entry = false;
    let mut stacks = 0;
    for i in 0..count {
        let p = read::<56>(file, length, offset + u64::from(i) * 56)?;
        let kind = u32_at(&p, 0);
        let flags = u32_at(&p, 4);
        if kind == PT_INTERP {
            return Err(bad());
        }
        if kind == PT_GNU_STACK {
            stacks += 1;
            if flags & 1 != 0 {
                return Err(bad());
            }
        }
        if kind != PT_LOAD && kind != PT_DYNAMIC {
            continue;
        }
        let at = u64_at(&p, 8);
        let va = u64_at(&p, 16);
        let size = u64_at(&p, 32);
        let memory = u64_at(&p, 40);
        let align = u64_at(&p, 48);
        end(at, size, length)?;
        let virtual_end = va.checked_add(memory).ok_or_else(bad)?;
        if size > memory || (align > 1 && (!align.is_power_of_two() || va % align != at % align)) {
            return Err(bad());
        }
        if kind == PT_LOAD {
            if flags & 3 == 3 {
                return Err(bad());
            } // No simultaneously writable executable load.
            if flags & 1 != 0 && entry >= va && entry < virtual_end && entry - va < size {
                executable_entry = true;
            }
        } else {
            if dynamic.is_some()
                || size == 0
                || !size.is_multiple_of(16)
                || size / 16 > MAX_DYNAMIC_ENTRIES
            {
                return Err(bad());
            }
            dynamic = Some((at, va, size));
        }
    }
    if !executable_entry || stacks != 1 {
        return Err(bad());
    }
    let Some((at, va, size)) = dynamic else {
        return if u16_at(&h, 16) == 2 {
            Ok(())
        } else {
            Err(bad())
        };
    };
    // Dynamic metadata must actually be backed by a coherent file-backed LOAD.
    let mut backed = false;
    for i in 0..count {
        let p = read::<56>(file, length, offset + u64::from(i) * 56)?;
        if u32_at(&p, 0) != PT_LOAD {
            continue;
        }
        let load_at = u64_at(&p, 8);
        let load_va = u64_at(&p, 16);
        let load_size = u64_at(&p, 32);
        if at >= load_at
            && va >= load_va
            && at - load_at == va - load_va
            && at - load_at <= load_size
            && size <= load_size - (at - load_at)
        {
            backed = true;
        }
    }
    if !backed {
        return Err(bad());
    }
    let mut terminated = false;
    for i in 0..size / 16 {
        let d = read::<16>(file, length, at + i * 16)?;
        let tag = u64_at(&d, 0);
        let value = u64_at(&d, 8);
        if !allowed_dynamic(tag, value) || (terminated && (tag != 0 || value != 0)) {
            return Err(bad());
        }
        terminated |= tag == 0;
    }
    if !terminated {
        return Err(bad());
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn fixture() -> Vec<u8> {
    // Structurally valid, header-shaped test data, never executed.
    let mut b = vec![0; 512];
    b[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1]);
    b[16..18].copy_from_slice(&3u16.to_le_bytes());
    b[18..20].copy_from_slice(&62u16.to_le_bytes());
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[24..32].copy_from_slice(&0x1180u64.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes());
    b[52..54].copy_from_slice(&64u16.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&3u16.to_le_bytes());
    let put32 = |b: &mut [u8], n: usize, v: u32| b[n..n + 4].copy_from_slice(&v.to_le_bytes());
    let put64 = |b: &mut [u8], n: usize, v: u64| b[n..n + 8].copy_from_slice(&v.to_le_bytes());
    put32(&mut b, 64, PT_LOAD);
    put32(&mut b, 68, 5);
    put64(&mut b, 80, 0x1000);
    put64(&mut b, 96, 512);
    put64(&mut b, 104, 512);
    put64(&mut b, 112, 4096);
    put32(&mut b, 120, PT_DYNAMIC);
    put32(&mut b, 124, 4);
    put64(&mut b, 128, 256);
    put64(&mut b, 136, 0x1100);
    put64(&mut b, 152, 32);
    put64(&mut b, 160, 32);
    put64(&mut b, 168, 8);
    put32(&mut b, 176, PT_GNU_STACK);
    put32(&mut b, 180, 6);
    put64(&mut b, 256, 21);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    fn check(b: &[u8]) -> Result<(), Failure> {
        let mut file = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut file, b).unwrap();
        validate(&file, b.len() as u64)
    }
    fn put(b: &mut [u8], n: usize, v: u64) {
        b[n..n + 8].copy_from_slice(&v.to_le_bytes());
    }
    #[test]
    fn static_pie_and_static_exec_are_admitted_as_data() {
        let mut b = fixture();
        check(&b).unwrap();
        b[16] = 2;
        b[120..124].fill(0);
        check(&b).unwrap();
    }
    #[test]
    fn interpreter_rejected_even_with_a_static_header() {
        let mut b = fixture();
        b[120..124].copy_from_slice(&PT_INTERP.to_le_bytes());
        assert_eq!(check(&b), Err(bad()));
    }
    #[test]
    fn needed_search_audit_and_filter_tags_rejected() {
        for tag in [
            1,
            15,
            29,
            0x6fff_fefb,
            0x6fff_fefc,
            0x7fff_fffd,
            0x7fff_ffff,
        ] {
            let mut b = fixture();
            put(&mut b, 256, tag);
            assert_eq!(check(&b), Err(bad()), "{tag}");
        }
    }
    #[test]
    fn unknown_dynamic_tag_rejected() {
        let mut b = fixture();
        put(&mut b, 256, 0x1234);
        assert_eq!(check(&b), Err(bad()));
    }
    #[test]
    fn missing_dynamic_terminator_rejected() {
        let mut b = fixture();
        put(&mut b, 272, 21);
        assert_eq!(check(&b), Err(bad()));
    }
    #[test]
    fn nonzero_tail_after_terminator_rejected() {
        let mut b = fixture();
        put(&mut b, 256, 0);
        put(&mut b, 272, 21);
        assert_eq!(check(&b), Err(bad()));
    }
    #[test]
    fn truncated_and_overflowed_program_tables_rejected() {
        for (offset, count) in [(u64::MAX, 3), (480, 3), (64, 129)] {
            let mut b = fixture();
            put(&mut b, 32, offset);
            b[56..58].copy_from_slice(&(count as u16).to_le_bytes());
            assert_eq!(check(&b), Err(bad()));
        }
        assert_eq!(check(&fixture()[..63]), Err(bad()));
    }
    #[test]
    fn extended_and_empty_header_counts_rejected() {
        for count in [0, u16::MAX] {
            let mut b = fixture();
            b[56..58].copy_from_slice(&count.to_le_bytes());
            assert_eq!(check(&b), Err(bad()));
        }
    }
    #[test]
    fn wrong_class_architecture_and_entry_rejected() {
        for (at, value) in [(4, 1), (18, 183)] {
            let mut b = fixture();
            b[at] = value;
            assert_eq!(check(&b), Err(bad()));
        }
        let mut b = fixture();
        put(&mut b, 24, 0);
        assert_eq!(check(&b), Err(bad()));
    }
    #[test]
    fn load_file_memory_and_arithmetic_bounds_rejected() {
        for (at, value) in [(96, 1024), (104, 1), (80, u64::MAX)] {
            let mut b = fixture();
            put(&mut b, at, value);
            assert_eq!(check(&b), Err(bad()));
        }
    }
    #[test]
    fn load_alignment_and_write_execute_rejected() {
        let mut b = fixture();
        put(&mut b, 112, 3);
        assert_eq!(check(&b), Err(bad()));
        let mut b = fixture();
        b[68] = 7;
        assert_eq!(check(&b), Err(bad()));
    }
    #[test]
    fn executable_entry_and_nonexecuting_stack_required() {
        for (at, value) in [(68, 4), (180, 7), (176, 0)] {
            let mut b = fixture();
            b[at] = value;
            assert_eq!(check(&b), Err(bad()));
        }
    }
    #[test]
    fn dynamic_must_be_bounded_and_backed_by_load() {
        for (at, value) in [(128, 480), (136, 0x1101), (152, 17), (152, 16 * 257)] {
            let mut b = fixture();
            put(&mut b, at, value);
            assert_eq!(check(&b), Err(bad()));
        }
    }
    #[test]
    fn duplicate_dynamic_segments_rejected() {
        let mut b = fixture();
        let p = b[120..176].to_vec();
        b[176..232].copy_from_slice(&p);
        assert_eq!(check(&b), Err(bad()));
    }
    #[test]
    fn unsupported_loader_flags_and_entry_sizes_rejected() {
        for (tag, value) in [(30, 4), (0x6fff_fffb, 2), (9, 16), (11, 16)] {
            let mut b = fixture();
            put(&mut b, 256, tag);
            put(&mut b, 264, value);
            assert_eq!(check(&b), Err(bad()));
        }
    }
}
