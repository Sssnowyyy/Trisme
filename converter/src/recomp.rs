//! the converter's use of 3dsrecomp: decrypting the dump and recompiling
//! its code for the Wii U.

use std::path::{Path, PathBuf};

use recomp3ds::crypto::{decrypt_to_cxi_with_progress, KeyOptions};

pub struct Decrypted {
    pub title_id: u64,
    pub product_code: String,
    pub was_encrypted: bool,
}

/// the dump's executable as a decrypted .cxi at out.
pub fn decrypt(rom: &Path, out: &Path, keys: Option<&Path>, seeddb: Option<&Path>) -> Result<Decrypted, String> {
    let options = KeyOptions { keys: keys.map(Path::to_owned), seeddb: seeddb.map(Path::to_owned) };
    let last = std::sync::atomic::AtomicU64::new(0);
    let info = decrypt_to_cxi_with_progress(rom, out, &options, &|done, total| {
        // a line every 10%
        let tenth = if total == 0 { 10 } else { done * 10 / total };
        if tenth > last.swap(tenth, std::sync::atomic::Ordering::Relaxed) {
            println!("    {}%", tenth * 10);
        }
    })?;
    Ok(Decrypted { title_id: info.title_id, product_code: info.product_code, was_encrypted: !info.already_decrypted })
}

pub struct Code {
    pub library: PathBuf,
    pub summary: String,
}

/// the game's code as C, compiled for the Wii U into a static library.
pub fn recompile(cxi: &Path, dir: &Path, overrides: Option<&Path>) -> Result<Code, String> {
    let info = recomp3ds::wiiu::write_wiiu_code(cxi, dir, overrides)?;
    Ok(Code {
        summary: format!(
            "{} entries in {} files, {:.1} MiB of C, {:.1} MiB compiled, in {:.0?}",
            info.entry_count,
            info.sources,
            info.c_bytes as f64 / (1 << 20) as f64,
            info.library_bytes as f64 / (1 << 20) as f64,
            info.took
        ),
        library: info.library,
    })
}
