//! PlayStation Portable modules: the MIPS ELF files with the type 0xFFA0 (`.prx`) or the plain
//! executable type, a module info record, and library tables that name what the module imports
//! and exports.
//!
//! A module imports by library and by a 32-bit hash of the function name (a NID); the file holds
//! no function names. The tables are read here for what they say, never turned into names: an
//! import is shown as `<Library>_<NID>`. Every table can be corrupt, so every read is checked, every
//! loop is capped, and a bad table gives a warning instead of an error.

use crate::binary::{Binary, Export, Import, ImportSlot, MAX_IMPORTS};
use serde_json::{Value, json};

/// The ELF `e_machine` of MIPS.
pub const EM_MIPS: u16 = 8;
/// The ELF `e_type` of a relocatable PSP module.
pub const ET_PRX: u16 = 0xFFA0;

/// Size of the module info record.
const MODULE_INFO_LEN: usize = 52;
/// Most library entries read from either table.
const MAX_LIBRARIES: usize = 1024;
/// The most NIDs read from one module's import (or export) tables in all: every library may point
/// at the same table, so a cap per library alone lets a small file ask for gigabytes.
const MAX_NIDS: usize = MAX_IMPORTS;
/// The most NIDs read from the export tables in all: a module exports a handful (a system library
/// some hundreds), and each export function is looked up in the sections.
const MAX_EXPORT_NIDS: usize = 16_384;
/// Most characters kept of a library or module name.
const MAX_NAME: usize = 64;
/// Bytes in one function stub (a jump and its delay slot).
const STUB_LEN: u64 = 8;

/// The module info record (`.rodata.sceModuleInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    pub attributes: u16,
    pub version_minor: u8,
    pub version_major: u8,
    pub name: String,
    pub gp: u32,
    pub ent_top: u32,
    pub ent_end: u32,
    pub stub_top: u32,
    pub stub_end: u32,
}

impl ModuleInfo {
    /// Parse the 52-byte record; `None` when fewer bytes are given.
    pub fn parse(b: &[u8]) -> Option<ModuleInfo> {
        let b = b.get(..MODULE_INFO_LEN)?;
        let w = |o: usize| -> Option<u32> { Some(u32::from_le_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?)) };
        let name_bytes = b.get(4..32)?;
        let end = name_bytes.iter().position(|&c| c == 0).unwrap_or(name_bytes.len());
        Some(ModuleInfo {
            attributes: u16::from_le_bytes(b.get(0..2)?.try_into().ok()?),
            version_minor: *b.get(2)?,
            version_major: *b.get(3)?,
            name: clean(name_bytes.get(..end)?),
            gp: w(32)?,
            ent_top: w(36)?,
            ent_end: w(40)?,
            stub_top: w(44)?,
            stub_end: w(48)?,
        })
    }
}

/// Text from the file made safe to show: printable ASCII only, short.
fn clean(bytes: &[u8]) -> String {
    bytes.iter().take(MAX_NAME).map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '?' }).collect()
}

/// True when this ELF is a PSP module: MIPS, little-endian, and the PRX type or PSP section names.
pub fn is_psp_module(machine: u16, e_type: u16, b: &Binary) -> bool {
    machine == EM_MIPS
        && b.bits == 32
        && b.little_endian
        && (e_type == ET_PRX || b.sections.iter().any(|s| s.name == ".rodata.sceModuleInfo" || s.name.starts_with(".sceStub.text")))
}

fn u32_at(b: &Binary, data: &[u8], va: u64) -> Option<u32> {
    let s = b.read(data, va, 4)?;
    Some(u32::from_le_bytes(s.try_into().ok()?))
}

fn u16_at(b: &Binary, data: &[u8], va: u64) -> Option<u16> {
    let s = b.read(data, va, 2)?;
    Some(u16::from_le_bytes(s.try_into().ok()?))
}

fn str_at(b: &Binary, data: &[u8], va: u64) -> Option<String> {
    let s = b.read(data, va, MAX_NAME)?;
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    Some(clean(s.get(..end)?))
}

fn nid_text(nid: u32) -> String {
    format!("{nid:08X}")
}

