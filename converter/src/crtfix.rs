//! repairs the long-branch stubs the linker puts inside __init and __fini
//! when a program's code passes 32 MiB.
//!
//! __init and __fini aren't functions in any one file: each startup object
//! (crti, crtbegin, crtend, crtn) puts a fragment in .init and .fini, and the
//! linker lays the fragments end to end. crtend's fragment is a lone
//! `bl __do_global_ctors_aux`, crtbegin's in .fini a `bl __do_global_dtors_aux`.
//! a PowerPC bl reaches 32 MiB. with a big game's recompiled code in between,
//! the target is out of reach and the linker routes the call through a stub,
//! which it puts right after the input section with the bl, that is, in the
//! middle of __init:
//!
//! ```text
//!     bl    1f                 ; the stub, the next instruction
//! 1:  lis   r12, target@ha
//!     addi  r12, r12, target@l
//!     mtctr r12
//!     bctr                     ; jumps; the return address is still 1b
//!     ...                      ; the rest of __init
//! ```
//!
//! the target returns to the stub, which calls it again: the constructors
//! run forever (or the destructors, on exit). changing that bctr into bctrl
//! makes the stub call the target, which then returns to the rest of __init.
//! the first bl's return address is overwritten by bctrl and was never used.

use std::path::Path;

const BL_NEXT: u32 = 0x4800_0005; // bl .+4
const BCTR: u32 = 0x4E80_0420;
const BCTRL: u32 = 0x4E80_0421;
const MTCTR_R12: u32 = 0x7D89_03A6;

fn is_lis_r12(word: u32) -> bool {
    word >> 16 == 0x3D80
}

fn is_addi_r12_r12(word: u32) -> bool {
    word >> 16 == 0x398C
}

/// patches every `bl .+4` followed by a stub of the shape above in code,
/// and returns how many it changed.
pub fn patch_words(code: &mut [u8]) -> usize {
    let word = |code: &[u8], at: usize| u32::from_be_bytes(code[at..at + 4].try_into().unwrap());
    let mut patched = 0;
    let mut at = 0;
    while at + 20 <= code.len() {
        if word(code, at) == BL_NEXT
            && is_lis_r12(word(code, at + 4))
            && is_addi_r12_r12(word(code, at + 8))
            && word(code, at + 12) == MTCTR_R12
            && word(code, at + 16) == BCTR
        {
            code[at + 16..at + 20].copy_from_slice(&BCTRL.to_be_bytes());
            patched += 1;
            at += 20;
        } else {
            at += 4;
        }
    }
    patched
}

struct Section {
    name: String,
    offset: usize,
    size: usize,
}

fn sections(elf: &[u8]) -> Result<Vec<Section>, String> {
    let be16 = |at: usize| u16::from_be_bytes(elf[at..at + 2].try_into().unwrap()) as usize;
    let be32 = |at: usize| u32::from_be_bytes(elf[at..at + 4].try_into().unwrap()) as usize;
    if elf.len() < 0x34 || &elf[..4] != b"\x7fELF" || elf[4] != 1 || elf[5] != 2 {
        return Err("not a 32-bit big-endian ELF".into());
    }
    let (shoff, shentsize, shnum, shstrndx) = (be32(0x20), be16(0x2E), be16(0x30), be16(0x32));
    if shoff + shentsize * shnum > elf.len() || shstrndx >= shnum {
        return Err("bad section headers".into());
    }
    let header = |index: usize| shoff + index * shentsize;
    let strings = be32(header(shstrndx) + 16);
    let mut out = Vec::new();
    for index in 0..shnum {
        let h = header(index);
        let name_at = strings + be32(h);
        let end = elf[name_at..].iter().position(|&b| b == 0).map_or(name_at, |n| name_at + n);
        let (kind, offset, size) = (be32(h + 4), be32(h + 16), be32(h + 20));
        // SHT_NOBITS has no bytes in the file
        if kind != 8 && offset + size <= elf.len() {
            out.push(Section { name: String::from_utf8_lossy(&elf[name_at..end]).into_owned(), offset, size });
        }
    }
    Ok(out)
}

/// patches the stubs in a linked program's .init and .fini, wherever the
/// linker script put them (their own sections, or inside .text). returns
/// how many it changed.
pub fn patch_elf(path: &Path) -> Result<usize, String> {
    let mut elf = std::fs::read(path).map_err(|e| format!("could not read {}, {e}", path.display()))?;
    let found = sections(&elf)?;
    let mut patched = 0;
    let named: Vec<&Section> = found.iter().filter(|s| s.name == ".init" || s.name == ".fini").collect();
    // wut's script folds .init and .fini into .text; scan all executable
    // code then, the pattern being specific enough not to occur otherwise
    let targets: Vec<&Section> = if named.is_empty() { found.iter().filter(|s| s.name == ".text").collect() } else { named };
    for section in targets {
        patched += patch_words(&mut elf[section.offset..section.offset + section.size]);
    }
    if patched > 0 {
        std::fs::write(path, &elf).map_err(|e| format!("could not write {}, {e}", path.display()))?;
    }
    Ok(patched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[u32]) -> Vec<u8> {
        list.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    #[test]
    fn patches_the_stub_inside_init() {
        // __init as Link Between Worlds' program had it
        let mut code = words(&[
            0x9421FFF0, 0x7C0802A6, 0x90010014, 0x48014C2D, // prologue, bl frame_dummy
            BL_NEXT, 0x3D800813, 0x398C16E0, MTCTR_R12, BCTR, // bl stub; stub
            0x80010014, 0x7C0803A6, 0x38210010, 0x4E800020, // epilogue
        ]);
        assert_eq!(patch_words(&mut code), 1);
        assert_eq!(&code[32..36], &BCTRL.to_be_bytes());
        // everything else as it was
        assert_eq!(patch_words(&mut code), 0);
    }

    #[test]
    fn leaves_ordinary_stubs_alone() {
        // a stub reached by a bl elsewhere, after a function's blr
        let mut code = words(&[0x4E800020, 0x3D800810, 0x398C37B4, MTCTR_R12, BCTR]);
        assert_eq!(patch_words(&mut code), 0);
        // bl .+4 followed by something else (a PIC base load)
        let mut code = words(&[BL_NEXT, 0x7FE802A6, 0x3D800810, 0x398C37B4, MTCTR_R12, BCTR]);
        assert_eq!(patch_words(&mut code), 0);
    }
}
