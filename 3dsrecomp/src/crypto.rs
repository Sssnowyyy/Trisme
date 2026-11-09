//! decrypting a retail dump, a .3ds, .cia or .cxi, into a .cxi the rest of
//! 3dsrecomp reads, with the AES keys of the user's own console. none are
//! built in: they come from a Citra/Azahar aes_keys.txt the user dumped,
//! with GodMode9 for example, and seeds from a seeddb.bin.
//!
//! the work follows Citra's NCCH loader and Azahar's CIA installer, which
//! follow GodMode9 and 3dbrew. a .cia's content may be encrypted with the
//! title key, AES-CBC, the key itself encrypted with a common key. the NCCH
//! in it, or a cartridge's first partition, is encrypted with AES-CTR, its
//! exheader, ExeFS header, icon and banner with the primary key, made from
//! keyslot 0x2C and the first 16 bytes of the NCCH's signature, and every
//! other ExeFS file and the RomFS with the secondary key, whose keyslot the
//! crypto method picks, with a KeyY a seed may change. the decrypted .cxi
//! says NoCrypto in its flags, and keeps its hashes, which are over the
//! plain bytes. dumps run to gigabytes, so it is streamed in chunks.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, KeyInit, KeyIvInit, StreamCipher, StreamCipherSeek};
use aes::Aes128;
use sha2::{Digest, Sha256};

/// an AES-128 key, a KeyX, a KeyY or a normal key, or a counter or an IV.
pub type Key = [u8; 16];
type Ctr = ctr::Ctr128BE<Aes128>;

const MEDIA_UNIT: u64 = 0x200;
const NCCH_HEADER_SIZE: usize = 0x200;
/// the exheader proper and the access descriptor after it, which the
/// primary key encrypts together.
const EXHEADER_SIZE: u64 = 0x800;
const EXEFS_HEADER_SIZE: u64 = 0x200;
const CIA_HEADER_SIZE: u32 = 0x2020;
/// how much is decrypted at a time.
const CHUNK: usize = 8 << 20;

/// the key scrambler's constant, used when the key file has no generator,
/// as 3dbrew publishes it.
pub const GENERATOR: Key = 0x1FF9_E9AA_C5FE_0408_0245_91DC_5D52_768A_u128.to_be_bytes();

/// the NCCH keyslots, the original one every title uses for its primary
/// key, and the ones crypto methods 0x01, 0x0A and 0x0B use for the
/// secondary.
pub const NCCH_SLOT: u8 = 0x2C;
/// the keyslot for the common keys that encrypt a ticket's title key.
pub const COMMON_SLOT: u8 = 0x3D;
const COMMON_KEYS: usize = 6;

/// the names the key file goes by, Citra's and Lime3DS's, then Azahar's.
/// next to the ROM only the first, keys.txt being too common a name there.
const KEY_FILES: [&str; 2] = ["aes_keys.txt", "keys.txt"];
const SEED_FILE: &str = "seeddb.bin";

/// the key scrambler, which makes a normal key of a KeyX and a KeyY,
/// NormalKey = ROL128((ROL128(KeyX, 2) XOR KeyY) + C, 87), the keys taken
/// as 128-bit big-endian numbers.
pub fn scramble(key_x: &Key, key_y: &Key, generator: &Key) -> Key {
    let (x, y, c) = (u128::from_be_bytes(*key_x), u128::from_be_bytes(*key_y), u128::from_be_bytes(*generator));
    (x.rotate_left(2) ^ y).wrapping_add(c).rotate_left(87).to_be_bytes()
}

/// where to find the keys and seeds when not in the usual places.
#[derive(Clone, Debug, Default)]
pub struct KeyOptions {
    /// the key file, aes_keys.txt, looked for next to the ROM and in the
    /// emulators' sysdata folders when none.
    pub keys: Option<PathBuf>,
    /// the seed database, seeddb.bin, looked for the same way.
    pub seeddb: Option<PathBuf>,
}

/// what decrypting found.
#[derive(Clone, Debug)]
pub struct DecryptInfo {
    /// the executable's program id.
    pub title_id: u64,
    /// like CTR-P-BZLE.
    pub product_code: String,
    /// the dump was not encrypted, the .cxi was only taken out of it.
    pub already_decrypted: bool,
    /// the bytes of the .cxi.
    pub size: u64,
}

/// the keys of a key file.
#[derive(Clone, Debug, Default)]
pub struct Keys {
    /// the file they came from.
    pub path: Option<PathBuf>,
    generator: Option<Key>,
    key_x: HashMap<u8, Key>,
    key_y: HashMap<u8, Key>,
    normal: HashMap<u8, Key>,
    /// the KeyYs for keyslot 0x3D a ticket picks from.
    common_y: [Option<Key>; COMMON_KEYS],
}

/// 32 hex digits, the first word of value.
fn hex_key(value: &str) -> Option<Key> {
    let word = value.split_whitespace().next()?;
    let word = word.strip_prefix("0x").or_else(|| word.strip_prefix("0X")).unwrap_or(word);
    if word.len() != 32 || !word.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut key = [0; 16];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&word[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(key)
}

impl Keys {
    /// the keys in the text of a Citra or Azahar key file, name=hex lines,
    /// # starting comments. Azahar's keys.txt has sections, :AES and the
    /// like, and only the AES one is read; Citra's has none.
    pub fn parse(text: &str) -> Keys {
        let mut keys = Keys::default();
        let mut reading = true;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(section) = line.strip_prefix(':') {
                let section = section.trim();
                reading = section.is_empty() || section.eq_ignore_ascii_case("AES");
                continue;
            }
            if !reading {
                continue;
            }
            let Some((name, value)) = line.split_once('=') else { continue };
            if let Some(key) = hex_key(value) {
                keys.set(name.trim(), key);
            }
        }
        keys
    }

    /// the keys in the file at path.
    pub fn load(path: &Path) -> Result<Keys, String> {
        let text = std::fs::read_to_string(path).map_err(|error| format!("could not read the key file {}, {error}", path.display()))?;
        Ok(Keys { path: Some(path.to_owned()), ..Keys::parse(&text) })
    }

    fn set(&mut self, name: &str, key: Key) {
        if name == "generator" || name == "generatorConstant" {
            self.generator = Some(key);
        } else if let Some(index) = name.strip_prefix("common").and_then(|n| n.parse::<usize>().ok()) {
            if let Some(slot) = self.common_y.get_mut(index) {
                *slot = Some(key);
            }
        } else if let Some((slot, kind, rest)) = slot_name(name) {
            match (kind, rest) {
                ('X', "") => drop(self.key_x.insert(slot, key)),
                ('Y', "") => drop(self.key_y.insert(slot, key)),
                ('N', "") => drop(self.normal.insert(slot, key)),
                // the common KeyYs by the slot they go in, slot0x3DKeyY0..5
                ('Y', index) if slot == COMMON_SLOT => {
                    if let Some(slot) = index.parse::<usize>().ok().and_then(|index| self.common_y.get_mut(index)) {
                        *slot = Some(key);
                    }
                }
                _ => {}
            }
        }
    }

    /// the file has no keyslot or common key at all, not a 3DS key file.
    pub fn is_empty(&self) -> bool {
        self.key_x.is_empty() && self.key_y.is_empty() && self.normal.is_empty() && self.common_y.iter().all(Option::is_none)
    }

    /// the scrambler's constant, the file's or the published one.
    pub fn generator(&self) -> Key {
        self.generator.unwrap_or(GENERATOR)
    }

    pub fn key_x(&self, slot: u8) -> Option<Key> {
        self.key_x.get(&slot).copied()
    }

    pub fn key_y(&self, slot: u8) -> Option<Key> {
        self.key_y.get(&slot).copied()
    }

    /// a normal key the file gives as it is, slot0x..KeyN.
    pub fn normal_key(&self, slot: u8) -> Option<Key> {
        self.normal.get(&slot).copied()
    }

    pub fn common_key_y(&self, index: usize) -> Option<Key> {
        self.common_y.get(index).copied().flatten()
    }

    /// the normal key of a keyslot with key_y in it, from its KeyX.
    pub fn normal(&self, slot: u8, key_y: &Key) -> Option<Key> {
        Some(scramble(&self.key_x(slot)?, key_y, &self.generator()))
    }

    fn file_name(&self) -> String {
        self.path.as_ref().map_or_else(|| "the key file".to_owned(), |path| path.display().to_string())
    }
}

