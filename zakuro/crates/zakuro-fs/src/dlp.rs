//! the Download Play child a game card carries, a CIA in the RomFS of the
//! card's partition 2, which the console sends to others to play along.

use crate::reader::Reader;
use crate::{NcchHeader, Ncsd, ReadAt, RomFs};

/// up to len bytes at offset, fewer where the data ends.
fn head<S: ReadAt + ?Sized>(data: &S, offset: u64, len: usize) -> Option<Vec<u8>> {
    data.vec_at(offset, (len as u64).min(data.size().checked_sub(offset)?) as usize).ok()
}

/// where the child's CIA is in a card's image, its offset and size.
pub fn child_cia<S: ReadAt + ?Sized>(image: &S) -> Option<(u64, u64)> {
    let header = head(image, 0, 0x200)?;
    if !Ncsd::detect(&header) {
        return None;
    }
    let partition = Ncsd::parse(&header).ok()?.partitions[2];
    if !partition.is_present() {
        return None;
    }
    let ncch = NcchHeader::parse(&head(image, partition.offset, NcchHeader::SIZE)?).ok()?;
    if ncch.romfs_size == 0 {
        return None;
    }
    let romfs = RomFs::parse(image, partition.offset + ncch.romfs_offset).ok()?;
    // the CIA is the only file, at the root or under it
    let mut dirs = vec![romfs.root().ok()?];
    while let Some(dir) = dirs.pop() {
        let found = romfs.files(&dir).into_iter().find(|(_, file)| file.name.to_ascii_lowercase().ends_with(".cia"));
        if let Some((_, file)) = found {
            return Some((romfs.file_data_offset(&file), file.data_size));
        }
        dirs.extend(romfs.subdirs(&dir).into_iter().map(|(_, dir)| dir));
    }
    None
}

/// a CIA's SMDH, its titles and icons, which its meta section holds past
/// the dependency list, none for a CIA without one.
pub fn cia_icon<S: ReadAt + ?Sized>(cia: &S) -> Option<Vec<u8>> {
    const ALIGN: u64 = 0x40;
    const SMDH_SIZE: usize = 0x36C0;
    let header = head(cia, 0, 0x20)?;
    let r = Reader::new(&header);
    let align = |n: u64| n.next_multiple_of(ALIGN);
    let header = r.u32(0x00).ok()? as u64;
    let certificates = r.u32(0x08).ok()? as u64;
    let ticket = r.u32(0x0C).ok()? as u64;
    let tmd = r.u32(0x10).ok()? as u64;
    let meta = r.u32(0x14).ok()? as usize;
    let contents = r.u64(0x18).ok()?;
    if meta < 0x400 + SMDH_SIZE {
        return None;
    }
    let at = align(align(align(align(align(header) + certificates) + ticket) + tmd) + contents) + 0x400;
    let smdh = cia.vec_at(at, SMDH_SIZE).ok()?;
    (&smdh[..4] == b"SMDH").then_some(smdh)
}
