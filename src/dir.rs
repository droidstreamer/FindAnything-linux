// One folder's entries, packed (M22, 0.16.0): every name in one shared buffer, a fixed 24-byte
// row per entry (size, modified, created), the keys of subfolders on the side, and a small table
// of row numbers for finding a name. No allocation per name (was ~200 bytes per entry).
// A removed entry is marked dead and dropped at the next repack, so row numbers hold until then.

use super::Key;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;

// Size and dates of an entry, as stat gives them: seconds since 1970, 0 = unknown.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Meta {
    pub size: u64, // 0 for folders (their total is worked out from the files inside)
    pub mtime: u32,
    pub btime: u32, // created
}

// Seconds since 1970 as stored: before 1970 is 0, after 2106 the last second.
pub fn secs(t: i64) -> u32 {
    t.clamp(0, u32::MAX as i64) as u32
}

const IS_DIR: u8 = 1; // has a key in `subs`
const DEAD: u8 = 2;
const LINEAR: usize = 32; // up to this many rows, names are found without the table

#[derive(Clone, Copy)]
struct Row {
    size: u64,
    mtime: u32,
    btime: u32,
    off: u32, // into `names`
    len: u16,
    flags: u8,
}

pub struct Dir {
    pub parent: Key,  // == own key for a mount root
    pub name: OsString, // full path for a mount root
    pub id: Vec<u8>, // fsid + file handle, as fanotify reports it (empty if unavailable)
    pub ctime: i64,  // ns, taken just before the folder was listed; 0 = re-list at next reconcile
    rows: Vec<Row>,
    names: Vec<u8>,          // names of all rows, dead ones too, back to back
    subs: Vec<(u32, Key)>,   // (row, key) of each indexed subfolder, by row
    slots: Vec<u32>,         // row + 1 by name hash, 0 = empty; unused up to LINEAR rows
    dead: u32,
}

pub struct Entry<'a> {
    pub name: &'a [u8],
    pub kid: Option<Key>, // an indexed subfolder
    pub meta: Meta,
}

fn hash(n: &[u8]) -> u64 {
    // FNV-1a, then a final mix so the low bits (the slot) depend on every byte.
    let mut h = n.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    });
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^ h >> 33
}

impl Dir {
    pub fn new(parent: Key, name: OsString, id: Vec<u8>, ctime: i64) -> Self {
        Dir {
            parent,
            name,
            id,
            ctime,
            rows: Vec::new(),
            names: Vec::new(),
            subs: Vec::new(),
            slots: Vec::new(),
            dead: 0,
        }
    }

    fn name_of(&self, r: &Row) -> &[u8] {
        &self.names[r.off as usize..r.off as usize + r.len as usize]
    }

    // Row number of the live entry `n`.
    fn find(&self, n: &[u8]) -> Option<usize> {
        let live = |i: usize| {
            let r = &self.rows[i];
            r.len as usize == n.len() && r.flags & DEAD == 0 && self.name_of(r) == n
        };
        if self.slots.is_empty() {
            return (0..self.rows.len()).find(|&i| live(i));
        }
        let mask = self.slots.len() - 1;
        let mut s = hash(n) as usize & mask;
        loop {
            match self.slots[s] {
                0 => return None,
                r if live(r as usize - 1) => return Some(r as usize - 1),
                _ => s = (s + 1) & mask,
            }
        }
    }

    // Puts row `i` in the table (which must have room).
    fn slot_in(&mut self, i: usize) {
        let mask = self.slots.len() - 1;
        let mut s = hash(self.name_of(&self.rows[i])) as usize & mask;
        while self.slots[s] != 0 {
            s = (s + 1) & mask;
        }
        self.slots[s] = i as u32 + 1;
    }

    // Room for more rows? The table is kept at most 3/4 full (dead rows count: they keep their
    // slots until the repack).
    fn full(&self) -> bool {
        self.rows.len() * 4 > self.slots.len() * 3
    }

    // A new table, 3/8 to 3/4 full.
    fn reslot(&mut self) {
        self.slots = Vec::new();
        if self.rows.len() > LINEAR {
            self.slots = vec![0; (self.rows.len() * 3 / 2 + 1).next_power_of_two()];
            for i in 0..self.rows.len() {
                self.slot_in(i);
            }
        }
    }

    fn sub(&self, i: usize) -> Result<usize, usize> {
        self.subs.binary_search_by_key(&(i as u32), |s| s.0)
    }