/// a keyslot's name, slot0x2CKeyX, as its slot, the kind of key and
/// whatever follows.
fn slot_name(name: &str) -> Option<(u8, char, &str)> {
    let rest = name.strip_prefix("slot0x").or_else(|| name.strip_prefix("slot0X"))?;
    let at = rest.find("Key")?;
    let slot = u8::from_str_radix(&rest[..at], 16).ok()?;
    let mut tail = rest[at + 3..].chars();
    let kind = tail.next()?;
    Some((slot, kind, tail.as_str()))
}

/// the seeds of a seed database, by title id.
#[derive(Clone, Debug, Default)]
pub struct Seeds {
    pub path: Option<PathBuf>,
    seeds: HashMap<u64, Key>,
}

impl Seeds {
    /// a seeddb.bin, a count, 12 bytes of padding and then a title id, its
    /// seed and 8 reserved bytes for each.
    pub fn parse(bytes: &[u8]) -> Result<Seeds, String> {
        let count = u32_at(bytes.get(..4).ok_or("the seed database is empty")?, 0) as usize;
        let mut seeds = HashMap::new();
        for i in 0..count {
            let entry = bytes.get(0x10 + i * 0x20..0x10 + (i + 1) * 0x20).ok_or("the seed database is cut short")?;
            seeds.insert(u64_at(entry, 0), entry[8..24].try_into().unwrap());
        }
        Ok(Seeds { path: None, seeds })
    }

    pub fn load(path: &Path) -> Result<Seeds, String> {
        let bytes = std::fs::read(path).map_err(|error| format!("could not read the seed database {}, {error}", path.display()))?;
        let seeds = Seeds::parse(&bytes).map_err(|error| format!("{error}, {}", path.display()))?;
        Ok(Seeds { path: Some(path.to_owned()), ..seeds })
    }

    pub fn get(&self, title_id: u64) -> Option<Key> {
        self.seeds.get(&title_id).copied()
    }
}

/// the emulators' sysdata folders, where they keep the key file and the
/// seed database, in the order they are looked in: Azahar's, then Citra's
/// and Lime3DS's from before it, on macOS in Application Support, on Linux
/// and the BSDs under XDG_DATA_HOME, and on Windows under APPDATA.
pub fn sysdata_dirs() -> Vec<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
    let mut dirs = Vec::new();
    let home = var("HOME");
    if cfg!(target_os = "macos") {
        if let Some(home) = &home {
            for name in ["Azahar", "Citra", "Lime3DS"] {
                dirs.push(home.join("Library/Application Support").join(name).join("sysdata"));
            }
        }
    }
    if !cfg!(windows) {
        if let Some(data) = var("XDG_DATA_HOME").or_else(|| home.map(|home| home.join(".local/share"))) {
            for name in ["azahar-emu", "citra-emu", "lime3ds-emu"] {
                dirs.push(data.join(name).join("sysdata"));
            }
        }
    }
    if let Some(appdata) = var("APPDATA") {
        for name in ["Azahar", "Citra", "Lime3DS"] {
            dirs.push(appdata.join(name).join("sysdata"));
        }
    }
    dirs
}

/// the ROM's own folder.
fn rom_dir(rom: &Path) -> PathBuf {
    match rom.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_owned(),
        _ => PathBuf::from("."),
    }
}

/// the folders a file for the ROM at rom is looked for in, the ROM's own
/// first.
fn search_dirs(rom: &Path) -> Vec<PathBuf> {
    std::iter::once(rom_dir(rom)).chain(sysdata_dirs()).collect()
}

/// the places the key file for the ROM at rom is looked for when --keys
/// gives none, in order: aes_keys.txt next to the ROM, then aes_keys.txt
/// and keys.txt in each of sysdata_dirs.
pub fn key_file_locations(rom: &Path) -> Vec<PathBuf> {
    let own = rom_dir(rom).join(KEY_FILES[0]);
    std::iter::once(own).chain(sysdata_dirs().into_iter().flat_map(|dir| KEY_FILES.map(|name| dir.join(name)))).collect()
}

/// the places the seed database is looked for when --seeddb gives none.
pub fn seeddb_locations(rom: &Path) -> Vec<PathBuf> {
    search_dirs(rom).into_iter().map(|dir| dir.join(SEED_FILE)).collect()
}

/// the keys for the ROM at rom, from the file options give, or else the
/// first of key_file_locations there is.
pub fn load_keys(rom: &Path, options: &KeyOptions) -> Result<Keys, String> {
    find_keys(rom, options, &key_file_locations(rom))
}

fn find_keys(rom: &Path, options: &KeyOptions, locations: &[PathBuf]) -> Result<Keys, String> {
    if let Some(path) = &options.keys {
        return Keys::load(path);
    }
    // the first file with any 3DS keys in it, past others by the same name
    let mut without = Vec::new();
    for path in locations.iter().filter(|path| path.is_file()) {
        match Keys::load(path) {
            Ok(keys) if !keys.is_empty() => return Ok(keys),
            _ => without.push(path.clone()),
        }
    }
    Err(missing_keys(rom, locations, &without))
}

/// what to tell when there is no key file, or none with keys in it.
fn missing_keys(rom: &Path, locations: &[PathBuf], without: &[PathBuf]) -> String {
    let mut message = format!(
        "{} is encrypted, and decrypting it needs the AES keys of your own 3DS, but no key file with them was found.\n\
         dump them from your console, with GodMode9 for example, as a Citra/Azahar aes_keys.txt, lines like\n\
         slot0x2CKeyX=<32 hex digits>, and give it with --keys <file>, or put it in the first of these places:",
        rom.display()
    );
    for path in locations {
        message.push_str(&format!("\n  {}", path.display()));
        if without.contains(path) {
            message.push_str(" (there, but with no 3DS AES keys in it)");
        }
    }
    message
}

