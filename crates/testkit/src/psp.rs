//! A synthetic PlayStation Portable module (`.prx`): a little-endian MIPS ELF32 of type 0xFFA0
//! with a module info record, one import library of two functions and one variable, and one
//! export entry of one function and one variable. Addresses equal file offsets. The names and
//! hashes are invented.

pub const MODULE_NAME: &str = "TestModule";
pub const IMPORT_LIBRARY: &str = "ThreadManForUser";
pub const FUNCTION_NIDS: [u32; 2] = [0x446D_8DE6, 0x0000_00AB];
pub const VARIABLE_NID: u32 = 0x1234_5678;
pub const EXPORT_FUNCTION_NID: u32 = 0xD632_ACDB;
pub const EXPORT_VARIABLE_NID: u32 = 0xF01D_73A7;

pub const TEXT: u32 = 0x100;
pub const STUBS: u32 = 0x120;
pub const ENT: u32 = 0x140;
pub const STUB_TABLE: u32 = 0x160;
pub const MODULE_INFO: u32 = 0x180;
pub const RESIDENT: u32 = 0x1c0;
pub const NIDS: u32 = 0x200;
pub const EXPORT_DATA: u32 = RESIDENT;

/// What to break in the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    None,
    /// The library name pointer leads outside the image.
    BadNamePointer,
    /// The NID table pointer leads outside the image.
    BadNidPointer,
    /// 65535 functions and 255 variables claimed.
    HugeCounts,
    /// The import entry says it is 0 words long.
    ZeroEntrySize,
    /// The stub table range runs far past the image.
    HugeStubRange,
    /// The program header's module info pointer and the section both point nowhere.
    NoModuleInfo,
    /// The stub pointer leads outside the image.
    BadStubPointer,
}

fn put(v: &mut [u8], at: usize, x: &[u8]) {
    v[at..at + x.len()].copy_from_slice(x);
}
fn p32(v: &mut [u8], at: u32, x: u32) {
    put(v, at as usize, &x.to_le_bytes());
}
fn p16(v: &mut [u8], at: u32, x: u16) {
    put(v, at as usize, &x.to_le_bytes());
}

