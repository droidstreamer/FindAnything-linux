// Find Anything: an Everything-style filename index for Linux.
// `fa daemon` scans each filesystem once, then keeps the in-memory index live with
// fanotify (one FAN_MARK_FILESYSTEM per filesystem). `fa find` / `fa status` query it
// over a unix socket. Everything worth debugging goes to the log file.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod dir;
mod mft;
mod query;

use dir::{Dir, Meta};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SOCK: &str = "/run/findanything.sock";
const STATE: &str = "/var/cache/findanything/index.bin"; // snapshot saved on shutdown
                                                         // Filesystems indexed when no paths are given. Everything else (proc, tmpfs, overlay...) is skipped.
const REAL_FS: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "vfat", "exfat", "ntfs", "ntfs3", "fuseblk",
    "jfs", "zfs", "bcachefs",
];

// ---------- logging ----------

static LOG: Mutex<Option<fs::File>> = Mutex::new(None);
static DEBUG: AtomicBool = AtomicBool::new(false); // Detailed log: every event and search
static SUMMARY: AtomicBool = AtomicBool::new(false); // Summary log: totals file + one line per find
static LOG_MAX: AtomicU64 = AtomicU64::new(0); // bytes; 0 = no cap
static LOG_BYTES: AtomicU64 = AtomicU64::new(0); // written to the current log file
static LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
static RECENT: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new()); // last INFO/WARN lines, for problem reports
static START: OnceLock<Instant> = OnceLock::new();
static OWNER: OnceLock<(u32, u32)> = OnceLock::new(); // --owner: gets the socket and logs (the boot service has no sudo user)
static STATE_PATH: OnceLock<PathBuf> = OnceLock::new(); // --state, for quit-nosave
// Set once the saved index is loaded (search works from then on, while drives are still checked).
static IDX: OnceLock<Arc<Mutex<Index>>> = OnceLock::new();
// Startup check or rebuild under way: what, since when, and whether a drive is being read (then
// the folder count and time on it are added: "checking <drive> (n of m drives), 1,234 folders...").
static PROGRESS: Mutex<Option<(String, Instant, bool)>> = Mutex::new(None);
static SCANNED: AtomicUsize = AtomicUsize::new(0); // entries read on the current drive, for progress
static CHECKED: AtomicUsize = AtomicUsize::new(0); // folders checked on the current drive, for progress
static EXPECTED: AtomicUsize = AtomicUsize::new(0); // the current drive's entries at the last save, 0: unknown
// The fanotify fd, and the bytes and count of events read, for the changes still waiting in the kernel.
static FAN_FD: AtomicI32 = AtomicI32::new(-1);
static EVENT_BYTES: AtomicU64 = AtomicU64::new(0);
static EVENT_COUNT: AtomicU64 = AtomicU64::new(0);
                                                   // Disk reads happen outside the index lock, so searches never wait on a slow drive. Changing a
                                                   // live drive's tree (event batches, checking a live drive) also holds WRITER, so events can't
                                                   // interleave with a check of the same tree. Drives that aren't live get no events: no WRITER.
static WRITER: Mutex<()> = Mutex::new(());

fn log_line(level: &str, msg: String) {
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    let line = format!("[{t:10.3}] {level:5} {msg}\n");
    if level != "DEBUG" && level != "STAT" {
        eprint!("{line}");
    }
    if level == "INFO" || level == "WARN" {
        // Kept for problem reports, whatever the log setting (Off included).
        let mut r = RECENT.lock().unwrap();
        if r.len() >= 150 {
            r.pop_front();
        }
        r.push_back(line.chars().take(400).collect());
    }
    let mut log = LOG.lock().unwrap();
    if let Some(f) = log.as_mut() {
        let _ = f.write_all(line.as_bytes());
        let max = LOG_MAX.load(Relaxed);
        if max > 0 && LOG_BYTES.fetch_add(line.len() as u64, Relaxed) + line.len() as u64 > max {
            // Over the cap: keep the full file as `.1` (one old file only) and start a new one.
            if let Some(p) = LOG_PATH.lock().unwrap().as_ref() {
                let mut old = p.clone().into_os_string();
                old.push(".1");
                let _ = fs::rename(p, &old);
                *log = fs::OpenOptions::new().create(true).append(true).open(p).ok();
                sudo_owner(p);
                LOG_BYTES.store(0, Relaxed);
            }
        }
    }
}
macro_rules! info { ($($a:tt)*) => { log_line("INFO", format!($($a)*)) } }
macro_rules! warn { ($($a:tt)*) => { log_line("WARN", format!($($a)*)) } }
// Summary log (and Detailed): finds, per-drive times; never one line per file change.
macro_rules! summ { ($($a:tt)*) => { if DEBUG.load(Relaxed) || SUMMARY.load(Relaxed) { log_line("STAT", format!($($a)*)) } } }
macro_rules! debug { ($($a:tt)*) => { if DEBUG.load(Relaxed) { log_line("DEBUG", format!($($a)*)) } } }

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
fn errstr(e: i32) -> String {
    io::Error::from_raw_os_error(e).to_string() // already ends in "(os error N)"
}

// ---------- index ----------

type Key = (u64, u64); // (st_dev, st_ino) of a directory

// Is `n` (not empty, ASCII lower case) in `hay`, ignoring ASCII case? First byte by scan, then the rest.
fn has(hay: &[u8], n: &[u8]) -> bool {
    let Some(end) = hay.len().checked_sub(n.len()) else {
        return false;
    };
    let (a, b) = (n[0], n[0].to_ascii_uppercase());
    let mut i = 0;
    while let Some(p) = hay[i..=end].iter().position(|&x| x == a || x == b) {
        i += p;
        if hay[i + 1..i + n.len()].eq_ignore_ascii_case(&n[1..]) {
            return true;
        }
        i += 1;
        if i > end {
            break;
        }
    }
    false
}

struct Mount {
    path: PathBuf,
    fstype: String,
    source: String,
    root: Key,
    fsid: [u8; 8],
    live: Result<(), String>,
    startup: String, // "scanned 1234 entries in 56 ms" / "snapshot: ... re-listed ..."
    checking: bool,  // being read or checked right now
    every: u64,      // not live: seconds between timed re-checks
    last: Option<(Instant, u64)>, // not live: when the last check ended, and how long it took (ms)
}

#[derive(Default)]
struct Stats {
    events: u64,
    renames: u64,
    stale_parent: u64,   // parent dir handle no longer opens (already deleted)
    unknown_parent: u64, // parent dir not in the index
    overflows: u64,
    scan_errors: u64,
    creates: u64,
    deletes: u64,
    slowest_ms: u64, // slowest event batch
    writes: u64,     // files written or changed: new size and dates
}

// Folders never indexed: by name (node_modules) anywhere, or by full path. Kept next to the snapshot.
#[derive(Clone, Default)]
struct Exclude {
    names: HashSet<OsString>,
    paths: HashSet<PathBuf>,
}

impl Exclude {
    fn parse(list: &str) -> Self {
        let mut e = Self::default();
        for l in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match l.strip_prefix('/') {
                Some(_) => e.paths.insert(PathBuf::from(if l.len() > 1 {
                    l.trim_end_matches('/')
                } else {
                    l
                })),
                None => e.names.insert(l.into()),
            };
        }
        e
    }

    fn list(&self) -> String {
        let mut v: Vec<String> = self
            .names
            .iter()
            .map(|n| n.to_string_lossy().into_owned())
            .chain(self.paths.iter().map(|p| p.display().to_string()))
            .collect();
        v.sort();
        v.iter().map(|l| format!("{l}\n")).collect()
    }

    fn file(state: &Path) -> PathBuf {
        state.with_extension("exclude")
    }

    // Is the folder `name` (full path from `path`, only worked out if paths are listed) excluded?
    fn hit(&self, name: &OsStr, path: impl FnOnce() -> PathBuf) -> bool {
        self.names.contains(name) || (!self.paths.is_empty() && self.paths.contains(&path()))
    }
}

#[derive(Default)]
struct Index {
    dirs: HashMap<Key, Box<Dir>>,
    mounts: Vec<Mount>,
    ids: HashMap<Vec<u8>, Key>, // dir handle -> dir, so events resolve without opening anything
    generation: u64,            // bumped on every change, so clients only re-search when needed
    no_dac_cap: bool,           // open_by_handle_at not permitted (user namespace): open by path
    log: PathBuf,
    skip: HashSet<PathBuf>, // every mount point: scans never cross into another mount
    exclude: Exclude,       // folders never indexed (Preferences)
    orphans: Vec<Key>,      // dirs detached by a rename, re-attached or dropped per batch
    queue: (Option<u64>, String), // event queue limit (None: unlimited) and how it was set, for status
    last_full: Option<(Instant, u64)>, // last full re-check after an overflow: when it ended, how long it took (ms)
    recheck: HashSet<Key>, // new dirs, and dirs an event couldn't open by path: listed after the batch
    state: PathBuf,        // snapshot file
    snap_roots: HashSet<Key>, // drive roots not mounted now (from the snapshot or unmounted): kept hidden, reconciled if they return
    stats: Stats,
    startup: String, // how this start went, for status: "loaded the saved index, ..., ready in 7.1 s"
}

impl Index {
    fn path_of(&self, mut k: Key) -> Option<PathBuf> {
        let mut parts = Vec::new();
        for _ in 0..4096 {
            let d = self.dirs.get(&k)?;
            if d.parent == k {
                if !self.mounts.iter().any(|m| m.root == k) {
                    return None; // drive not mounted: its tree is kept but hidden
                }
                let mut p = PathBuf::from(&d.name);
                p.extend(parts.iter().rev());
                return Some(p);
            }
            parts.push(&d.name);
            k = d.parent;
        }
        None
    }

    // Removes a directory and everything below it, and its entry in its parent.
    fn remove_dir(&mut self, k: Key) {
        if let Some(d) = self.dirs.get(&k) {
            let (p, n) = (d.parent, d.name.clone());
            if let Some(pd) = self.dirs.get_mut(&p).filter(|_| p != k) {
                if pd.get(&n) == Some(Some(k)) {
                    pd.remove(&n);
                }
            }
        }
        self.generation += 1;
        let mut stack = vec![k];
        while let Some(k) = stack.pop() {
            if let Some(d) = self.dirs.remove(&k) {
                if self.ids.get(&d.id) == Some(&k) {
                    self.ids.remove(&d.id);
                }
                stack.extend(d.subdirs());
            }
        }
    }

    // Puts freshly read directories in, replacing whatever was there under the same keys.
    fn insert(&mut self, b: Vec<(Key, Dir)>) {
        for (k, dir) in b {
            if let Some(old) = self.dirs.remove(&k) {
                for c in old.subdirs() {
                    self.remove_dir(c);
                }
            }
            if !dir.id.is_empty() {
                self.ids.insert(dir.id.clone(), k);
            }
            self.dirs.insert(k, Box::new(dir));
        }
        self.generation += 1;
    }