fn missing_seeds(rom: &Path, program_id: u64, locations: &[PathBuf]) -> String {
    let mut message = format!(
        "{} uses seed crypto, and decrypting it needs the seed for {program_id:016X} from a seeddb.bin,\n\
         which you can build on your own console with GodMode9, and give with --seeddb <file> or put in one of:",
        rom.display()
    );
    for path in locations {
        message.push_str(&format!("\n  {}", path.display()));
    }
    message
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn be64(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// a ticket's encrypted title key and what decrypts it.
#[derive(Clone, Copy, Debug)]
struct TitleKey {
    encrypted: Key,
    title_id: u64,
    common_index: u8,
    /// the index of the content, which its IV starts with.
    content_index: u16,
}

/// where the executable NCCH is in a dump.
#[derive(Debug)]
struct Container {
    offset: u64,
    /// the bytes the dump gives it.
    size: u64,
    /// the .cia's title id, which also names the title when the NCCH header
    /// cannot be read yet.
    title_id: Option<u64>,
    /// for a .cia whose executable content is encrypted with the title key.
    title_key: Option<TitleKey>,
}

/// the size of a signature of type, padded to 64 bytes, with the type.
fn signature_size(kind: u32) -> Result<usize, String> {
    match kind & 0xFFFF {
        0x0000 | 0x0003 => Ok(4 + 0x200 + 0x3C),
        0x0001 | 0x0004 => Ok(4 + 0x100 + 0x3C),
        0x0002 | 0x0005 => Ok(4 + 0x3C + 0x40),
        _ => Err(format!("unknown signature type {kind:#X}")),
    }
}

fn read_at(file: &mut File, offset: u64, len: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0; len];
    file.seek(SeekFrom::Start(offset)).and_then(|_| file.read_exact(&mut bytes)).map_err(|error| error.to_string())?;
    Ok(bytes)
}

/// finds the executable NCCH, a cartridge image's first partition, a .cia's
/// content 0, or a .cxi, all of it.
fn locate(file: &mut File) -> Result<Container, String> {
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    let start = read_at(file, 0, 0x200).map_err(|_| "too short to be a 3DS dump".to_owned())?;
    match &start[0x100..0x104] {
        b"NCSD" => Ok(Container {
            offset: u32_at(&start, 0x120) as u64 * MEDIA_UNIT,
            size: u32_at(&start, 0x124) as u64 * MEDIA_UNIT,
            title_id: Some(u64_at(&start, 0x108)),
            title_key: None,
        }),
        b"NCCH" => Ok(Container { offset: 0, size: length, title_id: None, title_key: None }),
        _ if u32_at(&start, 0) == CIA_HEADER_SIZE => locate_cia(file, &start),
        _ => Err("not a 3DS cartridge image, .cia or executable".into()),
    }
}

/// the header, certificates, ticket, TMD and contents of a .cia each start
/// on 64 bytes, and the contents follow in the order of the TMD's records,
/// those the header's bitmap has.
fn locate_cia(file: &mut File, header: &[u8]) -> Result<Container, String> {
    let align = |n: u64| n.next_multiple_of(0x40);
    let ticket_offset = align(align(CIA_HEADER_SIZE as u64) + u32_at(header, 0x08) as u64);
    let ticket_size = u32_at(header, 0x0C) as usize;
    let tmd_offset = align(ticket_offset + ticket_size as u64);
    let tmd_size = u32_at(header, 0x10) as usize;
    let contents = align(tmd_offset + tmd_size as u64);
    let bitmap = read_at(file, 0x20, 0x2000)?;

    let tmd = read_at(file, tmd_offset, tmd_size)?;
    let short = || "the .cia's TMD is cut short".to_owned();
    let signature = signature_size(be32(tmd.get(..4).ok_or_else(short)?, 0)).map_err(|error| format!("the .cia's TMD has an {error}"))?;
    let body = tmd.get(signature..).ok_or_else(short)?;
    if body.len() < 0xC4 + 64 * 0x24 {
        return Err(short());
    }
    let title_id = be64(body, 0x4C);
    let mut offset = contents;
    for i in 0..be16(body, 0x9E) as usize {
        let record = body.get(0xC4 + 64 * 0x24 + i * 0x30..).filter(|r| r.len() >= 0x30).ok_or_else(short)?;
        let (index, kind, size) = (be16(record, 4), be16(record, 6), be64(record, 8));
        if bitmap[index as usize / 8] & (0x80 >> (index % 8)) == 0 {
            continue;
        }
        if index != 0 {
            offset = align(offset + size);
            continue;
        }
        let title_key = if kind & 1 != 0 {
            let ticket = read_at(file, ticket_offset, ticket_size)?;
            let short = || "the .cia's ticket is cut short".to_owned();
            let signature = signature_size(be32(ticket.get(..4).ok_or_else(short)?, 0))
                .map_err(|error| format!("the .cia's ticket has an {error}"))?;
            let body = ticket.get(signature..).filter(|body| body.len() >= 0xB2).ok_or_else(short)?;
            Some(TitleKey {
                encrypted: body[0x7F..0x8F].try_into().unwrap(),
                title_id: be64(body, 0x9C),
                common_index: body[0xB1],
                content_index: index,
            })
        } else {
            None
        };
        if title_key.is_some() && size % 16 != 0 {
            return Err("the .cia's encrypted executable is not a whole number of AES blocks".into());
        }
        return Ok(Container { offset, size, title_id: Some(title_id), title_key });
    }
    Err("the .cia has no executable".into())
}

/// the dump's NCCH, read through the title key when the .cia has one.
struct Source {
    file: File,
    offset: u64,
    size: u64,
    /// the content's cipher and its IV.
    cbc: Option<(Aes128, Key)>,
}

impl Source {
    /// fills buf from at in the NCCH.
    fn read(&mut self, at: u64, buf: &mut [u8]) -> Result<(), String> {
        let end = at + buf.len() as u64;
        if end > self.size {
            return Err(format!("the dump is cut short, it ends before {end:#X} in the executable"));
        }
        let io = |error: std::io::Error| format!("could not read the dump, {error}");
        let Some((cipher, iv)) = &self.cbc else {
            self.file.seek(SeekFrom::Start(self.offset + at)).map_err(io)?;
            return self.file.read_exact(buf).map_err(io);
        };
        // CBC decrypts a block with the one before it, so reading starts a
        // block early unless it starts at the start
        let start = at & !15;
        let from = start.saturating_sub(16);
        let mut raw = vec![0; ((end.next_multiple_of(16)) - from) as usize];
        self.file.seek(SeekFrom::Start(self.offset + from)).map_err(io)?;
        self.file.read_exact(&mut raw).map_err(io)?;
        let first = (start - from) as usize;
        let mut plain = raw[first..].to_vec();
        for (i, block) in plain.chunks_exact_mut(16).enumerate() {
            cipher.decrypt_block(GenericArray::from_mut_slice(block));
            let previous = if start == 0 && i == 0 { &iv[..] } else { &raw[first + i * 16 - 16..first + i * 16] };
            block.iter_mut().zip(previous).for_each(|(byte, mask)| *byte ^= mask);
        }
        let skip = (at - start) as usize;
        buf.copy_from_slice(&plain[skip..skip + buf.len()]);
        Ok(())
    }
}

/// decrypts one AES-CBC block.
fn cbc_block(key: &Key, iv: &Key, block: &Key) -> Key {
    let mut out = GenericArray::from(*block);
    Aes128::new(&GenericArray::from(*key)).decrypt_block(&mut out);
    let mut out: Key = out.into();
    out.iter_mut().zip(iv).for_each(|(byte, mask)| *byte ^= mask);
    out
}

/// the fields of an NCCH header decrypting needs.
struct Header {
    raw: [u8; NCCH_HEADER_SIZE],
}

impl Header {
    fn read(source: &mut Source) -> Result<Header, String> {
        let mut raw = [0; NCCH_HEADER_SIZE];
        source.read(0, &mut raw)?;
        if &raw[0x100..0x104] != b"NCCH" {
            return Err(if source.cbc.is_some() {
                "the executable has no NCCH header once decrypted with the title key, the common key may be wrong".into()
            } else {
                "the executable partition has no NCCH header".into()
            });
        }
        Ok(Header { raw })
    }

    /// the KeyY of the primary key, the start of the signature.
    fn key_y(&self) -> Key {
        self.raw[..16].try_into().unwrap()
    }

    fn content_size(&self) -> u64 {
        u32_at(&self.raw, 0x104) as u64 * MEDIA_UNIT
    }

    fn partition_id(&self) -> [u8; 8] {
        self.raw[0x108..0x110].try_into().unwrap()
    }

    fn version(&self) -> u16 {
        u16_at(&self.raw, 0x112)
    }

    fn program_id(&self) -> u64 {
        u64_at(&self.raw, 0x118)
    }

    fn product_code(&self) -> String {
        let code = &self.raw[0x150..0x160];
        let end = code.iter().position(|&b| b == 0).unwrap_or(code.len());
        String::from_utf8_lossy(&code[..end]).into_owned()
    }

    fn exheader_size(&self) -> u32 {
        u32_at(&self.raw, 0x180)
    }

    fn flags(&self) -> [u8; 8] {
        self.raw[0x188..0x190].try_into().unwrap()
    }

    /// where a region is, from a pair of offset and size in media units.
    fn region(&self, at: usize) -> (u64, u64) {
        (u32_at(&self.raw, at) as u64 * MEDIA_UNIT, u32_at(&self.raw, at + 4) as u64 * MEDIA_UNIT)
    }

    fn exefs(&self) -> (u64, u64) {
        self.region(0x1A0)
    }

    fn romfs(&self) -> (u64, u64) {
        self.region(0x1B0)
    }
}

/// the flags of a decrypted NCCH, NoCrypto set, fixed key and seed crypto
/// cleared and the crypto method back to the original.
pub fn decrypted_flags(mut flags: [u8; 8]) -> [u8; 8] {
    flags[3] = 0;
    flags[7] = (flags[7] | 0x04) & !(0x01 | 0x20);
    flags
}

/// whether the NCCH needs decrypting. NoCrypto says it does not. without
/// it, a dump decrypted with its flags left as they were is told by its
/// exheader, whose jump id is the program's, the check Citra makes, or with
/// no exheader by its ExeFS header naming its files.
fn ncch_encrypted(source: &mut Source, header: &Header) -> Result<bool, String> {
    if header.flags()[7] & 0x04 != 0 {
        return Ok(false);
    }
    if header.exheader_size() != 0 {
        let mut jump = [0; 8];
        source.read(NCCH_HEADER_SIZE as u64 + 0x1C8, &mut jump)?;
        return Ok(u64::from_le_bytes(jump) as u32 != header.program_id() as u32);
    }
    let (exefs, size) = header.exefs();
    if size >= EXEFS_HEADER_SIZE {
        let mut table = [0; EXEFS_HEADER_SIZE as usize];
        source.read(exefs, &mut table)?;
        let names: Vec<&[u8]> = table[..0xA0].chunks(16).map(|entry| &entry[..8]).filter(|name| name[0] != 0).collect();
        let readable = |name: &&[u8]| name.iter().take_while(|&&b| b != 0).all(|b| b.is_ascii_graphic());
        return Ok(names.is_empty() || !names.iter().all(readable));
    }
    Ok(true)
}

/// the counters of the exheader, the ExeFS and the RomFS, by the NCCH's
/// version: 0 and 2 have the partition id backwards and the section's
/// number, 1 the partition id and the section's offset, as though the whole
/// NCCH were one stream.
fn counters(header: &Header) -> Result<[Key; 3], String> {
    let id = header.partition_id();
    let mut base = [0; 16];
    match header.version() {
        0 | 2 => {
            for (i, byte) in id.iter().rev().enumerate() {
                base[i] = *byte;
            }
            Ok([1u8, 2, 3].map(|section| {
                let mut counter = base;
                counter[8] = section;
                counter
            }))
        }
        1 => {
            base[..8].copy_from_slice(&id);
            Ok([NCCH_HEADER_SIZE as u64, header.exefs().0, header.romfs().0].map(|offset| {
                let mut counter = base;
                counter[12..].copy_from_slice(&(offset as u32).to_be_bytes());
                counter
            }))
        }
        version => Err(format!("the NCCH has version {version}, which 3dsrecomp does not know how to decrypt")),
    }
}

/// a stretch of the NCCH and the key stream that decrypts it, which starts
/// at origin.
#[derive(Clone, Copy, Debug)]
struct Range {
    start: u64,
    end: u64,
    key: Key,
    counter: Key,
    origin: u64,
}

impl Range {
    /// decrypts the part of buf, which starts at at in the NCCH, that this
    /// range covers.
    fn apply(&self, at: u64, buf: &mut [u8]) {
        let (start, end) = (self.start.max(at), self.end.min(at + buf.len() as u64));
        if start >= end {
            return;
        }
        let mut cipher = Ctr::new(&GenericArray::from(self.key), &GenericArray::from(self.counter));
        cipher.seek(start - self.origin);
        cipher.apply_keystream(&mut buf[(start - at) as usize..(end - at) as usize]);
    }
}

/// what decrypting a dump goes by, and where it looks for keys and seeds.
struct Context<'a> {
    rom: &'a Path,
    options: &'a KeyOptions,
    key_locations: Vec<PathBuf>,
    seed_locations: Vec<PathBuf>,
    keys: Option<Keys>,
}