pub fn psp_prx(fault: Fault) -> Vec<u8> {
    let mut f = vec![0u8; 0x400];
    // Code and the two function stubs (`jr ra; nop`).
    p32(&mut f, TEXT, 0x0000_0000);
    for i in 0..2 {
        p32(&mut f, STUBS + 8 * i, 0x03e0_0008);
    }
    // Import entry.
    let name_va = RESIDENT + 0x20;
    put(&mut f, name_va as usize, IMPORT_LIBRARY.as_bytes());
    let (name_ptr, nid_ptr, stub_ptr) = match fault {
        Fault::BadNamePointer => (0x00ff_0000, NIDS, STUBS),
        Fault::BadNidPointer => (name_va, 0x00ff_0000, STUBS),
        Fault::BadStubPointer => (name_va, NIDS, 0x00ff_0000),
        _ => (name_va, NIDS, STUBS),
    };
    p32(&mut f, STUB_TABLE, name_ptr);
    p16(&mut f, STUB_TABLE + 4, 0x0011);
    p16(&mut f, STUB_TABLE + 6, 0x4001);
    let (words, vars, funcs) = match fault {
        Fault::ZeroEntrySize => (0u8, 1u8, 2u16),
        Fault::HugeCounts => (5, 255, 65535),
        _ => (5, 1, 2),
    };
    f[(STUB_TABLE + 8) as usize] = words;
    f[(STUB_TABLE + 9) as usize] = vars;
    p16(&mut f, STUB_TABLE + 10, funcs);
    p32(&mut f, STUB_TABLE + 12, nid_ptr);
    p32(&mut f, STUB_TABLE + 16, stub_ptr);
    for (i, n) in FUNCTION_NIDS.iter().enumerate() {
        p32(&mut f, NIDS + 4 * i as u32, *n);
    }
    p32(&mut f, NIDS + 8, VARIABLE_NID);
    // Export entry: the system entry (no name), one function, one variable.
    p32(&mut f, ENT, 0);
    p16(&mut f, ENT + 6, 0x8000);
    f[(ENT + 8) as usize] = 4;
    f[(ENT + 9) as usize] = 1;
    p16(&mut f, ENT + 10, 1);
    p32(&mut f, ENT + 12, EXPORT_DATA);
    p32(&mut f, EXPORT_DATA, EXPORT_FUNCTION_NID);
    p32(&mut f, EXPORT_DATA + 4, EXPORT_VARIABLE_NID);
    p32(&mut f, EXPORT_DATA + 8, TEXT);
    p32(&mut f, EXPORT_DATA + 12, EXPORT_DATA);
    // Module info.
    let mi = MODULE_INFO;
    p16(&mut f, mi, 0);
    f[(mi + 2) as usize] = 2;
    f[(mi + 3) as usize] = 1;
    put(&mut f, (mi + 4) as usize, MODULE_NAME.as_bytes());
    p32(&mut f, mi + 36, ENT);
    p32(&mut f, mi + 40, ENT + 16);
    p32(&mut f, mi + 44, STUB_TABLE);
    let stub_end = if fault == Fault::HugeStubRange { 0x7fff_0000 } else { STUB_TABLE + 20 };
    p32(&mut f, mi + 48, stub_end);
    // Section names.
    let mut names = vec![0u8];
    let mut name_of = |n: &str| -> u32 {
        let at = names.len() as u32;
        names.extend_from_slice(n.as_bytes());
        names.push(0);
        at
    };
    let secs: Vec<(u32, u32, u32, u32, u32)> = vec![
        // (name, flags, addr, size, type)
        (name_of(".text"), 6, TEXT, 0x10, 1),
        (name_of(".sceStub.text.ThreadManForUser"), 6, STUBS, 0x10, 1),
        (name_of(".lib.ent"), 2, ENT, 0x10, 1),
        (name_of(".lib.stub"), 2, STUB_TABLE, 0x14, 1),
        (name_of(".rodata.sceModuleInfo"), 2, MODULE_INFO, 0x34, 1),
        (name_of(".rodata.sceResident"), 2, RESIDENT, 0x40, 1),
        (name_of(".rodata.sceNid"), 2, NIDS, 0x10, 1),
    ];
    let shstr_name = name_of(".shstrtab");
    let shstr_off = f.len() as u32;
    f.extend_from_slice(&names);
    while f.len() % 4 != 0 {
        f.push(0);
    }
    let shoff = f.len() as u32;
    f.resize(f.len() + 40 * (secs.len() + 2), 0);
    let mut sh = |i: usize, name: u32, ty: u32, flags: u32, addr: u32, off: u32, size: u32| {
        let at = shoff + 40 * i as u32;
        p32(&mut f, at, name);
        p32(&mut f, at + 4, ty);
        p32(&mut f, at + 8, flags);
        p32(&mut f, at + 12, addr);
        p32(&mut f, at + 16, off);
        p32(&mut f, at + 20, size);
        p32(&mut f, at + 32, 4);
    };
    for (i, (n, fl, a, s, t)) in secs.iter().enumerate() {
        sh(i + 1, *n, *t, *fl, *a, *a, *s);
    }
    sh(secs.len() + 1, shstr_name, 3, 0, 0, shstr_off, names.len() as u32);
    // ELF header and one program header.
    put(&mut f, 0, b"\x7fELF\x01\x01\x01");
    p16(&mut f, 16, 0xFFA0);
    p16(&mut f, 18, 8);
    p32(&mut f, 20, 1);
    p32(&mut f, 24, TEXT);
    p32(&mut f, 28, 52);
    p32(&mut f, 32, shoff);
    p32(&mut f, 36, 0x1000_0001);
    p16(&mut f, 40, 52);
    p16(&mut f, 42, 32);
    p16(&mut f, 44, 1);
    p16(&mut f, 46, 40);
    p16(&mut f, 48, (secs.len() + 2) as u16);
    p16(&mut f, 50, (secs.len() + 1) as u16);
    let ph = 52u32;
    p32(&mut f, ph, 1);
    p32(&mut f, ph + 4, 0);
    p32(&mut f, ph + 8, 0);
    p32(&mut f, ph + 12, if fault == Fault::NoModuleInfo { 0 } else { MODULE_INFO });
    p32(&mut f, ph + 16, shstr_off);
    p32(&mut f, ph + 20, shstr_off);
    p32(&mut f, ph + 24, 5);
    p32(&mut f, ph + 28, 0x40);
    f
}