    // Starts indexing one filesystem: watches it first (so nothing that changes meanwhile is
    // missed) and lists it as a drive, so a tree kept from the snapshot is searchable at once.
    // `fill` then reads or checks it. Returns (root, whether the snapshot tree is reused).
    fn register(
        &mut self,
        path: PathBuf,
        fstype: String,
        source: String,
        fan: Option<RawFd>,
    ) -> Option<(Key, bool)> {
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                warn!("{path:?}: {e}, skipped");
                return None;
            }
        };
        let root = (meta.dev(), meta.ino());
        if self.mounts.iter().any(|m| m.root == root) {
            info!("{path:?}: same directory already indexed (bind mount), skipped");
            return None;
        }
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut sfs) } != 0 {
            warn!("{path:?}: statfs failed: {}, skipped", errstr(errno()));
            return None;
        }
        let fsid: [u8; 8] = unsafe { std::mem::transmute(sfs.f_fsid) };
        let live = match fan {
            None => Err("fanotify unavailable".to_string()),
            // Test hook: FA_TEST_NOT_LIVE=<fstype> treats that filesystem as giving no events.
            Some(_) if std::env::var("FA_TEST_NOT_LIVE").is_ok_and(|t| t == fstype) => {
                Err("not live (test)".to_string())
            }
            Some(f) => {
                let mark = |mask| unsafe {
                    libc::fanotify_mark(
                        f,
                        libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
                        mask,
                        libc::AT_FDCWD,
                        c.as_ptr(),
                    )
                };
                // Test hook: FA_TEST_NO_WRITE_EVENTS acts as if the kernel refused the write events.
                let names = NAME_EVENTS | libc::FAN_ONDIR;
                let all = names | libc::FAN_CLOSE_WRITE | libc::FAN_ATTRIB;
                let refused = if std::env::var_os("FA_TEST_NO_WRITE_EVENTS").is_some() {
                    Some("test".to_string())
                } else if mark(all) == 0 {
                    None
                } else {
                    Some(errstr(errno()))
                };
                match refused {
                    None => Ok(()),
                    Some(why) if mark(names) == 0 => {
                        info!("{path:?}: no write events from this drive ({why}): names stay live, sizes and dates of changed files are updated when their folder is re-read");
                        Ok(())
                    }
                    Some(_) => Err(format!("fanotify_mark: {}", errstr(errno()))),
                }
            }
        };
        // ponytail: a kept drive on the same device number is another drive now (it was replaced): drop it.
        // Same for a kept drive with the same mount path: the drive came back under a new device number (remounted, e.g. ntfs-3g to ntfs3).
        let stale: Vec<Key> = self
            .snap_roots
            .iter()
            .filter(|r| {
                **r != root
                    && (r.0 == root.0
                        || self.dirs.get(r).is_some_and(|d| d.name == path.as_os_str()))
            })
            .copied()
            .collect();
        for r in stale {
            info!("{path:?}: dropping its kept copy {r:?} (replaced, or back under a new device number)");
            self.snap_roots.remove(&r);
            self.remove_dir(r);
        }
        let snap = self.snap_roots.remove(&root) && self.dirs.contains_key(&root);
        if snap {
            let d = self.dirs.get_mut(&root).unwrap();
            d.name = path.clone().into_os_string();
            d.parent = root; // a drive's root is its own parent, even if an old index had it inside another drive
        } else if self.dirs.contains_key(&root) {
            self.remove_dir(root);
        }
        let every = poll_every(0);
        self.mounts.push(Mount {
            path,
            fstype,
            source,
            root,
            fsid,
            live,
            startup: "checking".into(),
            checking: true,
            every,
            last: None,
        });
        Some((root, snap))
    }

    fn remove_mount(&mut self, path: &Path) {
        if let Some(i) = self.mounts.iter().position(|m| m.path == path) {
            let m = self.mounts.remove(i);
            if removable(path) {
                self.remove_dir(m.root);
                info!("{path:?} unmounted: a removable drive, forgotten (read again if it comes back)");
            } else {
                self.snap_roots.insert(m.root);
                info!("{path:?} unmounted: its entries are hidden until it is mounted again");
            }
        }
    }

    // Device numbers of NVMe (and other) drives can change between boots. A kept drive found
    // again at the same path, with the same root folder and the same filesystem id (fsid, from
    // the UUID), is the same drive: its saved tree is moved to the new number instead of being
    // dropped and read from scratch (0.22.1). Called at startup, before the drives register.
    // ponytail: startup only; a drive mounted later under a new number is still read again
    // (NTFS on ntfs3 is re-read from its MFT anyway); do it in register if that ever matters.
    fn rekey_moved(&mut self, targets: &[(PathBuf, String, String)]) {
        let mut map = HashMap::new();
        for (p, _, _) in targets {
            let (Some(k), Some(fsid)) = (dir_key(p), fs_id(p)) else {
                continue;
            };
            if fsid == [0; 8] {
                continue; // no real id (ramfs and the like): can't tell it is the same drive
            }
            let found = self.snap_roots.iter().find(|r| {
                r.0 != k.0
                    && r.1 == k.1
                    && self.dirs.get(r).is_some_and(|d| {
                        d.name == p.as_os_str() && d.id.get(..8) == Some(&fsid[..])
                    })
            });
            if let Some(r) = found {
                info!(
                    "{p:?}: same drive under a new device number ({} -> {}): its kept copy is reused",
                    devname(r.0),
                    devname(k.0)
                );
                map.insert(r.0, k.0);
            }
        }
        self.rekey(&map);
    }

    // Moves every folder on device `from` to device `map[from]`, all at once (numbers can swap).
    // Anything still kept under a number a moved drive now uses belonged to another drive: dropped.
    fn rekey(&mut self, map: &HashMap<u64, u64>) {
        if map.is_empty() {
            return;
        }
        let taken: HashSet<u64> = map.values().copied().collect();
        let gone: Vec<Key> = self
            .dirs
            .keys()
            .filter(|k| taken.contains(&k.0) && !map.contains_key(&k.0))
            .copied()
            .collect();
        for k in gone {
            self.snap_roots.remove(&k);
            self.remove_dir(k);
        }
        let f = |k: Key| (map.get(&k.0).copied().unwrap_or(k.0), k.1);
        self.dirs = std::mem::take(&mut self.dirs)
            .into_iter()
            .map(|(k, mut d)| {
                d.rekey(&f);
                (f(k), d)
            })
            .collect();
        self.ids.values_mut().for_each(|k| *k = f(*k));
        self.snap_roots = self.snap_roots.iter().map(|&k| f(k)).collect();
        self.generation += 1;
    }

    // Drops kept drives that aren't wanted any more: removable ones (read afresh when plugged
    // in) and /boot (no longer indexed). Called once the drives mounted at startup are claimed.
    fn forget_kept(&mut self) {
        let gone: Vec<Key> = self
            .snap_roots
            .iter()
            .filter(|k| {
                self.dirs.get(k).is_some_and(|d| {
                    removable(Path::new(&d.name)) || Path::new(&d.name).starts_with("/boot")
                })
            })
            .copied()
            .collect();
        for k in gone {
            info!(
                "forgetting {:?} (not mounted, or no longer indexed)",
                self.dirs[&k].name
            );
            self.snap_roots.remove(&k);
            self.remove_dir(k);
        }
    }

    // ---------- snapshot ----------
    // Plain binary: magic, drive roots, then every directory with its listing. Readable by root only
    // (it names every file on the system). Written to a temp file and renamed, so never half-written.
    fn save(&self) {
        let t = Instant::now();
        debug!("memory before saving: {}", mem());
        if let Some(d) = self.state.parent() {
            let _ = fs::DirBuilder::new().recursive(true).mode(0o700).create(d);
        }
        let tmp = self.state.with_extension("tmp");
        let r = (|| -> io::Result<()> {
            let _ = fs::remove_file(&tmp);
            let f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            let mut w = BufWriter::with_capacity(1 << 20, f);
            w.write_all(SNAP_MAGIC)?;
            w64(&mut w, (self.mounts.len() + self.snap_roots.len()) as u64)?;
            for k in self.mounts.iter().map(|m| &m.root).chain(&self.snap_roots) {
                wkey(&mut w, *k)?;
            }
            w64(&mut w, self.dirs.len() as u64)?;
            for (k, d) in &self.dirs {
                wkey(&mut w, *k)?;
                wkey(&mut w, d.parent)?;
                w64(&mut w, d.ctime as u64)?;
                wbytes(&mut w, d.name.as_bytes())?;
                wbytes(&mut w, &d.id)?;
                d.write_entries(&mut w)?;
            }
            w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            fs::rename(&tmp, &self.state)
        })();
        match r {
            Ok(()) => {
                info!(
                    "saved the index to {:?} ({} directories) in {} ms, memory {}",
                    self.state,
                    self.dirs.len(),
                    t.elapsed().as_millis(),
                    rss()
                );
                debug!("memory after saving: {}", mem());
            }
            Err(e) => warn!("saving the index to {:?} failed: {e}", self.state),
        }
        // Each drive's entries, kept when the index is deleted: progress estimates for the next full read.
        let _ = fs::write(self.state.with_extension("drives"), self.drives());
    }

    // Loads the snapshot into an empty index; on any error the index stays empty (full scan).
    fn load(&mut self) {
        let t = Instant::now();
        let r = (|| -> io::Result<(HashSet<Key>, HashMap<Key, Box<Dir>>)> {
            let mut r = BufReader::with_capacity(1 << 20, fs::File::open(&self.state)?);
            let mut magic = [0u8; 8];
            r.read_exact(&mut magic)?;
            if &magic != SNAP_MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    if magic[..7] == SNAP_MAGIC[..7] && magic[7] < SNAP_MAGIC[7] {
                        "saved by a version before 0.16.0, which stored the index another way"
                    } else {
                        "not a snapshot of this version"
                    },
                ));
            }
            let roots = (0..r64(&mut r)?)
                .map(|_| rkey(&mut r))
                .collect::<io::Result<_>>()?;
            let mut dirs = HashMap::new();
            for _ in 0..r64(&mut r)? {
                let k = rkey(&mut r)?;
                let parent = rkey(&mut r)?;
                let ctime = r64(&mut r)? as i64;
                let name = OsString::from_vec(rbytes(&mut r)?);
                let id = rbytes(&mut r)?;
                let mut d = Box::new(Dir::new(parent, name, id, ctime));
                d.read_entries(&mut r)?;
                dirs.insert(k, d);
            }
            Ok((roots, dirs))
        })();
        match r {
            Ok((roots, dirs)) => {
                info!(
                    "loaded the saved index {:?}: {} directories in {} ms",
                    self.state,
                    dirs.len(),
                    t.elapsed().as_millis()
                );
                self.ids = dirs
                    .iter()
                    .filter(|(_, d)| !d.id.is_empty())
                    .map(|(k, d)| (d.id.clone(), *k))
                    .collect();
                self.snap_roots = roots;
                self.dirs = dirs;
                self.startup = "loaded the saved index".into();
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                info!("no saved index at {:?}: full scan", self.state);
                self.startup = "no saved index".into();
            }
            Err(e) => {
                warn!("saved index {:?} unusable ({e}): full scan", self.state);
                self.startup = format!("saved index not usable ({e})");
            }
        }
    }

    // The directory an event's file handle points at, opened (O_PATH) for checking names in it.
    // The handle is only ever opened on its own filesystem, and only where that is safe
    // (handle_mounts); everywhere else the folder is opened by its path in the index.
    fn resolve(&mut self, id: &[u8]) -> Result<(OwnedFd, Key), i32> {
        if id.len() < 16 {
            return Err(libc::EINVAL);
        }
        let (fsid, handle) = id.split_at(8);
        let known = self.ids.get(id).copied();
        let htype = i32::from_ne_bytes(handle[4..8].try_into().unwrap());
        let mounts: Vec<PathBuf> = if self.no_dac_cap {
            vec![]
        } else {
            handle_mounts(&self.mounts, fsid, htype)
        };
        if !mounts.is_empty() {
            let mut buf = vec![0u32; handle.len().div_ceil(4)]; // file_handle needs 4-byte alignment
            unsafe {
                std::ptr::copy_nonoverlapping(
                    handle.as_ptr(),
                    buf.as_mut_ptr() as *mut u8,
                    handle.len(),
                )
            };
            let mut err = libc::ESTALE;
            for path in &mounts {
                // Opened per event, not kept: a held fd would stop the drive from unmounting ("target is busy").
                let Ok(mc) = CString::new(path.as_os_str().as_bytes()) else {
                    continue;
                };
                let mfd = unsafe {
                    libc::open(
                        mc.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                    )
                };
                if mfd < 0 {
                    continue;
                }
                let mfd = unsafe { OwnedFd::from_raw_fd(mfd) };
                let fd = unsafe {
                    libc::open_by_handle_at(
                        mfd.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::file_handle,
                        libc::O_PATH | libc::O_CLOEXEC,
                    )
                };
                if fd >= 0 {
                    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                    let mut st: libc::stat = unsafe { std::mem::zeroed() };
                    unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
                    let k = (st.st_dev as u64, st.st_ino as u64);
                    if known.is_none() {
                        debug!("  handle not in index, opened as {k:?}");
                    }
                    return Ok((fd, k));
                }
                err = errno();
                debug!("  open_by_handle_at on {path:?}: {}", errstr(err));
                if err == libc::EPERM {
                    break;
                }
            }
            if err != libc::EPERM {
                return Err(err);
            }
            warn!("open_by_handle_at not permitted (no CAP_DAC_READ_SEARCH): resolving events by path from now on");
            self.no_dac_cap = true;
        }
        // By path: only folders in the index, and only if the path still leads to that same folder.
        // If it doesn't (moved or deleted since the event), the folder is re-checked after the batch,
        // once the batch's own rename events have put it at its new path.
        let k = known.ok_or(libc::ESTALE)?;
        let c = CString::new(
            self.path_of(k)
                .ok_or(libc::ESTALE)?
                .into_os_string()
                .into_vec(),
        )
        .map_err(|_| libc::EINVAL)?;
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            self.recheck.insert(k);
            return Err(errno());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0
            || (st.st_dev as u64, st.st_ino as u64) != k
        {
            self.recheck.insert(k);
            return Err(libc::ESTALE);
        }
        Ok((fd, k))
    }

    // Makes the index entry `name` in the dir behind `rec` match what is on disk right now.
    // Checking reality instead of replaying event order makes merged or out-of-order
    // events (create+delete of a temp file, etc.) come out right.
    // `detach`: the old entry may be a directory that moved (rename source): keep its
    // subtree until the end of the batch so the rename target can re-attach it.
    fn sync(&mut self, rec: &Rec, detach: bool) {
        let (dfd, pk) = match self.resolve(rec.id) {
            Ok(x) => x,
            Err(e) => {
                self.stats.stale_parent += 1;
                debug!("  parent of {:?} not openable: {}", rec.name, errstr(e));
                return;
            }
        };
        if !self.dirs.contains_key(&pk) {
            self.stats.unknown_parent += 1;
            debug!(
                "  parent {pk:?} of {:?} not in index (handle {})",
                rec.name,
                hex(rec.id)
            );
            return;
        }
        // A name came, went or moved in the folder: its own modified date moved too, even when
        // the index ends up as it was (a file deleted and made again before we looked).
        self.touch(pk, dfd.as_raw_fd());
        let name = rec.name;
        let mut actual = stat_at(dfd.as_raw_fd(), name);
        if actual.as_ref().is_some_and(|a| a.dir)
            && self
                .exclude
                .hit(name, || self.path_of(pk).unwrap_or_default().join(name))
        {
            actual = None; // an excluded folder: as if it weren't there
        }
        let old = self.dirs[&pk].get(name);
        debug!(
            "  {:?}/{:?}: index {:?} -> disk {:?}",
            self.path_of(pk).unwrap_or_default(),
            name,
            old,
            actual.as_ref().map(|a| (a.dir, a.key))
        );
        match (&actual, old) {
            // Same file, or same folder: only its size and dates can have changed.
            (Some(a), Some(None)) if !a.dir => {
                self.dirs.get_mut(&pk).unwrap().set(name, None, a.meta);
                return;
            }
            (Some(a), Some(Some(o))) if a.dir && a.key == o => {
                self.dirs.get_mut(&pk).unwrap().set(name, Some(o), a.meta);
                return;
            }
            _ => {}
        }
        self.generation += 1;
        if let Some(Some(o)) = old {
            if detach {
                self.dirs.get_mut(&pk).unwrap().remove(name);
                self.orphans.push(o);
            } else {
                self.remove_dir(o);
            }
        }
        let Some(a) = actual else {
            self.dirs.get_mut(&pk).unwrap().remove(name);
            return;
        };
        if !a.dir {
            self.dirs.get_mut(&pk).unwrap().set(name, None, a.meta);
            return;
        }
        let k = a.key;
        self.dirs.get_mut(&pk).unwrap().set(name, Some(k), a.meta);
        if self.orphans.contains(&k) && self.dirs.contains_key(&k) {
            let d = self.dirs.get_mut(&k).unwrap(); // moved directory: re-attach, no rescan
            d.parent = pk;
            d.name = name.into();
            return;
        }
        if self.dirs.contains_key(&k) {
            self.remove_dir(k); // stale record (inode reused): forget it
            self.dirs.get_mut(&pk).unwrap().set(name, Some(k), a.meta);
        }
        // Listed after the batch, outside the index lock (recheck): a big folder
        // unpacked or moved here would otherwise stall every search while it is read.
        let Some(path) = self.path_of(pk) else { return };
        let id = dir_id(&path.join(name), k.0, &mut HashMap::new());
        if !id.is_empty() {
            self.ids.insert(id.clone(), k);
        }
        self.dirs
            .insert(k, Box::new(Dir::new(pk, name.into(), id, 0)));
        self.recheck.insert(k);
        debug!("  new dir: read after this batch");
    }

    // A file was written, or its attributes changed: its new size and dates, if it is listed.
    // Names are left to the create, delete and rename events. Folders the index doesn't hold
    // (excluded, not read yet) are passed over quietly: these events come for every file.
    fn sync_meta(&mut self, rec: &Rec) {
        let Some(&pk) = self.ids.get(rec.id) else {
            return;
        };
        if rec.name == "." {
            // The folder itself (its own handle, name "."): as listed in its parent.
            if let Ok((fd, k)) = self.resolve(rec.id) {
                if k == pk {
                    self.stats.writes += 1;
                    self.touch(k, fd.as_raw_fd());
                }
            }
            return;
        }
        let Some(kid) = self.dirs.get(&pk).and_then(|d| d.get(rec.name)) else {
            return;
        };
        let Ok((dfd, k)) = self.resolve(rec.id) else {
            return;
        };
        match stat_at(dfd.as_raw_fd(), rec.name) {
            Some(a) if k == pk && a.dir == kid.is_some() && kid.is_none_or(|c| c == a.key) => {
                self.stats.writes += 1;
                self.dirs.get_mut(&pk).unwrap().set(rec.name, kid, a.meta);
            }
            _ => {} // gone or replaced meanwhile: its own event follows
        }
    }

    // Folder `k` (open as `fd`) changed: its modified date, as listed in its parent.
    fn touch(&mut self, k: Key, fd: RawFd) {
        let (Some(st), Some(d)) = (stat_at(fd, OsStr::new("")), self.dirs.get(&k)) else {
            return;
        };
        if d.parent == k || st.key != k {
            return; // a drive's own folder isn't listed anywhere
        }
        let (p, n) = (d.parent, d.name.clone());
        if let Some(pd) = self.dirs.get_mut(&p).filter(|pd| pd.get(&n) == Some(Some(k))) {
            pd.set(&n, Some(k), st.meta);
        }
    }

    // End of a read() batch: drop renamed-away dirs that nothing re-attached.
    fn reap_orphans(&mut self) {
        for k in std::mem::take(&mut self.orphans) {
            let attached = self.dirs.get(&k).map_or(true, |d| {
                self.dirs
                    .get(&d.parent)
                    .is_some_and(|p| p.get(&d.name) == Some(Some(k)))
            });
            if !attached {
                self.remove_dir(k);
            }
        }
    }

    // Returns true on a queue overflow: the caller then checks every live drive.
    fn handle_event(&mut self, mask: u64, recs: &[Rec]) -> bool {
        let t = Instant::now();
        let r = self.handle_event_inner(mask, recs);
        self.stats.slowest_ms = self.stats.slowest_ms.max(t.elapsed().as_millis() as u64);
        r
    }

    fn handle_event_inner(&mut self, mask: u64, recs: &[Rec]) -> bool {
        if mask & (NAME_EVENTS | libc::FAN_Q_OVERFLOW) == 0 {
            // Only written or attributes changed (FAN_CLOSE_WRITE, FAN_ATTRIB): size and dates.
            // Not counted or logged one by one: they come for every file saved.
            for r in recs {
                self.sync_meta(r);
            }
            return false;
        }
        self.stats.events += 1;
        if mask & libc::FAN_CREATE != 0 {
            self.stats.creates += 1;
        }
        if mask & libc::FAN_DELETE != 0 {
            self.stats.deletes += 1;
        }
        debug!("event {} ({} records)", mask_names(mask), recs.len());
        if mask & libc::FAN_Q_OVERFLOW != 0 {
            self.stats.overflows += 1;
            return true;
        }
        if mask & libc::FAN_RENAME != 0 {
            self.stats.renames += 1;
        }
        // Rename: source first (detaching), then target. Other events: one DFID_NAME record.
        for r in recs
            .iter()
            .filter(|r| r.kind == libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME)
        {
            self.sync(r, true);
        }
        for r in recs
            .iter()
            .filter(|r| r.kind != libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME)
        {
            self.sync(r, false);
        }
        false
    }

    // The first `limit` matches (folders end in "/") and how many there are in all. Every core
    // scans its share of the folders (by inode number), so it scales with the cores.
    fn search(&self, q: &query::Query, limit: usize) -> (Vec<OsString>, usize) {
        let parts = std::thread::available_parallelism().map_or(4, |n| n.get()) as u64;
        let needed = &q.needed();
        let found: Vec<(Vec<OsString>, usize)> = std::thread::scope(|s| {
            let t: Vec<_> = (0..parts)
                .map(|i| s.spawn(move || self.search_part(q, needed, limit, i, parts)))
                .collect();
            t.into_iter().map(|t| t.join().unwrap()).collect()
        });
        let total = found.iter().map(|f| f.1).sum();
        let mut out: Vec<OsString> = found.into_iter().flat_map(|f| f.0).collect();
        out.truncate(limit);
        (out, total)
    }

    // search() over the folders whose inode number is `part` modulo `parts`, skipping folders
    // without a `needed` word in any name.
    fn search_part(
        &self,
        q: &query::Query,
        needed: &[Vec<u8>],
        limit: usize,
        part: u64,
        parts: u64,
    ) -> (Vec<OsString>, usize) {
        let (mut out, mut total) = (Vec::new(), 0);
        // ponytail: linear scan with ASCII-only case folding, like Everything. Queries without a
        // plain word (`*.png`, `ext:`, `!x` alone) still visit every name.
        let mut full = Vec::new(); // reused for every entry's full path, if the query needs it
        for (k, d) in self.dirs.iter().filter(|(k, _)| k.1 % parts == part) {
            if !needed.iter().all(|w| has(d.names(), w)) {
                continue;
            }
            let mut shown = None; // is this folder on a mounted drive? worked out at its first match
            let mut folder: Option<Vec<u8>> = None; // its path ending in "/", if the query needs it
            for e in d.entries() {
                let (name, c) = (e.name, e.kid);
                if q.path && folder.is_none() {
                    let Some(p) = self.path_of(*k) else { break }; // drive not mounted
                    let mut p = p.into_os_string().into_vec();
                    if p.last() != Some(&b'/') {
                        p.push(b'/');
                    }
                    folder = Some(p);
                }
                if let Some(f) = &folder {
                    full.clear();
                    full.extend_from_slice(f);
                    full.extend_from_slice(name);
                    if c.is_some() {
                        full.push(b'/');
                    }
                }
                let kids: Vec<&[u8]> = match c.filter(|_| q.kids).and_then(|c| self.dirs.get(&c)) {
                    Some(cd) => cd.entries().map(|e| e.name).collect(),
                    None => Vec::new(),
                };
                let e = query::Entry {
                    name,
                    dir: c.is_some(),
                    folder: folder.as_deref().unwrap_or(b""),
                    full: &full,
                    kids: &kids,
                };
                if !q.matches(&e) {
                    continue;
                }
                if !*shown
                    .get_or_insert_with(|| self.mounts.iter().any(|m| m.root == self.root_of(*k)))
                {
                    break; // drive not mounted: its tree is kept but hidden
                }
                total += 1;
                if out.len() < limit {
                    let Some(p) = self.path_of(*k) else { continue };
                    let mut p = p.join(OsStr::from_bytes(name)).into_os_string();
                    if c.is_some() {
                        p.push("/");
                    }
                    out.push(p);
                }
            }
        }
        (out, total)
    }

    // Saves a new exclude list and drops the newly excluded folders from the index. Returns
    // true if a folder was taken off the list: the drives must then be re-read to bring it back.
    fn set_exclude(&mut self, list: &str) -> bool {
        let new = Exclude::parse(list);
        let back = self.exclude.names.difference(&new.names).next().is_some()
            || self.exclude.paths.difference(&new.paths).next().is_some();
        let f = Exclude::file(&self.state);
        if let Some(d) = f.parent() {
            let _ = fs::DirBuilder::new().recursive(true).mode(0o700).create(d);
        }
        if let Err(e) = fs::write(&f, new.list()) {
            warn!("saving the exclude list {f:?} failed: {e}");
        }
        let mut gone: Vec<Key> = self
            .dirs
            .iter()
            .filter(|(k, d)| d.parent != **k && new.names.contains(&d.name))
            .map(|(k, _)| *k)
            .collect();
        gone.extend(
            new.paths
                .iter()
                .filter_map(|p| dir_key(p))
                .filter(|k| self.dirs.get(k).is_some_and(|d| d.parent != *k)),
        );
        info!(
            "exclude list now {:?}: {} folders dropped from the index",
            new.list().lines().collect::<Vec<_>>(),
            gone.len()
        );
        for k in gone {
            self.remove_dir(k);
        }
        self.exclude = new;
        back
    }

    // Size and dates of `p` (listed in folder `k`) in the index and on disk, and a folder's total.
    fn meta_lines(&self, k: Key, p: &Path) -> String {
        let name = p.file_name().unwrap_or_default();
        let (Some(d), Some(m)) = (self.dirs.get(&k), self.dirs.get(&k).and_then(|d| d.meta(name)))
        else {
            return String::new();
        };
        let disk = stat_at(libc::AT_FDCWD, p.as_os_str()).map(|s| s.meta);
        let mut o = format!(
            "size and dates: index {}; disk {}{}\n",
            meta_str(m),
            disk.map_or("unreadable".into(), meta_str),
            match disk {
                Some(x) if same_meta(m, x) => ": same",
                Some(_) => ": DIFFERENT",
                None => "",
            }
        );
        if let Some(Some(c)) = d.get(name) {
            let (files, bytes) = self.total(c);
            o += &format!("folder total: {} in {files} files\n", bytes_str(bytes));
        }
        o
    }

    // (files, bytes) in a folder and everything below it.
    fn total(&self, k: Key) -> (u64, u64) {
        let (mut files, mut bytes) = (0, 0);
        let mut stack = vec![k];
        while let Some(k) = stack.pop() {
            if let Some(d) = self.dirs.get(&k) {
                for e in d.entries().filter(|e| e.kid.is_none()) {
                    files += 1;
                    bytes += e.meta.size;
                }
                stack.extend(d.subdirs());
            }
        }
        (files, bytes)
    }

    // The drive root a directory sits under (itself for a root).
    fn root_of(&self, mut k: Key) -> Key {
        for _ in 0..4096 {
            match self.dirs.get(&k) {
                Some(d) if d.parent != k => k = d.parent,
                _ => break,
            }
        }
        k
    }

    // (folders, entries) under a drive root.
    fn count(&self, root: Key) -> (usize, usize) {
        let (mut dirs, mut entries) = (0, 0);
        let mut stack = vec![root];
        while let Some(k) = stack.pop() {
            if let Some(d) = self.dirs.get(&k) {
                dirs += 1;
                entries += d.len();
                stack.extend(d.subdirs());
            }
        }
        (dirs, entries)
    }

    // One line per drive: state word, then plain-language detail. "polled" drives are re-checked on a timer.
    fn drive_state(m: &Mount) -> (&'static str, String) {
        match (&m.live, m.checking, m.last) {
            (Ok(()), true, _) => ("checking", "live updates; checking it now".into()),
            (Ok(()), false, _) => ("live", "live updates".into()),
            (Err(_), true, _) => ("checking", "no live updates; checking it now".into()),
            (Err(_), false, Some((t, ms))) => (
                "polled",
                format!(
                    "checked every {} min (last {} ago, took {})",
                    m.every / 60,
                    ago(t.elapsed().as_secs()),
                    ago(ms / 1000)
                ),
            ),
            (Err(_), false, None) => ("polled", format!("checked every {} min", m.every / 60)),
        }
    }

    // Why a drive isn't live, in plain words, with the fix where there is one.
    fn why_not_live(m: &Mount) -> String {
        match &m.live {
            Ok(()) => String::new(),
            Err(_) if m.fstype == "fuseblk" => "NTFS-3g (FUSE) drives send no change events. Mount it with the kernel ntfs3 driver for live updates (README: NTFS drives).".into(),
            Err(e) if e == "fanotify unavailable" => format!("Live updates need the daemon to run as root (Start Daemon asks for the password), so this drive is checked every {} minutes.", m.every / 60),
            // Plain words; the raw error goes to the log (NOT LIVE line) and `fa status`.
            Err(_) => {
                let fs = match m.fstype.as_str() {
                    "exfat" => "exFAT",
                    "vfat" | "msdos" | "fat" => "FAT",
                    "nfs" | "nfs4" | "cifs" | "smb3" => "Network",
                    t => t,
                };
                format!("{fs} drives don't report changes, so this one is checked every {} minutes.", m.every / 60)
            }
        }
    }

    // For the window: one tab-separated line per drive: path, fstype, source, state, files, detail, why not live.
    fn drives(&self) -> String {
        let mut o = String::new();
        for m in &self.mounts {
            let (state, detail) = Self::drive_state(m);
            let files = self.count(m.root).1;
            o += &format!(
                "{}\t{}\t{}\t{state}\t{files}\t{detail}\t{}\n",
                m.path.display(),
                m.fstype,
                m.source,
                Self::why_not_live(m)
            );
        }
        for k in &self.snap_roots {
            let name = self
                .dirs
                .get(k)
                .map_or("?".into(), |d| d.name.to_string_lossy().into_owned());
            o += &format!(
                "{name}\t\t\thidden\t{}\tnot mounted: kept hidden until it returns\t\n",
                self.count(*k).1
            );
        }
        o
    }

    fn status(&self, started: Instant) -> String {
        let entries: usize = self.dirs.values().map(|d| d.len()).sum();
        let s = &self.stats;
        let mut o = format!(
            "Find Anything {VERSION}, up {}s, memory {}\n{} entries in {} directories, generation {}\nlog {}\n",
            started.elapsed().as_secs(), rss(), entries, self.dirs.len(), self.generation, self.log.display()
        );
        o += &match fs::metadata(&self.state) {
            Ok(m) => format!(
                "index file {} ({} MB, saved {} ago)\n",
                self.state.display(),
                m.len().div_ceil(1 << 20),
                ago(m
                    .modified()
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map_or(0, |d| d.as_secs()))
            ),
            Err(_) => format!("index file {} (not saved yet)\n", self.state.display()),
        };
        if !self.startup.is_empty() {
            o += &format!("startup: {}\n", self.startup);
        }
        if DEBUG.load(Relaxed) {
            o += "detailed log on\n";
        } else if SUMMARY.load(Relaxed) {
            o += "summary log on\n";
        }
        if PROGRESS.lock().unwrap().is_some() {
            o += &format!("busy: {}\n", progress());
        }
        for m in &self.mounts {
            let state = match (&m.live, Self::drive_state(m)) {
                (Ok(()), (_, d)) => format!("LIVE, {d}"),
                (Err(e), (_, d)) => format!("NOT LIVE ({e}), {d}"),
            };
            o += &format!(
                "  {} [{} {}]: {state}; {}\n",
                m.path.display(),
                m.fstype,
                m.source,
                m.startup
            );
        }
        for k in &self.snap_roots {
            o += &format!(
                "  {} (not mounted): entries kept hidden until it returns\n",
                self.dirs
                    .get(k)
                    .map_or("?".into(), |d| d.name.to_string_lossy())
            );
        }
        o += &format!("change queue: {}\n", self.queue.1);
        o += &format!("changes waiting: {}\n", changes_waiting());
        if let Some((t, ms)) = self.last_full {
            o += &format!(
                "last full re-check: after {}, {} ago, took {}\n",
                too_many(self.queue.0),
                ago(t.elapsed().as_secs()),
                ago(ms / 1000)
            );
        }
        o += &format!(
            "events {} (renames {}), size/date updates {}, parent gone {}, parent not indexed {}, overflows {}, scan errors {}\n",
            s.events, s.renames, s.writes, s.stale_parent, s.unknown_parent, s.overflows, s.scan_errors
        );
        o
    }
}

