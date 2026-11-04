// NTFS names straight from the Master File Table (M19, 0.13.0): one sequential read of the table
// instead of opening every folder. Read-only: the device is opened O_RDONLY and nothing is ever
// written. Anything unexpected is an Err, and the caller reads the drive folder by folder.
//
// Every in-use record gives its file names ($FILE_NAME: parent record + name). Names of a record
// with too many attributes sit in extension records, which name their base record, so the
// $ATTRIBUTE_LIST itself is never needed. DOS 8.3 names are skipped, as ntfs3 does.

use std::ffi::OsString;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

pub const USED: u8 = 1;
pub const DIR: u8 = 2;
pub const LINK: u8 = 4; // reparse point (symlink, junction...): listed by ntfs3 as a link, never followed
pub const EXT: u8 = 8; // extension record: its names belong to its base record

#[derive(Clone, Copy, Default)]
pub struct Rec {
    pub seq: u16,
    pub flags: u8,
    pub ctime: i64, // "MFT changed" time in ns since 1970, as ntfs3 reports ctime; 0 unknown
    pub size: u64,  // of the unnamed $DATA, as ntfs3 reports the size; 0 for folders
    pub mtime: u32, // seconds since 1970, as ntfs3 reports them; 0 unknown
    pub btime: u32, // created
}

pub struct Name {
    pub owner: u64,             // the record the name belongs to
    pub owner_seq: Option<u16>, // for a name in an extension record: the base's sequence it expects
    pub parent: u64,
    pub parent_seq: u16,
    pub name: OsString,
}

#[derive(Default)]
pub struct Stats {
    pub cluster: u64,
    pub rec_size: usize,
    pub extents: usize, // pieces the table is stored in
    pub bytes: u64,
    pub records: u64,
    pub used: u64,
    pub damaged: u64, // in use but failing the checks (after a second read): their names are missing
    pub reread: u64,  // looked damaged, fine when read again (written by the kernel meanwhile)
    pub ext: u64,
    pub dos: u64,
}

pub struct Mft {
    pub recs: Vec<Rec>,
    pub names: Vec<Name>,
    pub stats: Stats,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(o..o.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(o..o.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(o..o.checked_add(8)?)?.try_into().ok()?,
    ))
}

// Puts back the last two bytes of every 512-byte stride (the update sequence). False if a stride
// doesn't end in the record's sequence number: a torn or damaged record.
fn fixup(r: &mut [u8]) -> bool {
    let (Some(off), Some(n)) = (u16_at(r, 4), u16_at(r, 6)) else {
        return false;
    };
    let (off, n) = (off as usize, n as usize);
    if n < 2 || r.len() % 512 != 0 || n - 1 != r.len() / 512 || off + 2 * n > r.len() {
        return false;
    }
    let usn = [r[off], r[off + 1]];
    for i in 1..n {
        let end = i * 512;
        if r[end - 2..end] != usn {
            return false;
        }
        r[end - 2] = r[off + 2 * i];
        r[end - 1] = r[off + 2 * i + 1];
    }
    true
}

// The attributes of a fixed-up record: (type, whole attribute). None if one runs past the end.
fn attrs(r: &[u8]) -> Option<Vec<(u32, &[u8])>> {
    let used = (u32_at(r, 0x18)? as usize).min(r.len());
    let mut off = u16_at(r, 0x14)? as usize;
    let mut out = Vec::new();
    loop {
        let ty = u32_at(r, off)?;
        if ty == 0xFFFF_FFFF {
            return Some(out);
        }
        let len = u32_at(r, off + 4)? as usize;
        if len < 16 || off + len > used {
            return None;
        }
        out.push((ty, &r[off..off + len]));
        off += len;
    }
}

// A resident attribute's value.
fn value(a: &[u8]) -> Option<&[u8]> {
    if a.get(8) != Some(&0) {
        return None;
    }
    let (len, off) = (u32_at(a, 0x10)? as usize, u16_at(a, 0x14)? as usize);
    a.get(off..off.checked_add(len)?)
}