impl<'a> Context<'a> {
    fn new(rom: &'a Path, options: &'a KeyOptions) -> Context<'a> {
        Context { rom, options, key_locations: key_file_locations(rom), seed_locations: seeddb_locations(rom), keys: None }
    }

    fn keys(&mut self) -> Result<&Keys, String> {
        if self.keys.is_none() {
            self.keys = Some(find_keys(self.rom, self.options, &self.key_locations)?);
        }
        Ok(self.keys.as_ref().unwrap())
    }

    fn seeds(&self) -> Result<Seeds, String> {
        if let Some(path) = &self.options.seeddb {
            return Seeds::load(path);
        }
        match self.seed_locations.iter().find(|path| path.is_file()) {
            Some(path) => Seeds::load(path),
            None => Err(String::new()),
        }
    }

    /// opens the dump's NCCH, decrypting the title key when it needs one.
    fn open(&mut self) -> Result<(Source, Container), String> {
        let mut file = File::open(self.rom).map_err(|error| format!("could not open {}, {error}", self.rom.display()))?;
        let container = locate(&mut file)?;
        let cbc = match container.title_key {
            Some(ticket) => Some(self.title_key(&ticket)?),
            None => None,
        };
        let source = Source { file, offset: container.offset, size: container.size, cbc };
        Ok((source, container))
    }

    /// the title key and the content's IV: the ticket's key decrypted with
    /// keyslot 0x3D and the common KeyY the ticket picks, its IV the title
    /// id, and the content's IV its index.
    fn title_key(&mut self, ticket: &TitleKey) -> Result<(Aes128, Key), String> {
        let rom = self.rom.display().to_string();
        let keys = self.keys().map_err(|error| format!("{error}\n(the .cia's executable is encrypted with a title key)"))?;
        let index = ticket.common_index as usize;
        let key_y = keys.common_key_y(index).ok_or_else(|| {
            format!("{} has no common{index} (or slot0x3DKeyY{index}), the common KeyY the ticket of {rom} asks for", keys.file_name())
        })?;
        let common = keys
            .normal(COMMON_SLOT, &key_y)
            .ok_or_else(|| format!("{} has no slot0x3DKeyX, which the title key of {rom} is encrypted with", keys.file_name()))?;
        let mut iv = [0; 16];
        iv[..8].copy_from_slice(&ticket.title_id.to_be_bytes());
        let title_key = cbc_block(&common, &iv, &ticket.encrypted);
        let mut content_iv = [0; 16];
        content_iv[..2].copy_from_slice(&ticket.content_index.to_be_bytes());
        Ok((Aes128::new(&GenericArray::from(title_key)), content_iv))
    }

    /// the primary and secondary keys of the NCCH.
    fn ncch_keys(&mut self, header: &Header) -> Result<(Key, Key), String> {
        let rom = self.rom.display().to_string();
        let flags = header.flags();
        if flags[7] & 0x01 != 0 {
            // fixed-key crypto, the zero key, but system titles have a key
            // of their own
            if header.program_id() & (0x10 << 32) != 0 {
                return Err(format!("{rom} is a system title encrypted with the fixed system key, which 3dsrecomp does not decrypt"));
            }
            return Ok(([0; 16], [0; 16]));
        }
        let slot = match flags[3] {
            0x00 => NCCH_SLOT,
            0x01 => 0x25,
            0x0A => 0x18,
            0x0B => 0x1B,
            method => return Err(format!("{rom} has crypto method {method:#04X}, which 3dsrecomp does not know")),
        };
        let key_y = header.key_y();
        let secondary_y = if flags[7] & 0x20 != 0 { self.seeded(header, &key_y)? } else { key_y };
        let keys = self.keys()?;
        let primary = keys.normal(NCCH_SLOT, &key_y).ok_or_else(|| {
            format!("{} has no slot0x2CKeyX, the key every encrypted title's primary key is made from", keys.file_name())
        })?;
        let secondary = keys.normal(slot, &secondary_y).ok_or_else(|| {
            format!("{} has no slot0x{slot:02X}KeyX, which {rom}'s crypto method {:#04X} needs", keys.file_name(), flags[3])
        })?;
        Ok((primary, secondary))
    }

    /// the secondary KeyY of a title with seed crypto, the first 16 bytes
    /// of SHA-256 of the KeyY and the title's seed, which the header checks
    /// with the start of SHA-256 of the seed and the program id.
    fn seeded(&self, header: &Header, key_y: &Key) -> Result<Key, String> {
        let program_id = header.program_id();
        let missing = || missing_seeds(self.rom, program_id, &self.seed_locations);
        let seeds = self.seeds().map_err(|error| if error.is_empty() { missing() } else { error })?;
        let seed = seeds.get(program_id).ok_or_else(|| {
            let name = seeds.path.as_ref().map_or_else(|| "the seed database".to_owned(), |path| path.display().to_string());
            format!("{name} has no seed for {program_id:016X}\n{}", missing())
        })?;
        let check = Sha256::new().chain_update(seed).chain_update(program_id.to_le_bytes()).finalize();
        if check[..4] != header.raw[0x114..0x118] {
            return Err(format!("the seed for {program_id:016X} does not match the one {} was made with", self.rom.display()));
        }
        let hash = Sha256::new().chain_update(key_y).chain_update(seed).finalize();
        Ok(hash[..16].try_into().unwrap())
    }

    /// the ranges to decrypt, none when the NCCH is not encrypted.
    fn plan(&mut self, source: &mut Source, header: &Header) -> Result<Vec<Range>, String> {
        if !ncch_encrypted(source, header)? {
            return Ok(Vec::new());
        }
        let (primary, secondary) = self.ncch_keys(header)?;
        let [exheader_counter, exefs_counter, romfs_counter] = counters(header)?;
        let mut ranges = Vec::new();
        if header.exheader_size() != 0 {
            let start = NCCH_HEADER_SIZE as u64;
            ranges.push(Range { start, end: start + EXHEADER_SIZE, key: primary, counter: exheader_counter, origin: start });
        }
        let (exefs, exefs_size) = header.exefs();
        if exefs_size != 0 {
            let end = exefs + exefs_size;
            let range = |start, end, key| Range { start, end, key, counter: exefs_counter, origin: exefs };
            // the header tells where the files are, and icon and banner are
            // the primary key's like it, the rest the secondary's, each to
            // the end of its last media unit
            let mut table = [0; EXEFS_HEADER_SIZE as usize];
            source.read(exefs, &mut table)?;
            range(exefs, exefs + EXEFS_HEADER_SIZE, primary).apply(exefs, &mut table);
            let mut secondary_files: Vec<(u64, u64)> = table[..0xA0]
                .chunks(16)
                .filter(|entry| entry[0] != 0 && u32_at(entry, 12) != 0)
                .filter(|entry| {
                    let name = &entry[..8];
                    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(8)];
                    name != b"icon" && name != b"banner"
                })
                .map(|entry| {
                    let start = exefs + EXEFS_HEADER_SIZE + u32_at(entry, 8) as u64;
                    (start.min(end), (start + (u32_at(entry, 12) as u64).next_multiple_of(MEDIA_UNIT)).min(end))
                })
                .filter(|(start, end)| start < end)
                .collect();
            secondary_files.sort_unstable();
            let mut cursor = exefs;
            for (start, stop) in secondary_files {
                if start > cursor {
                    ranges.push(range(cursor, start, primary));
                }
                let start = start.max(cursor);
                if stop > start {
                    ranges.push(range(start, stop, secondary));
                }
                cursor = cursor.max(stop);
            }
            if cursor < end {
                ranges.push(range(cursor, end, primary));
            }
        }
        let (romfs, romfs_size) = header.romfs();
        if romfs_size != 0 {
            ranges.push(Range { start: romfs, end: romfs + romfs_size, key: secondary, counter: romfs_counter, origin: romfs });
        }
        Ok(ranges)
    }
}

