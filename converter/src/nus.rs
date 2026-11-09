//! packs an unpacked Wii U title (code/ content/ meta/) into an installable
//! WUP: title.tmd, title.tik, title.cert and the encrypted 0000000X.app
//! contents with their .h3 hash tables, which WUP Installer GX2 installs on a
//! console with signature patches (Aroma, Mocha).
//!
//! the output follows NUSPacker's (github.com/Maschell/nuspacker), the tool
//! this replaces:
//!
//! - files go into contents by NUSPacker's default rules
//!   (ContentRules.getCommonRules), applied the way ContentRulesService does,
//!   a later rule taking files from an earlier one; content 0 is the FST.
//! - files sit in their content 0x20-aligned, in FST order (Content).
//! - unhashed contents are padded to 0x8000 and AES-128-CBC encrypted with the
//!   title key, IV the content id (2 bytes, big-endian) then zeros; their TMD
//!   hash is the SHA-1 of the padded plain content (Encryption,
//!   ContentHashes).
//! - hashed contents are 0x10000-byte blocks of 0x400 bytes of H0/H1/H2
//!   SHA-1s and 0xFC00 bytes of data, the H3s go in 0000000X.h3, and their
//!   TMD hash is the SHA-1 of that file (ContentHashes,
//!   Encryption.encryptChunkHashed).
//! - the TMD and the ticket are laid out as TMD.getAsData and
//!   Ticket.getAsData lay them out, the title key in the ticket encrypted with
//!   the common key, IV the title id then zeros; title.cert is NUSPacker's
//!   structural chain (Cert.getCertAsData), or a real one the user passes.
//!
//! where NUSPacker does something by accident, this does not: the last data
//! block of a hashed content is padded with zeros (NUSPacker leaves the
//! previous block's bytes there), and a content's group id goes in the FST
//! zero-extended (NUSPacker sign-extends ids of 0x8000 and up).
//!
//! the common key comes from the user, and neither it nor the title key is
//! ever printed.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;
use sha1::Digest as _;
use sha1::Sha1;
use sha2::Digest as _;
use sha2::Sha256;

/// bytes of file data in a block of a hashed content
const DATA_BLOCK: usize = 0xFC00;
/// a block of a hashed content: its hashes, then its data
const HASHED_BLOCK: usize = 0x10000;
const HASH_AREA: usize = 0x400;
/// unhashed contents and the FST are padded to this
const CONTENT_PADDING: u64 = 0x8000;
/// files are this aligned in their content
const FILE_ALIGNMENT: u64 = 0x20;
/// NUSPacker's ContentRulesService.MAX_CONTENT_LENGTH, (long)(0xBFFFFFFF *
/// 0.975): a rule's files go on in a new content past this
const MAX_CONTENT_LENGTH: u64 = 3_140_694_834;

const TYPE_CONTENT: u16 = 0x2000;
const TYPE_ENCRYPTED: u16 = 0x0001;
const TYPE_HASHED: u16 = 0x0002;

/// FST entry flags of the files of code/, meta/ and content/ (Settings)
const FLAGS_CODE: u16 = 0x0000;
const FLAGS_META: u16 = 0x0040;
const FLAGS_CONTENT: u16 = 0x0400;
const GROUP_CODE: u16 = 0x0000;
const GROUP_META: u16 = 0x0400;

/// what packing needs besides the files: the user's common key, the title
/// key, and a certificate chain if the user has one.
pub struct PackKeys {
    common: [u8; 16],
    title: [u8; 16],
    /// the title key was made up for this pack
    pub random_title_key: bool,
    pub cert: Option<PathBuf>,
}

impl PackKeys {
    /// the common key from its file, the title key from hex or a random
    /// one, and the certificate chain to copy, checked before anything slow
    /// runs.
    pub fn load(common_key_file: &Path, title_key: Option<&str>, cert: Option<&Path>) -> Result<PackKeys, String> {
        let common = read_key_file(common_key_file)?;
        let (title, random_title_key) = match title_key {
            Some(text) => (parse_hex_key(text).ok_or("--title-key needs 32 hex digits (16 bytes)")?, false),
            None => (random_key()?, true),
        };
        if let Some(cert) = cert {
            if !cert.is_file() {
                return Err(format!("{} (--cert) is not a file", cert.display()));
            }
        }
        Ok(PackKeys { common, title, random_title_key, cert: cert.map(Path::to_owned) })
    }

    #[cfg(test)]
    fn new(common: [u8; 16], title: [u8; 16]) -> PackKeys {
        PackKeys { common, title, random_title_key: false, cert: None }
    }
}

/// a 16-byte key in a file: 16 raw bytes, or 32 hex digits with any
/// whitespace around or between them. what the file holds is never shown.
pub fn read_key_file(path: &Path) -> Result<[u8; 16], String> {
    let bytes = std::fs::read(path).map_err(|e| format!("could not read the common key file {}, {e}", path.display()))?;
    if let Ok(raw) = <[u8; 16]>::try_from(bytes.as_slice()) {
        return Ok(raw);
    }
    std::str::from_utf8(&bytes).ok().and_then(parse_hex_key).ok_or_else(|| {
        format!("{} does not hold a 16-byte key: it should be 16 raw bytes or 32 hex digits", path.display())
    })
}

/// 32 hex digits, whitespace allowed anywhere, into 16 bytes.
pub fn parse_hex_key(text: &str) -> Option<[u8; 16]> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if digits.len() != 32 {
        return None;
    }
    let mut key = [0u8; 16];
    for (i, pair) in digits.chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        key[i] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(key)
}

fn random_bytes(buf: &mut [u8]) -> Result<(), String> {
    getrandom::getrandom(buf).map_err(|e| format!("could not get random bytes from the system, {e}"))
}

fn random_key() -> Result<[u8; 16], String> {
    let mut key = [0u8; 16];
    random_bytes(&mut key)?;
    Ok(key)
}

/// what code/app.xml says, as NUSPacker reads it (XMLParser.getAppXMLInfo):
/// hex numbers, 0 for what is missing.
struct AppInfo {
    title_id: u64,
    title_version: u16,
    os_version: u64,
    app_type: u32,
    group_id: u16,
}

impl AppInfo {
    fn read(path: &Path) -> Result<AppInfo, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("could not read {}, {e}", path.display()))?;
        let hex = |tag: &str| xml_text(&text, tag).and_then(|value| u64::from_str_radix(value.trim(), 16).ok()).unwrap_or(0);
        let info = AppInfo {
            title_id: hex("title_id"),
            title_version: hex("title_version") as u16,
            os_version: hex("os_version"),
            app_type: hex("app_type") as u32,
            group_id: hex("group_id") as u16,
        };
        if info.title_id == 0 {
            return Err(format!("{} has no title_id", path.display()));
        }
        Ok(info)
    }
}

/// the text of the first <tag ...>text</tag>.
fn xml_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(at) = xml[from..].find(&format!("<{tag}")) {
        let start = from + at + tag.len() + 1;
        let rest = &xml[start..];
        // <tag> or <tag attributes>, not <tag_longer>
        if rest.starts_with('>') || rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            let open_end = start + rest.find('>')?;
            if xml[..open_end].ends_with('/') {
                return Some("");
            }
            let body = &xml[open_end + 1..];
            return Some(&body[..body.find(&format!("</{tag}"))?]);
        }
        from = start;
    }
    None
}

/// a file or folder of the title, as an FST entry.
struct Node {
    name: String,
    dir: bool,
    size: u64,
    source: PathBuf,
    parent: usize,
    children: Vec<usize>,
    content: Option<usize>,
    // the FST layout
    index: u32,
    name_offset: u32,
    offset: u64,
}