enum Parsed {
    Unused,
    Damaged,
    Rec {
        rec: Rec,
        base: Option<(u64, u16)>,
        names: Vec<(u64, u16, OsString)>,
        dos: u64,
        size: Option<u64>, // the unnamed $DATA's size, if this record holds its start
    },
}

// NTFS time in whole seconds since 1970.
fn nt_secs(t: u64) -> u32 {
    (nt_time(t) / 1_000_000_000).min(u32::MAX as i64) as u32
}

// NTFS time (100 ns since 1601) to ns since 1970.
fn nt_time(t: u64) -> i64 {
    (t as i64)
        .checked_sub(116_444_736_000_000_000)
        .and_then(|t| t.checked_mul(100))
        .filter(|t| *t > 0)
        .unwrap_or(0)
}

fn parse(r: &mut [u8]) -> Parsed {
    if r[..4] != *b"FILE" {
        return if r[..4] == [0; 4] {
            Parsed::Unused
        } else {
            Parsed::Damaged
        };
    }
    if !fixup(r) {
        return Parsed::Damaged;
    }
    let (Some(flags), Some(seq), Some(base)) = (u16_at(r, 0x16), u16_at(r, 0x10), u64_at(r, 0x20))
    else {
        return Parsed::Damaged;
    };
    if flags & 1 == 0 {
        return Parsed::Unused;
    }
    let Some(list) = attrs(r) else {
        return Parsed::Damaged;
    };
    let mut rec = Rec {
        seq,
        flags: USED | if flags & 2 != 0 { DIR } else { 0 },
        ..Default::default()
    };
    let base =
        (base & 0xFFFF_FFFF_FFFF != 0).then(|| (base & 0xFFFF_FFFF_FFFF, (base >> 48) as u16));
    if base.is_some() {
        rec.flags |= EXT;
    }
    let (mut names, mut dos, mut size) = (Vec::new(), 0, None);
    for (ty, a) in list {
        match ty {
            0x10 => {
                // $STANDARD_INFORMATION: times, then the file attributes
                let Some(v) = value(a) else {
                    return Parsed::Damaged;
                };
                if let (Some(b), Some(m), Some(c), Some(fa)) =
                    (u64_at(v, 0), u64_at(v, 8), u64_at(v, 0x10), u32_at(v, 0x20))
                {
                    (rec.btime, rec.mtime) = (nt_secs(b), nt_secs(m));
                    rec.ctime = nt_time(c);
                    if fa & 0x400 != 0 {
                        rec.flags |= LINK;
                    }
                }
            }
            0x30 => {
                // $FILE_NAME: parent reference, ..., name length, namespace, UTF-16 name
                let Some(v) = value(a) else {
                    return Parsed::Damaged;
                };
                let (Some(p), Some(&len), Some(&ns)) = (u64_at(v, 0), v.get(0x40), v.get(0x41))
                else {
                    return Parsed::Damaged;
                };
                let Some(n) = v.get(0x42..0x42 + 2 * len as usize) else {
                    return Parsed::Damaged;
                };
                if ns == 2 {
                    dos += 1; // DOS 8.3 alias of a long name
                    continue;
                }
                let n: Vec<u16> = n
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                names.push((
                    p & 0xFFFF_FFFF_FFFF,
                    (p >> 48) as u16,
                    String::from_utf16_lossy(&n).into(),
                ));
            }
            // $DATA without a name (named ones are alternate streams): resident, its value's
            // length; else the real size, kept in the piece that starts at cluster 0.
            0x80 if a.get(9) == Some(&0) => match a.get(8) {
                Some(0) => {
                    size = value(a).map(|v| v.len() as u64);
                    if value(a).is_some_and(|v| v.starts_with(b"IntxLNK\x01")) {
                        rec.flags |= LINK; // a symlink as ntfs-3g writes them (Interix)
                    }
                }
                Some(1) if u64_at(a, 0x10) == Some(0) => size = u64_at(a, 0x30),
                _ => {}
            },
            0xC0 => rec.flags |= LINK, // $REPARSE_POINT
            _ => {}
        }
    }
    Parsed::Rec {
        rec,
        base,
        names,
        dos,
        size,
    }
}