/// whether the dump at rom is encrypted, its .cia content with a title key
/// or its NCCH, so that it needs decrypt_to_cxi before the rest can read it.
pub fn is_encrypted(rom: &Path) -> Result<bool, String> {
    let mut file = File::open(rom).map_err(|error| format!("could not open {}, {error}", rom.display()))?;
    let container = locate(&mut file)?;
    if container.title_key.is_some() {
        return Ok(true);
    }
    let mut source = Source { file, offset: container.offset, size: container.size, cbc: None };
    let header = Header::read(&mut source)?;
    ncch_encrypted(&mut source, &header)
}

/// whether rom::Title reads the dump at rom as it is: not encrypted, and
/// with flags that say so, or at least no crypto method it would take for
/// encryption.
pub fn readable_as_is(rom: &Path) -> Result<bool, String> {
    let mut file = File::open(rom).map_err(|error| format!("could not open {}, {error}", rom.display()))?;
    let container = locate(&mut file)?;
    if container.title_key.is_some() {
        return Ok(false);
    }
    let mut source = Source { file, offset: container.offset, size: container.size, cbc: None };
    let header = Header::read(&mut source)?;
    let flags = header.flags();
    Ok(!ncch_encrypted(&mut source, &header)? && (flags[7] & 0x04 != 0 || flags[3] == 0))
}

/// writes the executable NCCH of the dump at rom, a .3ds, .cia or .cxi, to
/// out as a decrypted .cxi, or only takes it out when it is not encrypted.
pub fn decrypt_to_cxi(rom: &Path, out: &Path, keys: &KeyOptions) -> Result<DecryptInfo, String> {
    decrypt_to_cxi_with_progress(rom, out, keys, &|_, _| {})
}

/// decrypt_to_cxi, telling progress the bytes written so far and of how
/// many after every chunk.
pub fn decrypt_to_cxi_with_progress(
    rom: &Path,
    out: &Path,
    keys: &KeyOptions,
    progress: &dyn Fn(u64, u64),
) -> Result<DecryptInfo, String> {
    decrypt(Context::new(rom, keys), out, progress)
}

fn decrypt(mut context: Context, out: &Path, progress: &dyn Fn(u64, u64)) -> Result<DecryptInfo, String> {
    let rom = context.rom;
    if let (Ok(a), Ok(b)) = (rom.canonicalize(), out.canonicalize()) {
        if a == b {
            return Err(format!("{} would be written over itself", rom.display()));
        }
    }
    let (mut source, container) = context.open()?;
    let header = Header::read(&mut source)?;
    // everything that can fail for want of a key does before out is made
    let ranges = context.plan(&mut source, &header)?;
    let size = match header.content_size() {
        0 => source.size,
        size => size.min(source.size),
    };
    let info = DecryptInfo {
        title_id: header.program_id(),
        product_code: header.product_code(),
        already_decrypted: ranges.is_empty() && container.title_key.is_none(),
        size,
    };

    if let Some(dir) = out.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|error| format!("could not create {}, {error}", dir.display()))?;
    }
    let partial = out.with_file_name(format!("{}.part", out.file_name().and_then(|n| n.to_str()).unwrap_or("decrypted.cxi")));
    let written = (|| -> Result<(), String> {
        let mut file = File::create(&partial).map_err(|error| format!("could not create {}, {error}", partial.display()))?;
        let mut buf = vec![0; CHUNK];
        let mut at = 0;
        while at < size {
            let len = (size - at).min(CHUNK as u64) as usize;
            let chunk = &mut buf[..len];
            source.read(at, chunk)?;
            for range in &ranges {
                range.apply(at, chunk);
            }
            if at == 0 {
                let flags = decrypted_flags(header.flags());
                chunk[0x188..0x190].copy_from_slice(&flags);
            }
            file.write_all(chunk).map_err(|error| format!("could not write {}, {error}", partial.display()))?;
            at += len as u64;
            progress(at, size);
        }
        file.sync_all().map_err(|error| format!("could not write {}, {error}", partial.display()))?;
        std::fs::rename(&partial, out).map_err(|error| format!("could not write {}, {error}", out.display()))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written.map(|()| info)
}

/// the title id a dump goes by before anything is decrypted.
fn title_id(rom: &Path) -> Result<u64, String> {
    let mut file = File::open(rom).map_err(|error| format!("could not open {}, {error}", rom.display()))?;
    let container = locate(&mut file)?;
    if container.title_key.is_none() {
        let mut source = Source { file, offset: container.offset, size: container.size, cbc: None };
        if let Ok(header) = Header::read(&mut source) {
            return Ok(header.program_id());
        }
    }
    container.title_id.ok_or_else(|| "the dump has no title id".to_owned())
}

/// where decrypted copies are cached, the title's folder in 3dsrecomp's
/// cache, the one build works in.
pub fn cache_path(title_id: u64) -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|home| home.join("Library/Caches"))
    } else {
        var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("3dsrecomp").join(format!("{title_id:016X}")).join("decrypted.cxi")
}

