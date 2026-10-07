// Shared by build.rs (after digest verification) and payload unpacking.

/// musl uses PT_GNU_STACK.p_memsz as the default pthread stack size.
pub(crate) const THREAD_STACK: u64 = 8 << 20;

/// Locate the eight-byte p_memsz field in a static, little-endian ELF64 head.
/// A changed pin must still have exactly one complete GNU_STACK header.
pub(crate) fn stack_field(head: &[u8]) -> Result<usize, String> {
    if elf_interp(head).map_err(|e| e.to_string())? != ElfKind::Static || head[5] != 1 {
        return Err("thread-stack patch requires a static little-endian ELF64".into());
    }
    let phoff = u64::from_le_bytes(head[32..40].try_into().unwrap());
    let phoff = usize::try_from(phoff).map_err(|_| "program-header offset is too large")?;
    let size = usize::from(u16::from_le_bytes(head[54..56].try_into().unwrap()));
    let count = usize::from(u16::from_le_bytes(head[56..58].try_into().unwrap()));
    let mut field = None;
    for i in 0..count {
        let start = i
            .checked_mul(size)
            .and_then(|n| phoff.checked_add(n))
            .ok_or("program-header offset overflow")?;
        let end = start
            .checked_add(56)
            .ok_or("program-header offset overflow")?;
        let header = head.get(start..end).ok_or("truncated program header")?;
        if u32::from_le_bytes(header[..4].try_into().unwrap()) == 0x6474e551
            && field.replace(start + 40).is_some()
        {
            return Err("multiple PT_GNU_STACK headers".into());
        }
    }
    field.ok_or_else(|| "no PT_GNU_STACK header".into())
}

#[cfg(test)]
pub(crate) mod stack_tests {
    use super::*;

    pub(crate) fn elf() -> Vec<u8> {
        let mut head = vec![0; 64 + 56];
        head[..6].copy_from_slice(b"\x7fELF\x02\x01");
        head[32..40].copy_from_slice(&64u64.to_le_bytes());
        head[54..56].copy_from_slice(&56u16.to_le_bytes());
        head[56..58].copy_from_slice(&1u16.to_le_bytes());
        head[64..68].copy_from_slice(&0x6474e551u32.to_le_bytes());
        head
    }

    #[test]
    fn locates_the_same_field_when_upstream_already_sets_the_stack() {
        let mut head = elf();
        assert_eq!(stack_field(&head).unwrap(), 104);
        head[104..112].copy_from_slice(&THREAD_STACK.to_le_bytes());
        assert_eq!(stack_field(&head).unwrap(), 104);
    }

    #[test]
    fn rejects_missing_duplicate_truncated_or_wrong_endian_headers() {
        let mut head = elf();
        head[64..68].fill(0);
        assert!(stack_field(&head).unwrap_err().contains("no PT_GNU_STACK"));
        let mut head = elf();
        head.extend_from_within(64..120);
        head[56..58].copy_from_slice(&2u16.to_le_bytes());
        assert!(stack_field(&head).unwrap_err().contains("multiple"));
        let mut head = elf();
        head.pop();
        assert!(stack_field(&head).is_err());
        let mut head = elf();
        head[5] = 2;
        assert!(stack_field(&head).is_err());
    }
}