/// Read the module info and the import and export tables into `b`.
///
/// `info_hint` is the first program header's `p_paddr`: a file offset in a PRX, an address in an
/// executable. A module without section headers has no other way to find its module info.
pub fn enrich(data: &[u8], b: &mut Binary, e_type: u16, info_hint: Option<u64>) {
    b.info.insert("platform".into(), "psp".into());
    if b.kind == "unknown" || b.kind == "relocatable" {
        b.kind = "shared-library";
    }
    let info = find_module_info(data, b, e_type, info_hint);
    let (ent, stub) = match &info {
        Some(i) => {
            b.info.insert("module_name".into(), i.name.clone());
            b.info.insert("module_version".into(), format!("{}.{}", i.version_major, i.version_minor));
            b.info.insert("module_attributes".into(), format!("{:#06x}", i.attributes));
            b.info.insert("module_gp".into(), format!("{:#x}", i.gp));
            (range(i.ent_top, i.ent_end), range(i.stub_top, i.stub_end))
        }
        None => {
            b.warnings.push("PSP module info not found; reading the library tables from their sections".into());
            (section_range(b, ".lib.ent"), section_range(b, ".lib.stub"))
        }
    };
    let imports = read_stubs(data, b, stub);
    let exports = read_entries(data, b, ent);
    let mut libs = Vec::new();
    for l in &imports {
        if !b.libraries.contains(&l.name) && b.libraries.len() < MAX_LIBRARIES {
            b.libraries.push(l.name.clone());
        }
        libs.push(json!({
            "library": l.name,
            "version": format!("{:#06x}", l.version),
            "flags": format!("{:#06x}", l.flags),
            "functions": l.functions.len(),
            "variables": l.variables,
        }));
        for (nid, stub_addr) in &l.functions {
            if b.imports.len() >= MAX_IMPORTS {
                b.warnings.push("PSP imports: stopped at the import limit".into());
                break;
            }
            let name = format!("{}_{}", l.name, nid_text(*nid));
            if let Some(a) = stub_addr {
                b.import_slots.push(ImportSlot { addr: *a, name: name.clone(), library: Some(l.name.clone()), kind: "stub" });
            }
            b.imports.push(Import { name, library: Some(l.name.clone()), ordinal: None, nid: Some(*nid), slot: *stub_addr });
        }
    }
    let mut exported = Vec::new();
    for l in &exports {
        let lib = l.name.clone();
        exported.push(json!({
            "library": lib.clone().unwrap_or_else(|| "(system)".into()),
            "version": format!("{:#06x}", l.version),
            "attributes": format!("{:#06x}", l.attributes),
            "functions": l.functions.len(),
            "variables": l.variables.len(),
        }));
        for (nid, addr, is_function) in l.functions.iter().map(|(n, a)| (n, a, true)).chain(l.variables.iter().map(|(n, a)| (n, a, false))) {
            if b.exports.len() >= MAX_IMPORTS {
                b.warnings.push("PSP exports: stopped at the export limit".into());
                break;
            }
            let name = format!("{}_{}", lib.as_deref().unwrap_or("export"), nid_text(*nid));
            if is_function && b.is_exec(*addr) {
                b.function_starts.push(*addr);
            }
            b.exports.push(Export { name, addr: *addr, library: lib.clone(), nid: Some(*nid) });
        }
    }
    let psp: Value = json!({
        "module_info": info.as_ref().map(|i| json!({
            "name": i.name,
            "version": format!("{}.{}", i.version_major, i.version_minor),
            "attributes": format!("{:#06x}", i.attributes),
            "gp": format!("{:#x}", i.gp),
            "ent_top": format!("{:#x}", i.ent_top),
            "ent_end": format!("{:#x}", i.ent_end),
            "stub_top": format!("{:#x}", i.stub_top),
            "stub_end": format!("{:#x}", i.stub_end),
        })),
        "import_libraries": libs,
        "export_libraries": exported,
        "names_known": false,
    });
    match b.details.as_object_mut() {
        Some(o) => {
            o.insert("psp_module".into(), psp);
        }
        None => b.details = json!({"psp_module": psp}),
    }
}

fn range(top: u32, end: u32) -> Option<(u64, u64)> {
    (top != 0 && end >= top).then_some((u64::from(top), u64::from(end)))
}

fn section_range(b: &Binary, name: &str) -> Option<(u64, u64)> {
    let s = b.sections.iter().find(|s| s.name == name && s.alloc)?;
    Some((s.addr, s.addr.checked_add(s.size)?))
}

fn find_module_info(data: &[u8], b: &mut Binary, e_type: u16, info_hint: Option<u64>) -> Option<ModuleInfo> {
    if let Some(s) = b.sections.iter().find(|s| s.name == ".rodata.sceModuleInfo" && s.alloc)
        && let Some(bytes) = b.read(data, s.addr, MODULE_INFO_LEN)
        && let Some(i) = ModuleInfo::parse(bytes)
    {
        return Some(i);
    }
    let hint = info_hint.filter(|&h| h != 0)?;
    let parsed = if e_type == ET_PRX {
        let off = usize::try_from(hint).ok()?;
        data.get(off..off.checked_add(MODULE_INFO_LEN)?).and_then(ModuleInfo::parse)
    } else {
        // In an executable the field is an address, with the kernel-space bits masked off.
        b.read(data, hint & 0x7fff_ffff, MODULE_INFO_LEN).and_then(ModuleInfo::parse)
    };
    if parsed.is_none() {
        b.warnings.push("PSP module info: the pointer in the first program header leads nowhere".into());
    }
    parsed
}