// The table's own $DATA (record 0): the cluster runs it is stored in, and its size in bytes.
fn table_runs(r0: &mut [u8], cluster: u64) -> Result<(Vec<(u64, u64)>, u64), String> {
    if r0[..4] != *b"FILE" || !fixup(r0) {
        return Err("the MFT's first record is damaged".into());
    }
    let list = attrs(r0).ok_or("the MFT's first record is damaged")?;
    let (_, a) = list
        .into_iter()
        .find(|(ty, a)| *ty == 0x80 && a.get(8) == Some(&1) && a.get(9) == Some(&0))
        .ok_or("the MFT has no data runs in its first record")?;
    let bad = || "the MFT's data runs are damaged".to_string();
    let (start, size, ro) = (
        u64_at(a, 0x10).ok_or_else(bad)?,
        u64_at(a, 0x30).ok_or_else(bad)?,
        u16_at(a, 0x20).ok_or_else(bad)? as usize,
    );
    if start != 0 {
        return Err(bad());
    }
    let (mut runs, mut lcn, mut i) = (Vec::new(), 0i64, ro);
    while let Some(&h) = a.get(i) {
        if h == 0 {
            break;
        }
        let (ls, os) = ((h & 15) as usize, (h >> 4) as usize);
        if !(1..=8).contains(&ls) || !(1..=8).contains(&os) {
            return Err(bad()); // offset size 0 would be a sparse run: never in the MFT
        }
        let b = a.get(i + 1..i + 1 + ls + os).ok_or_else(bad)?;
        let len = b[..ls].iter().rev().fold(0u64, |v, &x| v << 8 | x as u64);
        let off = b[ls..].iter().rev().fold(
            if b[ls + os - 1] & 0x80 != 0 { -1i64 } else { 0 },
            |v, &x| v << 8 | x as i64,
        );
        lcn = lcn.checked_add(off).filter(|l| *l >= 0).ok_or_else(bad)?;
        runs.push((lcn as u64, len));
        i += 1 + ls + os;
    }
    if runs
        .iter()
        .fold(0u64, |t, r| t.saturating_add(r.1))
        .saturating_mul(cluster)
        < size
    {
        return Err(
            "the MFT continues in an attribute list (very fragmented): not supported yet".into(),
        );
    }
    Ok((runs, size))
}

// Record `i`, read fine: its flags, and its names and size (an extension record's go to its
// base record; sizes into `sizes` as (base, its sequence, size), for after the read).
fn take(
    i: u64,
    p: Parsed,
    recs: &mut [Rec],
    names: &mut Vec<Name>,
    sizes: &mut Vec<(u64, u16, u64)>,
    st: &mut Stats,
) {
    let Parsed::Rec {
        mut rec,
        base,
        names: ns,
        dos,
        size,
    } = p
    else {
        return;
    };
    match (base, size) {
        (Some((b, seq)), Some(s)) => sizes.push((b, seq, s)),
        (None, Some(s)) if rec.flags & (DIR | LINK) == 0 => rec.size = s, // a link has no size of its own
        _ => {}
    }
    st.used += 1;
    st.ext += base.is_some() as u64;
    st.dos += dos;
    for (parent, parent_seq, name) in ns {
        names.push(Name {
            owner: base.map_or(i, |b| b.0),
            owner_seq: base.map(|b| b.1),
            parent,
            parent_seq,
            name,
        });
    }
    recs[i as usize] = rec;
}

// Sizes found in extension records, to their base records (if still the same file).
fn put_sizes(recs: &mut [Rec], sizes: Vec<(u64, u16, u64)>) {
    for (b, seq, size) in sizes {
        if let Some(r) = recs.get_mut(b as usize).filter(|r| r.seq == seq && r.flags & (DIR | LINK) == 0) {
            r.size = size;
        }
    }
}