/// a content (a .app) and how its files are kept.
struct Content {
    hashed: bool,
    group: u16,
    parent_title: u64,
    flags: u16,
    /// its files, in FST order
    files: Vec<usize>,
    /// bytes of the plain content: its files, each 0x20-aligned
    data_len: u64,
    /// the .app's size and the TMD's hash, once packed
    size: u64,
    hash: [u8; 20],
}

impl Content {
    fn new(hashed: bool, group: u16, parent_title: u64, flags: u16) -> Content {
        Content { hashed, group, parent_title, flags, files: Vec::new(), data_len: 0, size: 0, hash: [0; 20] }
    }

    fn kind(&self) -> u16 {
        TYPE_CONTENT | TYPE_ENCRYPTED | if self.hashed { TYPE_HASHED } else { 0 }
    }

    /// the .app's size, from the plain size: NUSPacker's loops write one
    /// block more than whole blocks for hashed contents, and at least one
    /// padding block for unhashed ones
    fn packed_size(&self) -> u64 {
        if self.hashed {
            (self.data_len / DATA_BLOCK as u64 + 1) * HASHED_BLOCK as u64
        } else {
            padded_len(self.data_len)
        }
    }
}

fn align(value: u64, alignment: u64) -> u64 {
    value.div_ceil(alignment) * alignment
}

/// an unhashed content's plain size with its padding
fn padded_len(len: u64) -> u64 {
    align(len, CONTENT_PADDING).max(CONTENT_PADDING)
}

/// a NUSPacker content rule's pattern, as the Java regex matches a path
/// such as /code/app.xml. '.' in a literal is any character, as in the
/// regex.
#[derive(Clone, Copy)]
enum Pattern {
    Literal(&'static str),
    /// /meta/.*[^.xml)]+
    MetaRest,
    /// /meta/.*.jpg
    MetaJpg,
    /// /code/.*(.rpx|.rpl)
    CodeRpxRpl,
    /// /content/.*
    ContentAll,
}

impl Pattern {
    fn matches(self, path: &str) -> bool {
        // what the regexes' `.*` and `.` take after the folder: at least
        // one character before a three-letter ending
        let ends = |prefix: &str, endings: &[&str]| {
            path.strip_prefix(prefix).is_some_and(|rest| rest.chars().count() >= 4 && endings.iter().any(|e| rest.ends_with(e)))
        };
        match self {
            Pattern::Literal(pattern) => {
                path.chars().count() == pattern.chars().count() && path.chars().zip(pattern.chars()).all(|(c, p)| p == '.' || c == p)
            }
            Pattern::MetaRest => path.strip_prefix("/meta/").and_then(|rest| rest.chars().last()).is_some_and(|c| !".xml)".contains(c)),
            Pattern::MetaJpg => ends("/meta/", &["jpg"]),
            Pattern::CodeRpxRpl => ends("/code/", &["rpx", "rpl"]),
            Pattern::ContentAll => path.starts_with("/content/"),
        }
    }
}

struct Rule {
    pattern: Pattern,
    hashed: bool,
    group: u16,
    parent_title: u64,
    flags: u16,
    /// every file it matches gets a content of its own
    per_match: bool,
}

/// NUSPacker's ContentRules.getCommonRules, in its order.
fn common_rules(content_group: u16, parent_title: u64) -> Vec<Rule> {
    let code = |pattern| Rule { pattern, hashed: false, group: GROUP_CODE, parent_title: 0, flags: FLAGS_CODE, per_match: false };
    let meta = |pattern| Rule { pattern, hashed: true, group: GROUP_META, parent_title: 0, flags: FLAGS_META, per_match: false };
    vec![
        code(Pattern::Literal("/code/app.xml")),
        code(Pattern::Literal("/code/cos.xml")),
        meta(Pattern::Literal("/meta/meta.xml")),
        meta(Pattern::MetaRest),
        meta(Pattern::Literal("/meta/bootMovie.h264")),
        meta(Pattern::Literal("/meta/bootLogoTex.tga")),
        meta(Pattern::Literal("/meta/Manual.bfma")),
        meta(Pattern::MetaJpg),
        Rule { per_match: true, ..code(Pattern::CodeRpxRpl) },
        // hashed, though in code/
        Rule { hashed: true, ..code(Pattern::Literal("/code/preload.txt")) },
        code(Pattern::Literal("/code/fw.img")),
        code(Pattern::Literal("/code/fw.tmd")),
        code(Pattern::Literal("/code/htk.bin")),
        code(Pattern::Literal("/code/rvlt.tik")),
        code(Pattern::Literal("/code/rvlt.tmd")),
        Rule { pattern: Pattern::ContentAll, hashed: true, group: content_group, parent_title, flags: FLAGS_CONTENT, per_match: false },
    ]
}

/// the title's files and folders, and the contents they go in.
struct Title {
    nodes: Vec<Node>,
    contents: Vec<Content>,
    /// the FST's name table
    strings: Vec<u8>,
}

impl Title {
    fn read(layout: &Path) -> Result<Title, String> {
        let root = Node {
            name: String::new(),
            dir: true,
            size: 0,
            source: layout.to_owned(),
            parent: 0,
            children: Vec::new(),
            // content 0, the FST
            content: Some(0),
            index: 0,
            name_offset: 0,
            offset: 0,
        };
        let mut title = Title { nodes: vec![root], contents: vec![Content::new(false, 0, 0, 0)], strings: Vec::new() };
        title.read_dir(0)?;
        Ok(title)
    }

    /// a folder's files, then its folders, each by name (NUSPacker takes
    /// them in the order the system lists them; installing doesn't care)
    fn read_dir(&mut self, parent: usize) -> Result<(), String> {
        let dir = self.nodes[parent].source.clone();
        let read = std::fs::read_dir(&dir).map_err(|e| format!("could not read {}, {e}", dir.display()))?;
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        for entry in read {
            let entry = entry.map_err(|e| format!("could not read {}, {e}", dir.display()))?;
            let path = entry.path();
            let name = entry.file_name().into_string().map_err(|_| format!("{} has a name that is not UTF-8", path.display()))?;
            if name.contains(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}']) {
                return Err(format!("{} has a line break in its name", path.display()));
            }
            let metadata = std::fs::metadata(&path).map_err(|e| format!("could not read {}, {e}", path.display()))?;
            if metadata.is_dir() {
                dirs.push((name, path));
            } else {
                if metadata.len() > u32::MAX as u64 {
                    return Err(format!("{} is 4 GiB or more, more than a Wii U title's file can be", path.display()));
                }
                files.push((name, path, metadata.len()));
            }
        }
        files.sort();
        dirs.sort();
        for (name, source, size) in files {
            self.add(parent, name, false, size, source);
        }
        for (name, source) in dirs {
            let id = self.add(parent, name, true, 0, source);
            self.read_dir(id)?;
        }
        Ok(())
    }

    fn add(&mut self, parent: usize, name: String, dir: bool, size: u64, source: PathBuf) -> usize {
        let id = self.nodes.len();
        self.nodes.push(Node { name, dir, size, source, parent, children: Vec::new(), content: None, index: 0, name_offset: 0, offset: 0 });
        self.nodes[parent].children.push(id);
        id
    }

    /// the path in the title, code/app.xml say
    fn path(&self, mut id: usize) -> String {
        let mut parts = Vec::new();
        while id != 0 {
            parts.push(self.nodes[id].name.as_str());
            id = self.nodes[id].parent;
        }
        parts.reverse();
        parts.join("/")
    }

    fn new_content(&mut self, rule: &Rule) -> usize {
        self.contents.push(Content::new(rule.hashed, rule.group, rule.parent_title, rule.flags));
        self.contents.len() - 1
    }

