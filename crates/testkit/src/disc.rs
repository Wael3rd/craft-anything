//! Synthetic ISO 9660 disc images (cooked, raw Mode 1 / Mode 2 Form 1, GD-ROM high-density) for
//! reader and tool tests. Built from scratch; no real disc data.

pub const SECTOR: usize = 2048;
/// The first block of a Dreamcast GD-ROM high-density area.
pub const GDROM_HIGH_DENSITY: u32 = 45_000;
/// The block the builders put the root directory at (after 16 system sectors, the primary
/// descriptor and the terminator), counted from the image's first block.
pub const FIRST_BLOCK: u32 = 18;

/// A node of a synthetic volume (names are upper-cased on the disc).
pub enum Node {
    File(&'static str, Vec<u8>),
    Dir(&'static str, Vec<Node>),
    /// A file whose directory record carries CD-XA attributes (see [`xa`]).
    XaFile(&'static str, Vec<u8>, u16),
}

/// How the volume is wrapped in a file.
#[derive(Clone, Copy)]
pub enum Wrap {
    /// 2048-byte sectors (`.iso`).
    Cooked,
    /// 2352-byte Mode 1 sectors (`.bin`).
    RawMode1,
    /// 2352-byte Mode 2 Form 1 sectors (`.bin`).
    RawMode2Form1,
    /// Raw Mode 1 track whose blocks are numbered from 45000, as in a GD-ROM high-density area.
    GdRom,
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    if let Some(s) = b.get_mut(o..o + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

/// One directory record (padded to an even length).
pub fn record(lba: u32, size: u32, dir: bool, id: &[u8]) -> Vec<u8> {
    record_with(lba, size, dir, id, &[])
}

/// One directory record with a system use field after the name (and its padding byte).
pub fn record_with(lba: u32, size: u32, dir: bool, id: &[u8], system_use: &[u8]) -> Vec<u8> {
    let pad = usize::from(id.len().is_multiple_of(2));
    let mut len = 33 + id.len() + pad + system_use.len();
    if len % 2 == 1 {
        len += 1;
    }
    let mut r = vec![0u8; len];
    r[0] = len as u8;
    put32(&mut r, 2, lba);
    put32(&mut r, 10, size);
    r[25] = if dir { 2 } else { 0 };
    r[32] = id.len() as u8;
    r[33..33 + id.len()].copy_from_slice(id);
    let su = 33 + id.len() + pad;
    r[su..su + system_use.len()].copy_from_slice(system_use);
    r
}

/// The 14-byte CD-XA system use field with `attributes` (0x0800 Mode 2 Form 1, 0x1000 Mode 2
/// Form 2, 0x2000 interleaved, 0x4000 CD audio, 0x8000 directory).
pub fn xa(attributes: u16) -> [u8; 14] {
    let [hi, lo] = attributes.to_be_bytes();
    [0, 0, 0, 0, hi, lo, b'X', b'A', 1, 0, 0, 0, 0, 0]
}

/// Directory records packed into sectors the way a disc does: a record never crosses a sector.
pub fn pack_records(records: &[Vec<u8>]) -> Vec<[u8; SECTOR]> {
    let mut out: Vec<[u8; SECTOR]> = vec![[0u8; SECTOR]];
    let mut pos = 0usize;
    for r in records {
        if pos + r.len() > SECTOR {
            out.push([0u8; SECTOR]);
            pos = 0;
        }
        if let Some(s) = out.last_mut() {
            s[pos..pos + r.len()].copy_from_slice(r);
        }
        pos += r.len();
    }
    out
}

struct Layout {
    blocks: Vec<[u8; SECTOR]>,
    first: u32,
    /// Add a child to every nested folder that points back at the root.
    loop_root: Option<u32>,
}

impl Layout {
    fn data(&mut self, data: &[u8]) -> u32 {
        let lba = self.first + self.blocks.len() as u32;
        for chunk in data.chunks(SECTOR) {
            let mut b = [0u8; SECTOR];
            b[..chunk.len()].copy_from_slice(chunk);
            self.blocks.push(b);
        }
        lba
    }

    fn dir(&mut self, parent: u32, nodes: &[Node], nested: bool) -> u32 {
        let idx = self.blocks.len();
        self.blocks.push([0u8; SECTOR]);
        let me = self.first + idx as u32;
        let mut recs = record(me, SECTOR as u32, true, &[0]);
        recs.extend(record(parent, SECTOR as u32, true, &[1]));
        for n in nodes {
            match n {
                Node::File(name, data) => {
                    let lba = self.data(data);
                    recs.extend(record(lba, data.len() as u32, false, format!("{}.;1", name.to_uppercase()).as_bytes()));
                }
                Node::XaFile(name, data, attributes) => {
                    let lba = self.data(data);
                    recs.extend(record_with(lba, data.len() as u32, false, format!("{}.;1", name.to_uppercase()).as_bytes(), &xa(*attributes)));
                }
                Node::Dir(name, kids) => {
                    let child = self.dir(me, kids, true);
                    recs.extend(record(child, SECTOR as u32, true, name.to_uppercase().as_bytes()));
                }
            }
        }
        if let (true, Some(root)) = (nested, self.loop_root) {
            recs.extend(record(root, SECTOR as u32, true, b"BACK"));
        }
        let n = recs.len().min(SECTOR);
        self.blocks[idx][..n].copy_from_slice(&recs[..n]);
        me
    }
}

fn sync_header(i: usize, mode: u8) -> [u8; 16] {
    let mut h = [0xffu8; 16];
    h[0] = 0;
    h[11] = 0;
    h[14] = (i % 75) as u8;
    h[15] = mode;
    h
}

/// A complete image: 16 empty system sectors, a primary descriptor, the terminator, then the volume
/// from block 18. `loop_to_root` adds a folder inside each sub-folder that points back at the root.
pub fn iso_image(root: &[Node], wrap: Wrap, system_id: &str, volume_id: &str, loop_to_root: bool) -> Vec<u8> {
    let base = if matches!(wrap, Wrap::GdRom) { GDROM_HIGH_DENSITY } else { 0 };
    image_numbered(root, wrap, system_id, volume_id, loop_to_root, base)
}

/// As [`iso_image`], with the image's first block numbered `base` (a track that starts part-way
/// into a disc numbers its blocks from where it starts).
pub fn iso_image_numbered(root: &[Node], wrap: Wrap, system_id: &str, volume_id: &str, base: u32) -> Vec<u8> {
    image_numbered(root, wrap, system_id, volume_id, false, base)
}

fn image_numbered(root: &[Node], wrap: Wrap, system_id: &str, volume_id: &str, loop_to_root: bool, base: u32) -> Vec<u8> {
    let first = base + FIRST_BLOCK;
    let mut l = Layout { blocks: Vec::new(), first, loop_root: loop_to_root.then_some(first) };
    let root_lba = l.dir(first, root, false);
    wrap_volume(&l.blocks, root_lba, SECTOR as u32, wrap, system_id, volume_id)
}

/// A cooked image whose volume is `blocks` from block 18 on, with the root directory at block 18
/// spanning `root_size` bytes. The caller writes every directory record (the root's first must be
/// its own `.` record, pointing at block 18, for the image to open); for hostile-input tests.
pub fn cooked_volume(blocks: &[[u8; SECTOR]], root_size: u32) -> Vec<u8> {
    wrap_volume(blocks, FIRST_BLOCK, root_size, Wrap::Cooked, "TEST", "HOSTILE")
}

/// A hostile cooked image of `blocks` directory sectors (2 KiB each): every sector holds eight
/// folders with 200-character names that point at the next sector and claim 64 MiB, so each
/// folder overlaps all those after it and the same records are listed again under ever longer paths.
pub fn overlapping_dirs(blocks: usize) -> Vec<u8> {
    let claim = 64 << 20;
    let mut out = Vec::with_capacity(blocks);
    for k in 0..blocks {
        let me = FIRST_BLOCK + k as u32;
        let mut recs = Vec::new();
        if k == 0 {
            recs.push(record(me, claim, true, &[0]));
        }
        for j in 0..8 {
            let name = format!("{:Z>200}", format!("{k}-{j}"));
            recs.push(record(me + 1, claim, true, name.as_bytes()));
        }
        out.extend(pack_records(&recs).into_iter().take(1));
    }
    cooked_volume(&out, claim)
}

fn wrap_volume(blocks: &[[u8; SECTOR]], root_lba: u32, root_size: u32, wrap: Wrap, system_id: &str, volume_id: &str) -> Vec<u8> {
    let mut pvd = [0u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    for (o, s) in [(8usize, system_id), (40, volume_id)] {
        let mut f = [b' '; 32];
        let n = s.len().min(32);
        f[..n].copy_from_slice(&s.as_bytes()[..n]);
        pvd[o..o + 32].copy_from_slice(&f);
    }
    put32(&mut pvd, 80, FIRST_BLOCK + blocks.len() as u32);
    pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
    let rr = record(root_lba, root_size, true, &[0]);
    pvd[156..156 + 34].copy_from_slice(&rr[..34]);
    let mut term = [0u8; SECTOR];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    let mut sectors: Vec<[u8; SECTOR]> = vec![[0u8; SECTOR]; 16];
    sectors.push(pvd);
    sectors.push(term);
    sectors.extend_from_slice(blocks);
    let mut out = Vec::new();
    for (i, s) in sectors.iter().enumerate() {
        match wrap {
            Wrap::Cooked => out.extend_from_slice(s),
            Wrap::RawMode1 | Wrap::GdRom => {
                out.extend_from_slice(&sync_header(i, 1));
                out.extend_from_slice(s);
                out.extend_from_slice(&[0u8; 288]);
            }
            Wrap::RawMode2Form1 => {
                out.extend_from_slice(&sync_header(i, 2));
                out.extend_from_slice(&[0, 0, 8, 0, 0, 0, 8, 0]);
                out.extend_from_slice(s);
                out.extend_from_slice(&[0u8; 280]);
            }
        }
    }
    out
}