/// the path of the dump at rom in a form rom::Title reads: rom itself when
/// it needs no decrypting, or else a decrypted .cxi in the cache, made the
/// first time and kept for as long as the dump stays the same. note hears
/// what is going on.
pub fn readable(rom: &Path, keys: &KeyOptions, note: &dyn Fn(&str)) -> Result<PathBuf, String> {
    if readable_as_is(rom)? {
        return Ok(rom.to_owned());
    }
    let cached = cache_path(title_id(rom)?);
    // what the copy was made from, the dump's path, size and time
    let metadata = std::fs::metadata(rom).map_err(|error| format!("could not read {}, {error}", rom.display()))?;
    let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
    let stamp = format!(
        "{}\n{}\n{}\n",
        rom.canonicalize().unwrap_or_else(|_| rom.to_owned()).display(),
        metadata.len(),
        modified.map_or(0, |time| time.as_secs())
    );
    let stamp_path = cached.with_extension("cxi.source");
    if cached.is_file() && std::fs::read_to_string(&stamp_path).is_ok_and(|text| text == stamp) {
        note(&format!("using the decrypted copy {}", cached.display()));
        return Ok(cached);
    }
    let _ = std::fs::remove_file(&stamp_path);
    note(&format!("{} is encrypted, decrypting it to {}", rom.display(), cached.display()));
    decrypt_to_cxi(rom, &cached, keys)?;
    std::fs::write(&stamp_path, stamp).map_err(|error| format!("could not write {}, {error}", stamp_path.display()))?;
    Ok(cached)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncrypt;

    const KEY_X_2C: Key = *b"made-up 0x2C KX!";
    const KEY_X_25: Key = *b"made-up 0x25 KX!";
    const KEY_X_18: Key = *b"made-up 0x18 KX!";
    const KEY_X_1B: Key = *b"made-up 0x1B KX!";
    const KEY_X_3D: Key = *b"made-up 0x3D KX!";
    const COMMON_1: Key = *b"made-up common 1";
    const SEED: Key = *b"made-up seed 16b";
    const PROGRAM_ID: u64 = 0x0004_0000_0FF3_DE00;
    const PARTITION_ID: u64 = 0x0004_0000_0FF3_DE01;

    fn hex(key: &Key) -> String {
        key.iter().map(|b| format!("{b:02X}")).collect()
    }

    /// Citra's bytewise ROL128 and Add128 (arithmetic128.cpp), apart from
    /// the u128 ones scramble uses.
    fn citra_rol(input: &Key, rot: u32) -> Key {
        let (bytes, bits) = ((rot % 128 / 8) as usize, rot % 8);
        let mut out = [0; 16];
        for (i, byte) in out.iter_mut().enumerate() {
            let a = input[(i + bytes) % 16] as u32;
            let b = input[(i + bytes + 1) % 16] as u32;
            *byte = (((a << bits) | (b >> (8 - bits))) & 0xFF) as u8;
        }
        out
    }

    fn citra_add(a: &Key, b: &Key) -> Key {
        let (mut out, mut carry) = ([0; 16], 0u32);
        for i in (0..16).rev() {
            let sum = a[i] as u32 + b[i] as u32 + carry;
            carry = sum >> 8;
            out[i] = sum as u8;
        }
        out
    }

    fn citra_scramble(x: &Key, y: &Key, c: &Key) -> Key {
        let mut mixed = citra_rol(x, 2);
        mixed.iter_mut().zip(y).for_each(|(a, b)| *a ^= b);
        citra_rol(&citra_add(&mixed, c), 87)
    }

    /// a xorshift, for keys made up in bulk.
    fn noise(state: &mut u64) -> Key {
        let mut key = [0; 16];
        for byte in &mut key {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *byte = *state as u8;
        }
        key
    }

    #[test]
    fn the_scrambler_matches_the_formula() {
        // worked out apart from this code, with Python's big integers
        let x = 0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF_u128.to_be_bytes();
        let y = 0x0F1E_2D3C_4B5A_6978_8796_A5B4_C3D2_E1F0_u128.to_be_bytes();
        assert_eq!(scramble(&x, &y, &GENERATOR), 0x9C26_FBE8_4B17_AA47_CE10_06FA_5754_1AD0_u128.to_be_bytes());
        let mut state = 0x3D5_C0DE;
        for _ in 0..1000 {
            let (x, y, c) = (noise(&mut state), noise(&mut state), noise(&mut state));
            let normal = scramble(&x, &y, &c);
            assert_eq!(normal, citra_scramble(&x, &y, &c));
            // and undone, the KeyY back from the normal key and the KeyX
            let undone = u128::from_be_bytes(normal).rotate_right(87).wrapping_sub(u128::from_be_bytes(c))
                ^ u128::from_be_bytes(x).rotate_left(2);
            assert_eq!(undone.to_be_bytes(), y);
        }
    }

    #[test]
    fn key_files_read_as_citra_and_azahar_write_them() {
        let citra = format!(
            "# comment\nslot0x2CKeyX={}\nslot0x25KeyX=0x{}\nslot0x18keyX={}\n\ncommon0={}\nslot0x3DKeyY3={}  trailing\nslot0x2CKeyY={}\nslot0x11KeyN={}\nbogus=12\n",
            hex(&KEY_X_2C),
            hex(&KEY_X_25),
            hex(&KEY_X_18),
            hex(&COMMON_1),
            hex(&SEED),
            hex(&KEY_X_3D),
            hex(&KEY_X_1B)
        );
        let keys = Keys::parse(&citra);
        assert_eq!(keys.key_x(0x2C), Some(KEY_X_2C));
        assert_eq!(keys.key_x(0x25), Some(KEY_X_25));
        // Key, not key
        assert_eq!(keys.key_x(0x18), None);
        assert_eq!(keys.common_key_y(0), Some(COMMON_1));
        assert_eq!(keys.common_key_y(3), Some(SEED));
        assert_eq!(keys.key_y(0x2C), Some(KEY_X_3D));
        assert_eq!(keys.normal_key(0x11), Some(KEY_X_1B));
        assert_eq!(keys.generator(), GENERATOR);
        assert_eq!(keys.normal(0x2C, &SEED), Some(scramble(&KEY_X_2C, &SEED, &GENERATOR)));

        let azahar = format!(
            ":RSA\nslot0x2CKeyX={}\n:AES\ngeneratorConstant={}\nslot0x2cKeyX={}\n",
            hex(&KEY_X_25),
            hex(&SEED),
            hex(&KEY_X_2C)
        );
        let keys = Keys::parse(&azahar);
        assert_eq!(keys.key_x(0x2C), Some(KEY_X_2C));
        assert_eq!(keys.generator(), SEED);
        assert_eq!(Keys::parse(&format!("generator={}", hex(&COMMON_1))).generator(), COMMON_1);
    }

    #[test]
    fn decrypted_flags_say_no_crypto() {
        assert_eq!(decrypted_flags([0, 0, 0, 0x01, 1, 2, 0, 0x21]), [0, 0, 0, 0, 1, 2, 0, 0x04]);
        assert_eq!(decrypted_flags([0, 0, 0, 0x0B, 2, 3, 0, 0x22]), [0, 0, 0, 0, 2, 3, 0, 0x06]);
        assert_eq!(decrypted_flags([0, 0, 0, 0, 1, 3, 0, 0x04]), [0, 0, 0, 0, 1, 3, 0, 0x04]);
    }

    fn seeddb(entries: &[(u64, Key)]) -> Vec<u8> {
        let mut out = vec![0; 0x10];
        out[..4].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (id, seed) in entries {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(seed);
            out.extend_from_slice(&[0; 8]);
        }
        out
    }

    #[test]
    fn seed_databases_read() {
        let seeds = Seeds::parse(&seeddb(&[(1, COMMON_1), (PROGRAM_ID, SEED)])).unwrap();
        assert_eq!(seeds.get(PROGRAM_ID), Some(SEED));
        assert_eq!(seeds.get(1), Some(COMMON_1));
        assert_eq!(seeds.get(2), None);
        assert!(Seeds::parse(&seeddb(&[(1, SEED)])[..0x20]).is_err());
    }

    fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
        out[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// where the test NCCH's parts are, and how long.
    const EXEFS: usize = 0xC00;
    const ICON_SIZE: usize = 0x2C0;
    const CODE_SIZE: usize = 0x450;
    const LOGO_SIZE: usize = 0x130;
    const EXEFS_SIZE: usize = 0x1000;
    const PLAIN: usize = EXEFS + EXEFS_SIZE;
    const ROMFS: usize = PLAIN + 0x200;
    const ROMFS_SIZE: usize = 0x3000;

    /// a plain NCCH, flags saying it is encrypted with the original key: a
    /// header, the exheader and access descriptor, an ExeFS of icon, .code
    /// and logo, a plain region and a RomFS.
    fn plain_ncch(version: u16) -> Vec<u8> {
        let mut state = 0xC7_1D_5EED;
        let mut fill = |out: &mut Vec<u8>, at: usize, len: usize| {
            for i in (at..at + len).step_by(16) {
                let block = noise(&mut state);
                let end = (i + 16).min(at + len);
                out[i..end].copy_from_slice(&block[..end - i]);
            }
        };
        let mut out = vec![0; ROMFS + ROMFS_SIZE];
        fill(&mut out, 0, 0x100);
        put(&mut out, 0x100, b"NCCH");
        let units = (out.len() / 0x200) as u32;
        put(&mut out, 0x104, &units.to_le_bytes());
        put(&mut out, 0x108, &PARTITION_ID.to_le_bytes());
        put(&mut out, 0x112, &version.to_le_bytes());
        put(&mut out, 0x118, &PROGRAM_ID.to_le_bytes());
        put(&mut out, 0x150, b"CTR-P-TEST");
        put(&mut out, 0x180, &0x400u32.to_le_bytes());
        out[0x18C] = 1;
        out[0x18D] = 0x03;
        put(&mut out, 0x190, &[(PLAIN / 0x200) as u32, 1].map(u32::to_le_bytes).concat());
        put(&mut out, 0x1A0, &[(EXEFS / 0x200) as u32, (EXEFS_SIZE / 0x200) as u32, 1].map(u32::to_le_bytes).concat());
        put(&mut out, 0x1B0, &[(ROMFS / 0x200) as u32, (ROMFS_SIZE / 0x200) as u32, 1].map(u32::to_le_bytes).concat());
        // the exheader, its jump id the program's, and the access descriptor
        fill(&mut out, 0x200, 0x800);
        put(&mut out, 0x200, b"test\0\0\0\0");
        put(&mut out, 0x200 + 0x1C8, &PROGRAM_ID.to_le_bytes());
        // .code is not compressed
        out[0x20D] = 0;
        // the ExeFS header and the files, icon at 0, .code at 0x600, past a
        // gap, logo at 0xC00
        for (i, (name, offset, size)) in [(&b"icon"[..], 0, ICON_SIZE), (b".code", 0x600, CODE_SIZE), (b"logo", 0xC00, LOGO_SIZE)]
            .into_iter()
            .enumerate()
        {
            put(&mut out, EXEFS + i * 16, name);
            put(&mut out, EXEFS + i * 16 + 8, &[offset as u32, size as u32].map(u32::to_le_bytes).concat());
            fill(&mut out, EXEFS + 0x200 + offset, size);
        }
        fill(&mut out, PLAIN, 0x200);
        fill(&mut out, ROMFS, ROMFS_SIZE);
        put(&mut out, ROMFS, b"IVFC");
        out
    }

    /// AES-CTR the way 3dbrew tells it, block by block, the counter the
    /// section's base plus the block's number within the section.
    fn reference_ctr(data: &mut [u8], key: &Key, base: &Key, offset_in_section: usize) {
        let cipher = Aes128::new(&GenericArray::from(*key));
        for (i, byte) in data.iter_mut().enumerate() {
            let position = offset_in_section + i;
            let counter = u128::from_be_bytes(*base).wrapping_add((position / 16) as u128);
            let mut block = GenericArray::from(counter.to_be_bytes());
            cipher.encrypt_block(&mut block);
            *byte ^= block[position % 16];
        }
    }

    /// encrypts plain as a retail NCCH of a version and crypto method is,
    /// written apart from the decrypting code, from 3dbrew's NCCH page.
    fn reference_encrypt(plain: &[u8], method: u8, seed: bool, fixed: bool) -> Vec<u8> {
        let mut out = plain.to_vec();
        out[0x18B] = method;
        out[0x18F] = if seed { 0x20 } else { 0 } | if fixed { 0x01 } else { 0 };
        if seed {
            let check = Sha256::new().chain_update(SEED).chain_update(PROGRAM_ID.to_le_bytes()).finalize();
            put(&mut out, 0x114, &check[..4]);
        }
        let key_y: Key = plain[..16].try_into().unwrap();
        let secondary_y: Key = if seed { Sha256::digest([key_y, SEED].concat())[..16].try_into().unwrap() } else { key_y };
        let secondary_x = match method {
            0x00 => KEY_X_2C,
            0x01 => KEY_X_25,
            0x0A => KEY_X_18,
            0x0B => KEY_X_1B,
            _ => unreachable!(),
        };
        let (primary, secondary) = if fixed {
            ([0; 16], [0; 16])
        } else {
            (citra_scramble(&KEY_X_2C, &key_y, &GENERATOR), citra_scramble(&secondary_x, &secondary_y, &GENERATOR))
        };
        let version = u16_at(plain, 0x112);
        // the section's counter: version 0 and 2, the partition id as a
        // big-endian number then the section's number; version 1, the
        // partition id's bytes as stored then the section's byte offset
        let base = |section: u8, offset: usize| -> Key {
            let mut counter = [0; 16];
            if version == 1 {
                counter[..8].copy_from_slice(&PARTITION_ID.to_le_bytes());
                counter[12..].copy_from_slice(&(offset as u32).to_be_bytes());
            } else {
                counter[..8].copy_from_slice(&PARTITION_ID.to_be_bytes());
                counter[8] = section;
            }
            counter
        };
        reference_ctr(&mut out[0x200..0xA00], &primary, &base(1, 0x200), 0);
        // the ExeFS header, icon and the gap after it with the primary key,
        // .code and logo, to the end of their media units, the secondary
        let exefs = base(2, EXEFS);
        for (start, end, key) in [
            (0, 0x200 + ICON_SIZE.next_multiple_of(0x200), primary),
            (0x600, 0x800, primary),
            (0x800, 0x800 + CODE_SIZE.next_multiple_of(0x200), secondary),
            (0xE00, 0xE00 + LOGO_SIZE.next_multiple_of(0x200), secondary),
        ] {
            reference_ctr(&mut out[EXEFS + start..EXEFS + end], &key, &exefs, start);
        }
        reference_ctr(&mut out[ROMFS..ROMFS + ROMFS_SIZE], &secondary, &base(3, ROMFS), 0);
        out
    }

    /// a folder of the test's own, where nothing is found by itself.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("3dsrecomp-crypto-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn key_file(dir: &Path) -> PathBuf {
        let text = format!(
            "slot0x2CKeyX={}\nslot0x25KeyX={}\nslot0x18KeyX={}\nslot0x1BKeyX={}\nslot0x3DKeyX={}\ncommon1={}\n",
            hex(&KEY_X_2C),
            hex(&KEY_X_25),
            hex(&KEY_X_18),
            hex(&KEY_X_1B),
            hex(&KEY_X_3D),
            hex(&COMMON_1)
        );
        let path = dir.join("my-keys.txt");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// decrypts the dump at rom with nothing looked for anywhere but where
    /// options say.
    fn decrypt_alone(rom: &Path, out: &Path, options: &KeyOptions) -> Result<DecryptInfo, String> {
        let context = Context { rom, options, key_locations: Vec::new(), seed_locations: Vec::new(), keys: None };
        decrypt(context, out, &|_, _| {})
    }

    fn expected(plain: &[u8]) -> Vec<u8> {
        let mut expected = plain.to_vec();
        let flags = decrypted_flags(plain[0x188..0x190].try_into().unwrap());
        put(&mut expected, 0x188, &flags);
        expected
    }

    #[test]
    fn ncchs_of_every_version_and_crypto_method_decrypt() {
        let dir = scratch("ncch");
        let options = KeyOptions { keys: Some(key_file(&dir)), seeddb: Some(dir.join("seeds.bin")) };
        std::fs::write(dir.join("seeds.bin"), seeddb(&[(1, COMMON_1), (PROGRAM_ID, SEED)])).unwrap();
        for (version, method, seed) in [
            (0, 0x00, false),
            (2, 0x01, false),
            (1, 0x00, false),
            (1, 0x0A, false),
            (0, 0x0B, false),
            (0, 0x00, true),
            (2, 0x01, true),
            (1, 0x01, true),
        ] {
            let plain = plain_ncch(version);
            let encrypted = reference_encrypt(&plain, method, seed, false);
            let (rom, out) = (dir.join("game.cxi"), dir.join("out.cxi"));
            std::fs::write(&rom, &encrypted).unwrap();
            assert!(is_encrypted(&rom).unwrap());
            assert!(!readable_as_is(&rom).unwrap());
            let info = decrypt_alone(&rom, &out, &options).unwrap();
            assert_eq!((info.title_id, info.product_code.as_str(), info.already_decrypted), (PROGRAM_ID, "CTR-P-TEST", false));
            let decrypted = std::fs::read(&out).unwrap();
            // the header as it was, the seed check too, but for the flags
            let mut want = expected(&plain);
            want[0x114..0x118].copy_from_slice(&encrypted[0x114..0x118]);
            assert!(decrypted == want, "version {version}, method {method:#X}, seed {seed}");
            assert!(!is_encrypted(&out).unwrap() && readable_as_is(&out).unwrap());
            // and it reads as a title
            let title = crate::rom::Title::load(&out).unwrap();
            assert_eq!(title.program_id(), PROGRAM_ID);
            assert_eq!(title.code().unwrap(), &plain[EXEFS + 0x800..EXEFS + 0x800 + CODE_SIZE]);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_wrong_or_missing_seed_is_told() {
        let dir = scratch("seed");
        let rom = dir.join("game.cxi");
        std::fs::write(&rom, reference_encrypt(&plain_ncch(0), 0x00, true, false)).unwrap();
        let keys = Some(key_file(&dir));
        let out = dir.join("out.cxi");
        let message = decrypt_alone(&rom, &out, &KeyOptions { keys: keys.clone(), seeddb: None }).unwrap_err();
        assert!(message.contains("seed crypto") && message.contains(&format!("{PROGRAM_ID:016X}")), "{message}");
        std::fs::write(dir.join("seeds.bin"), seeddb(&[(PROGRAM_ID, COMMON_1)])).unwrap();
        let message = decrypt_alone(&rom, &out, &KeyOptions { keys, seeddb: Some(dir.join("seeds.bin")) }).unwrap_err();
        assert!(message.contains("does not match"), "{message}");
        assert!(!out.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fixed_key_titles_need_no_key_file_but_system_ones_are_refused() {
        let dir = scratch("fixed");
        let (rom, out) = (dir.join("game.cxi"), dir.join("out.cxi"));
        let plain = plain_ncch(0);
        std::fs::write(&rom, reference_encrypt(&plain, 0x00, false, true)).unwrap();
        decrypt_alone(&rom, &out, &KeyOptions::default()).unwrap();
        assert!(std::fs::read(&out).unwrap() == expected(&plain));
        let mut system = reference_encrypt(&plain, 0x00, false, true);
        put(&mut system, 0x118, &(PROGRAM_ID | 0x10 << 32).to_le_bytes());
        std::fs::write(&rom, system).unwrap();
        assert!(decrypt_alone(&rom, &out, &KeyOptions::default()).unwrap_err().contains("fixed system key"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_keys_are_told_before_anything_is_written() {
        let dir = scratch("missing");
        let (rom, out) = (dir.join("game.cxi"), dir.join("out.cxi"));
        std::fs::write(&rom, reference_encrypt(&plain_ncch(0), 0x00, false, false)).unwrap();
        // a keys.txt of something else is passed over
        std::fs::write(dir.join("keys.txt"), "# other keys\nabcdef0123456789abcdef0123456789\ncommon_key=0123\n").unwrap();
        let locations = vec![dir.join("aes_keys.txt"), dir.join("keys.txt"), PathBuf::from("/sysdata/aes_keys.txt")];
        let context = Context { rom: &rom, options: &KeyOptions::default(), key_locations: locations, seed_locations: Vec::new(), keys: None };
        let message = decrypt(context, &out, &|_, _| {}).unwrap_err();
        assert!(message.contains("is encrypted") && message.contains("--keys") && message.contains("GodMode9"), "{message}");
        assert!(message.contains(&format!("\n  {}", dir.join("aes_keys.txt").display())), "{message}");
        assert!(message.contains("keys.txt (there, but with no 3DS AES keys in it)\n  /sysdata/aes_keys.txt"), "{message}");
        assert!(!out.exists() && !dir.join("out.cxi.part").exists());
        // a key file without the key the crypto method needs names it
        let keys = dir.join("partial.txt");
        std::fs::write(&keys, format!("slot0x2CKeyX={}\n", hex(&KEY_X_2C))).unwrap();
        std::fs::write(&rom, reference_encrypt(&plain_ncch(0), 0x0A, false, false)).unwrap();
        let message = decrypt_alone(&rom, &out, &KeyOptions { keys: Some(keys), seeddb: None }).unwrap_err();
        assert!(message.contains("no slot0x18KeyX"), "{message}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decrypted_dumps_are_taken_out_as_they_are() {
        let dir = scratch("plain");
        let (rom, out) = (dir.join("game.cxi"), dir.join("out.cxi"));
        // decrypted with its flags left saying encrypted
        let plain = plain_ncch(0);
        std::fs::write(&rom, &plain).unwrap();
        assert!(!is_encrypted(&rom).unwrap());
        let info = decrypt_alone(&rom, &out, &KeyOptions::default()).unwrap();
        assert!(info.already_decrypted);
        assert!(std::fs::read(&out).unwrap() == expected(&plain));
        // a cartridge image with it as its first partition
        let mut cart = vec![0; 0x4000];
        put(&mut cart, 0x100, b"NCSD");
        put(&mut cart, 0x108, &PROGRAM_ID.to_le_bytes());
        put(&mut cart, 0x120, &[0x4000 / 0x200, (plain.len() / 0x200) as u32].map(u32::to_le_bytes).concat());
        cart.extend_from_slice(&reference_encrypt(&plain, 0x01, false, false));
        cart.extend_from_slice(&[0xAA; 0x400]);
        let cart_path = dir.join("game.3ds");
        std::fs::write(&cart_path, cart).unwrap();
        let keys = KeyOptions { keys: Some(key_file(&dir)), seeddb: None };
        assert!(!decrypt_alone(&cart_path, &out, &keys).unwrap().already_decrypted);
        assert!(std::fs::read(&out).unwrap() == expected(&plain));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn cbc_encrypt(key: &Key, iv: &Key, data: &mut [u8]) {
        let cipher = Aes128::new(&GenericArray::from(*key));
        let mut previous = *iv;
        for block in data.chunks_exact_mut(16) {
            block.iter_mut().zip(&previous).for_each(|(byte, mask)| *byte ^= mask);
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
            previous.copy_from_slice(block);
        }
    }

    /// a .cia holding content as content 0, encrypted with a title key
    /// when there is one, and a manual after it.
    fn cia(content: &[u8], title_key: Option<&Key>) -> Vec<u8> {
        const RECORDS: usize = 0xC4 + 64 * 0x24;
        let (certificates, ticket_size) = (0xA00usize, 0x350usize);
        let tmd_size = 0x140 + RECORDS + 2 * 0x30;
        let mut out = vec![0; 0x2020];
        put(&mut out, 0, &CIA_HEADER_SIZE.to_le_bytes());
        put(&mut out, 0x08, &[certificates as u32, ticket_size as u32, tmd_size as u32].map(u32::to_le_bytes).concat());
        out[0x20] = 0xC0;
        out.resize(0x2040 + certificates, 0);
        // the ticket, its key encrypted with common key 1
        let mut ticket = vec![0; ticket_size];
        put(&mut ticket, 0, &0x0001_0004u32.to_be_bytes());
        if let Some(key) = title_key {
            let mut encrypted = *key;
            let mut iv = [0; 16];
            iv[..8].copy_from_slice(&PROGRAM_ID.to_be_bytes());
            cbc_encrypt(&citra_scramble(&KEY_X_3D, &COMMON_1, &GENERATOR), &iv, &mut encrypted);
            put(&mut ticket, 0x140 + 0x7F, &encrypted);
        }
        put(&mut ticket, 0x140 + 0x9C, &PROGRAM_ID.to_be_bytes());
        ticket[0x140 + 0xB1] = 1;
        out.extend(ticket);
        out.resize(out.len().next_multiple_of(0x40), 0);
        let mut tmd = vec![0; tmd_size];
        put(&mut tmd, 0, &0x0001_0004u32.to_be_bytes());
        put(&mut tmd, 0x140 + 0x4C, &PROGRAM_ID.to_be_bytes());
        put(&mut tmd, 0x140 + 0x9E, &2u16.to_be_bytes());
        for (i, (index, size)) in [(0u16, content.len()), (1, 0x200)].into_iter().enumerate() {
            let record = 0x140 + RECORDS + i * 0x30;
            put(&mut tmd, record + 4, &index.to_be_bytes());
            put(&mut tmd, record + 6, &(if index == 0 && title_key.is_some() { 1u16 } else { 0 }).to_be_bytes());
            put(&mut tmd, record + 8, &(size as u64).to_be_bytes());
        }
        out.extend(tmd);
        out.resize(out.len().next_multiple_of(0x40), 0);
        let mut content = content.to_vec();
        if let Some(key) = title_key {
            cbc_encrypt(key, &[0; 16], &mut content);
        }
        out.extend(content);
        out.resize(out.len().next_multiple_of(0x40), 0);
        out.extend([0x55; 0x200]);
        out
    }

    #[test]
    fn cias_encrypted_with_a_title_key_decrypt() {
        let dir = scratch("cia");
        let (rom, out) = (dir.join("game.cia"), dir.join("out.cxi"));
        let plain = plain_ncch(2);
        let title_key = *b"made-up titlekey";
        let keys = KeyOptions { keys: Some(key_file(&dir)), seeddb: None };
        std::fs::write(&rom, cia(&reference_encrypt(&plain, 0x01, false, false), Some(&title_key))).unwrap();
        assert!(is_encrypted(&rom).unwrap());
        let info = decrypt_alone(&rom, &out, &keys).unwrap();
        assert_eq!((info.title_id, info.already_decrypted, info.size), (PROGRAM_ID, false, plain.len() as u64));
        assert!(std::fs::read(&out).unwrap() == expected(&plain));
        // the common KeyY under the name of its slot does as well
        let other = dir.join("slot-keys.txt");
        let text = std::fs::read_to_string(keys.keys.as_ref().unwrap()).unwrap().replace("common1=", "slot0x3DKeyY1=");
        std::fs::write(&other, text).unwrap();
        decrypt_alone(&rom, &out, &KeyOptions { keys: Some(other), seeddb: None }).unwrap();
        assert!(std::fs::read(&out).unwrap() == expected(&plain));
        // a title key over a plain NCCH, and a .cia with neither
        std::fs::write(&rom, cia(&plain, Some(&title_key))).unwrap();
        assert!(!decrypt_alone(&rom, &out, &keys).unwrap().already_decrypted);
        assert!(std::fs::read(&out).unwrap() == expected(&plain));
        std::fs::write(&rom, cia(&plain, None)).unwrap();
        assert!(!is_encrypted(&rom).unwrap());
        assert!(decrypt_alone(&rom, &out, &KeyOptions::default()).unwrap().already_decrypted);
        assert!(std::fs::read(&out).unwrap() == expected(&plain));
        // reading through CBC from anywhere, across chunks' edges
        std::fs::write(&rom, cia(&plain, Some(&title_key))).unwrap();
        let mut context = Context { rom: &rom, options: &keys, key_locations: Vec::new(), seed_locations: Vec::new(), keys: None };
        let (mut source, _) = context.open().unwrap();
        for (at, len) in [(0, 5), (3, 40), (16, 16), (0x1F1, 0x30), (plain.len() - 7, 7)] {
            let mut buf = vec![0; len];
            source.read(at as u64, &mut buf).unwrap();
            assert_eq!(buf, &plain[at..at + len], "{at:#X}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