    /// ContentRulesService.applyRules
    fn apply_rules(&mut self, rules: &[Rule]) -> Result<(), String> {
        for rule in rules {
            if rule.per_match {
                self.apply_per_match(0, "", rule);
            } else {
                let first = self.new_content(rule);
                let mut state = RuleState { first, current: first, size: 0 };
                if !self.apply_shared(0, "", rule, &mut state) {
                    // nothing matched, so nothing went in a split-off content
                    self.contents.pop();
                }
            }
        }
        for id in 1..self.nodes.len() {
            if self.nodes[id].content.is_none() {
                return Err(if self.nodes[id].dir {
                    format!("the folder {} is empty, which a WUP can't hold; put a file in it", self.path(id))
                } else {
                    format!("{} matches none of the rules that put files into contents", self.path(id))
                });
            }
        }
        Ok(())
    }

    /// setContentRecursiveRule: the rule's files share a content (or more
    /// past MAX_CONTENT_LENGTH); a folder takes the rule's first content
    fn apply_shared(&mut self, id: usize, path: &str, rule: &Rule, state: &mut RuleState) -> bool {
        let path = format!("{path}{}/", self.nodes[id].name);
        if self.nodes[id].children.is_empty() {
            // an empty folder, matched as path/
            if rule.pattern.matches(&path) {
                self.nodes[id].content = Some(state.current);
                return true;
            }
            return false;
        }
        let mut matched = false;
        for child in self.nodes[id].children.clone() {
            if self.nodes[child].dir {
                if self.apply_shared(child, &path, rule, state) {
                    self.nodes[id].content = Some(state.first);
                    matched = true;
                }
            } else if rule.pattern.matches(&format!("{path}{}", self.nodes[child].name)) {
                let size = self.nodes[child].size;
                if state.size > 0 && state.size + size > MAX_CONTENT_LENGTH {
                    state.current = self.new_content(rule);
                    state.size = 0;
                }
                state.size += size;
                self.nodes[child].content = Some(state.current);
                matched = true;
            }
        }
        if matched {
            self.nodes[id].content = Some(state.first);
        }
        matched
    }

    /// setNewContentRecursiveRule: a content for each file the rule
    /// matches; a folder takes the last one made within it
    fn apply_per_match(&mut self, id: usize, path: &str, rule: &Rule) -> Option<usize> {
        let path = format!("{path}{}/", self.nodes[id].name);
        let mut result = None;
        if self.nodes[id].children.is_empty() && rule.pattern.matches(&path) {
            result = Some(self.new_content(rule));
        }
        for child in self.nodes[id].children.clone() {
            if self.nodes[child].dir {
                if let Some(content) = self.apply_per_match(child, &path, rule) {
                    result = Some(content);
                }
            } else if rule.pattern.matches(&format!("{path}{}", self.nodes[child].name)) {
                let content = self.new_content(rule);
                self.nodes[child].content = Some(content);
                result = Some(content);
            }
        }
        if let Some(content) = result {
            self.nodes[id].content = Some(content);
        }
        result
    }

    /// FST.update: entry numbers, names and where each file sits in its
    /// content, all in FST order (a folder, its files, then its folders)
    fn lay_out(&mut self) -> Result<(), String> {
        let mut next_index = 0u32;
        self.lay_out_node(0, &mut next_index)?;
        if self.strings.len() > 0xFF_FFFF + 1 {
            return Err("the title's file names take more room than an FST has".into());
        }
        Ok(())
    }

    fn lay_out_node(&mut self, id: usize, next_index: &mut u32) -> Result<(), String> {
        let node = &mut self.nodes[id];
        node.name_offset = self.strings.len() as u32;
        self.strings.extend_from_slice(node.name.as_bytes());
        self.strings.push(0);
        node.index = *next_index;
        *next_index += 1;
        if !node.dir {
            let content = &mut self.contents[node.content.expect("every file has a content")];
            node.offset = content.data_len;
            content.data_len += align(node.size, FILE_ALIGNMENT);
            content.files.push(id);
        }
        for child in self.nodes[id].children.clone() {
            self.lay_out_node(child, next_index)?;
        }
        Ok(())
    }

    /// the entry number after a folder and all it holds
    fn next_index(&self, id: usize) -> u32 {
        fn count(nodes: &[Node], id: usize) -> u32 {
            1 + nodes[id].children.iter().map(|&child| count(nodes, child)).sum::<u32>()
        }
        self.nodes[id].index + count(&self.nodes, id)
    }

    /// FST.getAsData, with the FST's own content still sized 0, as when
    /// NUSPacker encrypts and hashes it
    fn fst(&self) -> Vec<u8> {
        let mut fst = Vec::new();
        fst.extend_from_slice(b"FST\0");
        fst.extend_from_slice(&0x20u32.to_be_bytes());
        fst.extend_from_slice(&(self.contents.len() as u32).to_be_bytes());
        fst.extend_from_slice(&[0; 0x14]);
        // Content.getFSTContentHeaderAsData
        let mut offset = 0u64;
        for (id, content) in self.contents.iter().enumerate() {
            let mut header = [0u8; 0x20];
            let blocks = if id == 0 { 0 } else { content.size / CONTENT_PADDING };
            let mut written = blocks;
            let kind = if id == 0 {
                0
            } else if content.hashed {
                written = blocks.saturating_sub((blocks / 64 + 1) * 2);
                2
            } else {
                1
            };
            header[0..4].copy_from_slice(&(offset as u32).to_be_bytes());
            header[4..8].copy_from_slice(&(written as u32).to_be_bytes());
            header[8..16].copy_from_slice(&content.parent_title.to_be_bytes());
            header[0x10..0x14].copy_from_slice(&(content.group as u32).to_be_bytes());
            header[0x14] = kind;
            fst.extend_from_slice(&header);
            offset += if id == 0 { blocks + 2 } else { blocks };
        }
        let total = self.nodes.len() as u32;
        for (id, node) in self.nodes.iter().enumerate() {
            let mut entry = [0u8; 0x10];
            if id == 0 {
                entry[0] = 1;
                entry[8..12].copy_from_slice(&total.to_be_bytes());
            } else {
                entry[0] = if node.dir { 1 } else { 0 } | if node.name.ends_with("nfs") { 2 } else { 0 };
                entry[1..4].copy_from_slice(&node.name_offset.to_be_bytes()[1..]);
                if node.dir {
                    entry[4..8].copy_from_slice(&self.nodes[node.parent].index.to_be_bytes());
                    entry[8..12].copy_from_slice(&self.next_index(id).to_be_bytes());
                } else {
                    entry[4..8].copy_from_slice(&((node.offset >> 5) as u32).to_be_bytes());
                    entry[8..12].copy_from_slice(&(node.size as u32).to_be_bytes());
                }
                let content = node.content.expect("every entry has a content");
                entry[12..14].copy_from_slice(&self.contents[content].flags.to_be_bytes());
                entry[14..16].copy_from_slice(&(content as u16).to_be_bytes());
            }
            fst.extend_from_slice(&entry);
        }
        fst.extend_from_slice(&self.strings);
        fst.resize(align(fst.len() as u64, CONTENT_PADDING) as usize, 0);
        fst
    }