    fn kid_of(&self, i: usize) -> Option<Key> {
        if self.rows[i].flags & IS_DIR == 0 {
            return None;
        }
        self.sub(i).ok().map(|j| self.subs[j].1)
    }

    fn set_kid(&mut self, i: usize, kid: Option<Key>) {
        match (self.sub(i), kid) {
            (Ok(j), Some(k)) => self.subs[j].1 = k,
            (Ok(j), None) => {
                self.subs.remove(j);
            }
            (Err(j), Some(k)) => self.subs.insert(j, (i as u32, k)),
            (Err(_), None) => {}
        }
        let f = &mut self.rows[i].flags;
        *f = if kid.is_some() { *f | IS_DIR } else { *f & !IS_DIR };
    }

    // Adds `n` or, if it is there, replaces its kind and size and dates. True if it was new.
    pub fn set(&mut self, n: &OsStr, kid: Option<Key>, m: Meta) -> bool {
        let n = n.as_bytes();
        if let Some(i) = self.find(n) {
            let r = &mut self.rows[i];
            (r.size, r.mtime, r.btime) = (m.size, m.mtime, m.btime);
            self.set_kid(i, kid);
            return false;
        }
        // ponytail: names over 64 KB, or a folder holding 4 GB of names, can't be stored: left out
        // (Linux names are at most 255 bytes).
        let (Ok(len), Ok(off)) = (u16::try_from(n.len()), u32::try_from(self.names.len())) else {
            return false;
        };
        if u32::try_from(self.names.len() + n.len()).is_err() || self.rows.len() >= u32::MAX as usize - 1 {
            return false;
        }
        self.names.extend_from_slice(n);
        let i = self.rows.len();
        self.rows.push(Row {
            size: m.size,
            mtime: m.mtime,
            btime: m.btime,
            off,
            len,
            flags: 0,
        });
        if kid.is_some() {
            self.subs.push((i as u32, kid.unwrap())); // the last row: stays in order
            self.rows[i].flags |= IS_DIR;
        }
        if self.full() {
            self.reslot();
        } else {
            self.slot_in(i);
        }
        true
    }

    // Was `n` here, and an indexed subfolder (Some(Some(key))) or not (Some(None))?
    pub fn get(&self, n: &OsStr) -> Option<Option<Key>> {
        self.find(n.as_bytes()).map(|i| self.kid_of(i))
    }

    pub fn meta(&self, n: &OsStr) -> Option<Meta> {
        self.find(n.as_bytes()).map(|i| self.row_meta(i))
    }

    fn row_meta(&self, i: usize) -> Meta {
        let r = &self.rows[i];
        Meta {
            size: r.size,
            mtime: r.mtime,
            btime: r.btime,
        }
    }

    // Takes `n` out; returns what `get` gave for it.
    pub fn remove(&mut self, n: &OsStr) -> Option<Option<Key>> {
        let i = self.find(n.as_bytes())?;
        let kid = self.kid_of(i);
        self.set_kid(i, None);
        self.rows[i].flags |= DEAD;
        self.dead += 1;
        if self.dead as usize > 16 && self.dead as usize * 2 > self.rows.len() {
            self.repack();
        }
        Some(kid)
    }

    // Drops dead rows and their names, and gives back spare room.
    pub fn repack(&mut self) {
        let (rows, names, subs) = (
            std::mem::take(&mut self.rows),
            std::mem::take(&mut self.names),
            std::mem::take(&mut self.subs),
        );
        let mut j = 0;
        for (i, r) in rows.iter().enumerate().filter(|(_, r)| r.flags & DEAD == 0) {
            let off = self.names.len() as u32;
            self.names
                .extend_from_slice(&names[r.off as usize..r.off as usize + r.len as usize]);
            while subs.get(j).is_some_and(|s| (s.0 as usize) < i) {
                j += 1;
            }
            if r.flags & IS_DIR != 0 {
                self.subs.push((self.rows.len() as u32, subs[j].1));
            }
            self.rows.push(Row { off, ..*r });
        }
        self.dead = 0;
        self.rows.shrink_to_fit();
        self.names.shrink_to_fit();
        self.subs.shrink_to_fit();
        self.reslot();
    }

    pub fn len(&self) -> usize {
        self.rows.len() - self.dead as usize
    }