// Do the index's size and dates match the disk's? A created date the disk doesn't give
// (statx without one, e.g. FUSE) is not a difference.
fn same_meta(index: Meta, disk: Meta) -> bool {
    index.size == disk.size && index.mtime == disk.mtime && (disk.btime == 0 || index.btime == disk.btime)
}

// "1,234 bytes, modified 2026-10-16 14:03:05 UTC, created ..."
fn meta_str(m: Meta) -> String {
    let t = |s: u32| match s {
        0 => "unknown".to_string(),
        s => utc(s as i64),
    };
    format!(
        "{} bytes, modified {}, created {}",
        group(m.size as usize),
        t(m.mtime),
        t(m.btime)
    )
}

// "1.2 GB (1,234,567,890 bytes)"
fn bytes_str(b: u64) -> String {
    let (mut v, mut u) = (b as f64, 0);
    while v >= 1000.0 && u < 4 {
        v /= 1000.0;
        u += 1;
    }
    match u {
        0 => format!("{b} bytes"),
        _ => format!("{v:.1} {} ({} bytes)", ["", "kB", "MB", "GB", "TB"][u], group(b as usize)),
    }
}

// Seconds since 1970 as "2026-10-16 14:03:05 UTC" (civil-from-days, no time zone files needed).
fn utc(s: i64) -> String {
    let (d, r) = (s.div_euclid(86400), s.rem_euclid(86400));
    let z = d + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    format!(
        "{year}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        r / 3600,
        r % 3600 / 60,
        r % 60
    )
}