    /// TMD.getAsData
    fn tmd(&self, app: &AppInfo) -> Vec<u8> {
        let mut records = Vec::new();
        for (id, content) in self.contents.iter().enumerate() {
            let mut record = [0u8; 0x30];
            record[0..4].copy_from_slice(&(id as u32).to_be_bytes());
            record[4..6].copy_from_slice(&(id as u16).to_be_bytes());
            record[6..8].copy_from_slice(&content.kind().to_be_bytes());
            record[8..16].copy_from_slice(&content.size.to_be_bytes());
            record[16..36].copy_from_slice(&content.hash);
            records.extend_from_slice(&record);
        }
        // 64 content info records; the first covers every content
        let mut infos = vec![0u8; 64 * 0x24];
        infos[2..4].copy_from_slice(&(self.contents.len() as u16).to_be_bytes());
        infos[4..0x24].copy_from_slice(&Sha256::digest(&records));

        let mut tmd = vec![0u8; 0x204];
        tmd[0..4].copy_from_slice(&0x0001_0004u32.to_be_bytes()); // RSA-2048, SHA-256; signature left zero
        put_name(&mut tmd[0x140..0x180], "Root-CA00000003-CP0000000b");
        tmd[0x180] = 1; // version
        tmd[0x184..0x18C].copy_from_slice(&app.os_version.to_be_bytes());
        tmd[0x18C..0x194].copy_from_slice(&app.title_id.to_be_bytes());
        tmd[0x194..0x198].copy_from_slice(&0x0000_0100u32.to_be_bytes()); // title type
        tmd[0x198..0x19A].copy_from_slice(&app.group_id.to_be_bytes());
        tmd[0x19A..0x19E].copy_from_slice(&app.app_type.to_be_bytes());
        tmd[0x1DC..0x1DE].copy_from_slice(&app.title_version.to_be_bytes());
        tmd[0x1DE..0x1E0].copy_from_slice(&(self.contents.len() as u16).to_be_bytes());
        // boot index 0
        tmd[0x1E4..0x204].copy_from_slice(&Sha256::digest(&infos));
        tmd.extend_from_slice(&infos);
        tmd.extend_from_slice(&records);
        tmd
    }
}

struct RuleState {
    first: usize,
    current: usize,
    size: u64,
}

fn put_name(field: &mut [u8], name: &str) {
    field[..name.len()].copy_from_slice(name.as_bytes());
}

/// Ticket.getAsData: the title key encrypted with the common key, IV the
/// title id; the signature and part of the ticket id are random, as
/// NUSPacker makes them.
fn ticket(title_id: u64, keys: &PackKeys) -> Result<Vec<u8>, String> {
    let mut ticket = vec![0u8; 0x350];
    ticket[0..4].copy_from_slice(&0x0001_0004u32.to_be_bytes());
    random_bytes(&mut ticket[4..0x104])?;
    put_name(&mut ticket[0x140..0x180], "Root-CA00000003-XS0000000c");
    ticket[0x1BC] = 1; // version 1, CA CRL 0, signer CRL 0
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(&title_id.to_be_bytes());
    let mut key = keys.title;
    cbc_encrypt(&Aes128::new(GenericArray::from_slice(&keys.common)), &iv, &mut key);
    ticket[0x1BF..0x1CF].copy_from_slice(&key);
    // ticket id 0005 and six random bytes
    ticket[0x1D1] = 0x05;
    random_bytes(&mut ticket[0x1D2..0x1D8])?;
    ticket[0x1DC..0x1E4].copy_from_slice(&title_id.to_be_bytes());
    ticket[0x1E4..0x1F4].copy_from_slice(&[0, 0, 0, 0x11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x05]);
    // the version 1 header and its content index section
    const V1_HEADER: [u8; 0x30] = [
        0x00, 0x01, 0x00, 0x14, 0x00, 0x00, 0x00, 0xAC, 0x00, 0x00, 0x00, 0x14, 0x00, 0x01, 0x00, 0x14, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x28, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x84, //
        0x00, 0x00, 0x00, 0x84, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x01, //
    ];
    ticket[0x2A4..0x2D4].copy_from_slice(&V1_HEADER);
    Ok(ticket)
}

/// Cert.getCertAsData: the certificates a WUP's TMD and ticket name,
/// CA00000003 (signed by Root with RSA-4096) then CP0000000b and XS0000000c
/// (signed by Root-CA00000003 with RSA-2048), each with an RSA-2048 key,
/// with the right sizes and names and the signatures and keys left zero. a
/// console with signature patches doesn't check them.
fn dummy_cert() -> Vec<u8> {
    let mut cert = vec![0u8; 0xA00];
    // (start, signature type, where the issuer goes, issuer, name)
    for (start, signature, issuer_at, issuer, name) in [
        (0x000, 0x0001_0003u32, 0x240, "Root", "CA00000003"),
        (0x400, 0x0001_0004u32, 0x140, "Root-CA00000003", "CP0000000b"),
        (0x700, 0x0001_0004u32, 0x140, "Root-CA00000003", "XS0000000c"),
    ] {
        cert[start..start + 4].copy_from_slice(&signature.to_be_bytes());
        let issuer_at = start + issuer_at;
        put_name(&mut cert[issuer_at..issuer_at + 0x40], issuer);
        // key type 1, RSA-2048, then the certificate's name
        cert[issuer_at + 0x40..issuer_at + 0x44].copy_from_slice(&1u32.to_be_bytes());
        put_name(&mut cert[issuer_at + 0x44..issuer_at + 0x84], name);
    }
    cert
}

/// AES-128-CBC over whole blocks in place; gives the last ciphertext block,
/// the IV to go on with.
fn cbc_encrypt(aes: &Aes128, iv: &[u8; 16], data: &mut [u8]) -> [u8; 16] {
    let mut previous = *iv;
    for block in data.chunks_exact_mut(16) {
        for (byte, chain) in block.iter_mut().zip(previous) {
            *byte ^= chain;
        }
        aes.encrypt_block(GenericArray::from_mut_slice(block));
        previous.copy_from_slice(block);
    }
    previous
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let digest = Sha1::digest(data);
    let mut hash = [0u8; 20];
    hash.copy_from_slice(&digest);
    hash
}

fn content_iv(id: usize) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[..2].copy_from_slice(&(id as u16).to_be_bytes());
    iv
}

/// reads a content's plain bytes: its files one after the other, each
/// padded with zeros to 0x20, and zeros after the last.
struct ContentReader<'a> {
    nodes: &'a [Node],
    files: &'a [usize],
    next: usize,
    file: Option<(File, u64)>,
    padding: u64,
}

impl<'a> ContentReader<'a> {
    fn new(nodes: &'a [Node], files: &'a [usize]) -> ContentReader<'a> {
        ContentReader { nodes, files, next: 0, file: None, padding: 0 }
    }

    fn fill(&mut self, buf: &mut [u8]) -> Result<(), String> {
        let mut at = 0;
        while at < buf.len() {
            if let Some((file, left)) = &mut self.file {
                if *left == 0 {
                    self.file = None;
                    continue;
                }
                let want = (*left).min((buf.len() - at) as u64) as usize;
                let node = &self.nodes[self.files[self.next - 1]];
                let read = file.read(&mut buf[at..at + want]).map_err(|e| format!("could not read {}, {e}", node.source.display()))?;
                if read == 0 {
                    return Err(format!("{} got shorter while it was packed", node.source.display()));
                }
                *left -= read as u64;
                at += read;
            } else if self.padding > 0 {
                let zeros = self.padding.min((buf.len() - at) as u64) as usize;
                buf[at..at + zeros].fill(0);
                self.padding -= zeros as u64;
                at += zeros;
            } else if self.next < self.files.len() {
                let node = &self.nodes[self.files[self.next]];
                self.next += 1;
                let file = File::open(&node.source).map_err(|e| format!("could not open {}, {e}", node.source.display()))?;
                self.file = Some((file, node.size));
                self.padding = align(node.size, FILE_ALIGNMENT) - node.size;
            } else {
                buf[at..].fill(0);
                at = buf.len();
            }
        }
        Ok(())
    }
}

fn threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get()).min(16)
}