struct ImportLib {
    name: String,
    version: u16,
    flags: u16,
    /// (NID, stub address when it lies inside the file's image).
    functions: Vec<(u32, Option<u64>)>,
    /// How many variables it imports (their NIDs aren't read).
    variables: usize,
}

fn read_stubs(data: &[u8], b: &mut Binary, range: Option<(u64, u64)>) -> Vec<ImportLib> {
    let mut out = Vec::new();
    let Some((top, end)) = range else { return out };
    let mut pos = top;
    let mut nids_left = MAX_NIDS;
    while pos < end && out.len() < MAX_LIBRARIES {
        let (Some(name_ptr), Some(version), Some(flags), Some(size_vars), Some(func_count)) =
            (u32_at(b, data, pos), u16_at(b, data, pos + 4), u16_at(b, data, pos + 6), u16_at(b, data, pos + 8), u16_at(b, data, pos + 10))
        else {
            b.warnings.push(format!("PSP imports: the table is cut off at {pos:#x}"));
            break;
        };
        let words = u64::from(size_vars & 0xff);
        let var_count = usize::from(size_vars >> 8);
        let (Some(nid_ptr), Some(stub_ptr)) = (u32_at(b, data, pos + 12), u32_at(b, data, pos + 16)) else {
            b.warnings.push(format!("PSP imports: the entry at {pos:#x} is cut off"));
            break;
        };
        if words < 5 {
            b.warnings.push(format!("PSP imports: the entry at {pos:#x} says it is {words} words long (at least 5); stopped reading"));
            break;
        }
        let name = if name_ptr == 0 { None } else { str_at(b, data, u64::from(name_ptr)) };
        let name = match name.filter(|n| !n.is_empty()) {
            Some(n) => n,
            None => {
                b.warnings.push(format!("PSP imports: the entry at {pos:#x} has no readable library name"));
                format!("library_{pos:x}")
            }
        };
        // The NID table holds the functions' NIDs, one per stub. Imported variables are counted
        // only: where their NIDs are kept isn't read.
        let mut lib = ImportLib { name, version, flags, functions: Vec::new(), variables: var_count };
        let listed = usize::from(func_count);
        let wanted = listed.min(nids_left);
        nids_left -= wanted;
        if wanted < listed {
            b.warnings.push(format!("PSP imports: stopped at {MAX_NIDS} NIDs in all"));
        }
        // One read for the whole table (a lookup per NID costs a search of every section).
        let table = b.read(data, u64::from(nid_ptr), wanted.saturating_mul(4)).unwrap_or_default();
        let nids: Vec<u32> = table.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        if nids.len() < wanted {
            b.warnings.push(format!("PSP imports: {} lists {wanted} NIDs but only {} are in the file", lib.name, nids.len()));
        }
        // The stubs are 8 bytes each, back to back: mapped when the first and last are in one section.
        let stub_at = |i: usize| (i as u64).checked_mul(STUB_LEN).and_then(|o| u64::from(stub_ptr).checked_add(o));
        let last = nids.len().checked_sub(1).and_then(stub_at);
        let all_mapped = match (stub_at(0).and_then(|a| b.section_at(a)), last.and_then(|a| b.section_at(a))) {
            (Some(first), Some(end)) => std::ptr::eq(first, end),
            _ => false,
        };
        for (i, nid) in nids.iter().enumerate() {
            let addr = stub_at(i).filter(|a| all_mapped || b.is_mapped(*a));
            lib.functions.push((*nid, addr));
        }
        out.push(lib);
        match words.checked_mul(4).and_then(|n| pos.checked_add(n)) {
            Some(next) => pos = next,
            None => break,
        }
    }
    if pos < end && out.len() >= MAX_LIBRARIES {
        b.warnings.push("PSP imports: stopped at the library limit".into());
    }
    out
}

struct ExportLib {
    name: Option<String>,
    version: u16,
    attributes: u16,
    functions: Vec<(u32, u64)>,
    variables: Vec<(u32, u64)>,
}