// "45 s", "3 min", "2 h 5 min"
fn ago(s: u64) -> String {
    match s {
        0..=89 => format!("{s} s"),
        90..=5399 => format!("{} min", (s + 30) / 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

// Filesystems where opening a stale file handle (a folder deleted since the event) is a quiet
// ESTALE. Not ntfs3: any failed handle lookup there marks the volume dirty (fs/ntfs3/inode.c,
// ntfs_iget5), which needs chkdsk and stops it mounting read-write. Not f2fs: a freed node can
// flag the volume for fsck. Everything not listed is resolved by path instead.
const HANDLE_FS: &[&str] = &["ext3", "ext4", "xfs", "btrfs"];
// btrfs handle types (FILEID_BTRFS_*): a subvolume reports its own fsid, which no mount has.
const BTRFS_HANDLES: std::ops::RangeInclusive<i32> = 0x4d..=0x4f;

// The drives an event's file handle may be opened on: only live drives of the same filesystem
// (same fsid), never another one, where the same bytes can name a real but different inode
// (2026-10-07: an ext4 handle opened on ntfs3 marked an NTFS drive dirty). A btrfs handle whose
// fsid no mount has (a subvolume inside a mounted drive) may go to the btrfs drives only.
// Empty: open the folder by path.
fn handle_mounts(mounts: &[Mount], fsid: &[u8], htype: i32) -> Vec<PathBuf> {
    let safe =
        |m: &&Mount| m.live.is_ok() && m.fsid != [0; 8] && HANDLE_FS.contains(&m.fstype.as_str());
    if mounts.iter().any(|m| m.live.is_ok() && m.fsid == fsid) {
        return mounts
            .iter()
            .filter(safe)
            .filter(|m| m.fsid == fsid)
            .map(|m| m.path.clone())
            .collect();
    }
    if BTRFS_HANDLES.contains(&htype) {
        return mounts
            .iter()
            .filter(safe)
            .filter(|m| m.fstype == "btrfs")
            .map(|m| m.path.clone())
            .collect();
    }
    vec![]
}

// fsid + file handle of a directory: the same bytes fanotify puts in its events.
// `fsids` caches st_dev -> fsid (btrfs subvolumes each have their own).
fn dir_id(path: &Path, dev: u64, fsids: &mut HashMap<u64, [u8; 8]>) -> Vec<u8> {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return vec![];
    };
    let fsid = *fsids.entry(dev).or_insert_with(|| {
        let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
        unsafe { libc::statfs(c.as_ptr(), &mut sfs) };
        unsafe { std::mem::transmute(sfs.f_fsid) }
    });
    let mut buf = [0u32; 2 + 32]; // file_handle header + MAX_HANDLE_SZ (128 bytes)
    buf[0] = 128;
    let mut mnt = 0;
    if unsafe {
        libc::name_to_handle_at(
            libc::AT_FDCWD,
            c.as_ptr(),
            buf.as_mut_ptr() as *mut libc::file_handle,
            &mut mnt,
            0,
        )
    } != 0
    {
        return vec![];
    }
    let fh: Vec<u8> = buf
        .iter()
        .flat_map(|w| w.to_ne_bytes())
        .take(8 + buf[0] as usize)
        .collect();
    [&fsid[..], &fh].concat()
}

// A name in a folder listing. `stat`: None if it went before it could be looked at.
struct Listed {
    name: OsString,
    dir: bool, // a folder (a symlink is not)
    stat: Option<Stat>,
}

impl Listed {
    fn meta(&self) -> Meta {
        self.stat.as_ref().map_or(Meta::default(), |s| s.meta)
    }

    // Its key, if it is a folder that can be read in turn. Never another drive's root: a drive
    // mounted after the mount list was read (at boot, the desktop mounts some drives after the
    // service starts) is not in `skip`, and must not be read as part of this one (0.22.1).
    fn kid(&self) -> Option<Key> {
        self.stat
            .as_ref()
            .filter(|s| s.dir && self.dir && !s.mount_root)
            .map(|s| s.key)
    }
}

// A folder's names, each with whether it is a folder, its size and dates. Read-only on the drive:
// opened O_NOATIME, so listing doesn't update the folder's access time (a metadata write, e.g. on
// NTFS); O_NOFOLLOW, so a folder swapped for a symlink is never followed onto another drive.
// A listing that fails part way is an error, never taken as complete (that would drop names), and
// so is a path that no longer leads to folder `k` (moved, or its drive unmounted under it).
fn list_dir(path: &Path, k: Key) -> io::Result<Vec<Listed>> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mut fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_NOATIME) };
    if fd < 0 && errno() == libc::EPERM {
        fd = unsafe { libc::open(c.as_ptr(), flags) }; // O_NOATIME needs the owner or root
    }
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 || (st.st_dev as u64, st.st_ino as u64) != k {
        unsafe { libc::close(fd) };
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let mut out = Vec::new();
    let r = loop {
        unsafe { *libc::__errno_location() = 0 };
        let e = unsafe { libc::readdir64(dir) };
        if e.is_null() {
            break if errno() == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            };
        }
        let e = unsafe { &*e };
        let name = unsafe { std::ffi::CStr::from_ptr(e.d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = OsStr::from_bytes(name);
        let stat = stat_at(fd, name);
        let dir = match (&stat, e.d_type) {
            (Some(s), _) => s.dir,
            (None, t) => t == libc::DT_DIR,
        };
        out.push(Listed {
            name: name.to_owned(),
            dir,
            stat,
        });
    };
    unsafe { libc::closedir(dir) };
    r.map(|_| out)
}

// Lists a directory tree from disk, handing it to `out` in batches (the caller inserts them, so
// the index is only locked per batch). Returns (entries seen, folders that could not be listed).
fn read_tree(
    path: PathBuf,
    k: Key,
    parent: Key,
    name: OsString,
    skip: &HashSet<PathBuf>,
    ex: &Exclude,
    out: &mut dyn FnMut(Vec<(Key, Dir)>),
) -> (usize, u64) {
    let (mut n, mut errs) = (0, 0);
    let mut fsids = HashMap::new();
    let ct = fs::symlink_metadata(&path).map_or(0, |m| ctime_ns(&m));
    let mut stack = vec![(path, k, parent, name, ct)];
    let mut batch = Vec::new();
    while let Some((path, k, parent, name, ctime)) = stack.pop() {
        let id = dir_id(&path, k.0, &mut fsids);
        let mut d = Dir::new(parent, name, id, ctime);
        match list_dir(&path, k) {
            Err(e) => {
                errs += 1;
                debug!("scan {path:?}: {e}");
            }
            Ok(rd) => {
                for l in rd {
                    n += 1;
                    let mut ck = None;
                    if l.dir {
                        let cp = path.join(&l.name);
                        if ex.hit(&l.name, || cp.clone()) {
                            continue;
                        }
                        if !skip.contains(&cp) {
                            ck = l.kid();
                            if let (Some(ck), Some(st)) = (ck, &l.stat) {
                                stack.push((cp, ck, k, l.name.clone(), fresh_ns(st.ctime)));
                            }
                        }
                    }
                    d.set(&l.name, ck, l.meta());
                }
            }
        }
        d.shrink();
        SCANNED.fetch_add(d.len(), Relaxed);
        batch.push((k, d));
        if batch.len() >= 256 {
            out(std::mem::take(&mut batch));
        }
    }
    if !batch.is_empty() {
        out(batch);
    }
    (n, errs)
}

// Reads a tree into the index without holding it during disk reads. Returns entries seen.
fn scan(idx: &Mutex<Index>, path: PathBuf, k: Key, parent: Key, name: OsString) -> usize {
    let (skip, ex) = {
        let ix = idx.lock().unwrap();
        (ix.skip.clone(), ix.exclude.clone())
    };
    let (n, errs) = read_tree(path, k, parent, name, &skip, &ex, &mut |b| {
        idx.lock().unwrap().insert(b)
    });
    idx.lock().unwrap().stats.scan_errors += errs;
    n
}

// ---------- NTFS: names from the MFT (M19) ----------

// Read this drive from its MFT instead of folder by folder? Kernel ntfs3 drives only, not
// removable ones (a stick from anywhere stays on the walk), and not with FA_NO_MFT set.
// Test hook: FA_TEST_MFT=1 also takes ntfs-3g (FUSE) image mounts, without the device and handle checks.
fn mft_wanted(fstype: &str, path: &Path) -> bool {
    (matches!(fstype, "ntfs3" | "ntfs") || fstype.starts_with("fuse") && mft_test())
        && !removable(path)
        && std::env::var_os("FA_NO_MFT").is_none()
}

fn mft_test() -> bool {
    std::env::var_os("FA_TEST_MFT").is_some()
}

// Every folder of the NTFS drive at `path` from the MFT on `dev`, parents first (as `insert`
// needs), skipping mount points and excluded folders as read_tree does. Checked against the
// drive: sampled folders must have the inode numbers and (`handles`) fanotify handles worked out
// here, else Err (the caller walks the folders). Returns the folders, their entries, a log line.
fn mft_tree(
    dev: &Path,
    path: &Path,
    root: Key,
    skip: &HashSet<PathBuf>,
    ex: &Exclude,
    handles: bool,
) -> Result<(Vec<(Key, Dir)>, usize, String), String> {
    if root.1 != 5 {
        return Err(format!(
            "its root folder is inode {}, not MFT record 5",
            root.1
        ));
    }
    let t = Instant::now();
    let m = mft::read(dev, &SCANNED)?;
    let read_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let rec = |r: u64| m.recs.get(r as usize).copied().unwrap_or_default();
    let ok = |r: u64, seq: Option<u16>| {
        let x = rec(r);
        x.flags & mft::USED != 0 && x.flags & mft::EXT == 0 && seq.is_none_or(|s| s == x.seq)
    };
    // Names by parent. Records below 16 are NTFS's own ($MFT, $Extend...), hidden by ntfs3 too.
    let mut kids: HashMap<u64, Vec<(&OsStr, u64)>> = HashMap::new();
    let mut orphans = 0;
    for n in &m.names {
        if n.owner < 16 || n.owner == n.parent || !ok(n.owner, n.owner_seq) {
            continue;
        }
        if !ok(n.parent, Some(n.parent_seq)) || rec(n.parent).flags & mft::DIR == 0 {
            orphans += 1; // its folder is gone or was reused: chkdsk material, not listed
            continue;
        }
        kids.entry(n.parent).or_default().push((&n.name, n.owner));
    }
    // ntfs3's fanotify handle for a folder: FILEID_INO32_GEN (record number, sequence number).
    let fsid = dir_id(path, root.0, &mut HashMap::new())
        .get(..8)
        .map(<[u8]>::to_vec);
    let id = |r: u64| -> Vec<u8> {
        let Some(f) = &fsid else { return vec![] };
        [8u32, 1, r as u32, rec(r).seq as u32]
            .iter()
            .fold(f.clone(), |mut v, w| {
                v.extend(w.to_ne_bytes());
                v
            })
    };
    let (mut out, mut entries, mut sample) = (Vec::new(), 0, Vec::new());
    let mut seen = HashSet::from([5u64]);
    let mut queue = VecDeque::from([(5u64, path.to_path_buf(), root, path.as_os_str().to_owned())]);
    while let Some((r, p, parent, name)) = queue.pop_front() {
        let mut d = Dir::new(parent, name, id(r), fresh_ns(rec(r).ctime));
        for &(n, c) in kids.get(&r).map_or(&[][..], Vec::as_slice) {
            entries += 1;
            let mut ck = None;
            if rec(c).flags & (mft::DIR | mft::LINK) == mft::DIR {
                let cp = p.join(n);
                if ex.hit(n, || cp.clone()) {
                    continue;
                }
                if !skip.contains(&cp) && seen.insert(c) {
                    ck = Some((root.0, c));
                    queue.push_back((c, cp, (root.0, r), n.to_owned()));
                }
            }
            let x = rec(c);
            let meta = Meta {
                size: if x.flags & mft::DIR != 0 { 0 } else { x.size },
                mtime: x.mtime,
                btime: x.btime,
            };
            d.set(n, ck, meta);
        }
        d.shrink();
        if out.len() < 32 || out.len() % 4096 == 0 {
            sample.push((p, out.len()));
        }
        out.push(((root.0, r), d));
    }
    let tree_ms = t.elapsed().as_millis();
    // A folder deleted since the read is skipped; one that is there must match.
    let mut fsids = HashMap::new();
    let mut checked = 0;
    for (p, i) in &sample {
        let (k, d) = &out[*i];
        match dir_key(p) {
            None => continue,
            Some(dk) if dk != *k => {
                return Err(format!(
                    "{p:?} is inode {} on the drive but record {} in the MFT",
                    dk.1, k.1
                ))
            }
            _ => {}
        }
        if handles && dir_id(p, k.0, &mut fsids) != d.id {
            return Err(format!(
                "the fanotify handle of {p:?} is not the one worked out from the MFT"
            ));
        }
        checked += 1;
    }
    if checked == 0 {
        return Err("no folder could be checked against the drive".into());
    }
    let s = &m.stats;
    let secs = (read_ms.max(1) as f64) / 1000.0;
    let line = format!(
        "read the NTFS MFT on {}: {} records ({} in use, {} extension, {} damaged, {} read twice), {} MB in {} pieces at {:.0} MB/s in {read_ms} ms; \
         {} folders, {} entries, {} DOS names and {orphans} orphans skipped; tree in {tree_ms} ms; {checked} folders checked against the drive \
         (record size {}, cluster {})",
        dev.display(), group(s.records as usize), group(s.used as usize), s.ext, s.damaged, s.reread, s.bytes >> 20, s.extents, (s.bytes >> 20) as f64 / secs,
        group(out.len()), group(entries), s.dos, s.rec_size, s.cluster
    );
    Ok((out, entries, line))
}

// Reads a registered ntfs3 drive from its MFT into the index. None: not an MFT drive (walk it).
// Some(Err): the MFT can't be used (the caller walks the drive and logs why).
fn mft_fill(idx: &Mutex<Index>, path: &Path, root: Key) -> Option<Result<String, String>> {
    let (src, skip, ex) = {
        let ix = idx.lock().unwrap();
        let m = ix.mounts.iter().find(|m| m.root == root)?;
        if !mft_wanted(&m.fstype, path) {
            return None;
        }
        (m.source.clone(), ix.skip.clone(), ix.exclude.clone())
    };
    Some((|| {
        // Only the very device this drive is mounted from.
        let dev = PathBuf::from(&src);
        match fs::metadata(&dev) {
            Ok(md) if md.file_type().is_block_device() && md.rdev() == root.0 || mft_test() => {}
            _ => return Err(format!("{src} is not the device of this drive")),
        }
        debug!("memory before reading the MFT of {path:?}: {}", mem());
        // The kernel's pending changes go to the disk first (what it does every few seconds
        // anyway), so the table read is current; changes after this come in as events.
        if let Ok(d) = fs::File::open(path) {
            unsafe { libc::syncfs(d.as_raw_fd()) };
        }
        let (tree, entries, line) = mft_tree(&dev, path, root, &skip, &ex, !mft_test())?;
        let (t, dirs) = (Instant::now(), tree.len());
        idx.lock().unwrap().insert(tree);
        info!(
            "{path:?}: {line}; put in the index in {} ms",
            t.elapsed().as_millis()
        );
        debug!("memory after reading the MFT of {path:?}: {}", mem());
        Ok(format!(
            "read from the MFT: {entries} entries in {dirs} folders"
        ))
    })())
}

// `fa mftcheck PATH [--device DEV]`: reads an NTFS drive both ways, from its MFT and folder by
// folder, and prints the time each took and every difference. Changes nothing. Exit 2 if they differ.
fn mft_check(args: &[String]) {
    let (mut dev, mut path) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => dev = it.next().map(PathBuf::from),
            p => path = Some(PathBuf::from(p)),
        }
    }
    let Some(path) = path.map(|p| fs::canonicalize(&p).unwrap_or(p)) else {
        return println!("usage: sudo fa mftcheck PATH [--device DEV]");
    };
    let Some(root) = dir_key(&path) else {
        return println!("{} is not a folder", path.display());
    };
    let all = mountinfo();
    let Some(dev) = dev.or_else(|| {
        all.iter()
            .find(|m| m.path == path)
            .map(|m| PathBuf::from(&m.source))
    }) else {
        return println!("{} is not a mount point: give --device", path.display());
    };
    let skip: HashSet<PathBuf> = all.iter().map(|m| m.path.clone()).collect();
    let ex = Exclude::default();
    let t = Instant::now();
    let (mft, entries, line) = match mft_tree(&dev, &path, root, &skip, &ex, false) {
        Ok(x) => x,
        Err(e) => {
            println!("MFT not usable: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "MFT: {line}\nMFT: {entries} entries in {} folders, {} ms in all",
        mft.len(),
        t.elapsed().as_millis()
    );
    let t = Instant::now();
    let mut walk = Vec::new();
    let (n, errs) = read_tree(
        path.clone(),
        root,
        root,
        path.clone().into_os_string(),
        &skip,
        &ex,
        &mut |b| walk.extend(b),
    );
    println!(
        "walk: {n} entries in {} folders ({errs} could not be listed), {} ms",
        walk.len(),
        t.elapsed().as_millis()
    );
    let (a, b): (HashMap<Key, Dir>, HashMap<Key, Dir>) =
        (mft.into_iter().collect(), walk.into_iter().collect());
    let at = |k: Key| {
        let (mut parts, mut k) = (Vec::new(), k);
        while let Some(d) = b.get(&k).or(a.get(&k)) {
            parts.push(d.name.clone());
            if d.parent == k || parts.len() > 4096 {
                break;
            }
            k = d.parent;
        }
        parts.iter().rev().collect::<PathBuf>()
    };
    let (mut diffs, mut ctimes, mut ids) = (Vec::new(), 0, 0);
    let (mut metas, mut meta_diffs) = ([0usize; 3], Vec::new()); // size, modified, created
    for (k, wd) in &b {
        let Some(md) = a.get(k) else {
            diffs.push(format!("folder only in the walk: {}", at(*k).display()));
            continue;
        };
        for e in wd.entries() {
            let name = OsStr::from_bytes(e.name);
            match md.get(name) {
                None => diffs.push(format!("only in the walk: {}", at(*k).join(name).display())),
                Some(mc) if mc != e.kid => diffs.push(format!(
                    "{}: walk {:?}, MFT {mc:?}",
                    at(*k).join(name).display(),
                    e.kid
                )),
                _ => {
                    let m = md.meta(name).unwrap_or_default();
                    let w = e.meta;
                    // A created time the walk can't see is not a difference (as in same_meta).
                    let d = [w.size != m.size, w.mtime != m.mtime, w.btime != 0 && w.btime != m.btime];
                    for (n, x) in metas.iter_mut().zip(d) {
                        *n += x as usize;
                    }
                    if d.contains(&true) && meta_diffs.len() < 20 {
                        meta_diffs.push(format!("{}: walk {w:?}, MFT {m:?}", at(*k).join(name).display()));
                    }
                }
            }
        }
        for e in md.entries().filter(|e| wd.get(OsStr::from_bytes(e.name)).is_none()) {
            diffs.push(format!(
                "only in the MFT: {}",
                at(*k).join(OsStr::from_bytes(e.name)).display()
            ));
        }
        ctimes += (wd.ctime != 0 && md.ctime != 0 && wd.ctime != md.ctime) as usize;
        ids += (wd.id != md.id) as usize;
    }
    for k in a.keys().filter(|k| !b.contains_key(k)) {
        diffs.push(format!("folder only in the MFT: {}", at(*k).display()));
    }
    // ntfs-3g (FUSE) has handles of its own: only ntfs3's must match.
    println!("folder ctimes that differ: {ctimes}; fanotify handles that differ: {ids} (must be 0 on ntfs3)");
    println!(
        "entries whose size differs: {}, modified time: {}, created time: {} (should all be 0, apart from files written during the check)",
        metas[0], metas[1], metas[2]
    );
    for d in &meta_diffs {
        println!("  {d}");
    }
    for d in diffs.iter().take(40) {
        println!("  {d}");
    }
    if diffs.is_empty() {
        println!("same names and folders both ways");
    } else {
        println!("{} differences", diffs.len());
        std::process::exit(2);
    }
}

// Brings a tree up to date with the disk without re-reading all of it. Any create, delete or
// rename in a directory bumps its ctime, and ctime can't be set back, so a directory whose
// ctime matches the one taken before its listing still has the same names. Only the others
// are re-listed. Disk reads happen outside the index lock; each folder's result is applied
// under it, so searches keep working while a slow drive is checked. A live drive's caller holds
// WRITER (no event can change the tree in between). Returns (directories checked, re-listed).
fn reconcile(idx: &Mutex<Index>, path: PathBuf, k: Key) -> (usize, usize) {
    debug!("memory before checking {path:?}: {}", mem());
    let r = reconcile_dir(idx, path.clone(), k);
    debug!("memory after checking {path:?}: {}", mem());
    r
}

fn reconcile_dir(idx: &Mutex<Index>, path: PathBuf, k: Key) -> (usize, usize) {
    let (skip, ex) = {
        let ix = idx.lock().unwrap();
        (ix.skip.clone(), ix.exclude.clone())
    };
    let (mut checked, mut relisted) = (0, 0);
    let mut stack = vec![(path, k)];
    while let Some((path, k)) = stack.pop() {
        let known = idx.lock().unwrap().dirs.get(&k).map(|d| {
            (
                d.ctime,
                d.entries()
                    .filter_map(|e| e.kid.map(|c| (path.join(OsStr::from_bytes(e.name)), c)))
                    .collect::<Vec<_>>(),
            )
        });
        let Some((dct, subs)) = known else { continue };
        checked += 1;
        CHECKED.fetch_add(1, Relaxed);
        let me = stat_at(libc::AT_FDCWD, path.as_os_str());
        let ct = me.as_ref().map_or(0, |m| fresh_ns(m.ctime));
        // A kept subfolder that is now another drive's root (a saved index from 0.22.0 or older
        // can hold a drive read as part of this one) fails this, so the folder is re-listed and
        // that drive dropped from this tree.
        let same = |(p, c): &(PathBuf, Key)| {
            stat_at(libc::AT_FDCWD, p.as_os_str()).is_some_and(|s| s.dir && s.key == *c && !s.mount_root)
        };
        if ct != 0 && ct == dct && subs.iter().all(same) {
            stack.extend(subs);
            continue;
        }
        relisted += 1;
        let rd = match list_dir(&path, k) {
            Ok(rd) => rd,
            Err(e) => {
                idx.lock().unwrap().stats.scan_errors += 1;
                debug!("reconcile {path:?}: cannot list: {e}; kept as it was");
                continue;
            }
        };
        debug!("re-listing {path:?}");
        let mut fresh = Dir::new(k, OsString::new(), vec![], 0);
        for l in rd {
            let cp = path.join(&l.name);
            if l.dir && ex.hit(&l.name, || cp.clone()) {
                continue;
            }
            let kid = if skip.contains(&cp) { None } else { l.kid() };
            fresh.set(&l.name, kid, l.meta());
        }
        fresh.shrink();
        SCANNED.fetch_add(fresh.len(), Relaxed);
        let keep: HashSet<Key> = fresh.subdirs().collect();
        let mut new = Vec::new(); // subfolders not in the index yet: read after unlocking
        let mut ix = idx.lock().unwrap();
        let Some(d) = ix.dirs.get_mut(&k) else {
            continue;
        };
        d.swap_entries(&mut fresh); // `fresh` now holds the old entries
        for o in fresh.subdirs() {
            // Gone from here, unless an earlier step already found it under its new parent.
            if !keep.contains(&o)
                && ix.dirs.get(&o).is_some_and(|d| d.parent == k)
                && !ix.mounts.iter().any(|m| m.root == o)
            {
                ix.remove_dir(o);
            }
        }
        let kids: Vec<(OsString, Key)> = ix.dirs[&k]
            .entries()
            .filter_map(|e| Some((OsStr::from_bytes(e.name).to_owned(), e.kid?)))
            .collect();
        for (name, ck) in kids {
            let cp = path.join(&name);
            if let Some(d) = ix.dirs.get_mut(&ck).filter(|d| d.parent == k) {
                d.name = name; // same dir (maybe renamed): check it in turn
                stack.push((cp, ck));
            } else {
                if ix.dirs.contains_key(&ck) {
                    ix.remove_dir(ck); // moved here from elsewhere: re-read it
                }
                new.push((cp, ck, name));
            }
        }
        let d = ix.dirs.get_mut(&k).unwrap();
        d.ctime = ct;
        let (p, n) = (d.parent, d.name.clone());
        // Its own modified date changed with its names: as listed in its parent.
        if let (Some(m), Some(pd)) = (&me, ix.dirs.get_mut(&p).filter(|_| p != k)) {
            if m.key == k && pd.get(&n) == Some(Some(k)) {
                pd.set(&n, Some(k), m.meta);
            }
        }
        ix.generation += 1;
        drop(ix);
        for (cp, ck, name) in new {
            scan(idx, cp, ck, k, name);
        }
    }
    (checked, relisted)
}

// Reads (or, from the snapshot, checks) a drive `register`ed earlier, then marks it ready.
// A live drive's caller holds WRITER.
fn fill(idx: &Mutex<Index>, path: PathBuf, root: Key, snap: bool) {
    let t = Instant::now();
    let how = if snap {
        // NTFS on ntfs3: one MFT read replaces the saved tree (M19b), far quicker than a stat per folder.
        match mft_fill(idx, &path, root) {
            Some(Ok(how)) => format!("snapshot replaced, {how}"),
            r => {
                if let Some(Err(e)) = r {
                    warn!("{path:?}: MFT not used for the startup check ({e}); checking folder by folder");
                }
                let (c, r) = reconcile(idx, path.clone(), root);
                format!("snapshot checked ({c} folders, {r} re-listed)")
            }
        }
    } else {
        match mft_fill(idx, &path, root) {
            Some(Ok(how)) => how,
            r => {
                if let Some(Err(e)) = r {
                    warn!("{path:?}: MFT not used ({e}); reading it folder by folder");
                }
                format!(
                    "scanned {} entries",
                    scan(idx, path.clone(), root, root, path.clone().into_os_string())
                )
            }
        }
    };
    let ms = t.elapsed().as_millis() as u64;
    let mut ix = idx.lock().unwrap();
    let Some(m) = ix.mounts.iter_mut().find(|m| m.root == root) else {
        return;
    };
    m.startup = format!("{how} in {ms} ms");
    m.checking = false;
    let what = format!(
        "{path:?} [{} {}, fsid {}]",
        m.fstype,
        m.source,
        hex(&m.fsid)
    );
    match &m.live {
        Ok(()) => info!("{what}: LIVE, {}", m.startup),
        Err(e) => {
            m.last = Some((Instant::now(), ms));
            m.every = poll_every(ms);
            warn!(
                "{what}: NOT LIVE ({e}), {}; re-checked every {} min",
                m.startup,
                m.every / 60
            );
        }
    }
}

// Seconds between timed re-checks of a drive whose last check took `ms`: ten times as long as a
// check, between 5 and 60 minutes, so a slow drive isn't busy all the time.
// FA_TEST_POLL_SECS overrides it for tests.
fn poll_every(ms: u64) -> u64 {
    if let Some(s) = std::env::var("FA_TEST_POLL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return s;
    }
    (ms / 100).clamp(300, 3600).div_ceil(60) * 60
}

// Drives without live events (NTFS-3g and other FUSE) are re-checked on a timer, the same quick
// way as at startup: only folders whose ctime moved are re-read. Low CPU and idle I/O priority.
// ponytail: FUSE reads are served by the FUSE process (ntfs-3g), so its own priority still applies.
fn poll_loop(idx: Arc<Mutex<Index>>) {
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 19);
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13); // IOPRIO_WHO_PROCESS, this thread, IOPRIO_CLASS_IDLE
    }
    let tick = poll_every(0).min(30);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(tick));
        let due: Vec<(PathBuf, Key)> = {
            let mut ix = idx.lock().unwrap();
            ix.mounts
                .iter_mut()
                .filter(|m| {
                    m.live.is_err()
                        && !m.checking
                        && m.last
                            .is_some_and(|(t, _)| t.elapsed().as_secs() >= m.every)
                })
                .map(|m| {
                    m.checking = true;
                    (m.path.clone(), m.root)
                })
                .collect()
        };
        for (p, k) in due {
            let t = Instant::now();
            let (c, r) = reconcile(&idx, p.clone(), k);
            let ms = t.elapsed().as_millis() as u64;
            let mut ix = idx.lock().unwrap();
            if let Some(m) = ix.mounts.iter_mut().find(|m| m.root == k) {
                m.checking = false;
                m.last = Some((Instant::now(), ms));
                m.every = poll_every(ms);
                info!(
                    "re-checked {p:?}: {c} folders, {r} re-listed in {ms} ms; next in {} min",
                    m.every / 60
                );
            }
        }
    }
}