/// blocks read, hashed and encrypted at a time: 16 MiB of data
const BATCH_BLOCKS: usize = 256;

/// SHA-1s of 16 hashes at a time (zeros for those past the end), one level
/// up the tree: count of them, as ContentHashes.calculateOtherHashes makes
/// them
fn hash_level(below: &[[u8; 20]], count: usize) -> Vec<[u8; 20]> {
    (0..count)
        .map(|group| {
            let mut hashes = [0u8; 16 * 20];
            for i in 0..16 {
                if let Some(hash) = below.get(group * 16 + i) {
                    hashes[i * 20..i * 20 + 20].copy_from_slice(hash);
                }
            }
            sha1(&hashes)
        })
        .collect()
}

/// the 16 hashes of a level that the block's group is in, zeros past the end
fn hash_group(out: &mut [u8], hashes: &[[u8; 20]], first: usize) {
    for i in 0..16 {
        if let Some(hash) = hashes.get(first + i) {
            out[i * 20..i * 20 + 20].copy_from_slice(hash);
        }
    }
}

/// a progress line now and then for big contents
struct Progress<'a> {
    log: &'a mut dyn FnMut(&str),
    what: String,
    total: u64,
    shown: u64,
}

impl Progress<'_> {
    fn update(&mut self, done: u64) {
        const BIG: u64 = 64 << 20;
        if self.total < BIG {
            return;
        }
        let percent = done * 100 / self.total.max(1);
        if percent >= self.shown + 25 && percent < 100 {
            self.shown = percent / 25 * 25;
            (self.log)(&format!("  {} {}%", self.what, self.shown));
        }
    }
}

impl Title {
    /// writes 0000000X.app (and its .h3 if hashed), and keeps its size and
    /// TMD hash
    fn pack_content(&mut self, id: usize, out: &Path, key: &Aes128, log: &mut dyn FnMut(&str)) -> Result<(), String> {
        let app = out.join(format!("{id:08X}.app"));
        let mut writer = File::create(&app).map_err(|e| format!("could not create {}, {e}", app.display()))?;
        let write_error = |e: std::io::Error| format!("could not write {}, {e}", app.display());
        let content = &self.contents[id];
        let mut reader = ContentReader::new(&self.nodes, &content.files);
        let (size, hash) = if content.hashed {
            let blocks = (content.data_len / DATA_BLOCK as u64 + 1) as usize;
            let threads = threads();
            let mut data = vec![0u8; BATCH_BLOCKS * DATA_BLOCK];
            let total = blocks as u64 * DATA_BLOCK as u64 * 2;
            let mut progress = Progress { log, what: format!("{id:08X}.app"), total, shown: 0 };

            // the data blocks' hashes first, as the hash blocks need them all
            let mut h0 = vec![[0u8; 20]; blocks];
            for first in (0..blocks).step_by(BATCH_BLOCKS) {
                let count = BATCH_BLOCKS.min(blocks - first);
                let data = &mut data[..count * DATA_BLOCK];
                reader.fill(data)?;
                let per_thread = count.div_ceil(threads);
                std::thread::scope(|scope| {
                    for (data, hashes) in data.chunks(per_thread * DATA_BLOCK).zip(h0[first..first + count].chunks_mut(per_thread)) {
                        scope.spawn(move || {
                            for (block, hash) in data.chunks(DATA_BLOCK).zip(hashes) {
                                *hash = sha1(block);
                            }
                        });
                    }
                });
                progress.update((first + count) as u64 * DATA_BLOCK as u64);
            }
            let h1 = hash_level(&h0, blocks / 16 + 1);
            let h2 = hash_level(&h1, blocks / 256 + 1);
            let h3 = hash_level(&h2, blocks / 4096 + 1);

            // then the blocks, each its hashes and its data
            let mut reader = ContentReader::new(&self.nodes, &content.files);
            let mut packed = vec![0u8; BATCH_BLOCKS * HASHED_BLOCK];
            let (h0, h1, h2) = (&h0, &h1, &h2);
            for first in (0..blocks).step_by(BATCH_BLOCKS) {
                let count = BATCH_BLOCKS.min(blocks - first);
                let data = &mut data[..count * DATA_BLOCK];
                reader.fill(data)?;
                let packed = &mut packed[..count * HASHED_BLOCK];
                let per_thread = count.div_ceil(threads);
                std::thread::scope(|scope| {
                    for (part, (data, packed)) in data.chunks(per_thread * DATA_BLOCK).zip(packed.chunks_mut(per_thread * HASHED_BLOCK)).enumerate() {
                        scope.spawn(move || {
                            for (i, (data, packed)) in data.chunks(DATA_BLOCK).zip(packed.chunks_mut(HASHED_BLOCK)).enumerate() {
                                let block = first + part * per_thread + i;
                                encrypt_hashed_block(key, id, block, data, packed, h0, h1, h2);
                            }
                        });
                    }
                });
                writer.write_all(packed).map_err(write_error)?;
                progress.update(total / 2 + (first + count) as u64 * DATA_BLOCK as u64);
            }
            let h3: Vec<u8> = h3.concat();
            let h3_path = out.join(format!("{id:08X}.h3"));
            std::fs::write(&h3_path, &h3).map_err(|e| format!("could not write {}, {e}", h3_path.display()))?;
            (blocks as u64 * HASHED_BLOCK as u64, sha1(&h3))
        } else {
            let size = padded_len(content.data_len);
            let mut progress = Progress { log, what: format!("{id:08X}.app"), total: size, shown: 0 };
            let mut buf = vec![0u8; 1 << 20];
            let mut hasher = Sha1::new();
            let mut iv = content_iv(id);
            let mut done = 0u64;
            while done < size {
                let chunk = &mut buf[..(size - done).min(1 << 20) as usize];
                reader.fill(chunk)?;
                hasher.update(&*chunk);
                iv = cbc_encrypt(key, &iv, chunk);
                writer.write_all(chunk).map_err(write_error)?;
                done += chunk.len() as u64;
                progress.update(done);
            }
            let mut hash = [0u8; 20];
            hash.copy_from_slice(&hasher.finalize());
            (size, hash)
        };
        writer.flush().map_err(write_error)?;
        let content = &mut self.contents[id];
        content.size = size;
        content.hash = hash;
        Ok(())
    }
}

/// Encryption.encryptChunkHashed: the block's H0, H1 and H2 groups,
/// encrypted with IV the content id (byte 1 of the hashes xored with it
/// first), then the data encrypted with IV the block's own H0.
#[allow(clippy::too_many_arguments)]
fn encrypt_hashed_block(key: &Aes128, id: usize, block: usize, data: &[u8], out: &mut [u8], h0: &[[u8; 20]], h1: &[[u8; 20]], h2: &[[u8; 20]]) {
    let (hashes, body) = out.split_at_mut(HASH_AREA);
    hashes.fill(0);
    hash_group(&mut hashes[0x000..0x140], h0, block / 16 * 16);
    hash_group(&mut hashes[0x140..0x280], h1, block / 256 * 16);
    hash_group(&mut hashes[0x280..0x3C0], h2, block / 4096 * 16);
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&h0[block][..16]);
    hashes[1] ^= id as u8;
    cbc_encrypt(key, &content_iv(id), hashes);
    body.copy_from_slice(data);
    cbc_encrypt(key, &iv, body);
}

/// what pack made.
pub struct Packed {
    pub title_id: u64,
    pub contents: usize,
    /// bytes of the WUP
    pub bytes: u64,
}