// Reads the whole table of the NTFS volume on `dev`. `progress` counts records read.
pub fn read(dev: &Path, progress: &AtomicUsize) -> Result<Mft, String> {
    let f = File::open(dev).map_err(|e| format!("cannot open {}: {e}", dev.display()))?; // O_RDONLY
    let io = |e: std::io::Error| format!("reading {}: {e}", dev.display());
    let mut boot = [0u8; 512];
    f.read_exact_at(&mut boot, 0).map_err(io)?;
    match &boot[3..11] {
        b"NTFS    " => {}
        b"-FVE-FS-" => return Err("BitLocker-encrypted".into()),
        _ => return Err("no NTFS boot sector".into()),
    }
    let bps = u16_at(&boot, 0x0B).unwrap() as u64;
    let spc = match boot[0x0D] {
        v @ 0..=0x80 => v as u64,
        v => 1 << (256 - v as u32).min(20),
    };
    let cluster = bps * spc;
    if !bps.is_power_of_two() || !(256..=4096).contains(&bps) || cluster == 0 || cluster > 2 << 20 {
        return Err(format!(
            "unexpected geometry: {bps}-byte sectors, {spc} per cluster"
        ));
    }
    let rec_size = match boot[0x40] as i8 {
        v if v > 0 => v as u64 * cluster,
        v if (-20..0).contains(&v) => 1 << -(v as i32),
        _ => 0,
    };
    if !rec_size.is_power_of_two() || !(512..=65536).contains(&rec_size) {
        return Err(format!("unexpected MFT record size {rec_size}"));
    }
    let rs = rec_size as usize;
    let at = u64_at(&boot, 0x30)
        .unwrap()
        .checked_mul(cluster)
        .ok_or("unexpected MFT position")?;
    let mut r0 = vec![0u8; rs];
    f.read_exact_at(&mut r0, at).map_err(io)?;
    let (runs, size) = table_runs(&mut r0, cluster)?;
    let mut st = Stats {
        cluster,
        rec_size: rs,
        extents: runs.len(),
        ..Default::default()
    };
    let mut recs = Vec::with_capacity((size / rec_size) as usize);
    let (mut names, mut sizes) = (Vec::new(), Vec::new());
    let mut buf: Vec<u8> = Vec::new(); // a record split over two runs waits here for its rest
    let mut left = size;
    let mut again = Vec::new(); // (record, device offset) of records that looked damaged
    for (lcn, len) in runs {
        let mut at = lcn.checked_mul(cluster).ok_or("unexpected MFT run")?;
        let mut n = len.saturating_mul(cluster);
        while n > 0 && left > 0 {
            let k = n.min(4 << 20).min(left);
            let s = buf.len();
            buf.resize(s + k as usize, 0);
            f.read_exact_at(&mut buf[s..], at).map_err(io)?;
            let from = at; // where buf[s..] came from
            (at, n, left, st.bytes) = (at + k, n - k, left - k, st.bytes + k);
            let whole = buf.len() / rs * rs;
            for (j, r) in buf[..whole].chunks_exact_mut(rs).enumerate() {
                let i = recs.len() as u64;
                st.records += 1;
                recs.push(Rec::default());
                match parse(r) {
                    Parsed::Unused => {}
                    Parsed::Damaged => {
                        st.damaged += 1;
                        if j * rs >= s {
                            again.push((i, from + (j * rs - s) as u64)); // not split over two runs
                        }
                    }
                    p => take(i, p, &mut recs, &mut names, &mut sizes, &mut st),
                }
            }
            buf.drain(..whole);
            progress.fetch_add(whole / rs, Relaxed);
        }
    }
    // A record the kernel was writing as it was read is torn (its strides disagree): read it again.
    let mut r = vec![0u8; rs];
    for (i, off) in again {
        if f.read_exact_at(&mut r, off).is_ok() {
            if let p @ (Parsed::Rec { .. } | Parsed::Unused) = parse(&mut r) {
                st.damaged -= 1;
                st.reread += 1;
                take(i, p, &mut recs, &mut names, &mut sizes, &mut st);
            }
        }
    }
    put_sizes(&mut recs, sizes);
    Ok(Mft {
        recs,
        names,
        stats: st,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 1 KiB record: header, $STANDARD_INFORMATION, one $FILE_NAME per name, resident $DATA
    // streams (name, value), end marker; fixups applied.
    fn record(seq: u16, dir: bool, base: u64, names: &[(u64, u8, &str)]) -> Vec<u8> {
        record_with(seq, dir, base, names, &[])
    }

    fn record_with(
        seq: u16,
        dir: bool,
        base: u64,
        names: &[(u64, u8, &str)],
        data: &[(&str, &[u8])],
    ) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // update sequence at 0x30, 3 entries
        r[6..8].copy_from_slice(&3u16.to_le_bytes());
        r[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&(1u16 | if dir { 2 } else { 0 }).to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut off = 0x38;
        // Attribute name (UTF-16) at 0x18, value after it.
        let mut put_named = |r: &mut Vec<u8>, ty: u32, name: &str, v: &[u8]| {
            let n: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let vo = (0x18 + n.len()).div_ceil(8) * 8;
            let len = (vo + v.len()).div_ceil(8) * 8;
            r[off..off + 4].copy_from_slice(&ty.to_le_bytes());
            r[off + 4..off + 8].copy_from_slice(&(len as u32).to_le_bytes());
            r[off + 9] = (n.len() / 2) as u8;
            r[off + 0x0A..off + 0x0C].copy_from_slice(&0x18u16.to_le_bytes());
            r[off + 0x10..off + 0x14].copy_from_slice(&(v.len() as u32).to_le_bytes());
            r[off + 0x14..off + 0x16].copy_from_slice(&(vo as u16).to_le_bytes());
            r[off + 0x18..off + 0x18 + n.len()].copy_from_slice(&n);
            r[off + vo..off + vo + v.len()].copy_from_slice(v);
            off += len;
        };
        let mut si = vec![0u8; 0x30];
        let t = |s: u64| (116_444_736_000_000_000u64 + s * 10_000_000).to_le_bytes();
        si[0..8].copy_from_slice(&t(1_700_000_000)); // created
        si[8..0x10].copy_from_slice(&t(1_760_000_000)); // modified
        si[0x10..0x18].copy_from_slice(&t(1)); // MFT changed: 1970 + 1 s
        put_named(&mut r, 0x10, "", &si);
        for &(parent, ns, name) in names {
            let u: Vec<u16> = name.encode_utf16().collect();
            let mut v = vec![0u8; 0x42 + 2 * u.len()];
            v[..8].copy_from_slice(&(parent | 7 << 48).to_le_bytes());
            v[0x40] = u.len() as u8;
            v[0x41] = ns;
            for (i, c) in u.iter().enumerate() {
                v[0x42 + 2 * i..0x44 + 2 * i].copy_from_slice(&c.to_le_bytes());
            }
            put_named(&mut r, 0x30, "", &v);
        }
        for (name, v) in data {
            put_named(&mut r, 0x80, name, v);
        }
        r[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        r[0x18..0x1C].copy_from_slice(&((off + 8) as u32).to_le_bytes());
        r[0x30..0x32].copy_from_slice(&[0xAB, 0xCD]);
        for i in 1..3 {
            let end = i * 512;
            let (a, b) = (r[end - 2], r[end - 1]);
            r[0x30 + 2 * i] = a;
            r[0x31 + 2 * i] = b;
            r[end - 2..end].copy_from_slice(&[0xAB, 0xCD]);
        }
        r
    }

    #[test]
    fn names_from_a_record() {
        let mut r = record(
            3,
            false,
            0,
            &[
                (5, 1, "Résumé 日本 😀.txt"),
                (5, 2, "RSUM~1.TXT"),
                (40, 0, "link"),
            ],
        );
        let Parsed::Rec {
            rec,
            base,
            names,
            dos,
            size,
        } = parse(&mut r)
        else {
            panic!("not parsed")
        };
        assert_eq!(
            (rec.seq, rec.flags, rec.ctime, base, dos, size),
            (3, USED, 1_000_000_000, None, 1, None)
        );
        assert_eq!((rec.btime, rec.mtime), (1_700_000_000, 1_760_000_000));
        assert_eq!(
            names
                .iter()
                .map(|n| (n.0, n.1, n.2.to_str().unwrap()))
                .collect::<Vec<_>>(),
            [(5, 7, "Résumé 日本 😀.txt"), (40, 7, "link")]
        );
        let mut e = record(1, false, 77 | 9 << 48, &[(5, 1, "more")]);
        assert!(matches!(
            parse(&mut e),
            Parsed::Rec {
                base: Some((77, 9)),
                ..
            }
        ));
        let mut torn = record(1, true, 0, &[(5, 1, "x")]);
        torn[1022] ^= 1; // the second stride lost its sequence number
        assert!(matches!(parse(&mut torn), Parsed::Damaged));
        assert!(matches!(parse(&mut [0u8; 1024]), Parsed::Unused));
    }

    // Size from the unnamed $DATA only (a named one is an alternate stream); an extension
    // record's size goes to its base record, a folder's is 0.
    #[test]
    fn sizes_from_records() {
        let size = |r: &mut Vec<u8>| match parse(r) {
            Parsed::Rec { size, .. } => size,
            _ => panic!("not parsed"),
        };
        let mut r = record_with(1, false, 0, &[(5, 1, "f")], &[("Zone.Identifier", b"[ZoneTransfer]"), ("", b"hello")]);
        assert_eq!(size(&mut r), Some(5));
        let mut r = record_with(1, false, 0, &[(5, 1, "f")], &[("Zone.Identifier", b"x")]);
        assert_eq!(size(&mut r), None);
        let mut r = record_with(1, false, 0, &[(5, 1, "l")], &[("", b"IntxLNK\x01.\0.\0")]);
        assert!(matches!(parse(&mut r), Parsed::Rec { rec, .. } if rec.flags & LINK != 0));
        let mut recs = vec![Rec::default(); 30];
        let (mut names, mut sizes, mut st) = (Vec::new(), Vec::new(), Stats::default());
        let base = record_with(4, false, 0, &[(5, 1, "big")], &[]);
        let ext = record_with(1, false, 20 | 4 << 48, &[], &[("", &[7; 9])]);
        let old_ext = record_with(1, false, 20 | 3 << 48, &[], &[("", &[7; 2])]); // for an earlier file in record 20
        let folder = record_with(1, true, 0, &[(5, 1, "d")], &[("", b"xyz")]);
        for (i, mut r) in [(21, ext), (20, base), (22, old_ext), (23, folder)] {
            let p = parse(&mut r);
            take(i, p, &mut recs, &mut names, &mut sizes, &mut st);
        }
        put_sizes(&mut recs, sizes);
        assert_eq!((recs[20].size, recs[23].size), (9, 0));
    }

    // Random damage and every truncation of a record: may come out damaged, must never panic.
    #[test]
    fn damaged_records_never_panic() {
        let good = record(2, true, 0, &[(5, 1, "folder"), (5, 0, "posix")]);
        let mut x = 0x2545F4914F6CDD1Du64;
        for _ in 0..20_000 {
            let mut r = good.clone();
            for _ in 0..1 + x % 8 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                r[(x % 1024) as usize] = (x >> 32) as u8;
            }
            let _ = parse(&mut r);
            let _ = table_runs(&mut r, 4096);
        }
        for len in [4, 6, 0x18, 0x40, 0x100, 512] {
            let mut r = good[..len].to_vec();
            let _ = parse(&mut r);
            let _ = attrs(&r);
        }
    }
}