// Re-reads every folder of every mounted drive (and forgets drives that aren't mounted), with
// search working throughout, then saves. Started by a client's "rebuild".
fn rebuild(idx: &Mutex<Index>) {
    let t = Instant::now();
    let drives: Vec<(PathBuf, Key, bool)> = {
        let mut ix = idx.lock().unwrap();
        for r in std::mem::take(&mut ix.snap_roots) {
            ix.remove_dir(r);
        }
        for d in ix.dirs.values_mut() {
            d.ctime = 0; // forces a re-list
        }
        ix.mounts
            .iter()
            .map(|m| (m.path.clone(), m.root, m.live.is_ok()))
            .collect()
    };
    let total = drives.len();
    for (i, (p, k, live)) in drives.into_iter().enumerate() {
        set_progress(format!(
            "rebuilding {} ({} of {total} drives)",
            p.display(),
            i + 1
        ));
        let _w = live.then(|| WRITER.lock().unwrap());
        let how = match mft_fill(idx, &p, k) {
            Some(Ok(how)) => how,
            r => {
                if let Some(Err(e)) = r {
                    warn!("{p:?}: MFT not used ({e}); re-reading it folder by folder");
                }
                format!("{} folders re-read", reconcile(idx, p.clone(), k).0)
            }
        };
        info!("rebuilt {p:?}: {how}");
    }
    *PROGRESS.lock().unwrap() = None;
    info!("rebuild done in {} s", t.elapsed().as_secs());
    idx.lock().unwrap().save();
}

fn set_progress(what: String) {
    SCANNED.store(0, Relaxed);
    CHECKED.store(0, Relaxed);
    EXPECTED.store(0, Relaxed);
    *PROGRESS.lock().unwrap() = Some((what, Instant::now(), true));
}

#[derive(Debug)]
struct Stat {
    dir: bool,
    key: Key,
    meta: Meta,
    ctime: i64,       // ns
    mount_root: bool, // the root folder of a mount (another drive, or a bind mount)
}

const STATX_ATTR_MOUNT_ROOT: u64 = 0x2000; // linux/stat.h (Linux 5.8); not in libc 0.2.189

// What the filesystem says about `name` in folder `dfd` ("" = the folder itself). Links are not
// followed and automounts not set off.
fn stat_at(dfd: RawFd, name: &OsStr) -> Option<Stat> {
    let c = CString::new(name.as_bytes()).ok()?;
    let mut x: libc::statx = unsafe { std::mem::zeroed() };
    let mut flags = libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT;
    if name.is_empty() {
        flags |= libc::AT_EMPTY_PATH;
    }
    let want = libc::STATX_TYPE
        | libc::STATX_INO
        | libc::STATX_SIZE
        | libc::STATX_MTIME
        | libc::STATX_CTIME
        | libc::STATX_BTIME;
    if unsafe { libc::statx(dfd, c.as_ptr(), flags, want, &mut x) } != 0 {
        return None;
    }
    let kind = x.stx_mode as u32 & libc::S_IFMT;
    let dir = kind == libc::S_IFDIR;
    Some(Stat {
        dir,
        key: (libc::makedev(x.stx_dev_major, x.stx_dev_minor), x.stx_ino),
        meta: Meta {
            // A folder's own size means nothing to a person, nor does a link's (its target's length).
            size: if dir || kind == libc::S_IFLNK { 0 } else { x.stx_size },
            mtime: dir::secs(x.stx_mtime.tv_sec),
            btime: if x.stx_mask & libc::STATX_BTIME != 0 {
                dir::secs(x.stx_btime.tv_sec)
            } else {
                0
            },
        },
        ctime: x.stx_ctime.tv_sec * 1_000_000_000 + x.stx_ctime.tv_nsec as i64,
        mount_root: x.stx_attributes_mask & x.stx_attributes & STATX_ATTR_MOUNT_ROOT != 0,
    })
}

// The filesystem id of the drive holding `p` (statfs f_fsid; from the UUID on ext4, btrfs, NTFS).
fn fs_id(p: &Path) -> Option<[u8; 8]> {
    let c = CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(c.as_ptr(), &mut sfs) } == 0)
        .then(|| unsafe { std::mem::transmute(sfs.f_fsid) })
}

// "259:2", as lsblk and /proc/self/mountinfo write a device number.
fn devname(d: u64) -> String {
    format!("{}:{}", libc::major(d), libc::minor(d))
}

fn dir_key(p: &Path) -> Option<Key> {
    fs::symlink_metadata(p)
        .ok()
        .filter(|m| m.is_dir())
        .map(|m| (m.dev(), m.ino()))
}

// A directory's ctime in ns, read just before listing it. 0 (= re-list next time) while it is
// under 3 s old: a change in the same clock tick (2 s on FAT) would not move it.
fn ctime_ns(m: &fs::Metadata) -> i64 {
    fresh_ns(m.ctime() * 1_000_000_000 + m.ctime_nsec())
}

// A ctime in ns, or 0 if it is under 3 s old (see ctime_ns).
fn fresh_ns(ns: i64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    if now - ns.div_euclid(1_000_000_000) < 3 {
        0
    } else {
        ns
    }
}

const SNAP_MAGIC: &[u8; 8] = b"SEIDX\0\0\x02"; // 2: packed folders with size and dates (0.16.0)
fn w64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn wkey(w: &mut impl Write, k: Key) -> io::Result<()> {
    w64(w, k.0)?;
    w64(w, k.1)
}
fn wbytes(w: &mut impl Write, b: &[u8]) -> io::Result<()> {
    let n = u16::try_from(b.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name too long"))?;
    w.write_all(&n.to_le_bytes())?;
    w.write_all(b)
}
fn r64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn rkey(r: &mut impl Read) -> io::Result<Key> {
    Ok((r64(r)?, r64(r)?))
}
fn rbytes(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut n = [0u8; 2];
    r.read_exact(&mut n)?;
    let mut v = vec![0u8; u16::from_le_bytes(n) as usize];
    r.read_exact(&mut v)?;
    Ok(v)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn rss() -> String {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .map(|l| l[6..].trim().to_string())
        })
        .unwrap_or_default()
}

// Memory in one phrase for the detailed log: process size and peak, and what the allocator holds
// (in use by the daemon / free but not given back to the system), in MB.
fn mem() -> String {
    let mi = unsafe { libc::mallinfo2() };
    let hwm = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .map(|l| l[6..].trim().to_string())
        })
        .unwrap_or_default();
    format!(
        "rss {}, peak {hwm}, allocator in use {} MB, free but held {} MB",
        rss(),
        mi.uordblks >> 20,
        mi.fordblks >> 20
    )
}

// Once a minute (Detailed Log on): memory, index size, events handled, changes waiting in the kernel.
fn memory_monitor(idx: Arc<Mutex<Index>>, fan_fd: Option<RawFd>) {
    loop {
        std::thread::sleep(Duration::from_secs(60));
        let (det, sum) = (DEBUG.load(Relaxed), SUMMARY.load(Relaxed));
        if !det && !sum {
            continue;
        }
        let mut waiting = 0i32;
        if let Some(fd) = fan_fd {
            unsafe { libc::ioctl(fd, libc::FIONREAD, &mut waiting) };
        }
        let ix = idx.lock().unwrap();
        let entries: usize = ix.dirs.values().map(|d| d.len()).sum();
        if det {
            debug!("memory: {}; {entries} entries in {} directories; events {}; kernel queue {} KB waiting", mem(), ix.dirs.len(), ix.stats.events, waiting / 1024);
        }
        // Summary: one small totals file, rewritten in place, so it never grows (kept next to the log).
        let st = &ix.stats;
        let totals = format!(
            "Find Anything {VERSION} totals, daemon up {} s\nmemory: {}\nentries: {entries} in {} directories\nevents: {} (create {}, delete {}, rename {}), size/date updates {}\nstale parents: {}, unknown parents: {}\nqueue overflows: {}, scan errors: {}\nkernel queue waiting: {} KB\nslowest event batch: {} ms\n",
            START.get_or_init(Instant::now).elapsed().as_secs(),
            mem(), ix.dirs.len(), st.events, st.creates, st.deletes, st.renames, st.writes,
            st.stale_parent, st.unknown_parent, st.overflows, st.scan_errors, waiting / 1024, st.slowest_ms
        );
        drop(ix);
        if sum {
            if let Some(p) = LOG_PATH.lock().unwrap().as_ref() {
                let mut t = p.clone().into_os_string();
                t.push(".totals");
                let t = PathBuf::from(t);
                if fs::write(&t, totals).is_ok() {
                    sudo_owner(&t); // the daemon is root; the file belongs to the user
                }
            }
        }
    }
}

fn mask_names(m: u64) -> String {
    let names = [
        (libc::FAN_CREATE, "CREATE"),
        (libc::FAN_DELETE, "DELETE"),
        (libc::FAN_RENAME, "RENAME"),
        (libc::FAN_MOVED_FROM, "MOVED_FROM"),
        (libc::FAN_MOVED_TO, "MOVED_TO"),
        (libc::FAN_ONDIR, "ONDIR"),
        (libc::FAN_CLOSE_WRITE, "CLOSE_WRITE"),
        (libc::FAN_ATTRIB, "ATTRIB"),
        (libc::FAN_Q_OVERFLOW, "OVERFLOW"),
    ];
    let v: Vec<&str> = names
        .iter()
        .filter(|(b, _)| m & b != 0)
        .map(|(_, n)| *n)
        .collect();
    format!("{} (0x{m:x})", v.join("|"))
}

// ---------- fanotify event parsing ----------

// Events that add, remove or move names (the rest only change size and dates).
const NAME_EVENTS: u64 = libc::FAN_CREATE | libc::FAN_DELETE | libc::FAN_RENAME;

struct Rec<'a> {
    kind: u8,
    id: &'a [u8], // fsid(8) + struct file_handle (handle_bytes, handle_type, f_handle[])
    name: &'a OsStr,
}

// Info records after the event metadata: header(4) fsid(8) file_handle(8+n) name\0.
fn parse_records(ev: &[u8], meta_len: usize) -> Vec<Rec<'_>> {
    let mut out = Vec::new();
    let mut off = meta_len;
    while off + 4 <= ev.len() {
        let kind = ev[off];
        let len = u16::from_ne_bytes([ev[off + 2], ev[off + 3]]) as usize;
        if len < 4 || off + len > ev.len() {
            break;
        }
        let r = &ev[off..off + len];
        if matches!(
            kind,
            libc::FAN_EVENT_INFO_TYPE_DFID_NAME
                | libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME
                | libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME
        ) && r.len() >= 20
        {
            let hb = u32::from_ne_bytes(r[12..16].try_into().unwrap()) as usize;
            if 20 + hb <= r.len() {
                let rest = &r[20 + hb..];
                let name = &rest[..rest.iter().position(|&b| b == 0).unwrap_or(rest.len())];
                out.push(Rec {
                    kind,
                    id: &r[4..20 + hb],
                    name: OsStr::from_bytes(name),
                });
            }
        } else {
            debug!("  skipped info record type {kind}");
        }
        off += len;
    }
    out
}