    pub fn entries(&self) -> impl Iterator<Item = Entry<'_>> + '_ {
        let mut j = 0;
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.flags & DEAD == 0)
            .map(move |(i, r)| {
                let mut kid = None;
                if r.flags & IS_DIR != 0 {
                    while (self.subs[j].0 as usize) < i {
                        j += 1;
                    }
                    kid = Some(self.subs[j].1);
                }
                Entry {
                    name: self.name_of(r),
                    kid,
                    meta: self.row_meta(i),
                }
            })
    }

    pub fn subdirs(&self) -> impl Iterator<Item = Key> + '_ {
        self.subs.iter().map(|s| s.1)
    }

    // The same folder under new keys (its drive came back under another device number).
    pub fn rekey(&mut self, f: &impl Fn(Key) -> Key) {
        self.parent = f(self.parent);
        self.subs.iter_mut().for_each(|s| s.1 = f(s.1));
    }

    // Every name back to back (and removed ones until the repack), for skipping a folder whose
    // names lack a search word. Names run together, so this can only say yes too often.
    pub fn names(&self) -> &[u8] {
        &self.names
    }

    // Bytes held, for the memory figures in tests.
    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.name.len()
            + self.id.capacity()
            + self.rows.capacity() * std::mem::size_of::<Row>()
            + self.names.capacity()
            + self.subs.capacity() * std::mem::size_of::<(u32, Key)>()
            + self.slots.capacity() * 4
    }

    // Snapshot form of the entries: count, then per entry: name length (u16), name, size,
    // modified, created, kind (0 file, 1 subfolder + its key).
    pub fn write_entries(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&(self.len() as u64).to_le_bytes())?;
        for e in self.entries() {
            w.write_all(&(e.name.len() as u16).to_le_bytes())?;
            w.write_all(e.name)?;
            w.write_all(&e.meta.size.to_le_bytes())?;
            w.write_all(&e.meta.mtime.to_le_bytes())?;
            w.write_all(&e.meta.btime.to_le_bytes())?;
            match e.kid {
                Some(k) => {
                    w.write_all(&[1])?;
                    w.write_all(&k.0.to_le_bytes())?;
                    w.write_all(&k.1.to_le_bytes())?;
                }
                None => w.write_all(&[0])?,
            }
        }
        Ok(())
    }

    pub fn read_entries(&mut self, r: &mut impl Read) -> io::Result<()> {
        fn take<const N: usize>(r: &mut impl Read) -> io::Result<[u8; N]> {
            let mut b = [0u8; N];
            r.read_exact(&mut b)?;
            Ok(b)
        }
        let bad = |what| io::Error::new(io::ErrorKind::InvalidData, what);
        let n = u64::from_le_bytes(take(r)?);
        let mut name = Vec::new();
        for _ in 0..n {
            name.resize(u16::from_le_bytes(take(r)?) as usize, 0);
            r.read_exact(&mut name)?;
            if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
                return Err(bad("a damaged name"));
            }
            let m = Meta {
                size: u64::from_le_bytes(take(r)?),
                mtime: u32::from_le_bytes(take(r)?),
                btime: u32::from_le_bytes(take(r)?),
            };
            let kid = match take::<1>(r)?[0] {
                0 => None,
                1 => Some((u64::from_le_bytes(take(r)?), u64::from_le_bytes(take(r)?))),
                _ => return Err(bad("a damaged entry")),
            };
            if !self.set(OsStr::from_bytes(&name), kid, m) {
                return Err(bad("a name listed twice"));
            }
        }
        self.shrink();
        Ok(())
    }

    // Trades entries with `o` (a fresh listing of the same folder); names, ids and dates stay.
    pub fn swap_entries(&mut self, o: &mut Dir) {
        std::mem::swap(&mut self.rows, &mut o.rows);
        std::mem::swap(&mut self.names, &mut o.names);
        std::mem::swap(&mut self.subs, &mut o.subs);
        std::mem::swap(&mut self.slots, &mut o.slots);
        std::mem::swap(&mut self.dead, &mut o.dead);
    }

    // After a folder is filled in one go: no spare room.
    pub fn shrink(&mut self) {
        self.rows.shrink_to_fit();
        self.names.shrink_to_fit();
        self.subs.shrink_to_fit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn m(size: u64) -> Meta {
        Meta {
            size,
            mtime: size as u32 ^ 7,
            btime: 3,
        }
    }

    // Checks every way of reading `d` against a plain map of what it should hold.
    fn same(d: &Dir, want: &HashMap<Vec<u8>, (Option<Key>, Meta)>) {
        assert_eq!(d.len(), want.len());
        let mut seen = 0;
        for e in d.entries() {
            assert_eq!(want.get(e.name), Some(&(e.kid, e.meta)), "{:?}", OsStr::from_bytes(e.name));
            seen += 1;
        }
        assert_eq!(seen, want.len());
        for (n, (k, me)) in want {
            let n = OsStr::from_bytes(n);
            assert_eq!(d.get(n), Some(*k));
            assert_eq!(d.meta(n), Some(*me));
        }
        let mut subs: Vec<Key> = d.subdirs().collect();
        let mut wsubs: Vec<Key> = want.values().filter_map(|v| v.0).collect();
        subs.sort();
        wsubs.sort();
        assert_eq!(subs, wsubs);
        assert!(d.subs.windows(2).all(|w| w[0].0 < w[1].0), "subfolders out of row order");
        for e in d.entries() {
            assert!(super::super::has(d.names(), &e.name.to_ascii_lowercase()));
        }
    }

    // Random adds, changes and removes on small folders (no table) and large ones (table), with
    // repacks along the way, checked against a HashMap after every step.
    #[test]
    fn matches_a_plain_map() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for &names in &[3u64, 8, 9, 32, 33, 40, 2000] {
            let mut d = Dir::new((1, 1), "x".into(), vec![], 0);
            let mut want = HashMap::new();
            for step in 0..(names * 30).max(400) {
                let n = format!("Name{}", rnd(names)).into_bytes();
                let os = OsStr::from_bytes(&n);
                match rnd(10) {
                    0..=4 => {
                        let kid = (rnd(3) == 0).then(|| (9, rnd(1000)));
                        let me = m(rnd(1 << 40));
                        assert_eq!(d.set(os, kid, me), !want.contains_key(&n));
                        want.insert(n, (kid, me));
                    }
                    5..=8 => assert_eq!(d.remove(os), want.remove(&n).map(|v| v.0)),
                    _ => assert_eq!(d.get(os), want.get(&n).map(|v| v.0)),
                }
                if step % 97 == 0 || names <= 9 {
                    same(&d, &want);
                }
            }
            same(&d, &want);
            d.repack();
            same(&d, &want);
            assert_eq!(d.dead, 0);
        }
    }

    // A big folder emptied one name at a time (rm -rf) stays quick and gives its room back.
    #[test]
    fn big_folder_emptied() {
        let mut d = Dir::new((1, 1), "x".into(), vec![], 0);
        for i in 0..200_000 {
            d.set(OsStr::new(&format!("f{i}")), None, m(i));
        }
        let t = std::time::Instant::now();
        for i in 0..200_000 {
            assert_eq!(d.remove(OsStr::new(&format!("f{i}"))), Some(None));
        }
        assert!(t.elapsed().as_secs() < 2, "{:?}", t.elapsed());
        assert_eq!(d.len(), 0);
        assert!(d.bytes() < 4096, "{} bytes left", d.bytes());
        d.set(OsStr::new("Last"), None, m(1));
        assert!(super::super::has(d.names(), b"last"));
    }

    #[test]
    fn snapshot_round_trip() {
        let mut d = Dir::new((1, 1), "x".into(), vec![], 0);
        for i in 0..50u64 {
            d.set(OsStr::new(&format!("é{i}")), (i % 5 == 0).then_some((2, i)), m(i << 33));
        }
        for i in 0..20 {
            d.remove(OsStr::new(&format!("é{i}")));
        }
        let mut buf = Vec::new();
        d.write_entries(&mut buf).unwrap();
        let mut e = Dir::new((1, 1), "x".into(), vec![], 0);
        e.read_entries(&mut &buf[..]).unwrap();
        let want: HashMap<Vec<u8>, (Option<Key>, Meta)> =
            d.entries().map(|x| (x.name.to_vec(), (x.kid, x.meta))).collect();
        same(&e, &want);
        // Cut short, or a name with a slash: refused, never half taken.
        assert!(Dir::new((1, 1), "x".into(), vec![], 0)
            .read_entries(&mut &buf[..buf.len() - 1])
            .is_err());
        let mut bad = Vec::new();
        bad.extend(1u64.to_le_bytes());
        bad.extend(3u16.to_le_bytes());
        bad.extend(b"a/b");
        bad.extend([0u8; 17]);
        assert!(Dir::new((1, 1), "x".into(), vec![], 0)
            .read_entries(&mut &bad[..])
            .is_err());
    }

    #[test]
    fn dates_clamped() {
        assert_eq!(secs(-5), 0);
        assert_eq!(secs(1_760_000_000), 1_760_000_000);
        assert_eq!(secs(i64::MAX), u32::MAX);
        assert_eq!(std::mem::size_of::<Row>(), 24);
    }
}