/// packs the unpacked title at layout (code/ content/ meta/) into an
/// installable WUP in out. log gets a line per content.
pub fn pack(layout: &Path, out: &Path, keys: &PackKeys, log: &mut dyn FnMut(&str)) -> Result<Packed, String> {
    for folder in ["code", "content", "meta"] {
        if !layout.join(folder).is_dir() {
            return Err(format!("{} has no {folder} folder; a title needs code/, content/ and meta/", layout.display()));
        }
    }
    let app = AppInfo::read(&layout.join("code").join("app.xml"))?;
    let parent_title = app.title_id & !0x0000_000F_0000_0000;
    let mut title = Title::read(layout)?;
    title.apply_rules(&common_rules(app.group_id, parent_title))?;
    title.lay_out()?;

    std::fs::create_dir_all(out).map_err(|e| format!("could not create {}, {e}", out.display()))?;
    let key = Aes128::new(GenericArray::from_slice(&keys.title));
    for id in 1..title.contents.len() {
        let content = &title.contents[id];
        let mut files: Vec<String> = content.files.iter().take(3).map(|&file| title.path(file)).collect();
        if content.files.len() > 3 {
            files.push(format!("and {} more", content.files.len() - 3));
        }
        log(&format!(
            "{id:08X}.app  {:<8} {:>8.1} MiB  {}",
            if content.hashed { "hashed" } else { "unhashed" },
            content.packed_size() as f64 / (1 << 20) as f64,
            files.join(", ")
        ));
        title.pack_content(id, out, &key, log)?;
        debug_assert_eq!(title.contents[id].size, title.contents[id].packed_size());
    }

    // the FST, which needs the other contents' sizes
    let mut fst = title.fst();
    title.contents[0].hash = sha1(&fst);
    title.contents[0].size = fst.len() as u64;
    log(&format!("00000000.app  FST      {:>8.1} MiB  {} files and folders", fst.len() as f64 / (1 << 20) as f64, title.nodes.len()));
    cbc_encrypt(&key, &content_iv(0), &mut fst);
    let write = |name: &str, data: &[u8]| {
        let path = out.join(name);
        std::fs::write(&path, data).map_err(|e| format!("could not write {}, {e}", path.display()))
    };
    write("00000000.app", &fst)?;

    write("title.tmd", &title.tmd(&app))?;
    write("title.tik", &ticket(app.title_id, keys)?)?;
    match &keys.cert {
        Some(cert) => {
            let data = std::fs::read(cert).map_err(|e| format!("could not read {}, {e}", cert.display()))?;
            write("title.cert", &data)?;
        }
        None => write("title.cert", &dummy_cert())?,
    }
    let bytes = title.contents.iter().map(|c| c.size).sum::<u64>()
        + title.contents.iter().filter(|c| c.hashed).map(|c| (c.size / HASHED_BLOCK as u64 / 4096 + 1) * 20).sum::<u64>();
    Ok(Packed { title_id: app.title_id, contents: title.contents.len(), bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockDecrypt;
    use std::collections::BTreeMap;

    /// a made-up key for the tests; not any console's
    const DUMMY_COMMON_KEY: [u8; 16] = *b"not-a-real-key!!";
    const TITLE_KEY: [u8; 16] = [0x5A; 16];

    fn cbc_decrypt(aes: &Aes128, iv: &[u8], data: &mut [u8]) {
        let mut previous = [0u8; 16];
        previous.copy_from_slice(&iv[..16]);
        for block in data.chunks_exact_mut(16) {
            let mut cipher = [0u8; 16];
            cipher.copy_from_slice(block);
            aes.decrypt_block(GenericArray::from_mut_slice(block));
            for (byte, chain) in block.iter_mut().zip(previous) {
                *byte ^= chain;
            }
            previous = cipher;
        }
    }

    fn be16(data: &[u8], at: usize) -> u16 {
        u16::from_be_bytes(data[at..at + 2].try_into().unwrap())
    }
    fn be32(data: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
    }
    fn be64(data: &[u8], at: usize) -> u64 {
        u64::from_be_bytes(data[at..at + 8].try_into().unwrap())
    }

    /// what the reader found in a WUP.
    struct Unpacked {
        title_id: u64,
        group_id: u16,
        /// (id, type, size)
        contents: Vec<(u32, u16, u64)>,
        /// path -> (content, FST flags, data)
        files: BTreeMap<String, (u16, u16, Vec<u8>)>,
        /// folder path -> content
        dirs: BTreeMap<String, u16>,
    }

    /// reads a WUP back the way an installer would, written from the format
    /// rather than from the packer: decrypts the title key, checks the TMD's
    /// hashes, decrypts every content and checks its hashes (H0 to H3 for
    /// hashed ones), and takes every file out through the FST.
    fn read_wup(dir: &Path, common_key: &[u8; 16]) -> Result<Unpacked, String> {
        let read = |name: &str| std::fs::read(dir.join(name)).map_err(|e| format!("{name}: {e}"));
        let ticket = read("title.tik")?;
        let tmd = read("title.tmd")?;
        let cert = read("title.cert")?;
        if ticket.len() != 0x350 || be32(&ticket, 0) != 0x0001_0004 || &ticket[0x140..0x15A] != b"Root-CA00000003-XS0000000c" {
            return Err("bad ticket".into());
        }
        if cert.len() != 0xA00 {
            return Err("bad cert".into());
        }
        let title_id = be64(&ticket, 0x1DC);
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&title_id.to_be_bytes());
        let mut title_key = [0u8; 16];
        title_key.copy_from_slice(&ticket[0x1BF..0x1CF]);
        cbc_decrypt(&Aes128::new(GenericArray::from_slice(common_key)), &iv, &mut title_key);
        let aes = Aes128::new(GenericArray::from_slice(&title_key));

        if be32(&tmd, 0) != 0x0001_0004 || &tmd[0x140..0x15A] != b"Root-CA00000003-CP0000000b" || be64(&tmd, 0x18C) != title_id {
            return Err("bad TMD header".into());
        }
        let count = be16(&tmd, 0x1DE) as usize;
        if tmd.len() != 0xB04 + count * 0x30 {
            return Err("bad TMD size".into());
        }
        if Sha256::digest(&tmd[0x204..0xB04]).as_slice() != &tmd[0x1E4..0x204] {
            return Err("TMD content info hash".into());
        }
        if be16(&tmd, 0x204) != 0 || be16(&tmd, 0x206) as usize != count || Sha256::digest(&tmd[0xB04..]).as_slice() != &tmd[0x208..0x228] {
            return Err("TMD content info 0".into());
        }
        if tmd[0x228..0xB04].iter().any(|&b| b != 0) {
            return Err("TMD content infos 1..63 aren't empty".into());
        }

        let mut contents = Vec::new();
        let mut plain: Vec<Vec<u8>> = Vec::new();
        for i in 0..count {
            let record = &tmd[0xB04 + i * 0x30..0xB04 + (i + 1) * 0x30];
            let (id, index, kind, size) = (be32(record, 0), be16(record, 4), be16(record, 6), be64(record, 8));
            if id as usize != i || index as usize != i {
                return Err(format!("content {i} has id {id:X} index {index:X}"));
            }
            let mut app = read(&format!("{id:08X}.app"))?;
            if app.len() as u64 != size {
                return Err(format!("{id:08X}.app is {} bytes, the TMD says {size}", app.len()));
            }
            contents.push((id, kind, size));
            let cid = id as u16;
            if kind & TYPE_HASHED != 0 {
                if kind != 0x2003 || app.len() % HASHED_BLOCK != 0 {
                    return Err(format!("{id:08X}: bad hashed content"));
                }
                let h3 = read(&format!("{id:08X}.h3"))?;
                if sha1(&h3).as_slice() != &record[16..36] {
                    return Err(format!("{id:08X}.h3 doesn't match the TMD"));
                }
                let blocks = app.len() / HASHED_BLOCK;
                if h3.len() % 20 != 0 || h3.len() / 20 < blocks.div_ceil(4096) {
                    return Err(format!("{id:08X}.h3 is too short"));
                }
                let mut data = Vec::with_capacity(blocks * DATA_BLOCK);
                for (block, chunk) in app.chunks_mut(HASHED_BLOCK).enumerate() {
                    let (hashes, body) = chunk.split_at_mut(HASH_AREA);
                    let mut iv = [0u8; 16];
                    iv[..2].copy_from_slice(&cid.to_be_bytes());
                    cbc_decrypt(&aes, &iv, hashes);
                    hashes[1] ^= cid as u8;
                    let h0 = &hashes[(block % 16) * 20..(block % 16) * 20 + 20];
                    cbc_decrypt(&aes, &h0[..16], body);
                    if sha1(body).as_slice() != h0 {
                        return Err(format!("{id:08X} block {block}: H0"));
                    }
                    let h1 = &hashes[0x140 + (block / 16 % 16) * 20..][..20];
                    if sha1(&hashes[..0x140]).as_slice() != h1 {
                        return Err(format!("{id:08X} block {block}: H1"));
                    }
                    let h2 = &hashes[0x280 + (block / 256 % 16) * 20..][..20];
                    if sha1(&hashes[0x140..0x280]).as_slice() != h2 {
                        return Err(format!("{id:08X} block {block}: H2"));
                    }
                    if sha1(&hashes[0x280..0x3C0]).as_slice() != &h3[(block / 4096) * 20..][..20] {
                        return Err(format!("{id:08X} block {block}: H3"));
                    }
                    data.extend_from_slice(body);
                }
                plain.push(data);
            } else {
                if kind != 0x2001 || app.len() as u64 % CONTENT_PADDING != 0 {
                    return Err(format!("{id:08X}: bad unhashed content"));
                }
                cbc_decrypt(&aes, &content_iv(i), &mut app);
                if sha1(&app).as_slice() != &record[16..36] {
                    return Err(format!("{id:08X}.app doesn't match its TMD hash"));
                }
                plain.push(app);
            }
        }

        let fst = &plain[0];
        if &fst[0..4] != b"FST\0" || be32(fst, 4) != 0x20 || be32(fst, 8) as usize != count {
            return Err("bad FST header".into());
        }
        for i in 0..count {
            let header = &fst[0x20 + i * 0x20..0x40 + i * 0x20];
            let expected = if i == 0 { 0 } else if contents[i].1 & TYPE_HASHED != 0 { 2 } else { 1 };
            if header[0x14] != expected {
                return Err(format!("FST content header {i}"));
            }
        }
        let entries = 0x20 + count * 0x20;
        let total = be32(fst, entries + 8) as usize;
        let names = entries + total * 0x10;
        let name = |entry: usize| {
            let at = names + (be32(fst, entry) & 0xFF_FFFF) as usize;
            let end = at + fst[at..].iter().position(|&b| b == 0).unwrap();
            String::from_utf8(fst[at..end].to_vec()).unwrap()
        };
        let mut files = BTreeMap::new();
        let mut dirs = BTreeMap::new();
        // (path, next) of the folders we are in
        let mut stack: Vec<(String, usize)> = vec![(String::new(), total)];
        for i in 1..total {
            while stack.last().unwrap().1 <= i {
                stack.pop();
            }
            let entry = entries + i * 0x10;
            let parent = &stack.last().unwrap().0;
            let path = if parent.is_empty() { name(entry) } else { format!("{parent}/{}", name(entry)) };
            let (flags, content) = (be16(fst, entry + 12), be16(fst, entry + 14));
            if fst[entry] & 1 != 0 {
                dirs.insert(path.clone(), content);
                stack.push((path, be32(fst, entry + 8) as usize));
            } else {
                let offset = (be32(fst, entry + 4) as usize) << 5;
                let size = be32(fst, entry + 8) as usize;
                let data = plain.get(content as usize).ok_or("file in a missing content")?;
                files.insert(path, (content, flags, data.get(offset..offset + size).ok_or("file past its content")?.to_vec()));
            }
        }
        Ok(Unpacked { title_id, group_id: be16(&tmd, 0x198), contents, files, dirs })
    }

    /// every file under dir, by path
    fn layout_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
                if entry.path().is_dir() {
                    walk(&entry.path(), &path, out);
                } else {
                    out.insert(path, std::fs::read(entry.path()).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, "", &mut out);
        out
    }

    fn compare(wup: &Unpacked, layout: &Path) {
        let expected = layout_files(layout);
        let got: BTreeMap<String, Vec<u8>> = wup.files.iter().map(|(path, (_, _, data))| (path.clone(), data.clone())).collect();
        assert_eq!(expected.keys().collect::<Vec<_>>(), got.keys().collect::<Vec<_>>());
        for (path, data) in &expected {
            assert!(got[path] == *data, "{path} differs");
        }
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("cia2wiiu-nus-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// bytes that don't repeat with the block size
    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    const APP_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<app type="complex" access="777">
  <version type="unsignedInt" length="4">16</version>
  <os_version type="hexBinary" length="8">000500101000400A</os_version>
  <title_id type="hexBinary" length="8">0005000010ABCD00</title_id>
  <title_version type="hexBinary" length="2">0010</title_version>
  <sdk_version type="unsignedInt" length="4">21204</sdk_version>
  <app_type type="hexBinary" length="4">80000000</app_type>
  <group_id type="hexBinary" length="4">0000ABCD</group_id>
</app>
"#;

    fn write_layout(root: &Path, content: &[(&str, Vec<u8>)]) {
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("code/app.xml", APP_XML.as_bytes().to_vec()),
            ("code/cos.xml", b"<app>cos</app>\n".to_vec()),
            ("code/game.rpx", pattern(300_001, 1)),
            ("meta/meta.xml", b"<menu>meta</menu>\n".to_vec()),
            ("meta/iconTex.tga", pattern(65_580, 2)),
            ("meta/bootTvTex.tga", pattern(70_000, 3)),
            ("meta/bootLogoTex.tga", pattern(28_000, 4)),
        ];
        for (path, data) in files.iter().chain(content) {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, data).unwrap();
        }
    }

    fn pack_and_read(layout: &Path, out: &Path) -> Unpacked {
        let keys = PackKeys::new(DUMMY_COMMON_KEY, TITLE_KEY);
        let packed = pack(layout, out, &keys, &mut |_| {}).unwrap();
        let wup = read_wup(out, &DUMMY_COMMON_KEY).unwrap();
        assert_eq!(packed.title_id, 0x0005_0000_10AB_CD00);
        assert_eq!(packed.contents, wup.contents.len());
        wup
    }

    #[test]
    fn packs_a_title_that_reads_back() {
        let scratch = Scratch::new("roundtrip");
        let layout = scratch.0.join("layout");
        // past 256 data blocks, so there are two H2 hashes, and a file in a
        // folder of content/
        let big = 0xFC00 * 300 + 12_345;
        write_layout(&layout, &[("content/game.cxi", pattern(big, 5)), ("content/more/notes.txt", b"hello".to_vec())]);
        let out = scratch.0.join("wup");
        let wup = pack_and_read(&layout, &out);
        compare(&wup, &layout);

        // the contents NUSPacker's rules make for this layout
        assert_eq!(wup.title_id, 0x0005_0000_10AB_CD00);
        assert_eq!(wup.group_id, 0xABCD);
        let kinds: Vec<u16> = wup.contents.iter().map(|c| c.1).collect();
        assert_eq!(kinds, [0x2001, 0x2001, 0x2001, 0x2003, 0x2003, 0x2003, 0x2001, 0x2003]);
        let content_of = |path: &str| (wup.files[path].0, wup.files[path].1);
        assert_eq!(content_of("code/app.xml"), (1, FLAGS_CODE));
        assert_eq!(content_of("code/cos.xml"), (2, FLAGS_CODE));
        assert_eq!(content_of("meta/meta.xml"), (3, FLAGS_META));
        assert_eq!(content_of("meta/iconTex.tga"), (4, FLAGS_META));
        assert_eq!(content_of("meta/bootTvTex.tga"), (4, FLAGS_META));
        assert_eq!(content_of("meta/bootLogoTex.tga"), (5, FLAGS_META));
        assert_eq!(content_of("code/game.rpx"), (6, FLAGS_CODE));
        assert_eq!(content_of("content/game.cxi"), (7, FLAGS_CONTENT));
        assert_eq!(content_of("content/more/notes.txt"), (7, FLAGS_CONTENT));
        assert_eq!(wup.dirs["code"], 6);
        assert_eq!(wup.dirs["meta"], 5);
        assert_eq!(wup.dirs["content"], 7);
        // the content's size: whole blocks and one more
        let blocks = (align(big as u64, 0x20) + 0x20) / 0xFC00 + 1;
        assert_eq!(wup.contents[7].2, blocks * 0x10000);
        assert_eq!(std::fs::read(out.join("00000007.h3")).unwrap().len(), 20);

        // the FST's content headers, as NUSPacker writes them
        let mut fst = std::fs::read(out.join("00000000.app")).unwrap();
        let key = Aes128::new(GenericArray::from_slice(&TITLE_KEY));
        cbc_decrypt(&key, &[0; 16], &mut fst);
        let header = |i: usize| &fst[0x20 + i * 0x20..0x40 + i * 0x20];
        assert_eq!(header(0)[..0x15], [0; 0x15]);
        assert_eq!(be32(header(1), 0), 2);
        assert_eq!(be32(header(1), 4), 1);
        let hashed_blocks = blocks * 2;
        assert_eq!(be32(header(7), 4) as u64, hashed_blocks - (hashed_blocks / 64 + 1) * 2);
        assert_eq!(be64(header(7), 8), 0x0005_0000_10AB_CD00);
        assert_eq!(be32(header(7), 0x10), 0xABCD);
        assert_eq!(be32(header(3), 0x10), 0x400);

        // the TMD carries app.xml's values
        let tmd = std::fs::read(out.join("title.tmd")).unwrap();
        assert_eq!(be64(&tmd, 0x184), 0x0005_0010_1000_400A);
        assert_eq!(be32(&tmd, 0x19A), 0x8000_0000);
        assert_eq!(be16(&tmd, 0x1DC), 0x0010);
        // the default cert
        assert_eq!(std::fs::read(out.join("title.cert")).unwrap(), dummy_cert());
    }

    #[test]
    fn hashed_content_of_whole_blocks_gets_one_more() {
        let scratch = Scratch::new("whole");
        let layout = scratch.0.join("layout");
        write_layout(&layout, &[("content/exact.bin", pattern(0xFC00 * 32, 6))]);
        let out = scratch.0.join("wup");
        let wup = pack_and_read(&layout, &out);
        compare(&wup, &layout);
        // 32 blocks of data, one of padding; 3 H1s, the last of nothing
        assert_eq!(wup.contents[7].2, 33 * 0x10000);
    }

    #[test]
    fn a_file_no_rule_takes_is_refused() {
        let scratch = Scratch::new("stray");
        let layout = scratch.0.join("layout");
        write_layout(&layout, &[("content/game.cxi", vec![1; 100]), ("stray.txt", vec![2; 10])]);
        let keys = PackKeys::new(DUMMY_COMMON_KEY, TITLE_KEY);
        let error = pack(&layout, &scratch.0.join("wup"), &keys, &mut |_| {}).err().unwrap();
        assert!(error.contains("stray.txt"), "{error}");
    }

    #[test]
    fn rule_patterns_match_like_the_regexes() {
        assert!(Pattern::Literal("/code/app.xml").matches("/code/app.xml"));
        assert!(Pattern::Literal("/code/app.xml").matches("/code/app_xml"));
        assert!(!Pattern::Literal("/code/app.xml").matches("/code/app.xmll"));
        assert!(Pattern::MetaRest.matches("/meta/iconTex.tga"));
        assert!(Pattern::MetaRest.matches("/meta/sub/a.bin"));
        assert!(!Pattern::MetaRest.matches("/meta/meta.xml"));
        assert!(!Pattern::MetaRest.matches("/meta/"));
        assert!(Pattern::MetaJpg.matches("/meta/a.jpg"));
        assert!(!Pattern::MetaJpg.matches("/meta/jpg"));
        assert!(Pattern::CodeRpxRpl.matches("/code/game.rpx"));
        assert!(Pattern::CodeRpxRpl.matches("/code/lib.rpl"));
        assert!(!Pattern::CodeRpxRpl.matches("/code/rpx"));
        assert!(Pattern::ContentAll.matches("/content/a/b"));
        assert!(!Pattern::ContentAll.matches("/meta/a"));
    }

    #[test]
    fn keys_read_from_raw_or_hex_files() {
        let scratch = Scratch::new("keys");
        let raw = scratch.0.join("raw.bin");
        std::fs::write(&raw, DUMMY_COMMON_KEY).unwrap();
        assert_eq!(read_key_file(&raw).unwrap(), DUMMY_COMMON_KEY);
        let hex: String = DUMMY_COMMON_KEY.iter().map(|b| format!("{b:02x}")).collect();
        let text = scratch.0.join("hex.txt");
        std::fs::write(&text, format!("  {} {}\r\n", &hex[..16], hex[16..].to_uppercase())).unwrap();
        assert_eq!(read_key_file(&text).unwrap(), DUMMY_COMMON_KEY);
        let bad = scratch.0.join("bad.txt");
        std::fs::write(&bad, "0123456789abcdef0123456789abcdeg\n").unwrap();
        let error = read_key_file(&bad).err().unwrap();
        assert!(!error.contains("0123"), "the error shows the file's contents: {error}");
        assert_eq!(parse_hex_key("00112233445566778899aabbccddeeff").unwrap()[15], 0xFF);
        assert!(parse_hex_key("0011").is_none());
    }

    #[test]
    fn xml_values_are_found() {
        assert_eq!(xml_text(APP_XML, "title_id"), Some("0005000010ABCD00"));
        assert_eq!(xml_text(APP_XML, "version"), Some("16"));
        assert_eq!(xml_text("<a><title_id_x>1</title_id_x><title_id>2</title_id></a>", "title_id"), Some("2"));
        assert_eq!(xml_text("<a/>", "b"), None);
    }

    /// checks a WUP the converter wrote against the layout it was packed
    /// from: NUS_VERIFY_WUP=<wup folder> NUS_VERIFY_LAYOUT=<code/content/meta
    /// folder> NUS_VERIFY_KEY=<the common key file it was packed with>
    /// cargo test --release -- --ignored verify_a_packed_wup
    #[test]
    #[ignore]
    fn verify_a_packed_wup() {
        let var = |name: &str| PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")));
        let key = read_key_file(&var("NUS_VERIFY_KEY")).unwrap();
        let wup = read_wup(&var("NUS_VERIFY_WUP"), &key).unwrap();
        compare(&wup, &var("NUS_VERIFY_LAYOUT"));
        for (id, kind, size) in &wup.contents {
            println!("{id:08X}.app type {kind:04X} {size} bytes");
        }
        for (path, (content, flags, data)) in &wup.files {
            println!("{path}: content {content}, flags {flags:04X}, {} bytes, identical", data.len());
        }
    }
}