fn event_loop(fan: OwnedFd, idx: Arc<Mutex<Index>>) {
    let mut buf = vec![0u8; 256 * 1024];
    let meta = std::mem::size_of::<libc::fanotify_event_metadata>();
    // Test hook: events are not read while this file exists, so a flood overflows the queue.
    let pause = std::env::var_os("FA_TEST_PAUSE_FILE").map(PathBuf::from);
    let mut tries: HashMap<Key, u8> = HashMap::new(); // re-checks put off: folder not at its path yet
    loop {
        while pause.as_ref().is_some_and(|p| p.exists()) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let n = unsafe { libc::read(fan.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            let e = errno();
            if e == libc::EINTR {
                continue;
            }
            warn!(
                "reading fanotify events failed: {}; live updates stopped",
                errstr(e)
            );
            return;
        }
        let n = n as usize;
        let _w = WRITER.lock().unwrap();
        let mut ix = idx.lock().unwrap();
        let mut overflow = false;
        let mut off = 0;
        while off + meta <= n {
            let m: libc::fanotify_event_metadata =
                unsafe { std::ptr::read_unaligned(buf[off..].as_ptr() as *const _) };
            let len = m.event_len as usize;
            if len < meta || off + len > n {
                warn!("malformed fanotify event (len {len}), skipping rest of batch");
                break;
            }
            if m.fd >= 0 {
                unsafe { libc::close(m.fd) }; // not expected with FID reporting
            }
            EVENT_BYTES.fetch_add(len as u64, Relaxed);
            EVENT_COUNT.fetch_add(1, Relaxed);
            let recs = parse_records(&buf[off..off + len], m.metadata_len as usize);
            overflow |= ix.handle_event(m.mask, &recs);
            off += len;
        }
        ix.reap_orphans();
        // New folders, and folders an event couldn't open by path, at their path now: listed
        // (ctime 0) with their subfolders checked or read, as at startup. Outside the index lock,
        // so searches go on; WRITER is still held, so events wait (queued) until this is done.
        let recheck: Vec<(PathBuf, Key)> = std::mem::take(&mut ix.recheck)
            .into_iter()
            .filter_map(|k| Some((ix.path_of(k)?, k)))
            .collect();
        if !overflow && !recheck.is_empty() {
            drop(ix);
            for (p, k) in recheck {
                if dir_key(&p) != Some(k) {
                    // Moved, with the rename event still queued (a later batch): try again after
                    // the next batches. Deleted or replaced: its parent's event drops it.
                    let n = tries.entry(k).or_insert(0);
                    *n += 1;
                    if *n <= 10 {
                        debug!("{p:?} not there yet: re-checked after the next batch");
                        idx.lock().unwrap().recheck.insert(k);
                    } else {
                        tries.remove(&k);
                    }
                    continue;
                }
                tries.remove(&k);
                if let Some(d) = idx.lock().unwrap().dirs.get_mut(&k) {
                    d.ctime = 0;
                }
                let (c, r) = reconcile(&idx, p.clone(), k);
                debug!("read {p:?} after the batch: {c} folders, {r} re-listed");
            }
            continue;
        }
        if overflow {
            let live: Vec<(PathBuf, Key)> = ix
                .mounts
                .iter()
                .filter(|m| m.live.is_ok())
                .map(|m| (m.path.clone(), m.root))
                .collect();
            let what = too_many(ix.queue.0);
            let again = ix
                .last_full
                .is_some_and(|(t, _)| t.elapsed().as_secs() < 600);
            drop(ix);
            // The status bar says what is going on, unless a startup check or rebuild already does.
            let mine = PROGRESS.lock().unwrap().is_none();
            if again {
                // Still flooding: re-checking non-stop would keep the drives busy for nothing. Events
                // meanwhile wait in the (capped) queue, or are dropped: the re-check covers them.
                let wait = backoff_secs();
                warn!("{what} again within 10 min of the last full re-check: waiting {} before the next", ago(wait));
                if mine {
                    *PROGRESS.lock().unwrap() = Some((format!("{what} again: re-checking drives in {}; some results may be out of date", ago(wait)), Instant::now(), false));
                }
                std::thread::sleep(std::time::Duration::from_secs(wait));
            }
            warn!("fanotify queue overflow ({what}): events were lost, checking every live drive");
            let t0 = Instant::now();
            let n = live.len();
            for (i, (p, k)) in live.into_iter().enumerate() {
                if mine {
                    set_progress(format!("{what}: re-checking {} ({} of {n} drives); some results may be out of date", p.display(), i + 1));
                }
                let t = Instant::now();
                let (c, r) = reconcile(&idx, p.clone(), k);
                info!(
                    "reconciled {p:?}: {c} folders checked, {r} re-listed in {} ms",
                    t.elapsed().as_millis()
                );
            }
            if mine {
                *PROGRESS.lock().unwrap() = None;
            }
            idx.lock().unwrap().last_full = Some((Instant::now(), t0.elapsed().as_millis() as u64));
        }
    }
}

// ---------- mounts ----------

struct MountInfo {
    root: OsString,
    path: PathBuf,
    fstype: String,
    source: String,
}

fn unescape(s: &str) -> OsString {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    OsString::from_vec(out)
}

fn mountinfo() -> Vec<MountInfo> {
    let s = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    s.lines()
        .filter_map(|l| {
            let (a, b) = l.split_once(" - ")?;
            let f: Vec<&str> = a.split(' ').collect();
            let g: Vec<&str> = b.split(' ').collect();
            Some(MountInfo {
                root: unescape(f.get(3)?),
                path: unescape(f.get(4)?).into(),
                fstype: g.first()?.to_string(),
                source: g.get(1).unwrap_or(&"").to_string(),
            })
        })
        .collect()
}

// USB keys and SD cards (mounted by the desktop under /run/media or /media): forgotten when
// unmounted, read afresh when plugged in again.
fn removable(p: &Path) -> bool {
    p.starts_with("/run/media") || p.starts_with("/media")
}

// Real filesystems, except /boot and /boot/efi (only the boot loader and kernels live there).
fn wanted_mounts(all: &[MountInfo], fstypes: &[String]) -> Vec<(PathBuf, String, String)> {
    all.iter()
        .filter(|m| fstypes.contains(&m.fstype) && !m.path.starts_with("/boot"))
        .map(|m| (m.path.clone(), m.fstype.clone(), m.source.clone()))
        .collect()
}

// Follows mounts and unmounts: the kernel flags /proc/self/mountinfo with POLLPRI on every change.
// The first pass also catches anything mounted or unmounted during the startup scan.
fn watch_mounts(idx: Arc<Mutex<Index>>, fan: Option<RawFd>, fstypes: Vec<String>) {
    let Ok(f) = fs::File::open("/proc/self/mountinfo") else {
        return warn!("cannot watch mounts");
    };
    loop {
        let all = mountinfo();
        let want = wanted_mounts(&all, &fstypes);
        let mut new = Vec::new();
        {
            let mut ix = idx.lock().unwrap();
            ix.skip = all.iter().map(|m| m.path.clone()).collect();
            let gone: Vec<PathBuf> = ix
                .mounts
                .iter()
                .filter(|m| !want.iter().any(|w| w.0 == m.path))
                .map(|m| m.path.clone())
                .collect();
            for p in gone {
                ix.remove_mount(&p);
            }
            for (p, t, s) in want {
                if !ix.mounts.iter().any(|m| m.path == p) {
                    info!("new mount {p:?} [{t} {s}]");
                    if let Some((root, snap)) = ix.register(p.clone(), t, s, fan) {
                        let live = ix.mounts.last().is_some_and(|m| m.live.is_ok());
                        new.push((p, root, snap, live));
                    }
                }
            }
        }
        // ponytail: a drive unmounted and mounted again while a timed re-check of it runs gets
        // checked twice at once; both converge on what's on disk.
        for (p, root, snap, live) in new {
            let _w = live.then(|| WRITER.lock().unwrap());
            fill(&idx, p, root, snap);
        }
        let mut p = libc::pollfd {
            fd: f.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        while unsafe { libc::poll(&mut p, 1, -1) } < 0 {
            if errno() != libc::EINTR {
                return warn!("watching mounts failed: {}", errstr(errno()));
            }
        }
    }
}

// ---------- commands ----------

// Hands a file to --owner, else the user who started us via sudo or pkexec (the GUI's "Start daemon").
fn sudo_owner(p: &Path) {
    let id = |v| std::env::var(v).ok().and_then(|s| s.parse::<u32>().ok());
    let uid = OWNER.get().map(|o| o.0).or(id("SUDO_UID")).or(id("PKEXEC_UID"));
    let gid = OWNER.get().map(|o| o.1).or(id("SUDO_GID"))
        .or_else(|| uid.and_then(|u| unsafe { libc::getpwuid(u).as_ref() }.map(|pw| pw.pw_gid)));
    if let (Some(u), Some(g)) = (uid, gid) {
        if let Ok(c) = CString::new(p.as_os_str().as_bytes()) {
            unsafe { libc::chown(c.as_ptr(), u, g) };
        }
    }
}

fn group(n: usize) -> String {
    let s = n.to_string();
    s.as_bytes()
        .rchunks(3)
        .rev()
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join(",")
}

// "checking /mnt/x (6 of 7 drives), 123,456 folders checked, 42 s on this drive"
fn progress() -> String {
    let done = match (CHECKED.load(Relaxed), SCANNED.load(Relaxed)) {
        (0, f) => match EXPECTED.load(Relaxed) {
            0 => format!("{} files read", group(f)),
            e => format!("{} of about {} files read", group(f), group(e)),
        },
        (c, _) => format!("{} folders checked", group(c)),
    };
    match &*PROGRESS.lock().unwrap() {
        Some((what, t, true)) => {
            format!("{what}, {done}, {} s on this drive", t.elapsed().as_secs())
        }
        Some((what, _, false)) => what.clone(),
        None => "indexing: reading the drive list".into(),
    }
}

// One daemon per socket: an flock'd `<socket>.lock` (freed by the kernel when the daemon dies),
// then stop any leftover `fa daemon` on the same socket (older builds had no lock).
fn single_instance(sock: &Path) -> fs::File {
    let mut lp = sock.as_os_str().to_owned();
    lp.push(".lock");
    let mut f = match fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lp)
    {
        Ok(f) => f,
        Err(e) => {
            warn!("cannot open lock file {lp:?}: {e}");
            std::process::exit(1);
        }
    };
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let pid = fs::read_to_string(&lp).unwrap_or_default();
        warn!(
            "a daemon is already running (pid {}); this one exits",
            pid.trim()
        );
        std::process::exit(3);
    }
    let _ = f.set_len(0);
    let _ = write!(f, "{}", std::process::id());
    let me = std::process::id().to_string();
    for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = e.file_name().to_string_lossy().into_owned();
        if pid == me || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(cmd) = fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let a: Vec<&[u8]> = cmd.split(|&b| b == 0).collect();
        let is_fa = a.first().is_some_and(|a0| {
            Path::new(OsStr::from_bytes(a0)).file_name() == Some(OsStr::new("fa"))
        });
        let theirs = a
            .iter()
            .position(|&x| x == b"--socket")
            .and_then(|i| a.get(i + 1))
            .map(|s| OsStr::from_bytes(s))
            .unwrap_or(OsStr::new(SOCK));
        if is_fa && a.get(1) == Some(&&b"daemon"[..]) && theirs == sock.as_os_str() {
            let p: libc::pid_t = pid.parse().unwrap();
            warn!("stopping leftover daemon pid {pid}");
            unsafe { libc::kill(p, libc::SIGTERM) };
            for _ in 0..20 {
                if !e.path().exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if e.path().exists() {
                warn!("pid {pid} ignored SIGTERM, killing it");
                unsafe { libc::kill(p, libc::SIGKILL) };
            }
        }
    }
    f
}

// Total RAM in bytes (0 if unknown).
fn mem_total() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("MemTotal:")?
                    .trim()
                    .strip_suffix("kB")?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
        })
        .map_or(0, |k| k * 1024)
}

// Change events the kernel may hold for us: 2% of RAM at ~200 bytes each (kernel memory, which
// can't be swapped), between 250,000 and 4,000,000. Past it the kernel drops events and says so,
// and every live drive is re-checked: slow, but bounded, where an unlimited queue is not.
// Changes still in the kernel queue: its size in bytes over the average event read so far.
// ponytail: an estimate (events differ by name length); exact would need counting in the kernel.
fn changes_waiting() -> u64 {
    let fd = FAN_FD.load(Relaxed);
    let mut bytes = 0i32;
    if fd < 0 || unsafe { libc::ioctl(fd, libc::FIONREAD, &mut bytes) } < 0 || bytes <= 0 {
        return 0;
    }
    let avg = match EVENT_COUNT.load(Relaxed) {
        0 => 64,
        n => (EVENT_BYTES.load(Relaxed) / n).max(1),
    };
    (bytes as u64).div_ceil(avg)
}

fn queue_cap(ram: u64) -> u64 {
    (ram / 50 / 200).clamp(250_000, 4_000_000)
}

// "over 1,600,000 changes at once"
fn too_many(cap: Option<u64>) -> String {
    cap.map_or("too many changes at once".into(), |c| {
        format!("over {} changes at once", group(c as usize))
    })
}

// Seconds to wait before another full re-check when the last one was under 10 min ago.
fn backoff_secs() -> u64 {
    std::env::var("FA_TEST_BACKOFF_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300)
}

// fanotify with a queue of `cap` events. The kernel takes the limit from a system setting when
// the queue is made, so it is raised for that moment and put straight back (a higher value set
// by the admin is kept). If the setting can't be written (not root, read-only /proc): unlimited,
// as before 0.7.2, else the kernel default. Returns (fd or -1, (limit, how it was set)).
fn fanotify_open(cap: u64) -> (i32, (Option<u64>, String)) {
    const SYSCTL: &str = "/proc/sys/fs/fanotify/max_queued_events";
    let base = libc::FAN_CLASS_NOTIF | libc::FAN_CLOEXEC | libc::FAN_REPORT_DFID_NAME;
    let init = |f| unsafe { libc::fanotify_init(f, (libc::O_RDONLY | libc::O_LARGEFILE) as u32) };
    let old: Option<u64> = fs::read_to_string(SYSCTL)
        .ok()
        .and_then(|s| s.trim().parse().ok());
    if let Some(old) = old {
        if old >= cap || fs::write(SYSCTL, cap.to_string()).is_ok() {
            let fd = init(base);
            if old < cap {
                if let Err(e) = fs::write(SYSCTL, old.to_string()) {
                    warn!("putting {SYSCTL} back to {old} failed: {e}");
                }
            }
            if fd >= 0 {
                let how = if old >= cap {
                    "system setting"
                } else {
                    "2% of memory"
                };
                return (
                    fd,
                    (
                        Some(old.max(cap)),
                        format!(
                            "up to {} changes waiting ({how})",
                            group(old.max(cap) as usize)
                        ),
                    ),
                );
            }
        }
    }
    let fd = init(base | libc::FAN_UNLIMITED_QUEUE);
    if fd >= 0 || errno() != libc::EPERM {
        return (
            fd,
            (None, "unlimited (the queue limit could not be set)".into()),
        );
    }
    let n = old.unwrap_or(16384);
    (
        init(base),
        (
            Some(n),
            format!(
                "up to {} changes waiting (kernel default: the daemon isn't root)",
                group(n as usize)
            ),
        ),
    )
}

fn daemon(args: &[String]) {
    let mut log = PathBuf::from("findanything.log");
    let mut sock = PathBuf::from(SOCK);
    let mut state = PathBuf::from(STATE);
    let mut fstypes: Vec<String> = REAL_FS.iter().map(|s| s.to_string()).collect();
    let mut paths = Vec::new();
    let mut fresh = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--debug" => DEBUG.store(true, Relaxed),
            "--summary" => SUMMARY.store(true, Relaxed),
            "--log-max-mb" => {
                let mb: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                LOG_MAX.store(mb << 20, Relaxed)
            }
            "--state" => state = it.next().map(PathBuf::from).unwrap_or(state),
            "--fresh" => fresh = true,
            "--fstypes" => {
                fstypes = it
                    .next()
                    .map(|s| s.split(',').map(String::from).collect())
                    .unwrap_or(fstypes)
            }
            "--log" => log = it.next().map(PathBuf::from).unwrap_or(log),
            "--socket" => sock = it.next().map(PathBuf::from).unwrap_or(sock),
            "--owner" => {
                let name = it.next().cloned().unwrap_or_default();
                let pw = CString::new(name.clone())
                    .ok()
                    .and_then(|c| unsafe { libc::getpwnam(c.as_ptr()).as_ref() });
                match pw {
                    Some(pw) => _ = OWNER.set((pw.pw_uid, pw.pw_gid)),
                    None => {
                        eprintln!("--owner: no user named {name:?}");
                        std::process::exit(2);
                    }
                }
            }
            p => paths.push(PathBuf::from(p)),
        }
    }
    let _ = STATE_PATH.set(state.clone());
    let started = *START.get_or_init(Instant::now);
    // SIGTERM (shutdown, kill), SIGINT (Ctrl-C), SIGHUP: blocked in every thread; one thread waits
    // for them and saves the index before exiting. Must come before any other thread starts.
    let mut sigs: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut sigs);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut sigs, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &sigs, std::ptr::null_mut());
    }
    std::thread::spawn(move || {
        let mut sig = 0;
        unsafe { libc::sigwait(&sigs, &mut sig) };
        shutdown(&format!("signal {sig} received"));
    });
    match fs::OpenOptions::new().create(true).append(true).open(&log) {
        Ok(f) => {
            LOG_BYTES.store(f.metadata().map_or(0, |m| m.len()), Relaxed);
            *LOG_PATH.lock().unwrap() = Some(log.clone());
            *LOG.lock().unwrap() = Some(f)
        }
        Err(e) => eprintln!("cannot open log {log:?}: {e}"),
    }
    sudo_owner(&log);

    let date = std::process::Command::new("date")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut uts) };
    let cs = |a: &[libc::c_char]| {
        unsafe { std::ffi::CStr::from_ptr(a.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    info!("==== Find Anything {VERSION} daemon starting, {date}");
    info!(
        "host {}, kernel {} {}, uid {}, debug {}",
        cs(&uts.nodename),
        cs(&uts.release),
        cs(&uts.machine),
        unsafe { libc::geteuid() },
        DEBUG.load(Relaxed)
    );
    info!(
        "log file {:?}, socket {sock:?}",
        fs::canonicalize(&log).unwrap_or(log.clone())
    );

    let _lock = single_instance(&sock);
    let _ = fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            warn!("cannot create socket {sock:?}: {e}");
            std::process::exit(1);
        }
    };
    // ponytail: socket readable by the sudo user only, so no per-user result filtering yet (M3).
    let _ = fs::set_permissions(&sock, fs::Permissions::from_mode(0o600));
    sudo_owner(&sock);
    // The socket answers from the start, so clients see "indexing" instead of "not running" during long scans.
    let server = std::thread::spawn(move || {
        // One thread per client, so a status request never waits behind a search.
        for s in listener.incoming().flatten() {
            std::thread::spawn(move || {
                if let Err(e) = serve(s, started) {
                    debug!("client error: {e}");
                }
            });
        }
    });

    let all = mountinfo();
    let watch = paths.is_empty(); // mounts are followed only when indexing every drive
    let targets: Vec<(PathBuf, String, String)> = if watch {
        wanted_mounts(&all, &fstypes)
    } else {
        paths
            .iter()
            .map(|p| {
                let p = fs::canonicalize(p).unwrap_or(p.clone());
                let m = all
                    .iter()
                    .filter(|m| p.starts_with(&m.path))
                    .max_by_key(|m| m.path.as_os_str().len());
                (
                    p,
                    m.map(|m| m.fstype.clone()).unwrap_or_default(),
                    m.map(|m| m.source.clone()).unwrap_or_default(),
                )
            })
            .collect()
    };
    for m in &all {
        debug!(
            "mountinfo: {:?} {} {} root={:?}",
            m.path, m.fstype, m.source, m.root
        );
    }

    let (fan, queue) = fanotify_open(queue_cap(mem_total()));
    let fan = if fan < 0 {
        warn!("fanotify_init failed: {}. Live updates need root: run `sudo fa daemon`. Continuing with a static index.", errstr(errno()));
        None
    } else {
        info!("fanotify ready; change queue: {}", queue.1);
        FAN_FD.store(fan, Relaxed);
        Some(unsafe { OwnedFd::from_raw_fd(fan) })
    };

    let log = fs::canonicalize(&log).unwrap_or(log);
    let exclude = Exclude::parse(&fs::read_to_string(Exclude::file(&state)).unwrap_or_default());
    let mut ix = Index {
        // Test hook: FA_TEST_NO_SKIP=1 starts with no mount list, as when a drive is mounted mid-scan.
        skip: if std::env::var_os("FA_TEST_NO_SKIP").is_some() {
            HashSet::new()
        } else {
            all.iter().map(|m| m.path.clone()).collect()
        },
        exclude,
        log,
        state,
        queue,
        ..Default::default()
    };
    *PROGRESS.lock().unwrap() = Some((
        "indexing: loading the saved index".into(),
        Instant::now(),
        false,
    ));
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(15));
        if PROGRESS.lock().unwrap().is_none() {
            break;
        }
        info!("{}", progress());
    });
    if fresh {
        // Start Over / Rebuild from the window: no saved index, so every drive is read from scratch.
        match fs::remove_file(&ix.state) {
            Ok(()) => info!("--fresh: deleted the saved index {:?}", ix.state),
            Err(e) => info!("--fresh: no saved index to delete ({e})"),
        }
    }
    // Entries per drive at the last save: "about N files" while a drive is read from scratch.
    let expected: HashMap<String, usize> = fs::read_to_string(ix.state.with_extension("drives"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            Some((f.first()?.to_string(), f.get(4)?.parse().ok()?))
        })
        .collect();
    ix.load();
    if fresh {
        ix.startup = "started over".into();
    }
    // Search works from here on: on the saved index at first, while each drive is checked.
    let idx = Arc::new(Mutex::new(ix));
    let _ = IDX.set(idx.clone());
    let fan_fd = fan.as_ref().map(|f| f.as_raw_fd());
    let mut drives = Vec::new();
    {
        let mut ix = idx.lock().unwrap();
        ix.rekey_moved(&targets);
        for (path, fstype, source) in targets {
            info!("indexing {path:?} [{fstype} {source}]");
            if let Some((root, snap)) = ix.register(path.clone(), fstype, source, fan_fd) {
                drives.push((path, root, snap, ix.mounts.last().unwrap().live.is_ok()));
            }
        }
        if watch {
            ix.forget_kept();
        }
    }
    // Live drives first, holding WRITER so their events wait until the check is done (the events
    // are queued, not lost); then the drives without live events, whose checks need no WRITER.
    drives.sort_by_key(|d| !d.3);
    let w = WRITER.lock().unwrap();
    if let Some(fan) = fan {
        let idx = idx.clone();
        std::thread::spawn(move || event_loop(fan, idx));
    }
    // Mounts are followed from now on, not only once startup is done: at boot the desktop mounts
    // some drives after the service starts, and they are listed (and read) at once (0.22.1).
    // ponytail: a live drive mounted now waits for WRITER, so it is read after the live drives above.
    if watch {
        let idx = idx.clone();
        let fstypes = fstypes.clone();
        std::thread::spawn(move || watch_mounts(idx, fan_fd, fstypes));
    }
    let total = drives.len();
    let mut w = Some(w);
    for (i, (path, root, snap, live)) in drives.into_iter().enumerate() {
        if !live {
            w.take();
        }
        set_progress(format!(
            "checking {} ({} of {total} drives)",
            path.display(),
            i + 1
        ));
        if !snap {
            EXPECTED.store(expected.get(&*path.to_string_lossy()).copied().unwrap_or(0), Relaxed);
        }
        fill(&idx, path, root, snap);
    }
    *PROGRESS.lock().unwrap() = None;
    drop(w);
    {
        let mut ix = idx.lock().unwrap();
        // A drive "scanned" was read from scratch; the others were checked against the saved copy.
        let fresh = ix
            .mounts
            .iter()
            .filter(|m| m.startup.starts_with("scanned") || m.startup.starts_with("read from the MFT"))
            .count();
        ix.startup += &format!(
            ", {fresh} of {total} drives read from scratch, ready in {:.1} s",
            started.elapsed().as_secs_f64()
        );
        info!(
            "index ready: {} directories, memory {}, scan errors {}",
            ix.dirs.len(),
            rss(),
            ix.stats.scan_errors
        );
    }
    // Also saved every 10 minutes (if anything changed), so a crash or power cut loses little.
    let i2 = idx.clone();
    std::thread::spawn(move || {
        let mut last = None;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(600));
            let ix = i2.lock().unwrap();
            let now = (ix.generation, ix.mounts.len());
            if last != Some(now) {
                last = Some(now);
                ix.save();
            }
        }
    });
    let i3 = idx.clone();
    std::thread::spawn(move || memory_monitor(i3, fan_fd));
    std::thread::spawn(move || poll_loop(idx));
    info!("listening; try `fa find <words>` or `fa status`");
    let _ = server.join();
}