fn read_entries(data: &[u8], b: &mut Binary, range: Option<(u64, u64)>) -> Vec<ExportLib> {
    let mut out = Vec::new();
    let Some((top, end)) = range else { return out };
    let mut pos = top;
    let mut nids_left = MAX_EXPORT_NIDS;
    while pos < end && out.len() < MAX_LIBRARIES {
        let (Some(name_ptr), Some(version), Some(attributes), Some(size_vars), Some(func_count), Some(table)) = (
            u32_at(b, data, pos),
            u16_at(b, data, pos + 4),
            u16_at(b, data, pos + 6),
            u16_at(b, data, pos + 8),
            u16_at(b, data, pos + 10),
            u32_at(b, data, pos + 12),
        ) else {
            b.warnings.push(format!("PSP exports: the table is cut off at {pos:#x}"));
            break;
        };
        let words = u64::from(size_vars & 0xff);
        let var_count = usize::from(size_vars >> 8);
        if words < 4 {
            b.warnings.push(format!("PSP exports: the entry at {pos:#x} says it is {words} words long (at least 4); stopped reading"));
            break;
        }
        let name = if name_ptr == 0 { None } else { str_at(b, data, u64::from(name_ptr)).filter(|n| !n.is_empty()) };
        let mut lib = ExportLib { name, version, attributes, functions: Vec::new(), variables: Vec::new() };
        let listed = usize::from(func_count).saturating_add(var_count);
        let total = listed.min(nids_left);
        nids_left -= total;
        if total < listed {
            b.warnings.push(format!("PSP exports: stopped at {MAX_EXPORT_NIDS} NIDs in all"));
        }
        // The NIDs come first, then one address per NID (the table's own count places them). One
        // read for the whole table: a lookup per word costs a search of every section.
        let words_at = |va: Option<u64>| {
            let bytes = va.and_then(|va| b.read(data, va, total.saturating_mul(4))).unwrap_or_default();
            bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect::<Vec<u32>>()
        };
        let nids = words_at(Some(u64::from(table)));
        let addrs = words_at((listed as u64).checked_mul(4).and_then(|o| u64::from(table).checked_add(o)));
        for i in 0..total {
            let (Some(&nid), Some(&addr)) = (nids.get(i), addrs.get(i)) else {
                b.warnings.push(format!("PSP exports: {} lists {total} entries but only {i} are in the file", lib.name.as_deref().unwrap_or("(system)")));
                break;
            };
            if i < usize::from(func_count) {
                lib.functions.push((nid, u64::from(addr)));
            } else {
                lib.variables.push((nid, u64::from(addr)));
            }
        }
        out.push(lib);
        match words.checked_mul(4).and_then(|n| pos.checked_add(n)) {
            Some(next) => pos = next,
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_info_needs_52_bytes() {
        assert!(ModuleInfo::parse(&[0; 51]).is_none());
        let mut raw = [0u8; 52];
        raw[3] = 2;
        raw[4..9].copy_from_slice(b"hello");
        let i = ModuleInfo::parse(&raw).expect("parses");
        assert_eq!((i.name.as_str(), i.version_major), ("hello", 2));
    }

    #[test]
    fn names_are_made_printable() {
        assert_eq!(clean(b"a\x01b\xff"), "a?b?");
    }
}

#[cfg(test)]
mod module_tests {
    use super::*;
    use crate::binary::Arch;
    use crate::parse_binary;
    use craft_anything_testkit::psp::*;

    fn parse(f: Fault) -> Binary {
        parse_binary(&psp_prx(f)).expect("parses")
    }

    #[test]
    fn a_prx_is_named_for_what_it_is() {
        let b = parse(Fault::None);
        assert_eq!(b.arch, Arch::Mips);
        assert_eq!(b.arch.name(), "mips");
        assert_eq!((b.bits, b.little_endian), (32, true));
        assert_eq!(b.info.get("platform").map(String::as_str), Some("psp"));
        assert_eq!(b.info.get("module_name").map(String::as_str), Some(MODULE_NAME));
        assert_eq!(b.info.get("module_version").map(String::as_str), Some("1.2"));
        assert_eq!(b.kind, "shared-library");
        assert!(b.warnings.is_empty(), "{:?}", b.warnings);
    }

    #[test]
    fn imports_are_read_by_library_and_hash() {
        let b = parse(Fault::None);
        assert_eq!(b.libraries, vec![IMPORT_LIBRARY.to_string()]);
        assert_eq!(b.imports.len(), 2);
        assert_eq!(b.imports[0].name, "ThreadManForUser_446D8DE6");
        assert_eq!(b.imports[0].nid, Some(FUNCTION_NIDS[0]));
        assert_eq!(b.imports[0].library.as_deref(), Some(IMPORT_LIBRARY));
        assert_eq!(b.imports[0].slot, Some(u64::from(STUBS)));
        assert_eq!(b.imports[1].slot, Some(u64::from(STUBS) + 8));
        assert_eq!(b.import_slots.len(), 2);
        assert!(b.import_slots.iter().all(|s| s.kind == "stub"));
        let json = serde_json::to_value(&b.imports[1]).expect("serialises");
        assert_eq!(json["nid"], "0x000000AB");
        let libs = &b.details["psp_module"]["import_libraries"];
        assert_eq!(libs[0]["functions"], 2);
        // Counted, not read: the NID after the functions' belongs to the next table, not to the variable.
        assert_eq!(libs[0]["variables"], 1);
        assert!(b.imports.iter().all(|i| i.nid != Some(0x1234_5678)));
    }

    #[test]
    fn exports_are_read_with_their_addresses() {
        let b = parse(Fault::None);
        assert_eq!(b.exports.len(), 2);
        assert_eq!(b.exports[0].name, "export_D632ACDB");
        assert_eq!(b.exports[0].addr, u64::from(TEXT));
        assert_eq!(b.exports[0].nid, Some(EXPORT_FUNCTION_NID));
        assert_eq!(b.exports[1].addr, u64::from(EXPORT_DATA));
        assert!(b.function_starts.contains(&u64::from(TEXT)));
        assert!(!b.function_starts.contains(&u64::from(EXPORT_DATA)));
    }

    #[test]
    fn module_info_is_found_through_the_program_header_without_sections() {
        // Rename the info section so only the program header's pointer finds it.
        let mut f = psp_prx(Fault::None);
        let at = f.windows(21).position(|w| w == b".rodata.sceModuleInfo").expect("name");
        f[at + 1] = b'R';
        let b = parse_binary(&f).expect("parses");
        assert_eq!(b.info.get("module_name").map(String::as_str), Some(MODULE_NAME));
        assert_eq!(b.imports.len(), 2);
    }

    #[test]
    fn corrupt_tables_warn_and_never_panic() {
        for fault in [
            Fault::BadNamePointer,
            Fault::BadNidPointer,
            Fault::HugeCounts,
            Fault::ZeroEntrySize,
            Fault::HugeStubRange,
            Fault::NoModuleInfo,
            Fault::BadStubPointer,
        ] {
            let b = parse(fault);
            assert!(b.imports.len() <= 200, "{fault:?}");
            match fault {
                Fault::HugeCounts | Fault::BadNidPointer | Fault::ZeroEntrySize | Fault::BadNamePointer => {
                    assert!(!b.warnings.is_empty(), "{fault:?} gave no warning");
                }
                _ => {}
            }
        }
        assert!(parse(Fault::BadStubPointer).imports.iter().all(|i| i.slot.is_none()));
        assert_eq!(parse(Fault::NoModuleInfo).libraries.len(), 1);
    }

    #[test]
    fn libraries_sharing_one_huge_nid_table_cost_a_bounded_amount() {
        // 1024 import entries, each claiming 65535 functions from the same 256 KB NID table: read
        // in full that would be 67 million NIDs from a file of a quarter of a megabyte.
        const ENTRIES: u64 = 1024;
        let table = ENTRIES * 20;
        let mut data = vec![0u8; usize::try_from(table + 4 * 65_535 + 16).unwrap()];
        for i in 0..ENTRIES {
            let at = usize::try_from(i * 20).unwrap();
            data[at + 8] = 5; // 5 words long, no variables
            data[at + 10..at + 12].copy_from_slice(&u16::MAX.to_le_bytes());
            data[at + 12..at + 16].copy_from_slice(&u32::try_from(table).unwrap().to_le_bytes());
        }
        let mut b = Binary::raw(Arch::Mips, 0, data.len() as u64, 0);
        let started = std::time::Instant::now();
        let libs = read_stubs(&data, &mut b, Some((0, table)));
        let read: usize = libs.iter().map(|l| l.functions.len()).sum();
        assert!(read <= MAX_NIDS, "{read}");
        assert!(b.warnings.iter().any(|w| w.contains("NIDs in all")), "{:?}", b.warnings.first());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn truncated_and_damaged_files_never_panic() {
        let f = psp_prx(Fault::None);
        for n in 0..f.len() {
            let _ = parse_binary(&f[..n]);
        }
        for i in 0..f.len() {
            for x in [0xffu8, 0x80, 0x01] {
                let mut g = f.clone();
                g[i] ^= x;
                let _ = parse_binary(&g);
            }
        }
    }
}
