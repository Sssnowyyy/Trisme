//! just enough of an NCCH (.cxi) to name and picture the game: its ids,
//! product code, flags and the ExeFS's icon.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MEDIA_UNIT: u64 = 0x200;

pub struct Cxi {
    pub program_id: u64,
    /// the ExeFS's "icon" file, an SMDH.
    pub icon: Option<Vec<u8>>,
    /// the exheader asks for the New 3DS's extended memory/clock.
    pub new3ds_only: bool,
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
}

pub fn read(path: &Path) -> Result<Cxi, String> {
    let fail = |what: &str, error: std::io::Error| format!("could not read the {what} of {}, {error}", path.display());
    let mut file = File::open(path).map_err(|e| fail("header", e))?;
    let mut header = [0u8; 0x200];
    file.read_exact(&mut header).map_err(|e| fail("header", e))?;
    if &header[0x100..0x104] != b"NCCH" {
        return Err(format!("{} is not an NCCH (.cxi)", path.display()));
    }
    let flags = &header[0x188..0x190];
    if flags[7] & 0x04 == 0 {
        return Err(format!("{} is still encrypted", path.display()));
    }
    let program_id = u64_at(&header, 0x118);

    // the ARM11 local system capabilities open the access control info at
    // exheader 0x200: program id, core version, then flag1 (New 3DS clock
    // and cache) and flag2, whose low bits are the New 3DS system mode,
    // nonzero for a title that wants the New 3DS's memory
    let mut exheader = [0u8; 0x400];
    let new3ds_only = if u32_at(&header, 0x180) >= 0x400 {
        file.seek(SeekFrom::Start(0x200)).map_err(|e| fail("exheader", e))?;
        file.read_exact(&mut exheader).map_err(|e| fail("exheader", e))?;
        exheader[0x20D] & 0x0F != 0
    } else {
        false
    };

    let exefs_offset = u32_at(&header, 0x1A0) as u64 * MEDIA_UNIT;
    let exefs_size = u32_at(&header, 0x1A4) as u64 * MEDIA_UNIT;
    let mut icon = None;
    if exefs_offset != 0 && exefs_size >= 0x200 {
        let mut exefs = [0u8; 0x200];
        file.seek(SeekFrom::Start(exefs_offset)).map_err(|e| fail("ExeFS", e))?;
        file.read_exact(&mut exefs).map_err(|e| fail("ExeFS", e))?;
        for entry in exefs[..0xA0].chunks_exact(16) {
            let name = String::from_utf8_lossy(&entry[..8]).trim_end_matches('\0').to_owned();
            let (offset, size) = (u32_at(entry, 8) as u64, u32_at(entry, 12) as u64);
            if name == "icon" && size >= 0x36C0 && size < 0x10000 {
                let mut data = vec![0u8; size as usize];
                file.seek(SeekFrom::Start(exefs_offset + 0x200 + offset)).map_err(|e| fail("icon", e))?;
                file.read_exact(&mut data).map_err(|e| fail("icon", e))?;
                icon = Some(data);
            }
        }
    }
    Ok(Cxi { program_id, icon, new3ds_only })
}