fn serve(s: UnixStream, started: Instant) -> io::Result<()> {
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line)?;
    let req = line.trim_end_matches('\n');
    let mut out = BufWriter::new(&s);
    if req == "quit" {
        shutdown("stop requested by a client");
    }
    if req == "quit-nosave" {
        // Rescan All: the saved index goes too, so the next start (the window's --fresh, or the
        // boot service, which takes no flags) reads every drive from scratch.
        if let Some(p) = STATE_PATH.get() {
            let _ = fs::remove_file(p);
        }
        info!("stop requested by a client, index not saved and deleted (starting over)");
        std::process::exit(0);
    }
    if let Some(l) = req.strip_prefix("log\t") {
        // The window's Debug logging setting, applied at once (the boot service starts with it off).
        let (level, mb) = l.split_once('\t').unwrap_or((l, ""));
        DEBUG.store(level == "detailed", Relaxed);
        SUMMARY.store(level == "summary", Relaxed);
        if let Ok(mb) = mb.parse::<u64>() {
            LOG_MAX.store(mb << 20, Relaxed);
        }
        info!("log set to {level}, {mb} MB, by a client");
        return out.write_all(b"ok\n");
    }
    let Some(idx) = IDX.get() else {
        if req == "status" {
            write!(
                out,
                "Find Anything {VERSION}, up {}s, starting\n{}\n",
                started.elapsed().as_secs(),
                progress()
            )?;
        }
        return Ok(()); // no results until the saved index is loaded
    };
    if req == "status" {
        let t = Instant::now();
        let ix = idx.lock().unwrap();
        let waited = t.elapsed();
        let st = ix.status(started);
        drop(ix);
        if waited > Duration::from_millis(100) {
            debug!("status waited {waited:.1?} for the index");
        }
        out.write_all(st.as_bytes())?;
    } else if req == "drives" {
        let d = idx.lock().unwrap().drives();
        out.write_all(d.as_bytes())?;
    } else if req == "rebuild" {
        out.write_all(if start_rebuild("requested by a client") {
            b"ok\n"
        } else {
            b"busy\n"
        })?;
    } else if let Some(r) = req.strip_prefix("report") {
        // "report", or "report\t<path of a file that should show>\t<the search>".
        let mut f = r.trim_start_matches('\t').splitn(2, '\t');
        let (p, q) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
        out.write_all(report(idx, started, p, q).as_bytes())?;
    } else if let Some(p) = req.strip_prefix("verify\t") {
        out.write_all(verify(idx, Path::new(p)).as_bytes())?;
    } else if req == "exclude" {
        let l = idx.lock().unwrap().exclude.list();
        out.write_all(l.as_bytes())?;
    } else if let Some(list) = req.strip_prefix("exclude\t") {
        // Reply: "ok", or "rebuilding" / "busy" when a folder came off the list and the drives need re-reading.
        let back = idx.lock().unwrap().set_exclude(&list.replace('\t', "\n"));
        out.write_all(
            match (
                back,
                back && start_rebuild("to bring back folders taken off the exclude list"),
            ) {
                (false, _) => b"ok\n",
                (true, true) => b"rebuilding\n",
                (true, false) => b"busy\n",
            },
        )?;
    } else if let Some((n, q)) = req.strip_prefix("find\t").and_then(|r| r.split_once('\t')) {
        // Reply: the total number of matches, then the first n paths (folders end in "/").
        let query = query::parse(q);
        let t = Instant::now();
        let ix = idx.lock().unwrap();
        let waited = t.elapsed().as_micros() as f64 / 1000.0;
        let before = if DEBUG.load(Relaxed) {
            mem()
        } else {
            String::new()
        };
        let (mut hits, total) = ix.search(&query, n.parse().unwrap_or(100));
        drop(ix);
        let ms = t.elapsed().as_micros() as f64 / 1000.0 - waited;
        let t = Instant::now();
        hits.sort_by(|a, b| query::natural(a.as_bytes(), b.as_bytes()));
        let sort = t.elapsed().as_micros() as f64 / 1000.0;
        let t = Instant::now();
        writeln!(out, "{total}")?;
        let rows = hits.len();
        let hits_kb = hits.iter().map(|h| h.len() + 1).sum::<usize>() / 1024;
        for h in hits {
            out.write_all(h.as_bytes())?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        if DEBUG.load(Relaxed) && total > 100_000 {
            debug!("memory for find {q:?} ({total} hits, result list {} KB): before: {before}; after: {}", hits_kb, mem());
        }
        summ!("find {q:?}: {total} hits in {ms:.1} ms (waited {waited:.1} ms for the index; sorted {rows} rows {sort:.1} ms; sent {:.1} ms)", t.elapsed().as_micros() as f64 / 1000.0);
    }
    Ok(())
}

// Starts a rebuild unless the daemon is busy checking drives.
fn start_rebuild(why: &str) -> bool {
    let mut p = PROGRESS.lock().unwrap();
    if p.is_some() {
        return false;
    }
    *p = Some(("rebuilding".into(), Instant::now(), false));
    info!("rebuild {why}");
    std::thread::spawn(|| rebuild(IDX.get().unwrap()));
    true
}

// Problem report (GUI "Report a Problem", `fa report`): for a file the user knows exists, where the
// index lost it and whether the search matches it; then status and the last INFO/WARN lines.
// Small enough to paste (~20 KB at most): no per-file lines.
fn report(idx: &Mutex<Index>, started: Instant, path: &str, q: &str) -> String {
    let mut o = format!("== Find Anything {VERSION} problem report ==\n");
    if !path.is_empty() {
        o += &why(idx, Path::new(path), q);
    }
    o += "\n== Status ==\n";
    o += &idx.lock().unwrap().status(started);
    o += "\n== Recent log lines (INFO and WARN, oldest first) ==\n";
    for l in RECENT.lock().unwrap().iter() {
        o += l;
    }
    o
}

// Follows `rel` from a drive root through the index. Ok: the key of the file's folder.
// Err: the folder where the index stops matching the disk, and how.
fn walk(ix: &Index, root: Key, top: &Path, rel: &Path) -> Result<Key, (PathBuf, Key, String)> {
    let (mut k, mut dir) = (root, top.to_path_buf());
    let n = rel.iter().count();
    for (i, c) in rel.iter().enumerate() {
        let child = dir.join(c);
        let Some(d) = ix.dirs.get(&k) else {
            return Err((dir, k, "this folder is not in the index".into()));
        };
        match d.get(c) {
            None => return Err((dir, k, format!("{c:?} is not listed in the index"))),
            Some(_) if i + 1 == n => return Ok(k),
            Some(None) => return Err((dir, k, format!("{c:?} is listed as a file, not a folder"))),
            Some(Some(ck)) if dir_key(&child) != Some(ck) => {
                return Err((dir, k, format!("{c:?} in the index is another folder than the one on disk now")))
            }
            Some(Some(ck)) => (k, dir) = (ck, child),
        }
    }
    Ok(k)
}

fn why(idx: &Mutex<Index>, p: &Path, q: &str) -> String {
    let mut o = format!("file: {}\n", p.display());
    let disk = fs::symlink_metadata(p);
    o += &match &disk {
        Ok(m) => format!("on disk: yes, a {}\n", if m.is_dir() { "folder" } else { "file" }),
        Err(e) => format!("on disk: no ({e})\n"),
    };
    let is_dir = disk.as_ref().is_ok_and(|m| m.is_dir());
    let mi = mountinfo();
    let fs_of = mi
        .iter()
        .filter(|m| p.starts_with(&m.path))
        .max_by_key(|m| m.path.as_os_str().len());
    let ix = idx.lock().unwrap();
    let Some(m) = ix
        .mounts
        .iter()
        .filter(|m| p.starts_with(&m.path))
        .max_by_key(|m| m.path.as_os_str().len())
    else {
        o += &match fs_of {
            Some(f) => format!("drive: not indexed; it is on {} [{} {}]\n", f.path.display(), f.fstype, f.source),
            None => "drive: not indexed, no mount found for it\n".into(),
        };
        return o;
    };
    let (root, top, live) = (m.root, m.path.clone(), m.live.is_ok());
    o += &format!(
        "drive: {} [{} {}]: {}; {}\n",
        top.display(),
        m.fstype,
        m.source,
        match &m.live {
            Ok(()) => "LIVE".into(),
            Err(e) => format!("NOT LIVE ({e})"),
        },
        m.startup
    );
    if m.checking {
        o += "drive is being checked right now: results from it may be out of date until it is done\n";
    }
    if let Some(f) = fs_of.filter(|f| f.path != top) {
        o += &format!("inside {} [{}], a mount that is not indexed\n", f.path.display(), f.fstype);
        return o;
    }
    let rel = p.strip_prefix(&top).unwrap_or(p).to_path_buf();
    if rel.as_os_str().is_empty() {
        o += "this is the drive's own folder\n";
        return o;
    }
    let mut at = top.clone();
    for (i, c) in rel.iter().enumerate() {
        at.push(c);
        if (i + 1 < rel.iter().count() || is_dir) && ix.exclude.hit(c, || at.clone()) {
            o += &format!("excluded: {} is on the exclude list (Preferences)\n", at.display());
        }
    }
    let found = match walk(&ix, root, &top, &rel) {
        Ok(k) => {
            o += "index: listed\n";
            o += &ix.meta_lines(k, p);
            drop(ix);
            Some(k)
        }
        Err((dir, k, how)) => {
            o += &format!("index: NOT listed: {how}, in {}\n", dir.display());
            let ict = ix.dirs.get(&k).map_or(0, |d| d.ctime);
            let dct = fs::symlink_metadata(&dir).map_or(0, |m| ctime_ns(&m));
            o += &format!(
                "folder ctime: index {ict}, disk {dct}: {}\n",
                if ict == 0 {
                    "waiting to be re-read"
                } else if ict == dct {
                    "unchanged since it was listed, so the change was missed as it happened (a lost event)"
                } else {
                    "changed since it was listed, not re-read yet"
                }
            );
            o += &format!("pending re-check: {}\n", if ix.recheck.contains(&k) { "yes" } else { "no" });
            drop(ix);
            // Re-read that folder now (events wait meanwhile on a live drive), so the file shows.
            // Not while a startup check or rebuild holds the drive: the report would wait for it.
            let w = WRITER.try_lock();
            if live && w.is_err() {
                o += "not re-read now: the drive is busy (startup check or rebuild); that will re-read it\n";
                warn!("missed: {p:?} ({how}, in {dir:?}); drive busy, not re-read");
                return o;
            }
            if let Some(d) = idx.lock().unwrap().dirs.get_mut(&k) {
                d.ctime = 0;
            }
            let (c, r) = reconcile(idx, dir.clone(), k);
            let after = walk(&idx.lock().unwrap(), root, &top, &rel).ok();
            let now = if after.is_some() { "listed now" } else { "still not listed" };
            o += &format!("re-read {} ({c} folders checked, {r} re-listed): {now}\n", dir.display());
            if let Some(k) = after {
                o += &idx.lock().unwrap().meta_lines(k, p);
            }
            warn!("missed: {p:?} ({how}, in {dir:?}); re-read it: {now}");
            after
        }
    };
    if let (Some(k), false) = (found, q.is_empty()) {
        let query = query::parse(q);
        let name = p.file_name().unwrap_or_default().as_bytes();
        let ix = idx.lock().unwrap();
        let mut folder = p.parent().unwrap_or(p).as_os_str().as_bytes().to_vec();
        if folder.last() != Some(&b'/') {
            folder.push(b'/');
        }
        let mut full = [&folder[..], name].concat();
        if is_dir {
            full.push(b'/');
        }
        let quick = ix.dirs.get(&k).is_some_and(|d| query.needed().iter().all(|w| has(d.names(), w)));
        let e = query::Entry { name, dir: is_dir, folder: &folder, full: &full, kids: &[] };
        o += &format!(
            "search {q:?}: {} (quick name check {})\n",
            if query.matches(&e) { "matches this file" } else { "does NOT match this file" },
            if quick { "passes" } else { "FAILS: a bug, the folder is skipped" }
        );
    }
    o
}

// `fa verify PATH`: the index against the disk, for PATH and everything below it: names, which
// are folders, sizes and dates, by the same rules as a scan (excluded folders and other drives
// left out). Read-only. A file changed while it runs can show as a difference.
fn verify(idx: &Mutex<Index>, p: &Path) -> String {
    let start = {
        let ix = idx.lock().unwrap();
        ix.mounts
            .iter()
            .filter(|m| p.starts_with(&m.path))
            .max_by_key(|m| m.path.as_os_str().len())
            .map(|m| {
                let rel = p.strip_prefix(&m.path).unwrap_or(p);
                let k = match rel.file_name() {
                    None => Some(m.root),
                    Some(n) => walk(&ix, m.root, &m.path, rel)
                        .ok()
                        .and_then(|pk| ix.dirs.get(&pk)?.get(n)?),
                };
                (k, ix.skip.clone(), ix.exclude.clone())
            })
    };
    let (k, skip, ex) = match start {
        None => return format!("{} is not on an indexed drive\n", p.display()),
        Some((None, ..)) => return format!("{} is not a folder in the index\n", p.display()),
        Some((Some(k), skip, ex)) => (k, skip, ex),
    };
    let (mut folders, mut entries, mut n, mut unread) = (0, 0, 0, Vec::new());
    let mut diffs = Vec::new();
    let mut diff = |s: String| {
        n += 1;
        if diffs.len() < 50 {
            diffs.push(s);
        }
    };
    let kind = |k: Option<Key>| if k.is_some() { "a folder" } else { "not a folder" };
    let mut stack = vec![(p.to_path_buf(), k)];
    while let Some((path, k)) = stack.pop() {
        let disk = match list_dir(&path, k) {
            Ok(l) => l,
            Err(e) => {
                unread.push(format!("{} ({e})", path.display())); // nor could the scan
                continue;
            }
        };
        folders += 1;
        let ix = idx.lock().unwrap();
        let Some(d) = ix.dirs.get(&k) else {
            diff(format!("{}: folder not in the index", path.display()));
            continue;
        };
        let mut seen = HashSet::new();
        for l in &disk {
            let cp = path.join(&l.name);
            if l.dir && ex.hit(&l.name, || cp.clone()) {
                continue;
            }
            entries += 1;
            seen.insert(l.name.as_bytes());
            let want = if skip.contains(&cp) { None } else { l.kid() };
            match d.get(&l.name) {
                None => diff(format!("only on disk: {}", cp.display())),
                Some(kid) if kid != want => diff(format!(
                    "{}: {} in the index, {} on disk",
                    cp.display(),
                    kind(kid),
                    kind(want)
                )),
                Some(kid) => {
                    let m = d.meta(&l.name).unwrap_or_default();
                    if !same_meta(m, l.meta()) {
                        diff(format!(
                            "{}: index {}; disk {}",
                            cp.display(),
                            meta_str(m),
                            meta_str(l.meta())
                        ));
                    }
                    if let Some(c) = kid {
                        stack.push((cp, c));
                    }
                }
            }
        }
        for e in d.entries().filter(|e| !seen.contains(e.name)) {
            diff(format!(
                "only in the index: {}",
                path.join(OsStr::from_bytes(e.name)).display()
            ));
        }
    }
    let mut o = format!(
        "verified {}: {folders} folders, {entries} entries, {n} differences\n",
        p.display()
    );
    for d in diffs {
        o += &format!("  {d}\n");
    }
    if !unread.is_empty() {
        o += &format!("{} folders could not be listed, by the scan either:\n", unread.len());
        for u in unread.iter().take(10) {
            o += &format!("  {u}\n");
        }
    }
    o
}

// Saves the index (if the startup scan finished) and exits.
fn shutdown(why: &str) -> ! {
    match IDX.get() {
        Some(idx) => {
            let ix = idx.lock().unwrap();
            info!("{why}; final status:\n{}", ix.status(*START.get().unwrap()));
            ix.save();
        }
        None => info!("{why} during the startup scan; nothing saved"),
    }
    std::process::exit(0);
}

fn client(req: String, sock: &Path) {
    let mut s = UnixStream::connect(sock).unwrap_or_else(|e| {
        eprintln!(
            "cannot reach the daemon at {}: {e}\nIs it running? Start it with: sudo fa daemon",
            sock.display()
        );
        std::process::exit(1)
    });
    let _ = s.write_all(req.as_bytes());
    let _ = s.shutdown(std::net::Shutdown::Write);
    if !req.starts_with("find\t") {
        let _ = io::copy(&mut s, &mut io::stdout().lock());
        return;
    }
    let mut lines = BufReader::new(s).lines();
    let total: usize = lines
        .next()
        .and_then(Result::ok)
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    let mut shown = 0;
    let mut o = io::stdout().lock();
    for l in lines.map_while(Result::ok) {
        shown += 1;
        let _ = writeln!(o, "{l}");
    }
    if total > shown {
        eprintln!("({shown} of {total} matches; -n {total} shows all)");
    }
}

// `fa find ... content:word`: the daemon finds the files by name, this process (the user) reads them.
fn find_content(q: &str, c: &query::Content, limit: usize, sock: &Path) {
    let mut s = UnixStream::connect(sock).unwrap_or_else(|e| {
        eprintln!("cannot reach the daemon at {}: {e}", sock.display());
        std::process::exit(1)
    });
    const MAX: usize = 1_000_000; // files read at most; narrow the name part for more
    let _ = s.write_all(format!("find\t{MAX}\t{q}\n").as_bytes());
    let _ = s.shutdown(std::net::Shutdown::Write);
    let mut lines = BufReader::new(s).lines();
    let total: usize = lines
        .next()
        .and_then(Result::ok)
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    if total > MAX {
        eprintln!("({total} files match the name part; reading the first {MAX})");
    }
    let mut o = io::stdout().lock();
    let mut shown = 0;
    for l in lines.map_while(Result::ok) {
        if shown == limit {
            eprintln!("(stopped at {limit} matches; -n raises it)");
            break;
        }
        if query::file_has(&l, c) {
            shown += 1;
            let _ = writeln!(o, "{l}");
        }
    }
}

const USAGE: &str = "Find Anything — instant filename search

  sudo fa daemon [--debug | --summary] [--log-max-mb N] [--log FILE] [--socket PATH] [--state FILE] [--fresh] [--fstypes T,T] [--owner USER] [PATH...]
        Index PATHs (default: every real filesystem, following mounts and unmounts) and keep
        the index live. Log goes to FILE (default ./findanything.log); --debug logs every
        event. The index is saved to --state (default /var/cache/findanything/index.bin)
        on stop and checked against the disk at the next start; --fresh deletes it first, so
        every drive is read from scratch. --fstypes replaces the list of
        filesystem types indexed by default. NTFS drives (ntfs3) are first read from their MFT,
        not folder by folder; FA_NO_MFT=1 turns that off. --owner USER gets the socket and
        the logs (the boot service; else whoever ran sudo or pkexec).
  fa find [-n LIMIT] [--socket PATH] WORDS...
        Names containing all WORDS (case-insensitive); folders end in a slash. Default limit 100.
        Syntax like Everything: a|b, !word, \"two words\", *.png, IMG_????, ext:png;jpg, file:,
        folder:, /path/part/, parent:/dir, child:name, empty:, image: audio: video: doc: zip: exe:,
        and match options case: ww: startwith: endwith: wfn: nopunc: noaccents: (alone: every word).
        content:word reads the matching files (as you; text files up to 64 MB) for that word.
  fa exclude [--socket PATH] [--clear | FOLDER...]
        Show, set or clear the folders never indexed: names (node_modules) or full paths.
  fa status [--socket PATH]
        Index size, per-drive LIVE / NOT LIVE (re-checked every few minutes), event counters.
  fa rebuild [--socket PATH]
        Re-read every folder of every drive (search keeps working meanwhile), then save.
  fa report [--socket PATH] [FILE [WORDS...]]
        Problem report, small enough to share: for a FILE that should show, where the index lost
        it (the folder is re-read so it shows) and whether the search WORDS match it; then the
        status and the last INFO/WARN lines. The GUI's status-bar bug button saves the same report.
  fa verify [--socket PATH] FOLDER
        Compare the index with the disk for FOLDER and everything in it: names, folders, sizes
        and dates. Changes nothing; prints the number of differences and the first 50.
  fa stop [--socket PATH]
        Stop the daemon.
  sudo fa mftcheck PATH [--device DEV]
        Read the NTFS drive mounted at PATH (ntfs3) both ways, from its MFT and folder by folder,
        and print how long each took and any difference. Changes nothing.
  fa-gui
        Search window (type to search; menu can start/stop the daemon).
  fa --version";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut sock = PathBuf::from(SOCK);
    let mut limit = 100usize;
    let mut rest = Vec::new();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    if cmd == "daemon" {
        return daemon(&args[1..]);
    }
    if cmd == "mftcheck" {
        return mft_check(&args[1..]);
    }
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--socket" => sock = it.next().map(PathBuf::from).unwrap_or(sock),
            "-n" => limit = it.next().and_then(|n| n.parse().ok()).unwrap_or(limit),
            w => rest.push(w.to_string()),
        }
    }
    match cmd {
        "find" if !rest.is_empty() => match query::split_content(&rest.join(" ")) {
            (q, None) => client(format!("find\t{limit}\t{q}\n"), &sock),
            (q, Some(c)) => find_content(&q, &c, limit, &sock),
        },
        "status" => client("status\n".into(), &sock),
        "rebuild" => client("rebuild\n".into(), &sock),
        "report" => {
            let p = rest.first().and_then(|p| std::path::absolute(p).ok()).unwrap_or_default();
            let q = rest.get(1..).unwrap_or_default().join(" ");
            client(format!("report\t{}\t{q}\n", p.display()), &sock)
        }
        "verify" if rest.len() == 1 => {
            let p = std::path::absolute(&rest[0]).unwrap_or_default();
            client(format!("verify\t{}\n", p.display()), &sock)
        }
        "exclude" if rest.is_empty() => client("exclude\n".into(), &sock),
        "exclude" if rest == ["--clear"] => client("exclude\t\n".into(), &sock),
        "exclude" => client(format!("exclude\t{}\n", rest.join("\t")), &sock),
        "stop" => client("quit\n".into(), &sock),
        "--version" | "-V" => println!("Find Anything {VERSION} (Droid)"),
        _ => println!("{USAGE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_live_in_plain_words() {
        let m = |fstype: &str, e: &str| Mount {
            path: "/x".into(),
            fstype: fstype.into(),
            source: String::new(),
            root: (0, 0),
            fsid: [0; 8],
            live: Err(e.into()),
            startup: String::new(),
            checking: false,
            every: 300,
            last: None,
        };
        assert_eq!(
            Index::why_not_live(&m("exfat", "fanotify_mark: x")),
            "exFAT drives don't report changes, so this one is checked every 5 minutes."
        );
        assert!(Index::why_not_live(&m("ext4", "fanotify unavailable"))
            .starts_with("Live updates need the daemon to run as root"));
        assert!(!errstr(95).contains("errno"), "{}", errstr(95)); // was "(os error 95) (errno 95)"
    }

    // Two kept drives whose device numbers swapped at boot (like nvme0 and nvme1), and a third
    // kept on the number one of them now uses: the two keep their trees, the third is dropped.
    #[test]
    fn rekey_swapped_drives() {
        let mut ix = Index::default();
        let tree = |ix: &mut Index, dev: u64, name: &str| {
            let (root, sub) = ((dev, 2), (dev, 10));
            let mut r = Dir::new(root, name.into(), vec![dev as u8], 0);
            r.set(OsStr::new("sub"), Some(sub), Meta::default());
            let mut s = Dir::new(root, "sub".into(), vec![dev as u8, 1], 0);
            s.set(OsStr::new("f.txt"), None, Meta::default());
            ix.insert(vec![(root, r), (sub, s)]);
            ix.ids.insert(vec![dev as u8, 1], sub);
            ix.snap_roots.insert(root);
        };
        tree(&mut ix, 1, "/a");
        tree(&mut ix, 2, "/b");
        tree(&mut ix, 3, "/c");
        ix.rekey(&HashMap::from([(1, 2), (2, 3)])); // a: 1 -> 2, b: 2 -> 3; c's old 3 is taken
        assert_eq!(ix.dirs.len(), 4);
        assert_eq!(ix.snap_roots, HashSet::from([(2, 2), (3, 2)]));
        assert_eq!(ix.dirs[&(2, 2)].name, "/a");
        assert_eq!(ix.dirs[&(3, 2)].name, "/b");
        assert_eq!(ix.dirs[&(2, 10)].parent, (2, 2));
        assert_eq!(ix.dirs[&(2, 2)].get(OsStr::new("sub")), Some(Some((2, 10))));
        assert_eq!(ix.ids[&vec![1u8, 1]], (2, 10));
        assert_eq!(ix.ids[&vec![2u8, 1]], (3, 10));
        assert!(!ix.ids.contains_key(&vec![3u8, 1]));
    }

    #[test]
    fn matching_and_unescape() {
        assert_eq!(
            unescape(r"/mnt/My\040Drive"),
            OsString::from("/mnt/My Drive")
        );
    }

    #[test]
    fn packed_names() {
        assert!(
            has(b"abcHellboy.PNG", b"boy.p")
                && has(b"ab", b"ab")
                && has(b"xAB", b"ab")
                && !has(b"ab", b"abc")
                && !has(b"a/b", b"ab")
        );
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_760_623_385), "2025-10-16 14:03:05 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC"); // leap day
        assert_eq!(bytes_str(999), "999 bytes");
        assert_eq!(bytes_str(1_234_567), "1.2 MB (1,234,567 bytes)");
    }

    // 2026-10-07: an ext4 handle opened on an ntfs3 drive marked it dirty. A handle may only go
    // to a live drive of its own filesystem, never to ntfs3 at all.
    #[test]
    fn handles_stay_on_their_own_filesystem() {
        let m = |path: &str, fstype: &str, fsid: u8, live: bool| Mount {
            path: path.into(),
            fstype: fstype.into(),
            source: String::new(),
            root: (0, 0),
            fsid: [fsid; 8],
            live: if live { Ok(()) } else { Err("no".into()) },
            startup: String::new(),
            checking: false,
            every: 0,
            last: None,
        };
        let ms = [
            m("/", "ext4", 1, true),
            m("/ntfs", "ntfs3", 2, true),
            m("/b", "btrfs", 3, true),
            m("/b2", "btrfs", 4, true),
            m("/x", "ext4", 5, false),
            m("/f", "fuseblk", 0, false),
        ];
        let p = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(handle_mounts(&ms, &[1; 8], 1), p(&["/"])); // ext4: its own drive only
        assert_eq!(handle_mounts(&ms, &[2; 8], 1), p(&[])); // ntfs3: never, by path
        assert_eq!(handle_mounts(&ms, &[9; 8], 1), p(&[])); // unknown filesystem: nowhere
        assert_eq!(handle_mounts(&ms, &[9; 8], 0x4d), p(&["/b", "/b2"])); // btrfs subvolume: btrfs only
        assert_eq!(handle_mounts(&ms, &[2; 8], 0x4d), p(&[])); // the ntfs3 fsid wins over the btrfs type
        assert_eq!(handle_mounts(&ms, &[5; 8], 1), p(&[])); // not live: sends no events
        assert_eq!(handle_mounts(&ms, &[0; 8], 1), p(&[])); // FUSE (fsid 0)
    }

    #[test]
    fn listing() {
        let d = std::env::temp_dir().join(format!("fa-list-{}", std::process::id()));
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("f"), "").unwrap();
        std::os::unix::fs::symlink(d.join("sub"), d.join("link")).unwrap();
        fs::write(d.join("five"), "12345").unwrap();
        let k = dir_key(&d).unwrap();
        let mut l = list_dir(&d, k).unwrap();
        l.sort_by(|a, b| a.name.cmp(&b.name));
        let sub = dir_key(&d.join("sub")).unwrap();
        assert_eq!(
            l.iter()
                .map(|l| (l.name.to_str().unwrap(), l.dir, l.kid(), l.meta().size))
                .collect::<Vec<_>>(),
            vec![
                ("f", false, None, 0),
                ("five", false, None, 5),
                ("link", false, None, 0), // a link: no size of its own
                ("sub", true, Some(sub), 0)
            ]
        );
        let m = fs::metadata(d.join("five")).unwrap();
        let st = l.iter().find(|l| l.name == "five").unwrap().stat.as_ref().unwrap();
        assert_eq!((st.key, st.meta.mtime as i64), ((m.dev(), m.ino()), m.mtime()));
        assert_eq!(
            m.created().ok().map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs() as u32),
            Some(st.meta.btime).filter(|&b| b != 0)
        );
        assert!(list_dir(&d.join("link"), dir_key(&d.join("sub")).unwrap()).is_err()); // never followed
        assert!(list_dir(&d.join("sub"), k).is_err()); // not the folder expected
        assert!(list_dir(&d.join("f"), k).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn queue_limit() {
        let gb = 1 << 30;
        assert_eq!(queue_cap(4 * gb), 429_496); // 2% of 4 GB = 86 MB
        assert_eq!(queue_cap(16 * gb), 1_717_986); // ~340 MB
        assert_eq!(queue_cap(64 * gb), 4_000_000); // ceiling, ~800 MB
        assert_eq!(queue_cap(gb), 250_000); // floor
        assert_eq!(queue_cap(0), 250_000); // RAM unknown
        assert_eq!(too_many(Some(1_717_986)), "over 1,717,986 changes at once");
    }

    #[test]
    fn recheck_interval() {
        // Desktop log 2026-10-07: NTFS-3g checks took 17 s, 120 s and 292 s.
        assert_eq!(poll_every(17_413), 300); // floor: 5 min
        assert_eq!(poll_every(120_482), 1260); // ~10x, rounded up to whole minutes: 21 min
        assert_eq!(poll_every(292_574), 2940); // 49 min
        assert_eq!(poll_every(900_000), 3600); // cap: 1 h
        assert_eq!(ago(45), "45 s");
        assert_eq!(ago(7385), "2 h 3 min");
    }
}
