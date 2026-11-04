// Find Anything GUI: type to search the running `fa daemon`, like Everything.
// Talks to the daemon over its unix socket; checks it every 2 s so changes show up live.

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[path = "query.rs"]
#[allow(dead_code)]
mod query; // natural sort, content: terms

#[path = "diag.rs"]
mod diag;
use diag::dlog;

const SOCK: &str = "/run/findanything.sock";
const LIMIT: usize = 100_000; // rows shown; the total count is always exact
const VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_NOTES: &str = "\
<p>0.22.2</p><ul>\
<li>No personal paths inside the program: the build\u{2019}s home-folder prefix was embedded in crash messages and is now removed.</li>\
<li>What\u{2019}s New shows only this version.</li>\
<li>Help and notes use generic drive names; the notes are Markdown files; the check scripts are no longer shipped.</li>\
<li>Search and the daemon are unchanged since 0.22.1.</li></ul>";

fn ask(req: &str) -> std::io::Result<String> {
    let mut s = UnixStream::connect(std::env::var("FA_SOCKET").unwrap_or(SOCK.into()))?;
    s.write_all(req.as_bytes())?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut out = Vec::new();
    s.read_to_end(&mut out)?;
    // ponytail: non-UTF-8 filenames display lossily and won't open; fine for testing.
    Ok(String::from_utf8_lossy(&out).into_owned())
}

// The window's own settings and lists live in ~/.config/findanything/.
fn config(name: &str) -> PathBuf {
    glib::user_config_dir().join("findanything").join(name)
}

fn read_lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(config(name))
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn write_lines(name: &str, lines: &[String]) {
    let p = config(name);
    let _ = std::fs::create_dir_all(p.parent().unwrap()).and_then(|_| {
        std::fs::write(
            &p,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
    });
}

// Match options: (action, query modifier, status bar words, menu label).
const OPTIONS: &[(&str, &str, &str, &str)] = &[
    ("case", "case: ", "match case", "Match Case"),
    ("ww", "ww: ", "whole words", "Match Whole Words"),
    (
        "nopunc",
        "nopunc: ",
        "ignoring punctuation",
        "Ignore Punctuation",
    ),
    (
        "noaccents",
        "noaccents: ",
        "ignoring accents",
        "Ignore Accents",
    ),
];
// Where in the name a word must match: (state, query modifier, status bar words, menu label).
const POSITIONS: &[(&str, &str, &str, &str)] = &[
    ("anywhere", "", "", "Anywhere in Name"),
    ("start", "startwith: ", "start of name", "Start of Name"),
    ("end", "endwith: ", "end of name", "End of Name"),
    ("whole", "wfn: ", "whole name", "Whole Name"),
];
const FILTERS: &[(&str, &str)] = &[
    ("Everything", ""),
    ("Images", "image:"),
    ("Audio", "audio:"),
    ("Video", "video:"),
    ("Documents", "doc:"),
    ("Archives", "zip:"),
    ("Programs", "exe:"),
    ("Folders", "folder:"),
];
const SETTINGS: &[&str] = &[
    "col-folder",
    "col-size",
    "col-type",
    "col-date",
    "col-order",
    "folder-short",
    "thumb-size",
    "zebra",
    "search-delay",
    "case",
    "ww",
    "nopunc",
    "noaccents",
    "position",
    "preview",
    "grid",
    "log-level",
    "log-max-mb",
];
const THUMB: i32 = 256; // freedesktop "large" thumbnails
const WORKERS: usize = 4; // thumbnail makers, and content readers

thread_local! {
    // Modified times of the listed results, read on first use (rows on screen, or a date sort).
    static MTIMES: RefCell<HashMap<String, Option<i64>>> = RefCell::default();
}

fn mtime(p: &str) -> Option<i64> {
    MTIMES.with(|m| {
        *m.borrow_mut()
            .entry(p.to_string())
            .or_insert_with(|| std::fs::metadata(p).ok().map(|m| m.mtime()))
    })
}

thread_local! {
    // (is a folder, size in bytes or item count) of the listed results, read like the dates.
    static SIZES: RefCell<HashMap<String, Option<(bool, u64)>>> = RefCell::default();
}

const COUNT_CAP: u64 = 10_000; // a folder's items are counted up to here, so a huge folder can't stall the list

fn size_info(p: &str) -> Option<(bool, u64)> {
    let path = plain(p);
    SIZES.with(|m| {
        *m.borrow_mut().entry(p.to_string()).or_insert_with(|| {
            if p.ends_with('/') {
                std::fs::read_dir(path)
                    .ok()
                    .map(|d| (true, d.take(COUNT_CAP as usize + 1).count() as u64))
            } else {
                std::fs::metadata(path).ok().map(|m| (false, m.len()))
            }
        })
    })
}

// Like the file manager: "3 items" for a folder, "1.2 MB" for a file.
fn size_of(p: &str) -> String {
    match size_info(p) {
        Some((true, n)) if n > COUNT_CAP => format!("{}+ items", group(COUNT_CAP)),
        Some((true, 1)) => "1 item".into(),
        Some((true, n)) => format!("{} items", group(n)),
        Some((false, b)) => glib::format_size(b).into(),
        None => String::new(),
    }
}

// Folders first, then by size (items for a folders).
fn size_cmp(a: &str, b: &str) -> Ordering {
    let k = |p: &str| size_info(p).map_or((2, 0), |(d, n)| (if d { 0 } else { 1 }, n));
    k(a).cmp(&k(b)).then_with(|| nat(&name_of(a), &name_of(b)))
}

fn name_cmp(a: &str, b: &str) -> Ordering {
    nat(&name_of(a), &name_of(b)).then_with(|| nat(a, b))
}

fn folder_cmp(a: &str, b: &str) -> Ordering {
    nat(&folder_of(a), &folder_of(b)).then_with(|| nat(&name_of(a), &name_of(b)))
}

fn type_cmp(a: &str, b: &str) -> Ordering {
    nat(&ext_of(a), &ext_of(b)).then_with(|| nat(&name_of(a), &name_of(b)))
}

fn date_cmp(a: &str, b: &str) -> Ordering {
    mtime(plain(a)).cmp(&mtime(plain(b)))
}

fn clear_meta() {
    MTIMES.with(|m| m.borrow_mut().clear());
    SIZES.with(|m| m.borrow_mut().clear());
}

type Meta = (
    HashMap<String, Option<i64>>,
    HashMap<String, Option<(bool, u64)>>,
);

// Sorts results like the column `by` (id, descending), on a worker thread: on the main thread 100k
// rows held the window 0.7 s (name) to 2.6 s (size, every file stat'ed). The worker's own date and
// size caches fill as it reads the disk and go back with the rows (set_meta), so nothing is read twice.
fn sort_rows(mut rows: Vec<String>, by: Option<(String, bool)>) -> (Vec<String>, Option<Meta>) {
    let Some((id, desc)) = by else {
        return (rows, None);
    };
    let cmp = match id.as_str() {
        "folder" => folder_cmp,
        "size" => size_cmp,
        "type" => type_cmp,
        "date" => date_cmp,
        _ => name_cmp,
    };
    clear_meta(); // a pool thread may hold an older search's
    rows.sort_by(|a, b| if desc { cmp(b, a) } else { cmp(a, b) });
    (rows, Some((MTIMES.take(), SIZES.take())))
}

fn set_meta(m: Option<Meta>) {
    match m {
        Some((t, s)) => {
            MTIMES.set(t);
            SIZES.set(s);
        }
        None => clear_meta(),
    }
}

// Thumbnails: the shared freedesktop cache (~/.cache/thumbnails, also filled by the file manager),
// made on worker threads when missing. Grid cells queue themselves when bound (GridView binds far
// more than it shows); the nearest to the visible part go first, top left first; cells more than
// two screens away wait until scrolled closer, and a cell scrolled past is dropped.
thread_local! {
    static THUMB_QUEUE: RefCell<Vec<(String, glib::WeakRef<gtk::Image>)>> = RefCell::default();
    static THUMB_BUSY: Cell<usize> = const { Cell::new(0) };
    static THUMB_VIEW: RefCell<Option<gtk::Widget>> = RefCell::default(); // the grid's scrolled window
}

// One per core (4 to 8): decoding big JPEGs is CPU work.
fn thumb_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(4, 8)
}

// How far a cell is from the visible part of the grid, in screen heights (0: on screen). None
// while the grid isn't shown.
fn distance(img: &gtk::Image) -> Option<f32> {
    THUMB_VIEW.with(|v| {
        let v = v.borrow();
        let (v, h) = v.as_ref().map(|v| (v, v.height() as f32))?;
        let b = img
            .compute_bounds(v)
            .filter(|b| h > 0.0 && b.height() > 0.0 && v.is_mapped())?; // zero size: not laid out yet
        Some((-(b.y() + b.height())).max(b.y() - h).max(0.0) / h)
    })
}

fn want_thumb(img: &gtk::Image, path: &str) {
    img.set_widget_name(path);
    THUMB_QUEUE.with(|q| q.borrow_mut().push((path.to_string(), img.downgrade())));
}

// The image shows `p` while its widget name is `p` (set when queued, cleared on unbind). Not the
// tooltip: results have none since 0.21.1, which dropped every thumbnail.
fn shows(img: &gtk::Image, p: &str) -> bool {
    img.widget_name() == p
}

fn pump_thumbs() {
    while THUMB_BUSY.get() < thumb_workers() {
        let next = THUMB_QUEUE.with(|q| {
            let mut q = q.borrow_mut();
            q.retain(|(p, w)| w.upgrade().is_some_and(|i| shows(&i, p)));
            let near = q
                .iter()
                .enumerate()
                .filter_map(|(i, (_, w))| Some((i, distance(&w.upgrade()?)?)))
                .filter(|x| x.1 <= 2.0);
            let i = near.min_by(|a, b| a.1.total_cmp(&b.1))?.0;
            Some(q.remove(i))
        });
        let Some((p, w)) = next else { return };
        let Some(img) = w.upgrade() else { continue };
        THUMB_BUSY.set(THUMB_BUSY.get() + 1);
        glib::spawn_future_local(async move {
            let q = p.clone();
            let png = gio::spawn_blocking(move || thumbnail(&q))
                .await
                .ok()
                .flatten();
            THUMB_BUSY.set(THUMB_BUSY.get() - 1);
            if let Some(t) = png
                .filter(|_| shows(&img, &p))
                .and_then(|f| gdk::Texture::from_filename(f).ok())
            {
                img.set_paintable(Some(&t));
            }
            pump_thumbs();
        });
    }
}

// The cached thumbnail for `p` if it is current, else a new one saved to the cache (on a worker thread).
fn thumbnail(p: &str) -> Option<PathBuf> {
    use gtk::gdk_pixbuf::Pixbuf;
    let base = glib::user_cache_dir().join("thumbnails");
    if Path::new(p).starts_with(&base) {
        return Some(p.into()); // a thumbnail itself
    }
    let uri = gio::File::for_path(p).uri();
    let name = format!(
        "{}.png",
        glib::compute_checksum_for_string(glib::ChecksumType::Md5, uri.as_str())?
    );
    let mtime = std::fs::metadata(p).ok()?.mtime().to_string();
    for size in ["large", "x-large", "normal"] {
        let f = base.join(size).join(&name);
        if Pixbuf::from_file(&f)
            .ok()
            .and_then(|t| t.option("tEXt::Thumb::MTime"))
            .as_deref()
            == Some(mtime.as_str())
        {
            return Some(f);
        }
    }
    // ponytail: no fail cache; a file that can't be thumbnailed is tried again next session.
    let pb = make_thumb(p, &uri)?;
    let dir = base.join("large");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .ok()?;
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".fa-{}-{}.png",
        std::process::id(),
        N.fetch_add(1, Relaxed)
    ));
    let saved = pb
        .savev(
            &tmp,
            "png",
            &[
                ("tEXt::Thumb::URI", uri.as_str()),
                ("tEXt::Thumb::MTime", &mtime),
            ],
        )
        .is_ok()
        && std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).is_ok()
        && std::fs::rename(&tmp, dir.join(&name)).is_ok();
    if !saved {
        let _ = std::fs::remove_file(&tmp);
    }
    saved.then(|| dir.join(name))
}

// Pictures are scaled here; everything else goes to the system's thumbnailer for its type.
fn make_thumb(p: &str, uri: &str) -> Option<gtk::gdk_pixbuf::Pixbuf> {
    use gtk::gdk_pixbuf::Pixbuf;
    let (ct, _) = gio::content_type_guess(Some(p), None);
    if let Some(pb) = ct
        .starts_with("image/")
        .then(|| Pixbuf::from_file_at_scale(p, THUMB, THUMB, true).ok())
        .flatten()
    {
        return Some(pb);
    }
    let exec = thumbnailer(&ct)?;
    static N: AtomicU64 = AtomicU64::new(0);
    let out = glib::tmp_dir().join(format!(
        "fa-thumb-{}-{}.png",
        std::process::id(),
        N.fetch_add(1, Relaxed)
    ));
    let quote = |s: &str| glib::shell_quote(s).to_string_lossy().into_owned();
    let (mut cmd, mut it) = (String::new(), exec.chars());
    while let Some(c) = it.next() {
        match (c, (c == '%').then(|| it.next()).flatten()) {
            ('%', Some('s')) => cmd += &THUMB.to_string(),
            ('%', Some('u')) => cmd += &quote(uri),
            ('%', Some('i')) => cmd += &quote(p),
            ('%', Some('o')) => cmd += &quote(&out.to_string_lossy()),
            ('%', Some('%')) => cmd.push('%'),
            ('%', _) => {}
            (c, _) => cmd.push(c),
        }
    }
    let argv = glib::shell_parse_argv(&cmd).ok()?;
    let null = std::process::Stdio::null;
    let mut child = std::process::Command::new(argv.first()?)
        .args(&argv[1..])
        .stdin(null())
        .stdout(null())
        .stderr(null())
        .spawn()
        .ok()?;
    let t = Instant::now();
    let ok = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.success(),
            Ok(None) if t.elapsed() < Duration::from_secs(20) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill(); // stuck (a damaged video, a drive that went away)
                let _ = child.wait();
                break false;
            }
        }
    };
    let pb = ok
        .then(|| Pixbuf::from_file_at_scale(&out, THUMB, THUMB, true).ok())
        .flatten();
    let _ = std::fs::remove_file(&out);
    pb
}

// The installed thumbnailers (*.thumbnailer: MimeType and Exec), user's first.
fn thumbnailer(ct: &str) -> Option<&'static str> {
    static T: OnceLock<Vec<(Vec<String>, String)>> = OnceLock::new();
    let all = T.get_or_init(|| {
        let mut v = Vec::new();
        for d in std::iter::once(glib::user_data_dir()).chain(glib::system_data_dirs()) {
            let Ok(rd) = std::fs::read_dir(d.join("thumbnailers")) else {
                continue;
            };
            for e in rd.flatten() {
                let (k, g) = (glib::KeyFile::new(), "Thumbnailer Entry");
                if k.load_from_file(e.path(), glib::KeyFileFlags::NONE)
                    .is_err()
                {
                    continue;
                }
                let (Ok(exec), Ok(types)) = (k.string(g, "Exec"), k.string_list(g, "MimeType"))
                else {
                    continue;
                };
                if k.string(g, "TryExec")
                    .is_ok_and(|t| glib::find_program_in_path(t.as_str()).is_none())
                {
                    continue;
                }
                v.push((
                    types.iter().map(|t| t.to_string()).collect(),
                    exec.to_string(),
                ));
            }
        }
        v
    });
    all.iter()
        .find(|(types, _)| types.iter().any(|t| t == ct))
        .map(|t| t.1.as_str())
}

fn thumbable(path: &str) -> bool {
    let (ct, _) = gio::content_type_guess(Some(path), None);
    !path.ends_with('/') && (ct.starts_with("image/") || thumbnailer(&ct).is_some())
}

// A content search: the daemon lists the files by name, WORKERS threads read them (as you), and
// the window's timer moves what they found into the list.
#[derive(Default)]
struct Job {
    key: String, // the query it answers
    stop: AtomicBool,
    found: Mutex<Vec<String>>,
    read: AtomicUsize,
    files: AtomicUsize, // files to read
    total: AtomicUsize, // name matches in all; only the first LIMIT are read
    done: AtomicBool,
}

fn run_job(job: &Job, names: &str, c: &query::Content) {
    if let Ok(h) = ask(&format!("find\t{LIMIT}\t{names}\n")) {
        let mut lines = h.lines();
        job.total.store(
            lines.next().and_then(|l| l.parse().ok()).unwrap_or(0),
            Relaxed,
        );
        let files: Vec<&str> = lines.collect();
        job.files.store(files.len(), Relaxed);
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..WORKERS {
                s.spawn(|| {
                    while !job.stop.load(Relaxed) {
                        let Some(f) = files.get(next.fetch_add(1, Relaxed)) else {
                            break;
                        };
                        if query::file_has(f, c) {
                            job.found.lock().unwrap().push(f.to_string());
                        }
                        job.read.fetch_add(1, Relaxed);
                    }
                });
            }
        });
    }
    job.done.store(true, Relaxed);
}

struct Ui {
    win: adw::ApplicationWindow,
    entry: gtk::SearchEntry,
    filter: gtk::DropDown,
    hidden: RefCell<Vec<String>>, // paths hidden from results, with everything under them
    model: gtk::StringList,
    sel: gtk::MultiSelection,
    stack: gtk::Stack,
    status: gtk::Label, // the results, at the left of the status bar
    dot: gtk::Box,      // on the status button: up to date (green), indexing (blue, pulsing), stopped (ring)
    state: gtk::Label,  // the status button's text: "Up to date", "Indexing DATA · 3/6", "Stopped"
    folder: gtk::Box,   // the selected result's folder, list and thumbnails
    folder_label: gtk::Label,
    pop: gtk::Popover,  // the status button's popover: daemon, progress, drives, logging, report
    pop_dot: gtk::Box,
    pop_title: gtk::Label,
    pop_sub: gtk::Label,
    pop_body: gtk::Box,           // rebuilt when what it shows changes
    pop_key: RefCell<String>,     // what pop_body shows now
    drives_text: RefCell<String>, // the daemon's last "drives" reply, fetched while the popover is open
    toasts: adw::ToastOverlay,
    preview: gtk::Picture,
    preview_label: gtk::Label,
    preview_pane: gtk::Box,
    preview_seq: Cell<u64>,
    last: RefCell<Vec<String>>,
    listed: Cell<u64>, // bumped whenever `last` changes: a sort of an older list is dropped
    sorter: gtk::ColumnViewSorter,
    total: Cell<usize>,                          // matches in all, from the daemon
    searched: RefCell<(String, String)>,         // (query, index generation) of the shown results
    found_at: Cell<Option<(Instant, Duration)>>, // when the shown results were fetched, and how long it took
    status_win: glib::WeakRef<adw::Window>,      // Daemon Status, while open
    prefs_win: glib::WeakRef<adw::Window>,       // Preferences, while open
    status_text: RefCell<String>,
    seq: Cell<u64>,
    offline: adw::StatusPage,
    start: gtk::Button,
    starting: Cell<bool>,           // pkexec launched, daemon not answering yet
    job: RefCell<Option<Arc<Job>>>, // the content search shown, running or finished
    stop: gtk::Button,              // stops it
    bar: gtk::ProgressBar,          // indexing progress, while the daemon is busy with drives
    toggle: gtk::Button,            // in the popover: Start while stopped, Stop while running
    log_icon: gtk::Image,           // on the status button while the daemon writes a log
    idx_sub: gtk::Label,            // the indexing page (drives read from scratch, search locked):
    idx_bar: gtk::ProgressBar,      // "3 of 7 drives done · now indexing DATA", and its bar
    other: adw::Banner,             // the running daemon is another version than this window
}

const SEARCH_HINT: &str = "Search filenames on every drive";
const OFFLINE_DESC: &str = "The search index lives in a background service that needs root to watch your drives.\nStart it here (may ask for your password).\nTo have it start at every boot, run install.sh once (see INSTALL.md).";

impl Ui {
    // The big page shown instead of results: not running, starting, or indexing.
    fn page(&self, title: &str, desc: &str, button: bool) {
        self.lock(false);
        self.offline.set_title(title);
        self.offline.set_description(Some(desc));
        self.start.set_visible(button);
        self.stack.set_visible_child_name("offline");
    }

    // The status button's bar and the indexing page's follow the daemon's "busy" line: the share
    // of the drives read, or a pulse when it doesn't say.
    fn progress(&self, busy: Option<&str>) {
        self.bar.set_visible(busy.is_some());
        for b in [&self.bar, &self.idx_bar] {
            match busy.and_then(drive_fraction) {
                Some(f) => b.set_fraction(f),
                None if busy.is_some() => b.pulse(),
                None => {}
            }
        }
    }

    // The search box while drives are read from scratch: off, "Waiting for index…".
    fn lock(&self, on: bool) {
        if self.entry.is_sensitive() != on {
            return;
        }
        self.entry.set_sensitive(!on);
        self.entry.set_placeholder_text(Some(if on { "Waiting for index…" } else { SEARCH_HINT }));
    }

    // The status button (dot, text, progress, logging glyph) and its popover, from the daemon's
    // status; None: the daemon isn't answering.
    fn indicator(&self, status: Option<&str>) {
        let busy = status.and_then(|s| s.lines().find_map(|l| l.strip_prefix("busy: ")));
        let (class, label) = match (status, busy) {
            (None, _) if self.starting.get() => ("off", "Starting…".to_string()),
            (None, _) => ("off", "Stopped".to_string()),
            (Some(s), Some(b)) => ("busy", busy_label(b, from_scratch(s))),
            (Some(_), None) => ("up", "Up to date".to_string()),
        };
        for d in [&self.dot, &self.pop_dot] {
            d.set_css_classes(&["fa-dot", class]);
        }
        self.state.set_label(&label);
        self.progress(busy);
        // The daemon logs until it restarts; the glyph goes as soon as the switch is off.
        self.log_icon.set_visible(
            self.text("log-level") != "off" && status.is_some_and(|s| s.lines().any(|l| l.ends_with("log on"))),
        );
        let (title, sub) = match status {
            None if self.starting.get() => ("Starting the daemon", "If asked, enter your password in the system dialog".to_string()),
            None => ("Daemon stopped", "Search is off".to_string()),
            Some(s) => ("Daemon running", format!("Up {} · {} memory", uptime(s), memory(s))),
        };
        self.pop_title.set_label(title);
        self.pop_sub.set_label(&sub);
        if self.pop.is_visible() {
            self.fill_pop(status, busy);
        }
    }

    // Below the popover's header, as design 1c: progress while indexing, totals, one row per
    // drive, then Debug logging and Report an issue.
    fn fill_pop(&self, status: Option<&str>, busy: Option<&str>) {
        let key = format!(
            "{busy:?}\n{:?}\n{}\n{}",
            status.map(|s| s.lines().filter(|l| l.contains(" entries in ") || l.starts_with("changes waiting")).collect::<Vec<_>>()),
            self.text("log-level"),
            self.drives_text.borrow()
        );
        if *self.pop_key.borrow() == key {
            return;
        }
        *self.pop_key.borrow_mut() = key;
        while let Some(c) = self.pop_body.first_child() {
            self.pop_body.remove(&c);
        }
        let Some(st) = status else { return };
        let label = |text: &str, css: &[&str]| {
            gtk::Label::builder()
                .label(text)
                .xalign(0.0)
                .css_classes(css.to_vec())
                .build()
        };
        let scratch = from_scratch(st);
        let now = busy.and_then(busy_drive);
        let cur = busy.and_then(drive_done);
        if let Some(b) = busy {
            let p = gtk::Box::new(gtk::Orientation::Vertical, 7);
            p.add_css_class("fa-progress");
            let bar = gtk::ProgressBar::builder().css_classes(["fa-bar6"]).build();
            match now {
                Some((path, k, n)) => {
                    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    let what = if scratch || !b.starts_with("checking") { "Indexing" } else { "Checking" };
                    let l = label(&format!("{what} {} · drive {k} of {n}", drive_name(path)), &["fa-bright"]);
                    l.set_hexpand(true);
                    l.set_ellipsize(pango::EllipsizeMode::Middle);
                    top.append(&l);
                    match cur {
                        Some(f) => {
                            top.append(&label(&format!("{}%", (f * 100.0) as u32), &["fa-accent", "numeric"]));
                            bar.set_fraction(f);
                        }
                        None => bar.pulse(),
                    }
                    p.append(&top);
                    p.append(&bar);
                    // "1,234 of about 5,000 files read" or "1,234 folders checked" (not the seconds on it)
                    let rest = b[b.find(" drives)").unwrap_or(0) + 8..].trim_start_matches([',', ';', ' ']);
                    let rest = rest.split(", ").next().unwrap_or(rest);
                    if !rest.is_empty() {
                        p.append(&label(rest, &["fa-small", "fa-dim", "numeric"]));
                    }
                    let segs = gtk::Box::builder().spacing(3).homogeneous(true).margin_top(3).build();
                    for i in 1..=n {
                        let f = if i < k { 1.0 } else if i == k { cur.unwrap_or(0.0) } else { 0.0 };
                        let s = gtk::ProgressBar::builder().fraction(f).css_classes(["fa-seg"]).build();
                        if i < k {
                            s.add_css_class("done");
                        }
                        segs.append(&s);
                    }
                    p.append(&segs);
                    p.append(&label(&format!("{} of {n} drives done · {} queued", k.saturating_sub(1), n.saturating_sub(k)), &["fa-small", "fa-dim"]));
                }
                None => {
                    p.append(&label(&capitalize(b.strip_prefix("indexing: ").unwrap_or(b)), &["fa-bright"]));
                    bar.pulse();
                    p.append(&bar);
                }
            }
            self.pop_body.append(&p);
        }
        let grid = gtk::Grid::builder().column_spacing(16).row_spacing(6).build();
        let mut words = st.lines().nth(1).unwrap_or_default().split(' ');
        let (items, folders) = (words.next().unwrap_or("0"), words.nth(2).unwrap_or("0"));
        let line = |p: &str| st.lines().find_map(|l| l.strip_prefix(p));
        // "0 waiting · capacity 1.7 M"; "up to 1,717,986 changes waiting (2% of memory)" is the limit
        let cap = line("change queue: ")
            .and_then(|q| q.strip_prefix("up to "))
            .and_then(|q| q.split(' ').next())
            .map_or("no limit".to_string(), |c| format!("capacity {}", short_num(c)));
        let queue = line("changes waiting: ").map(|w| format!("{} waiting · {cap}", group_num(w)));
        let rows = [
            Some(("Indexed", format!("{} items · {} folders", group_num(items), group_num(folders)))),
            queue.map(|q| ("Change queue", q)),
        ];
        for (i, (k, v)) in rows.into_iter().flatten().enumerate() {
            let k = label(k, &["fa-dim"]);
            k.set_valign(gtk::Align::Start);
            grid.attach(&k, 0, i as i32, 1, 1);
            let v = label(&v, &["fa-bright", "numeric"]);
            v.set_wrap(true);
            v.set_xalign(1.0);
            v.set_hexpand(true);
            v.set_justify(gtk::Justification::Right);
            grid.attach(&v, 1, i as i32, 1, 1);
        }
        self.pop_body.append(&grid);
        let drives = gtk::Box::new(gtk::Orientation::Vertical, 6);
        drives.append(&label("Drives", &["fa-small", "fa-heading"]));
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["fa-card"])
            .build();
        for l in self.drives_text.borrow().lines() {
            let [path, _fstype, _source, state, files, detail, _why] = l.split('\t').collect::<Vec<_>>()[..] else {
                continue;
            };
            let row = gtk::Box::builder().spacing(8).css_classes(["fa-drive"]).tooltip_text(detail).build();
            if list.first_child().is_some() {
                row.add_css_class("fa-sep");
            }
            let removable = path.starts_with("/media/") || path.starts_with("/run/media/");
            let icon = gtk::Image::from_icon_name(if removable { "drive-harddisk-usb-symbolic" } else { "drive-harddisk-symbolic" });
            icon.add_css_class("fa-dim");
            row.append(&icon);
            row.append(&label(drive_name(path), &["fa-bright"]));
            let mount = label(path, &["fa-mono"]);
            mount.set_max_width_chars(20);
            mount.set_hexpand(true);
            mount.set_ellipsize(pango::EllipsizeMode::Middle);
            row.append(&mount);
            let items = || format!("{} item{}", group_num(files), if files == "1" { "" } else { "s" });
            let (text, css) = match (state, now) {
                ("hidden", _) => ("Not mounted".to_string(), "fa-faint"),
                (_, Some((p, ..))) if p == path => {
                    let what = if scratch { "Indexing" } else { "Checking" };
                    (cur.map_or(what.to_string(), |f| format!("{what} · {}%", (f * 100.0) as u32)), "fa-accent")
                }
                ("checking", Some(_)) => ("Queued".to_string(), "fa-faint"),
                (_, Some(_)) => ("Up to date".to_string(), "fa-ok"),
                _ => (items(), "fa-dim"),
            };
            row.append(&label(&text, &["fa-small", css, "numeric"]));
            list.append(&row);
        }
        drives.append(&list);
        self.pop_body.append(&drives);
        let tools = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["fa-card"])
            .build();
        let log = gtk::Box::builder().spacing(10).css_classes(["fa-tool"]).build();
        let words = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
        words.append(&label("Debug logging", &["fa-bright"]));
        words.append(&label("Saves logs for troubleshooting", &["fa-small", "fa-dim"]));
        log.append(&words);
        let sw = gtk::Switch::builder()
            .active(self.text("log-level") != "off")
            .valign(gtk::Align::Center)
            .tooltip_text("Detailed log on or off (Preferences › Folders & Logs). Applies at once.")
            .build();
        let w = self.win.clone();
        sw.connect_active_notify(move |s| {
            w.change_action_state("log-level", &(if s.is_active() { "detailed" } else { "off" }).to_variant())
        });
        // The whole row switches it, as in the design.
        let click = gtk::GestureClick::new();
        let s2 = sw.clone();
        click.connect_released(move |_, _, _, _| s2.set_active(!s2.is_active()));
        words.add_controller(click);
        log.append(&sw);
        tools.append(&log);
        let rb = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let rl = label("Report an issue…", &["fa-bright"]);
        rl.set_hexpand(true);
        rb.append(&rl);
        let ri = gtk::Image::from_icon_name("fa-bug-symbolic");
        ri.set_pixel_size(18);
        ri.add_css_class("fa-dim");
        rb.append(&ri);
        let report = gtk::Button::builder().child(&rb).css_classes(["flat", "fa-tool", "fa-sep"]).build();
        let (w, pop) = (self.win.clone(), self.pop.clone());
        report.connect_clicked(move |_| {
            pop.popdown();
            let _ = WidgetExt::activate_action(&w, "win.report", None);
        });
        tools.append(&report);
        self.pop_body.append(&tools);
    }

    // The selected result's folder, in the list and the thumbnails (owner, 0.21.1). As in the
    // design, a result is always selected: the first one when none is.
    fn show_folder(&self) {
        if self.sel.selection().is_empty() && self.model.n_items() > 0 {
            self.sel.select_item(0, true); // selection-changed calls this again
            return;
        }
        let p = self.selected();
        self.folder.set_visible(p.is_some());
        if let Some(p) = p {
            let dir = Path::new(&p).parent().map_or(p.clone(), |d| d.display().to_string());
            self.folder_label.set_label(&dir);
            self.folder.set_tooltip_text(Some(&p));
        }
    }

    // In Preferences while it is in front (Rebuild, excluded folders), else in the main window.
    fn toast(&self, msg: &str) {
        let prefs = self
            .prefs_win
            .upgrade()
            .filter(|w| w.is_active())
            .and_then(|w| {
                w.content()?
                    .downcast::<adw::ToolbarView>()
                    .ok()?
                    .content()?
                    .downcast::<adw::ToastOverlay>()
                    .ok()
            });
        prefs
            .as_ref()
            .unwrap_or(&self.toasts)
            .add_toast(adw::Toast::new(msg));
    }

    // Status bar Report a Problem: what this window shows and asks, then the daemon's report on
    // `p` (the file that should show). Saved in the home folder, small enough to share.
    fn report(self: &Rc<Self>, p: Option<PathBuf>) {
        let q = self.query();
        let path = p.map(|p| p.display().to_string()).unwrap_or_default();
        let mut o = format!(
            "window {VERSION}\nsearch box: {:?}\nasked the daemon: {q:?}\nshown: {} rows of {} matches\n",
            self.search_text(),
            self.model.n_items(),
            self.total.get()
        );
        o += &diag::stalls(&gui_log());
        if !path.is_empty() && is_hidden(&self.hidden.borrow(), &path) {
            o += "HIDDEN in this window (Hide From Results; Preferences › Search shows it again)\n";
        }
        match ask(&format!("report\t{path}\t{q}\n")) {
            Ok(r) if !r.is_empty() => o += &r,
            Ok(_) => o += "daemon: still loading its saved index, no report\n",
            Err(e) => o += &format!("daemon: not reachable ({e})\n"),
        }
        let when = glib::DateTime::now_local()
            .and_then(|t| t.format("%Y-%m-%d_%H-%M-%S"))
            .map_or_else(|_| "now".into(), |s| s.to_string());
        let f = glib::home_dir().join(format!("FindAnything-report-{when}.txt"));
        if let Err(e) = std::fs::write(&f, &o) {
            return self.toast(&format!("Could not save the report: {e}"));
        }
        let t = adw::Toast::builder()
            .title(format!("Report saved: {} ({} KB)", f.display(), o.len().div_ceil(1024)))
            .button_label("Open")
            .timeout(15)
            .build();
        t.connect_button_clicked(glib::clone!(
            #[strong(rename_to = ui)]
            self,
            move |_| ui.open(&f)
        ));
        self.toasts.add_toast(t);
        self.research(); // the report may have re-read a folder
    }

    // Opens with the default app, like Files; asks which app only when there is none.
    fn open(self: &Rc<Self>, p: &Path) {
        let file = gio::File::for_path(p);
        let ctx = WidgetExt::display(&self.win).app_launch_context();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            if gio::AppInfo::launch_default_for_uri_future(&file.uri(), Some(&ctx))
                .await
                .is_ok()
            {
                return;
            }
            gtk::FileLauncher::new(Some(&file)).launch(
                Some(&ui.win),
                gio::Cancellable::NONE,
                glib::clone!(
                    #[strong]
                    ui,
                    move |r| {
                        if let Err(e) = r {
                            ui.toast(&format!("Could not open: {e}"))
                        }
                    }
                ),
            );
        });
    }

    // Daemon Status and Preferences: windows of their own, not tied to the main one. Escape closes
    // them (also Ctrl+W), and they never open taller than the screen.
    fn side_window(
        &self,
        title: &str,
        width: i32,
        height: i32,
    ) -> (adw::Window, adw::ToolbarView, adw::HeaderBar) {
        let tv = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        tv.add_top_bar(&header);
        let screen = self
            .win
            .surface()
            .and_then(|s| WidgetExt::display(&self.win).monitor_at_surface(&s))
            .map_or(height, |m| m.geometry().height() * 9 / 10);
        let w = adw::Window::builder()
            .title(title)
            .default_width(width)
            .default_height(height.min(screen))
            .content(&tv)
            .build();
        if let Some(app) = self.win.application() {
            w.set_application(Some(&app));
        }
        let keys = gtk::ShortcutController::new();
        keys.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::NamedAction::new("window.close")),
        ));
        w.add_controller(keys);
        w.connect_close_request(|w| {
            dlog!(
                "{:?} window: close requested",
                w.title().unwrap_or_default()
            ); // reached the app (the close bug)
            glib::Propagation::Proceed
        });
        (w, tv, header)
    }

    // The first selected result as listed (folders end in "/").
    fn first(&self) -> Option<glib::GString> {
        let s = self.sel.selection();
        (!s.is_empty())
            .then(|| self.model.string(s.minimum()))
            .flatten()
    }

    // The first selected result's path (folders lose their trailing "/").
    fn selected(&self) -> Option<String> {
        self.first().map(|p| plain(&p).to_string())
    }

    // Every selected result's path, in list order.
    fn picked(&self) -> Vec<String> {
        chosen(&self.sel)
    }

    // Ctrl+C / Ctrl+X: the selected files on the clipboard the way Files (Nautilus) pastes them.
    // A cut file stays listed until the paste moves it; live updates then drop it.
    fn put_files(&self, cut: bool) {
        let paths = self.picked();
        if paths.is_empty() {
            return;
        }
        let uris: Vec<String> = paths
            .iter()
            .map(|p| gio::File::for_path(p).uri().to_string())
            .collect();
        let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
        let list: String = uris.iter().map(|u| format!("{u}\r\n")).collect();
        let bytes = |s: String| glib::Bytes::from_owned(s.into_bytes());
        let p = gdk::ContentProvider::new_union(&[
            gdk::ContentProvider::for_bytes("x-special/gnome-copied-files", &bytes(gnome)),
            gdk::ContentProvider::for_bytes("text/uri-list", &bytes(list)),
            gdk::ContentProvider::for_bytes("text/plain;charset=utf-8", &bytes(paths.join("\n"))),
        ]);
        if let Err(e) = self.win.clipboard().set_content(Some(&p)) {
            return self.toast(&format!("Could not use the clipboard: {e}"));
        }
        let what = match paths.len() {
            1 => format!("“{}”", name_of(&paths[0])),
            n => format!("{n} items"),
        };
        self.toast(&if cut {
            format!("Cut {what}: paste in Files to move")
        } else {
            format!("Copied {what}: paste in Files")
        });
    }

    // Delete key or right-click: the selection to the Trash, after asking. Never deleted for good.
    fn trash(self: &Rc<Self>) {
        let paths = self.picked();
        let Some(first) = paths.first() else { return };
        let title = match paths.len() {
            1 => format!("Move “{}” to the Trash?", name_of(first)),
            n => format!("Move {n} Items to the Trash?"),
        };
        let mut body: Vec<String> = paths.iter().take(8).map(|p| p.to_string()).collect();
        if paths.len() > 8 {
            body.push(format!("and {} more", paths.len() - 8));
        }
        let d = adw::AlertDialog::new(Some(&title), Some(&body.join("\n")));
        d.add_responses(&[("cancel", "Cancel"), ("trash", "Move to Trash")]);
        d.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
        d.set_default_response(Some("trash"));
        d.set_close_response("cancel");
        let ui = self.clone();
        d.connect_response(None, move |_, r| {
            if r != "trash" {
                return;
            }
            let (ui, paths) = (ui.clone(), paths.clone());
            glib::spawn_future_local(async move {
                let mut failed = Vec::new();
                for p in &paths {
                    if let Err(e) = gio::File::for_path(p)
                        .trash_future(glib::Priority::DEFAULT)
                        .await
                    {
                        failed.push(format!("{}: {}", name_of(p), e.message()));
                    }
                }
                let moved = paths.len() - failed.len();
                let mut msg = match moved {
                    0 => String::new(),
                    1 => "Moved 1 item to the Trash".into(),
                    n => format!("Moved {n} items to the Trash"),
                };
                if let Some(f) = failed.first() {
                    dlog!("trash failed: {failed:?}");
                    msg = format!("{msg}{}Could not move {f}", if moved > 0 { ". " } else { "" });
                    if failed.len() > 1 {
                        msg += &format!(" (and {} more)", failed.len() - 1);
                    }
                }
                ui.toast(&msg);
                ui.research();
            });
        });
        d.present(Some(&self.win));
    }

    // Stops the daemon and starts this build's. `fresh` (Daemon Status › Rescan All): stopped
    // without saving and started with --fresh, so every drive is read from scratch. Settings,
    // hidden and excluded folders and logs stay. Else a plain restart (the version banner).
    fn restart(self: &Rc<Self>, fresh: bool) {
        for w in [self.prefs_win.upgrade(), self.status_win.upgrade()].into_iter().flatten() {
            w.close(); // the main window shows the progress
        }
        let _ = ask(if fresh { "quit-nosave\n" } else { "quit\n" });
        let ui = self.clone();
        glib::spawn_future_local(async move {
            // pkexec ends with the daemon; wait for both before starting again (a saving
            // daemon can take a few seconds).
            for i in 0..300 {
                if !ui.starting.get() && ask("status\n").is_err() {
                    break;
                }
                if i == 10 {
                    let _ = ask("quit\n"); // daemons before 0.19.1 don't know quit-nosave
                }
                glib::timeout_future(Duration::from_millis(100)).await;
            }
            ui.toast(if fresh {
                "Rescanning: every drive is read from scratch"
            } else {
                "Restarting the daemon from this build"
            });
            ui.start_daemon(fresh);
        });
    }

    // Hide From Results (as in 0.18.0): the selected results go at once; a folder takes
    // everything in it.
    fn hide(self: &Rc<Self>) {
        let picked = self.picked();
        if picked.is_empty() {
            return;
        }
        let mut h = self.hidden.borrow_mut();
        for p in picked {
            if !h.contains(&p) {
                h.push(p);
            }
        }
        write_lines("hidden.txt", &h);
        drop(h);
        self.toast("Hidden from results. Show it again in Preferences › Search");
        self.research();
    }

    // Starts the daemon through pkexec (asks for the password). `fresh`: without the saved index,
    // so every drive is read from scratch (Daemon Status › Rescan All).
    fn start_daemon(self: &Rc<Self>, fresh: bool) {
        let ui = self;
        let (fa, log) = daemon_files();
        if ui.starting.get() || ask("status\n").is_ok() {
            return ui.toast("The daemon is already running (menu › Daemon status)");
        }
        if service() {
            return ui.start_service();
        }
        let mut child = match std::process::Command::new("pkexec")
            .arg(&fa)
            .arg("daemon")
            .args(match ui.text("log-level").as_str() {
                "detailed" => Some("--debug"),
                "summary" => Some("--summary"),
                _ => None,
            })
            .arg("--log-max-mb")
            .arg(ui.number("log-max-mb").to_string())
            .arg("--log")
            .arg(&log)
            .args(fresh.then_some("--fresh"))
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return ui.toast(&format!(
                    "Could not run pkexec: {e}. Run  sudo {} daemon  instead",
                    fa.display()
                ))
            }
        };
        ui.starting.set(true);
        ui.refresh();
        // pkexec lives as long as the daemon, so its exit code says why the daemon ended.
        let ui = ui.clone();
        glib::spawn_future_local(async move {
            let code =
                gio::spawn_blocking(move || child.wait().ok().and_then(|s| s.code()))
                    .await
                    .ok()
                    .flatten();
            ui.starting.set(false);
            let msg = match code {
                Some(0) => String::new(), // stopped normally
                Some(3) => "A daemon was already running".into(),
                Some(126) => "Start cancelled: the password dialog was closed".into(),
                Some(127) => "Not authorized to start the daemon".into(),
                Some(c) => format!("The daemon failed (exit {c}); see {}", log.display()),
                None => {
                    format!("The daemon was stopped by a signal; see {}", log.display())
                }
            };
            if !msg.is_empty() {
                ui.toast(&msg);
            }
            ui.refresh();
        });
    }

    // The boot service (install.sh): systemd starts /opt/findanything/fa. Asks for the password
    // unless install.sh's polkit rule lets this user start it. A Rescan All has already deleted
    // the saved index (quit-nosave), since the service takes no --fresh.
    fn start_service(self: &Rc<Self>) {
        let mut child = match std::process::Command::new("systemctl")
            .args(["start", "findanything.service"])
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return self.toast(&format!("Could not run systemctl: {e}")),
        };
        self.starting.set(true);
        self.refresh();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let code = gio::spawn_blocking(move || child.wait().ok().and_then(|s| s.code()))
                .await
                .ok()
                .flatten();
            if code == Some(0) {
                // Started; refresh() clears `starting` once the daemon answers. If it never does
                // (it failed at once), show Stopped after 10 s.
                glib::timeout_future(Duration::from_secs(10)).await;
            } else {
                ui.toast(&format!(
                    "The service did not start (systemctl exit {}): password dialog closed, or see  journalctl -u findanything",
                    code.map_or("?".into(), |c| c.to_string())
                ));
            }
            ui.starting.set(false);
            ui.refresh();
        });
    }

    fn text(&self, name: &str) -> String {
        self.win
            .action_state(name)
            .and_then(|v| v.get())
            .unwrap_or_default()
    }

    fn number(&self, name: &str) -> i32 {
        self.win
            .action_state(name)
            .and_then(|v| v.get())
            .unwrap_or(5)
    }

    fn flag(&self, name: &str) -> bool {
        self.win
            .action_state(name)
            .and_then(|v| v.get())
            .unwrap_or(false)
    }

    fn position(&self) -> &'static (&'static str, &'static str, &'static str, &'static str) {
        let p: String = self
            .win
            .action_state("position")
            .and_then(|v| v.get())
            .unwrap_or_default();
        POSITIONS.iter().find(|x| x.0 == p).unwrap_or(&POSITIONS[0])
    }

    // The selected filter's query and the typed words.
    fn search_text(&self) -> String {
        let filter = FILTERS
            .get(self.filter.selected() as usize)
            .map_or("", |f| f.1);
        format!("{filter} {}", self.entry.text())
            .trim()
            .replace('\t', " ")
    }

    // What the daemon is asked: match options, then the filter and words. Empty: nothing to search.
    fn query(&self) -> String {
        let text = self.search_text();
        if text.is_empty() {
            return text;
        }
        let opts: String = OPTIONS
            .iter()
            .filter(|o| self.flag(o.0))
            .map(|o| o.1)
            .collect();
        format!("{opts}{}{text}", self.position().1)
    }

    // "match case, whole words" for the status bar.
    fn options_note(&self) -> String {
        let mut v: Vec<&str> = OPTIONS
            .iter()
            .filter(|o| self.flag(o.0))
            .map(|o| o.2)
            .collect();
        v.extend(Some(self.position().2).filter(|p| !p.is_empty()));
        v.join(", ")
    }

    // Results are fetched again at the next refresh even if the query is the same.
    fn research(self: &Rc<Self>) {
        *self.searched.borrow_mut() = Default::default();
        self.end_job();
        self.refresh();
    }

    // The column sort shown on the headers, as sort_rows takes it.
    fn sort_by(&self) -> Option<(String, bool)> {
        let c = self.sorter.primary_sort_column()?;
        let desc = self.sorter.primary_sort_order() == gtk::SortType::Descending;
        Some((c.id()?.to_string(), desc))
    }

    // `rows` become the shown results, with the dates and sizes read while sorting them. Rows
    // that were selected and are still there stay selected (a live update, a new sort).
    fn show_rows(&self, rows: Vec<String>, meta: Option<Meta>) {
        set_meta(meta);
        let s = self.sel.selection();
        let was: std::collections::HashSet<glib::GString> = (0..s.size() as u32)
            .filter_map(|i| self.model.string(s.nth(i)))
            .collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        diag::what("putting results in the list");
        self.model.splice(0, self.model.n_items(), &refs);
        if !was.is_empty() {
            let keep = gtk::Bitset::new_empty();
            for (i, r) in rows.iter().enumerate() {
                if was.contains(r.as_str()) {
                    keep.add(i as u32);
                }
            }
            self.sel
                .set_selection(&keep, &gtk::Bitset::new_range(0, rows.len() as u32));
        }
        diag::what("idle");
        diag::spliced();
        *self.last.borrow_mut() = rows;
        self.listed.set(self.listed.get() + 1);
        self.show_folder();
    }

    // A header click or a finished content search: the shown rows again in the column's order
    // (show_rows keeps the selection).
    fn resort(self: &Rc<Self>) {
        let by = self.sort_by();
        if by.is_none() {
            return;
        }
        let (rows, n) = (self.last.borrow().clone(), self.listed.get());
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let (t, what) = (Instant::now(), format!("{by:?}"));
            let Ok((rows, meta)) = gio::spawn_blocking(move || sort_rows(rows, by)).await else {
                return;
            };
            if ui.listed.get() != n {
                return dlog!("sort by {what} dropped: the list changed meanwhile");
            }
            let (sort, t) = (t.elapsed(), Instant::now());
            ui.show_rows(rows, meta);
            dlog!("sort by {what}: {} rows sorted {sort:.1?} on a worker, list update {:.1?}", ui.last.borrow().len(), t.elapsed());
        });
    }

    // The list or the thumbnail grid, whichever is chosen.
    fn show_results(&self) {
        self.stack
            .set_visible_child_name(if self.flag("grid") { "grid" } else { "results" });
    }

    fn end_job(&self) {
        if let Some(j) = self.job.take() {
            j.stop.store(true, Relaxed);
        }
        self.stop.set_visible(false);
    }

    // Starts a content search for `q`, unless it is the one shown. Not live: it runs again only
    // when the query changes (re-reading every file at each index change would never end).
    fn content_search(self: &Rc<Self>, q: &str) {
        if self.job.borrow().as_ref().is_some_and(|j| j.key == q) {
            return;
        }
        self.end_job();
        let (names, Some(c)) = query::split_content(q) else {
            return;
        };
        let job = Arc::new(Job {
            key: q.to_string(),
            ..Default::default()
        });
        *self.job.borrow_mut() = Some(job.clone());
        *self.searched.borrow_mut() = Default::default();
        clear_meta();
        self.model.splice(0, self.model.n_items(), &[] as &[&str]);
        self.last.borrow_mut().clear();
        self.listed.set(self.listed.get() + 1);
        self.stop.set_visible(true);
        let j = job.clone();
        std::thread::spawn(move || run_job(&j, &names, &c));
        let ui = self.clone();
        glib::timeout_add_local(Duration::from_millis(150), move || {
            if !ui
                .job
                .borrow()
                .as_ref()
                .is_some_and(|j| Arc::ptr_eq(j, &job))
            {
                return glib::ControlFlow::Break;
            }
            let done = job.done.load(Relaxed); // before taking the last finds
            let new: Vec<String> = std::mem::take(&mut *job.found.lock().unwrap())
                .into_iter()
                .filter(|h| !is_hidden(&ui.hidden.borrow(), h))
                .collect();
            if !new.is_empty() {
                let refs: Vec<&str> = new.iter().map(String::as_str).collect();
                ui.model.splice(ui.model.n_items(), 0, &refs);
                ui.last.borrow_mut().extend(new);
                ui.listed.set(ui.listed.get() + 1);
            }
            ui.status.set_label(&ui.content_line());
            ui.stop.set_visible(!done);
            if done {
                ui.resort(); // found in reading order; in the column's order once complete
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    // "Reading file contents… 1,200 of 5,000 files  ·  12 matches"
    fn content_line(&self) -> String {
        let Some(j) = self.job.borrow().clone() else {
            return String::new();
        };
        let (read, files, total) = (
            j.read.load(Relaxed),
            j.files.load(Relaxed),
            j.total.load(Relaxed),
        );
        let found = self.last.borrow().len();
        let mut line = match (j.done.load(Relaxed), j.stop.load(Relaxed)) {
            (false, _) if files == 0 => "Finding files to read…".to_string(),
            (false, _) => format!(
                "Reading file contents… {} of {} files",
                group(read as u64),
                group(files as u64)
            ),
            (true, true) => format!(
                "Stopped after {} of {} files",
                group(read as u64),
                group(files as u64)
            ),
            (true, false) => format!("Read {} files", group(files as u64)),
        };
        line += &format!(
            "  ·  {} match{}",
            group(found as u64),
            if found == 1 { "" } else { "es" }
        );
        if total > files {
            line += &format!(
                "  ·  only the first {} of {} files were read: add name words to narrow",
                group(files as u64),
                group(total as u64)
            );
        }
        let note = self.options_note();
        if !note.is_empty() {
            line += &format!("  ·  {note}");
        }
        line
    }

    fn running(&self, on: bool) {
        let (icon, label, action, tip) = if on {
            ("media-playback-stop-symbolic", "Stop", "win.stop", "Stop the daemon. It keeps running when this window is closed.")
        } else {
            ("media-playback-start-symbolic", "Start", "win.start", "Start the daemon (may ask for your password)")
        };
        if let Some(c) = self.toggle.child().and_downcast::<adw::ButtonContent>() {
            c.set_icon_name(icon);
            c.set_label(label);
        }
        self.toggle.set_action_name(Some(action));
        self.toggle.set_tooltip_text(Some(tip));
    }

    // The preview pane shows the selected picture, read and scaled down asynchronously.
    fn show_preview(self: &Rc<Self>) {
        if !self.preview_pane.is_visible() {
            return;
        }
        let n = self.preview_seq.get() + 1;
        self.preview_seq.set(n);
        self.preview.set_paintable(gdk::Paintable::NONE);
        let Some(p) = self.selected() else {
            return self
                .preview_label
                .set_label("Select a picture to preview it");
        };
        // The path is on the status line; the pane shows the picture, or why there is none.
        let (ct, _) = gio::content_type_guess(Some(p.as_str()), None);
        if !ct.starts_with("image/") || self.first().is_some_and(|o| o.ends_with('/')) {
            return self.preview_label.set_label(&format!(
                "No preview for {}",
                gio::content_type_get_description(&ct)
            ));
        }
        self.preview_label.set_label("");
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let r = match gio::File::for_path(&p)
                .read_future(glib::Priority::DEFAULT)
                .await
            {
                Ok(s) => {
                    gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale_future(&s, 1024, 1024, true).await
                }
                Err(e) => Err(e),
            };
            if ui.preview_seq.get() != n {
                return;
            }
            match r {
                Ok(pb) => ui
                    .preview
                    .set_paintable(Some(&gdk::Texture::for_pixbuf(&pb))),
                Err(e) => ui
                    .preview_label
                    .set_label(&format!("Could not load: {e}")),
            }
        });
    }

    // Asks the daemon for status + results off the main thread; the newest request wins.
    // Results are fetched again when the query changed, or when the index changed and enough
    // time has passed: 20× the last search (at least 2 s), or 30 s while the window isn't in use.
    // Any file change bumps the index (a browser cache writes all the time), so this keeps an
    // open window from searching non-stop.
    fn refresh(self: &Rc<Self>) {
        let q = self.query();
        let content = query::split_content(&q).1.is_some();
        let shown_q = q.clone();
        let n = self.seq.get() + 1;
        self.seq.set(n);
        let ui = self.clone();
        let searched = self.searched.borrow().clone();
        let every = |took: Duration| {
            if self.win.is_active() {
                (took * 20).max(Duration::from_secs(2))
            } else {
                Duration::from_secs(30)
            }
        };
        let wait = self
            .found_at
            .get()
            .is_some_and(|(t, took)| q == searched.0 && t.elapsed() < every(took));
        let typed = diag::TYPED.get();
        let want_drives = self.pop.is_visible();
        glib::spawn_future_local(async move {
            let (status, hits, drives) = gio::spawn_blocking(move || {
                let t = Instant::now();
                let status = ask("status\n");
                let drives = want_drives.then(|| ask("drives\n").ok()).flatten();
                let st_ms = t.elapsed();
                let hits = match &status {
                    Ok(_) if content => None, // the content search asks for itself
                    Ok(st) if q.is_empty() => Some((q, generation(st), String::new(), Duration::ZERO)),
                    Ok(_) if wait => None,
                    Ok(st) if (q.clone(), generation(st)) != searched => {
                        let t = Instant::now();
                        let h = ask(&format!("find\t{LIMIT}\t{q}\n")).ok();
                        dlog!("search #{n} {q:?}: status {st_ms:.1?}, find round trip {:.1?}, {} bytes", t.elapsed(), h.as_ref().map_or(0, |h| h.len()));
                        h.map(|h| (q, generation(st), h, t.elapsed()))
                    }
                    _ => None,
                };
                (status, hits, drives)
            })
            .await
            .unwrap_or((Err(std::io::ErrorKind::Other.into()), None, None));
            if let Some(d) = drives {
                *ui.drives_text.borrow_mut() = d;
            }
            if ui.seq.get() != n {
                if hits.is_some() {
                    dlog!("search #{n} dropped: #{} started meanwhile", ui.seq.get());
                }
                return;
            }
            let Ok(status) = status else {
                ui.other.set_revealed(false);
                ui.running(false);
                ui.indicator(None);
                ui.status.set_label("");
                if ui.starting.get() {
                    ui.page("Starting the Daemon…", "If asked, enter your password in the system dialog.\nThen it loads the index; progress shows here.", false);
                } else {
                    ui.page("Daemon Not Running", OFFLINE_DESC, true);
                }
                return;
            };
            ui.starting.set(false);
            ui.running(true);
            ui.indicator(Some(&status));
            // The log setting goes to the daemon whenever it differs (the boot service starts with
            // it off). ponytail: a new size cap alone waits for the next level change or start.
            let want = ui.text("log-level");
            if status.lines().find_map(|l| l.strip_suffix(" log on")).unwrap_or("off") != want {
                let _ = ask(&format!("log\t{want}\t{}\n", ui.number("log-max-mb")));
            }
            // Builds may not work together: say so when the daemon is another version. The boot
            // service only updates through install.sh, so no Restart button there.
            let theirs = daemon_version(&status);
            if service() {
                ui.other.set_title(&format!("The background service is version {theirs}, this window is {VERSION}. They may not work together: run install.sh in this window's build folder to update the service."));
                ui.other.set_button_label(None);
            } else {
                ui.other.set_title(&format!("The daemon running is version {theirs}, this window is {VERSION}. They may not work together: restart the daemon from this build."));
                ui.other.set_button_label(Some("Restart Daemon"));
            }
            ui.other.set_revealed(theirs != VERSION);
            let busy = status
                .lines()
                .find_map(|l| l.strip_prefix("busy: "))
                .map(String::from);
            if from_scratch(&status) {
                let b = busy
                    .or_else(|| status.lines().nth(1).map(String::from))
                    .unwrap_or_default();
                ui.idx_sub.set_label(&match busy_drive(&b) {
                    Some((p, k, n)) => format!("{} of {n} drives done · now indexing {}", k - 1, drive_name(p)),
                    None => capitalize(b.strip_prefix("indexing: ").unwrap_or(&b)),
                });
                ui.lock(true);
                ui.stack.set_visible_child_name("indexing");
                ui.status.set_label("Search available when indexing finishes");
                *ui.status_text.borrow_mut() = status;
                return;
            }
            ui.lock(false);
            if matches!(ui.stack.visible_child_name().as_deref(), Some("offline" | "indexing")) {
                ui.show_results();
                ui.entry.grab_focus(); // the Start button had it, and is gone
            }
            if content {
                ui.content_search(&shown_q);
                ui.status.set_label(&ui.content_line());
                *ui.status_text.borrow_mut() = status;
                return;
            }
            ui.end_job();
            if let Some((q, generation, hits, took)) = hits {
                let mut lines = hits.lines();
                let total: usize = lines.next().and_then(|l| l.parse().ok()).unwrap_or(0);
                let all: Vec<&str> = lines.collect();
                let hits: Vec<String> = all
                    .iter()
                    .filter(|h| !is_hidden(&ui.hidden.borrow(), h))
                    .map(|h| h.to_string())
                    .collect();
                let hidden = all.len() - hits.len();
                let by = ui.sort_by();
                let t = Instant::now();
                let (hits, meta) = match by.clone() {
                    Some(b) => gio::spawn_blocking(move || sort_rows(hits, Some(b)))
                        .await
                        .unwrap_or_default(),
                    None => (hits, None),
                };
                let sort = t.elapsed();
                if ui.seq.get() != n {
                    return dlog!("search #{n} dropped while sorting: #{} started meanwhile", ui.seq.get());
                }
                ui.found_at.set(Some((Instant::now(), took + sort))); // a slow sort refreshes less often too
                ui.total.set(total.saturating_sub(hidden));
                let same = *ui.last.borrow() == hits;
                let t = Instant::now();
                if !same {
                    ui.show_rows(hits, meta);
                }
                if ui.sort_by() != by {
                    ui.resort(); // a header was clicked while this sorted
                }
                dlog!("show #{n} {shown_q:?}: {total} matches, {} rows{}, sorted {sort:.1?} on a worker, list update {:.1?}, {:.1?} since the last keystroke", ui.last.borrow().len(), if same { " (unchanged)" } else { "" }, t.elapsed(), typed.map_or(Duration::ZERO, |t| t.elapsed()));
                *ui.searched.borrow_mut() = (q, generation);
            }
            ui.status.set_label(&summary(
                ui.last.borrow().len(),
                ui.total.get(),
                !ui.query().is_empty(),
            ));
            *ui.status_text.borrow_mut() = status;
        });
    }
}

// "checking /mnt/x (6 of 7 drives), 1,234 folders checked" -> how many drives are done: 5/7.
// All drives: the ones done plus the current one's share.
fn drive_fraction(busy: &str) -> Option<f64> {
    let (_, k, n) = busy_drive(busy)?;
    Some(((k as f64 - 1.0 + drive_done(busy).unwrap_or(0.0)) / n as f64).clamp(0.0, 1.0))
}

// The current drive's share: "…, 1,234 of about 5,000 files read" -> 0.2468, at most 0.99
// (the count is from the last save). None when the daemon has no count for it.
fn drive_done(busy: &str) -> Option<f64> {
    let (a, b) = busy[..busy.find(" files read")?].rsplit_once(" of about ")?;
    let num = |s: &str| s.rsplit(' ').next()?.replace(',', "").parse::<f64>().ok();
    let (a, b) = (num(a)?, num(b)?);
    (b > 0.0).then(|| (a / b).min(0.99))
}

// Drives read from scratch (first index, Rescan All): search is locked until they are done, as
// results would be incomplete. A restart's check against the saved index doesn't lock it.
fn from_scratch(status: &str) -> bool {
    let startup = status.lines().find_map(|l| l.strip_prefix("startup: "));
    status.lines().next().is_some_and(|l| l.ends_with("starting"))
        || status.lines().any(|l| l.starts_with("busy: "))
            && startup.is_some_and(|s| !s.starts_with("loaded") && !s.contains("ready in"))
}

// "1,717,986" -> "1.7 M", "250,000" -> "250 K"
fn short_num(n: &str) -> String {
    let Ok(v) = n.replace(',', "").parse::<f64>() else {
        return n.to_string();
    };
    let (v, u) = match v {
        v if v >= 1e6 => (v / 1e6, " M"),
        v if v >= 1e3 => (v / 1e3, " K"),
        v => (v, ""),
    };
    format!("{}{u}", format!("{v:.1}").trim_end_matches(".0"))
}

fn is_hidden(hidden: &[String], path: &str) -> bool {
    let p = plain(path);
    hidden.iter().any(|h| {
        p == h
            || p.strip_prefix(h.as_str())
                .is_some_and(|r| r.starts_with('/'))
    })
}

// The selected results' paths, in list order (folders lose their trailing "/").
fn chosen(sel: &gtk::MultiSelection) -> Vec<String> {
    let s = sel.selection();
    (0..s.size() as u32)
        .filter_map(|i| sel.item(s.nth(i)).and_downcast::<gtk::StringObject>())
        .map(|o| plain(&o.string()).to_string())
        .collect()
}

// Folder results end in "/" (from the daemon); the path without it.
fn plain(p: &str) -> &str {
    if p.len() > 1 {
        p.strip_suffix('/').unwrap_or(p)
    } else {
        p
    }
}

fn generation(status: &str) -> String {
    status
        .lines()
        .nth(1)
        .and_then(|l| l.rsplit("generation ").next())
        .unwrap_or("")
        .to_string()
}

fn entries(status: &str) -> u64 {
    status
        .lines()
        .nth(1)
        .and_then(|l| l.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

// "up 7385s" in the status's first line -> "2 h 3 min"
fn uptime(status: &str) -> String {
    let s = status
        .lines()
        .next()
        .and_then(|l| l.split("up ").nth(1))
        .and_then(|r| r.split('s').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    ago(s)
}

fn ago(s: u64) -> String {
    match s {
        0..=89 => format!("{s} s"),
        90..=5399 => format!("{} min", (s + 30) / 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

// The results, at the left of the status bar; the daemon's state is on the status button.
fn summary(shown: usize, total: usize, searching: bool) -> String {
    match (searching, total) {
        (false, _) => "Type to search".to_string(),
        (true, 1) => "1 result".to_string(),
        (true, t) if t > shown => format!(
            "first {} of {} results (type more to narrow)",
            group(shown as u64),
            group(t as u64)
        ),
        (true, t) => format!("{} results", group(t as u64)),
    }
}

// "Find Anything 0.19.2, up 60s, memory 1712345 kB" -> "1.7 GB"
fn memory(status: &str) -> String {
    let kb: u64 = status
        .lines()
        .next()
        .and_then(|l| l.split("memory ").nth(1))
        .and_then(|m| m.split(' ').next()?.parse().ok())
        .unwrap_or(0);
    match kb {
        1_000_000.. => format!("{:.1} GB", kb as f64 / 1e6),
        _ => format!("{} MB", kb / 1000),
    }
}

// "checking /mnt/x (6 of 7 drives), 12 folders checked" -> ("/mnt/x", 6, 7)
fn busy_drive(busy: &str) -> Option<(&str, usize, usize)> {
    let end = busy.find(" drives)")?;
    let open = busy[..end].rfind(" (")?;
    let (k, n) = busy[open + 2..end].split_once(" of ")?;
    let path = &busy[busy[..open].find('/')?..open];
    Some((path, k.parse().ok()?, n.parse().ok().filter(|n| *n > 0)?))
}

// The status button's text while the daemon is busy: "Indexing DATA · 3/6" (read from
// scratch, or a rebuild), "Checking DATA · 3/6" (a restart's check against the saved index).
fn busy_label(busy: &str, scratch: bool) -> String {
    let what = if scratch || !busy.starts_with("checking") { "Indexing" } else { "Checking" };
    match busy_drive(busy) {
        Some((p, k, n)) => format!("{what} {} · {k}/{n}", drive_name(p)),
        None => match busy.strip_prefix("indexing: ") {
            Some(r) => capitalize(r),
            None => what.to_string(),
        },
    }
}

// "/" -> "System", "/mnt/drives/DATA" -> "DATA"
fn drive_name(path: &str) -> &str {
    match path.rsplit('/').next() {
        Some(n) if !n.is_empty() => n,
        _ => "System",
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// Column texts and sort orders, from a result path (folders end in "/").
fn name_of(p: &str) -> String {
    let p = plain(p);
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or(p.into())
}

fn folder_of(p: &str) -> String {
    Path::new(plain(p))
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default()
}

thread_local! {
    static SHORT_FOLDER: Cell<bool> = const { Cell::new(false) }; // Folder column shows its last two folders
}

// The Folder column's text: the whole path, or "…/me/Documents" (sorting always uses the whole path).
fn folder_text(p: &str) -> String {
    let f = folder_of(p);
    let parts: Vec<&str> = f.split('/').filter(|s| !s.is_empty()).collect();
    if !SHORT_FOLDER.get() || parts.len() <= 2 {
        return f;
    }
    format!("…/{}", parts[parts.len() - 2..].join("/"))
}

fn type_of(p: &str) -> String {
    if p.ends_with('/') {
        return "Folder".into();
    }
    gio::content_type_get_description(&gio::content_type_guess(Some(p), None).0).into()
}

// "10/9/2026, 10:08 PM" in a 12-hour locale (the locale's date, then the time), "10/09/2026 22:08" otherwise.
fn date_of(p: &str) -> String {
    let d = mtime(plain(p)).and_then(|t| glib::DateTime::from_unix_local(t).ok());
    let twelve = d
        .as_ref()
        .and_then(|d| d.format("%p").ok())
        .is_some_and(|p| !p.is_empty());
    d.and_then(|d| {
        d.format(if twelve { "%x, %-I:%M %p" } else { "%x %H:%M" })
            .ok()
    })
    .map(String::from)
    .unwrap_or_default()
}

fn nat(a: &str, b: &str) -> Ordering {
    query::natural(a.as_bytes(), b.as_bytes())
}

// Type order: folders, then by extension.
fn ext_of(p: &str) -> String {
    if p.ends_with('/') {
        return String::new();
    }
    name_of(p)
        .rsplit_once('.')
        .map_or(".".into(), |(_, e)| format!(".{}", e.to_lowercase()))
}

// The icon the file manager shows: the icon theme's folder (home and the user's Documents,
// Music... folders have their own), or the file type's icon guessed from its name (nothing is read).
fn icon_of(p: &str) -> gio::Icon {
    if !p.ends_with('/') {
        return gio::content_type_get_icon(&gio::content_type_guess(Some(p), None).0);
    }
    let path = plain(p);
    let special = if Path::new(path) == glib::home_dir() {
        "user-home"
    } else {
        use glib::UserDirectory as D;
        [
            (D::Desktop, "user-desktop"),
            (D::Documents, "folder-documents"),
            (D::Downloads, "folder-download"),
            (D::Music, "folder-music"),
            (D::Pictures, "folder-pictures"),
            (D::PublicShare, "folder-publicshare"),
            (D::Templates, "folder-templates"),
            (D::Videos, "folder-videos"),
        ]
        .into_iter()
        .find(|(d, _)| glib::user_special_dir(*d).is_some_and(|x| x == Path::new(path)))
        .map_or("folder", |x| x.1)
    };
    gio::ThemedIcon::from_names(&[special, "folder"]).upcast()
}

// A result cell (list row or grid cell): right-click menu and dragging out as files.
fn controls(w: &gtk::Box, item: &gtk::ListItem, sel: &gtk::MultiSelection) {
    // Right-click: select the result (a selected one keeps the whole selection), then show the
    // row menu on it.
    let click = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .build();
    let (weak, s) = (item.downgrade(), sel.clone());
    click.connect_pressed(move |g, _, x, y| {
        let Some(item) = weak.upgrade() else { return };
        if !item.is_selected() {
            s.select_item(item.position(), true);
        }
        let w = g.widget().unwrap();
        let pop = gtk::PopoverMenu::from_model(Some(&row_menu()));
        pop.set_parent(&w);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.set_has_arrow(false);
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
    });
    w.add_controller(click);
    // Drag a result into another app as a file; a selected one takes the whole selection along.
    let drag = gtk::DragSource::builder()
        .actions(gdk::DragAction::COPY)
        .build();
    let (weak, sel) = (item.downgrade(), sel.clone());
    drag.connect_prepare(move |_, _, _| {
        let item = weak.upgrade()?;
        let paths = if item.is_selected() {
            chosen(&sel)
        } else {
            let p = item.item().and_downcast::<gtk::StringObject>()?.string();
            vec![plain(&p).to_string()]
        };
        let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
        let files = gdk::FileList::from_array(&files);
        Some(gdk::ContentProvider::for_value(&files.to_value()))
    });
    w.add_controller(drag);
}

thread_local! {
    static TILE: Cell<i32> = const { Cell::new(128) }; // picture size in the grid, Ctrl + wheel: 64 to THUMB
}

// Sizes a grid cell's picture and name for TILE.
fn size_cell(cell: &gtk::Widget) {
    let t = TILE.get();
    if let Some(img) = cell.first_child().and_downcast::<gtk::Image>() {
        img.set_pixel_size(t);
    }
    if let Some(l) = cell.last_child().and_downcast::<gtk::Label>() {
        l.set_width_chars(t / 7);
        l.set_max_width_chars(t / 7);
    }
}

// Thumbnail grid: only the cells on screen exist (GridView recycles them). Each shows the file
// type's icon at once and its thumbnail when ready.
fn grid(sel: &gtk::MultiSelection) -> gtk::GridView {
    let fac = gtk::SignalListItemFactory::new();
    let s = sel.clone();
    fac.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let cell = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        cell.append(&gtk::Image::new());
        cell.append(
            &gtk::Label::builder()
                .ellipsize(pango::EllipsizeMode::Middle)
                .build(),
        );
        size_cell(cell.upcast_ref());
        controls(&cell, item, &s);
        item.set_child(Some(&cell));
    });
    fac.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let path = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .unwrap()
            .string();
        let cell = item.child().unwrap();
        size_cell(&cell);
        let img = cell.first_child().and_downcast::<gtk::Image>().unwrap();
        cell.last_child()
            .and_downcast::<gtk::Label>()
            .unwrap()
            .set_label(&name_of(&path));
        img.set_from_gicon(&icon_of(&path));
        if !path.ends_with('/') && thumbable(&path) {
            want_thumb(&img, &path);
        }
    });
    fac.connect_unbind(|_, item| {
        if let Some(img) = item
            .downcast_ref::<gtk::ListItem>()
            .unwrap()
            .child()
            .and_then(|c| c.first_child())
        {
            img.set_widget_name(""); // its queued thumbnail is dropped
        }
    });
    gtk::GridView::builder()
        .model(sel)
        .factory(&fac)
        .max_columns(40)
        .build()
}

fn column(
    title: &str,
    icon: bool,
    sel: &gtk::MultiSelection,
    text: fn(&str) -> String,
    cmp: fn(&str, &str) -> Ordering,
) -> gtk::ColumnViewColumn {
    let fac = gtk::SignalListItemFactory::new();
    let sel = sel.clone();
    fac.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        if icon {
            row.append(&gtk::Image::builder().pixel_size(24).build());
        }
        row.append(
            &gtk::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(pango::EllipsizeMode::Middle)
                .build(),
        );
        controls(&row, item, &sel);
        item.set_child(Some(&row));
    });
    fac.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let path = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .unwrap()
            .string();
        let row = item.child().unwrap();
        let label = row.last_child().and_downcast::<gtk::Label>().unwrap();
        label.set_label(&text(&path));
        if let Some(img) = row.first_child().and_downcast::<gtk::Image>() {
            img.set_from_gicon(&icon_of(&path));
            img.set_tooltip_text(Some(&type_of(&path)));
        }
    });
    let c = gtk::ColumnViewColumn::new(Some(title), Some(fac));
    c.set_resizable(true);
    let s = |o: &glib::Object| o.downcast_ref::<gtk::StringObject>().unwrap().string();
    c.set_sorter(Some(&gtk::CustomSorter::new(move |a, b| {
        cmp(&s(a), &s(b)).into()
    })));
    c
}

// install.sh put the daemon in as a service that starts at boot: Start goes through systemd.
fn service() -> bool {
    Path::new("/etc/systemd/system/findanything.service").exists()
}

// README.md sits next to fa-gui in every build folder.
fn readme() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("README.md")))
        .unwrap_or_default()
}

// `fa` and the daemon log sit next to `fa-gui`, so the whole folder can go back on the USB.
fn daemon_files() -> (PathBuf, PathBuf) {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or(".".into());
    let log = dir.join(format!("fa-{}.log", glib::host_name()));
    (dir.join("fa"), log)
}

// The window log sits beside the daemon log (next to `fa-gui`), so one folder holds every log;
// in ~/.config/findanything when that folder is not writable.
fn gui_log() -> PathBuf {
    let dir = daemon_files().1.with_file_name("");
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap_or_default();
    if unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0 {
        dir.join("gui-debug.log")
    } else {
        config("gui-debug.log")
    }
}

// Another fa-gui of this user from a different file (another build folder, or replaced files).
fn other_window() -> Option<(i32, PathBuf)> {
    let me = std::env::current_exe().ok()?;
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid: i32 = e.file_name().to_str()?.parse().ok()?;
        let exe = std::fs::read_link(e.path().join("exe")).ok()?; // only our own processes
        (pid != std::process::id() as i32
            && exe.file_name()?.to_str()?.starts_with("fa-gui")
            && exe != me)
            .then_some((pid, exe))
    })
}

fn other_dialog(win: &gtk::Window, pid: i32, exe: &Path) {
    let d = adw::AlertDialog::new(
        Some("Another Find Anything Window Is Open"),
        Some(&format!("It runs from {}; this window is version {VERSION}. Different builds may not work together. Close the other one?", exe.parent().unwrap_or(exe).display())),
    );
    d.add_responses(&[("quit", "Quit This One"), ("close", "Close the Other")]);
    d.set_response_appearance("close", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("close"));
    d.set_close_response("close");
    let w = win.clone();
    d.connect_response(None, move |_, r| {
        if r == "close" {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        } else if let Some(app) = w.application() {
            app.quit();
        }
    });
    d.present(Some(win));
}

// "Find Anything 0.19.1, up 7s, ..." -> "0.19.1"
fn daemon_version(status: &str) -> &str {
    status
        .strip_prefix("Find Anything ")
        .and_then(|r| r.split(',').next())
        .unwrap_or("unknown")
}

fn group_num(s: &str) -> String {
    s.parse::<u64>().map(group).unwrap_or(s.to_string())
}

// One row per drive from the daemon's "drives" reply: name, filesystem, files, how it stays current.
fn drive_rows(win: &gtk::Widget) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title("Drives")
        .description("Every mounted drive with a real filesystem is indexed automatically.")
        .build();
    let text = ask("drives\n").unwrap_or_default();
    for l in text.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        let [path, fstype, _source, state, files, detail, why] = f[..] else {
            continue;
        };
        let kind = if fstype == "fuseblk" {
            "NTFS-3g".to_string()
        } else {
            fstype.to_string()
        };
        let subtitle = [
            kind.as_str(),
            &format!("{} files", group_num(files)),
            detail,
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("  ·  ");
        let title = if path == "/" {
            "System drive (/)"
        } else {
            path
        };
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .use_markup(false)
            .build();
        let (icon, css, tip) = match state {
            "live" => (
                "emblem-ok-symbolic",
                "success",
                "Live: changes show up within a second",
            ),
            "polled" => (
                "view-refresh-symbolic",
                "warning",
                "Checked on a timer: changes show up at the next check",
            ),
            "checking" => (
                "emblem-synchronizing-symbolic",
                "accent",
                "Being checked now",
            ),
            _ => (
                "drive-removable-media-symbolic",
                "dim-label",
                "Not mounted: its files are hidden until it returns",
            ),
        };
        let img = gtk::Image::builder()
            .icon_name(icon)
            .css_classes([css])
            .tooltip_text(tip)
            .build();
        row.add_prefix(&img);
        if !why.is_empty() {
            let b = gtk::Button::builder()
                .icon_name("dialog-information-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("Why isn't this drive live?")
                .css_classes(["flat"])
                .build();
            let (why, win) = (why.to_string(), win.clone());
            b.connect_clicked(move |_| {
                let d = adw::AlertDialog::new(Some("Not Live"), Some(&why));
                d.add_response("close", "Close");
                if readme().exists() {
                    d.add_response("readme", "Open README");
                }
                d.connect_response(None, |_, r| {
                    if r == "readme" {
                        let _ = gio::AppInfo::launch_default_for_uri(
                            &gio::File::for_path(readme()).uri(),
                            gio::AppLaunchContext::NONE,
                        );
                    }
                });
                d.present(Some(&win));
            });
            row.add_suffix(&b);
        }
        g.add(&row);
    }
    if text.is_empty() {
        g.add(
            &adw::ActionRow::builder()
                .title("Daemon not running")
                .subtitle("Start it from the menu to see the drives.")
                .build(),
        );
    }
    g
}

// A row in an editable Preferences list ("title" or "title\tsubtitle"); its button takes it off
// the list and hands the rest to `changed`.
fn list_row(
    g: &adw::PreferencesGroup,
    items: &Rc<RefCell<Vec<String>>>,
    item: &str,
    icon: &str,
    tip: &str,
    changed: &Rc<dyn Fn(&[String])>,
) {
    let (title, sub) = item.split_once('\t').unwrap_or((item, ""));
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(sub)
        .use_markup(false)
        .build();
    let b = gtk::Button::builder()
        .icon_name(icon)
        .valign(gtk::Align::Center)
        .tooltip_text(tip)
        .css_classes(["flat"])
        .build();
    let (g2, items, changed, item, row2) = (
        g.clone(),
        items.clone(),
        changed.clone(),
        item.to_string(),
        row.clone(),
    );
    b.connect_clicked(move |_| {
        items.borrow_mut().retain(|i| *i != item);
        g2.remove(&row2);
        changed(&items.borrow());
    });
    row.add_suffix(&b);
    g.add(&row);
}

fn row_menu() -> gio::Menu {
    let m = gio::Menu::new();
    let open = gio::Menu::new();
    open.append(Some("Open"), Some("win.open"));
    open.append(Some("Open Containing Folder"), Some("win.open-folder"));
    m.append_section(None, &open);
    let copy = gio::Menu::new();
    copy.append(Some("Copy  (Ctrl+C)"), Some("win.copy-files"));
    copy.append(Some("Cut  (Ctrl+X)"), Some("win.cut-files"));
    copy.append(Some("Copy Path"), Some("win.copy-path"));
    m.append_section(None, &copy);
    let rest = gio::Menu::new();
    rest.append(Some("Hide From Results…  (Ctrl+H)"), Some("win.hide"));
    rest.append(Some("Move to Trash  (Delete)"), Some("win.trash"));
    m.append_section(None, &rest);
    m
}

fn add_action(win: &adw::ApplicationWindow, name: &str, f: impl Fn() + 'static) {
    let a = gio::SimpleAction::new(name, None);
    a.connect_activate(move |_, _| f());
    win.add_action(&a);
}

// A setting the menu toggles (bool) or picks (string), kept in settings.txt; `f` applies it,
// now and on every change.
fn setting(
    win: &adw::ApplicationWindow,
    name: &'static str,
    default: glib::Variant,
    f: impl Fn(&glib::Variant) + 'static,
) {
    let saved = read_lines("settings.txt").iter().find_map(|l| {
        l.strip_prefix(&format!("{name}="))
            .and_then(|v| glib::Variant::parse(Some(default.type_()), v).ok())
    });
    let v = saved.unwrap_or(default);
    let f = Rc::new(f);
    let param = (v.type_() != glib::VariantTy::BOOLEAN).then(|| v.type_().to_owned());
    let a = gio::SimpleAction::new_stateful(name, param.as_deref(), &v);
    let (w, g) = (win.clone(), f.clone());
    a.connect_change_state(move |a, v| {
        let Some(v) = v else { return };
        a.set_state(v);
        g(v);
        let lines: Vec<String> = SETTINGS
            .iter()
            .filter_map(|n| w.action_state(n).map(|s| format!("{n}={}", s.print(false))))
            .collect();
        write_lines("settings.txt", &lines);
    });
    win.add_action(&a);
    f(&v);
}

const APP_ID: &str = "dev.droid.FindAnything";

fn write_if_changed(path: &std::path::Path, bytes: &[u8]) {
    if std::fs::read(path).ok().as_deref() != Some(bytes) {
        let _ = std::fs::create_dir_all(path.parent().unwrap())
            .and_then(|_| std::fs::write(path, bytes));
    }
}

// The logo is built into fa-gui and written to the user's icon theme; a launcher pointing at this fa-gui goes
// in ~/.local/share/applications so it shows in the app menu (not when run from a USB key, which goes away).
fn install_icon() {
    let data = glib::user_data_dir();
    let dir = data.join("icons/hicolor/256x256/apps");
    write_if_changed(
        &dir.join(format!("{APP_ID}.png")),
        include_bytes!("../assets/FindAnything_256x256.png"),
    );
    // The design's icons that the system theme lacks (bug, log, indexing), found by name here too.
    for (name, svg) in [
        ("fa-bug-symbolic", &include_bytes!("../assets/fa-bug-symbolic.svg")[..]),
        ("fa-log-symbolic", include_bytes!("../assets/fa-log-symbolic.svg")),
        ("fa-indexing-symbolic", include_bytes!("../assets/fa-indexing-symbolic.svg")),
    ] {
        write_if_changed(&dir.join(format!("{name}.svg")), svg);
    }
    if let Some(d) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&d).add_search_path(&dir);
    }
    gtk::Window::set_default_icon_name(APP_ID);
    if let Ok(exe) = std::env::current_exe() {
        if !exe.starts_with("/run/media") && !exe.starts_with("/media") {
            write_if_changed(
                &data.join(format!("applications/{APP_ID}.desktop")),
                desktop_entry(&exe.to_string_lossy()).as_bytes(),
            );
        }
    }
}

fn desktop_entry(exe: &str) -> String {
    let mut q = String::new();
    for c in exe.chars() {
        match c {
            '"' | '`' | '$' | '\\' => q.extend(['\\', c]),
            '%' => q.push_str("%%"),
            _ => q.push(c),
        }
    }
    format!("[Desktop Entry]\nType=Application\nName=Find Anything\nComment=Instant filename search on every drive\n\
Exec=\"{q}\"\nIcon={APP_ID}\nStartupWMClass={APP_ID}\nTerminal=false\nCategories=Utility;FileTools;\nKeywords=search;find;files;everything;\n")
}

// The header cells of a ColumnView (GTK keeps them in a private widget type), left to right.
fn titles(view: &gtk::ColumnView) -> Vec<gtk::Widget> {
    fn walk(w: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
        let mut c = w.first_child();
        while let Some(x) = c {
            if x.type_().name() == "GtkColumnViewTitle" {
                out.push(x.clone());
            }
            walk(&x, out);
            c = x.next_sibling();
        }
    }
    let mut out = Vec::new();
    walk(view.upcast_ref(), &mut out);
    out
}

// Drag a header to move its column; a click still sorts. GTK 4.22's own header gesture claims
// every press on a header, so a controller on the header cell never runs: this one sits on the
// view in the capture phase and takes presses on a header first (not Name, not a resize edge).
// The header under the pointer shows where the column will land (a bar on its left or right edge).
fn column_drag(view: &gtk::ColumnView) {
    fn text(w: &gtk::Widget) -> Option<String> {
        if let Some(l) = w.downcast_ref::<gtk::Label>() {
            return Some(l.text().to_string());
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            if let Some(t) = text(&x) {
                return Some(t);
            }
            c = x.next_sibling();
        }
        None
    }
    fn column(view: &gtk::ColumnView, t: &gtk::Widget) -> Option<gtk::ColumnViewColumn> {
        let name = text(t)?;
        view.columns()
            .iter::<gtk::ColumnViewColumn>()
            .flatten()
            .find(|c| c.title().as_deref() == Some(name.as_str()))
    }
    fn unmark(t: &gtk::Widget) {
        t.remove_css_class("fa-drop-before");
        t.remove_css_class("fa-drop-after");
    }
    // The shown header cell at x, y (view coordinates), with its left edge and width.
    fn under(view: &gtk::ColumnView, x: f64, y: f64) -> Option<(gtk::Widget, f64, f64)> {
        titles(view).into_iter().filter(|t| t.is_mapped()).find_map(|t| {
            let b = t.compute_bounds(view)?;
            let (l, w, top) = (b.x() as f64, b.width() as f64, b.y() as f64);
            ((l..l + w).contains(&x) && (top..top + b.height() as f64).contains(&y))
                .then_some((t, l, w))
        })
    }
    let g = gtk::GestureDrag::builder()
        .button(gdk::BUTTON_PRIMARY)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    let picked: Rc<RefCell<Option<(gtk::ColumnViewColumn, f64)>>> = Rc::default(); // column, header y
    let drop: Rc<RefCell<Option<(gtk::Widget, bool)>>> = Rc::default(); // header cell, after it
    let v = view.downgrade();
    g.connect_drag_begin(glib::clone!(
        #[strong]
        picked,
        move |g, x, y| {
            let Some(view) = v.upgrade() else { return };
            let col = under(&view, x, y)
                .filter(|(_, l, w)| x - l > 6.0 && l + w - x > 6.0) // the edges resize
                .and_then(|(t, ..)| column(&view, &t))
                .filter(|c| c.id().as_deref() != Some("name"));
            g.set_state(if col.is_some() {
                gtk::EventSequenceState::Claimed
            } else {
                gtk::EventSequenceState::Denied
            });
            *picked.borrow_mut() = col.map(|c| (c, y));
        }
    ));
    let v = view.downgrade();
    g.connect_drag_update(glib::clone!(
        #[strong]
        picked,
        #[strong]
        drop,
        move |g, dx, _| {
            let (Some(view), Some(y), Some((sx, _))) = (
                v.upgrade(),
                picked.borrow().as_ref().map(|p| p.1),
                g.start_point(),
            ) else {
                return;
            };
            if let Some((t, _)) = drop.take() {
                unmark(&t);
            }
            if dx.abs() < 8.0 {
                return; // still a click
            }
            let x = sx + dx;
            if let Some((t, l, w)) = under(&view, x, y) {
                let name = column(&view, &t).is_some_and(|c| c.id().as_deref() == Some("name"));
                let after = name || x >= l + w / 2.0;
                t.add_css_class(if after { "fa-drop-after" } else { "fa-drop-before" });
                *drop.borrow_mut() = Some((t, after));
            }
        }
    ));
    let v = view.downgrade();
    g.connect_drag_end(move |_, dx, _| {
        let (Some(view), Some((col, _))) = (v.upgrade(), picked.take()) else {
            return;
        };
        let target = drop.take();
        if let Some((t, _)) = &target {
            unmark(t);
        }
        if dx.abs() < 8.0 {
            // A click: sort as GTK does, the other way round on the second click.
            let again = view
                .sorter()
                .and_downcast::<gtk::ColumnViewSorter>()
                .is_some_and(|s| {
                    s.primary_sort_column().as_ref() == Some(&col)
                        && s.primary_sort_order() == gtk::SortType::Ascending
                });
            let order = if again { gtk::SortType::Descending } else { gtk::SortType::Ascending };
            return view.sort_by_column(Some(&col), order);
        }
        let Some((target, after)) = target.and_then(|(t, a)| Some((column(&view, &t)?, a))) else {
            return;
        };
        let cols: Vec<gtk::ColumnViewColumn> = view
            .columns()
            .iter::<gtk::ColumnViewColumn>()
            .flatten()
            .collect();
        let (Some(from), Some(at)) = (
            cols.iter().position(|c| *c == col),
            cols.iter().position(|c| *c == target),
        ) else {
            return;
        };
        let mut to = if after { at + 1 } else { at };
        if from < to {
            to -= 1;
        }
        if to != from && to > 0 {
            view.insert_column(to as u32, &col);
            dlog!("column drag: {} moved to {to}", col.id().unwrap_or_default());
        }
    });
    view.add_controller(g);
}

fn build(app: &adw::Application) {
    install_icon();
    let entry = gtk::SearchEntry::builder()
        .placeholder_text(SEARCH_HINT)
        .hexpand(true)
        .search_delay(0)
        .build(); // the pause is ours: see `pending` below
    let filter = gtk::DropDown::from_strings(&FILTERS.iter().map(|f| f.0).collect::<Vec<_>>());
    filter.set_tooltip_text(Some("Filter: only this kind of file"));
    let menu = gio::Menu::new();
    let search_menu = gio::Menu::new();
    let matching = gio::Menu::new();
    let opts = gio::Menu::new();
    for (name, _, _, label) in OPTIONS {
        opts.append(Some(label), Some(&format!("win.{name}")));
    }
    matching.append_section(None, &opts);
    let pos = gio::Menu::new();
    for (state, _, _, label) in POSITIONS {
        pos.append(Some(label), Some(&format!("win.position::{state}")));
    }
    matching.append_section(None, &pos);
    search_menu.append_submenu(Some("Match"), &matching);
    search_menu.append(Some("Preview Pane"), Some("win.preview"));
    menu.append_section(None, &search_menu);
    let app_menu = gio::Menu::new();
    app_menu.append(Some("Daemon Status"), Some("win.status"));
    app_menu.append(Some("Preferences"), Some("win.preferences"));
    app_menu.append(Some("About Find Anything"), Some("win.about"));
    menu.append_section(None, &app_menu);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Find Anything", &format!("Version {VERSION}"))));
    // Row 2: the search box, left-aligned and as wide as the window, then the type filter.
    let search_row = gtk::Box::builder()
        .spacing(6)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    search_row.append(&entry);
    search_row.append(&filter);
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .primary(true)
            .build(),
    );
    header.pack_end(
        &gtk::ToggleButton::builder()
            .icon_name("view-grid-symbolic")
            .action_name("win.grid")
            .tooltip_text("Thumbnails (Ctrl+G)")
            .build(),
    );

    // Results in the daemon's order (natural, by path) until a header is clicked; then sorted off the
    // main thread (sort_rows), so the column sorters only give the headers their arrows.
    let model = gtk::StringList::new(&[]);
    let sel = gtk::MultiSelection::new(Some(model.clone()));
    let view = gtk::ColumnView::builder()
        .model(&sel)
        .show_column_separators(true)
        .build();
    view.add_css_class("data-table");
    let css = gtk::CssProvider::new();
    css.load_from_string("columnview.zebra-on > listview > row:nth-child(even):not(:selected) { background-color: alpha(currentColor, 0.045); }\
        .fa-drop-before { box-shadow: inset 3px 0 0 0 @accent_bg_color; background-color: alpha(@accent_bg_color, 0.12); }\
        .fa-drop-after { box-shadow: inset -3px 0 0 0 @accent_bg_color; background-color: alpha(@accent_bg_color, 0.12); }\
        .fa-dot { min-width: 8px; min-height: 8px; border-radius: 99px; background-color: @success_color; }\
        .fa-dot.busy { background-color: @accent_color; animation: fa-pulse 1.6s ease-in-out infinite; }\
        .fa-dot.off { background-color: transparent; box-shadow: inset 0 0 0 1.5px alpha(currentColor, 0.6); }\
        @keyframes fa-pulse { 0% { opacity: 1; } 50% { opacity: 0.35; } 100% { opacity: 1; } }\
        progressbar.fa-mini > trough { min-width: 56px; min-height: 4px; }\
        progressbar.fa-mini > trough > progress { min-height: 4px; }\
        .fa-progress { background-color: alpha(@accent_bg_color, 0.08); border-radius: 8px; padding: 10px; }\
        progressbar.fa-seg > trough, progressbar.fa-seg > trough > progress { min-height: 4px; min-width: 0; border-radius: 2px; }\
        progressbar.fa-seg > trough { background-color: alpha(currentColor, 0.12); }\
        progressbar.fa-seg.done > trough > progress { background-color: @success_color; }\
        progressbar.fa-bar6 > trough, progressbar.fa-bar6 > trough > progress { min-height: 6px; border-radius: 3px; }\
        progressbar.fa-bar6 > trough { background-color: alpha(currentColor, 0.12); }\
        .fa-bar-text { font-size: 0.886em; }\
        .fa-results { opacity: 0.9; }\
        .fa-bar-dot { opacity: 0.4; }\
        .fa-bar-path { opacity: 0.62; }\
        .fa-pop { font-size: 0.886em; }\
        .fa-pop .fa-title { font-size: 1.077em; font-weight: 700; }\
        .fa-pop .fa-sub { opacity: 0.72; }\
        .fa-pop .fa-small { font-size: 0.923em; }\
        .fa-pop .fa-heading { font-weight: 700; opacity: 0.6; }\
        .fa-pop .fa-bright { opacity: 0.95; }\
        .fa-pop .fa-dim { opacity: 0.68; }\
        .fa-pop .fa-faint { opacity: 0.55; }\
        .fa-pop .fa-accent { color: @accent_color; }\
        .fa-pop .fa-ok { color: @success_color; }\
        .fa-pop .fa-mono { font-family: 'Ubuntu Mono', monospace; font-size: 0.885em; opacity: 0.55; }\
        .fa-pop .fa-card { background-color: alpha(currentColor, 0.06); border-radius: 8px; }\
        .fa-pop .fa-drive { padding: 6px 10px; }\
        .fa-pop .fa-drive image { -gtk-icon-size: 17px; }\
        .fa-pop .fa-tool { padding: 9px 10px; min-height: 0; border-radius: 0 0 8px 8px; font-weight: normal; }\
        .fa-pop .fa-sep { border-top: 1px solid alpha(currentColor, 0.06); }\
        .fa-pop .fa-head-btn { min-height: 30px; padding: 0 12px; border-radius: 6px; font-weight: normal; }\
        .fa-status-btn > button { font-weight: normal; min-height: 28px; padding: 0 6px 0 10px; }\
        .fa-pop switch { min-width: 36px; min-height: 18px; padding: 2px; }\
        .fa-pop switch > slider { min-width: 18px; min-height: 18px; margin: 0; }\
        .fa-tabs { border-spacing: 8px; }\
        .fa-tabs button { min-height: 36px; padding: 0 16px; font-size: 1.1em; font-weight: bold; border-radius: 999px; }\
        .fa-idx-icon { opacity: 0.3; }\
        .fa-idx-title { font-size: 1.227em; font-weight: 700; }\
        .fa-idx-sub { font-size: 0.955em; opacity: 0.68; }");
    if let Some(d) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &d,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    let cols = gio::Menu::new();
    cols.append(Some("Folder"), Some("win.col-folder"));
    cols.append(Some("Size"), Some("win.col-size"));
    cols.append(Some("Type"), Some("win.col-type"));
    cols.append(Some("Date Modified"), Some("win.col-date"));
    let col_btn = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Columns")
        .menu_model(&cols)
        .css_classes(["flat"])
        .build();
    search_row.append(&col_btn); // after the type filter; not for thumbnails: hidden with the grid
    let name_col = column("Name", true, &sel, name_of, name_cmp);
    let folder_col = column("Folder", false, &sel, folder_text, folder_cmp);
    let size_col = column("Size", false, &sel, size_of, size_cmp);
    let type_col = column("Type", false, &sel, type_of, type_cmp);
    let date_col = column("Date Modified", false, &sel, date_of, date_cmp);
    name_col.set_fixed_width(320);
    folder_col.set_expand(true);
    size_col.set_fixed_width(110);
    type_col.set_fixed_width(170);
    date_col.set_fixed_width(200);
    // Right-click a header: show/hide columns, move this one (also by dragging the header).
    for (id, c) in [
        ("name", &name_col),
        ("folder", &folder_col),
        ("size", &size_col),
        ("type", &type_col),
        ("date", &date_col),
    ] {
        c.set_id(Some(id));
        let m = gio::Menu::new();
        m.append_section(None, &cols);
        if id == "folder" {
            let f = gio::Menu::new();
            f.append(Some("Short Folder Path"), Some("win.folder-short"));
            m.append_section(None, &f);
        }
        if id != "name" {
            // Name stays the first column.
            let mv = gio::Menu::new();
            mv.append(
                Some("Move Column Left"),
                Some(&format!("win.move-col::left-{id}")),
            );
            mv.append(
                Some("Move Column Right"),
                Some(&format!("win.move-col::right-{id}")),
            );
            m.append_section(None, &mv);
        }
        c.set_header_menu(Some(&m));
        view.append_column(c);
    }
    column_drag(&view); // dragging a header is ours: it marks the drop spot, and Name cannot move
    // Test harness: FA_GUI_SORT=name|folder|size|type|date[-desc] clicks that header after 1.2 s.
    if let Some((c, desc)) = std::env::var("FA_GUI_SORT").ok().and_then(|id| {
        let (id, desc) = id.strip_suffix("-desc").map_or((id.as_str(), false), |i| (i, true));
        [&name_col, &folder_col, &size_col, &type_col, &date_col]
            .into_iter()
            .find(|c| c.id().as_deref() == Some(id))
            .map(|c| (c.clone(), desc))
    }) {
        let v = view.clone();
        let order = if desc { gtk::SortType::Descending } else { gtk::SortType::Ascending };
        glib::timeout_add_local_once(Duration::from_millis(1200), move || {
            v.sort_by_column(Some(&c), order)
        });
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&view)
        .vexpand(true)
        .hexpand(true)
        .build();
    let grid = grid(&sel);
    let grid_scroll = gtk::ScrolledWindow::builder()
        .child(&grid)
        .vexpand(true)
        .hexpand(true)
        .build();
    THUMB_VIEW.with(|v| *v.borrow_mut() = Some(grid_scroll.clone().upcast()));
    // Ctrl + mouse wheel: bigger or smaller tiles (saved as thumb-size); the wheel alone scrolls.
    let wheel = gtk::EventControllerScroll::builder()
        .flags(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        )
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    wheel.connect_scroll(glib::clone!(
        #[weak]
        grid_scroll,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |c, _, dy| {
            if !c
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK)
                || dy == 0.0
            {
                return glib::Propagation::Proceed;
            }
            let t = (TILE.get() - 32 * dy.signum() as i32).clamp(64, THUMB);
            let _ =
                WidgetExt::activate_action(&grid_scroll, "win.thumb-size", Some(&t.to_variant()));
            glib::Propagation::Stop
        }
    ));
    grid_scroll.add_controller(wheel);
    // Cells only have a place on screen once laid out: look again after every paint (scrolling,
    // resizing and new results all repaint).
    grid_scroll.connect_realize(|w| {
        if let Some(c) = w.frame_clock() {
            c.connect_after_paint(|_| pump_thumbs());
        }
    });

    let start = gtk::Button::builder()
        .label("Start Daemon")
        .action_name("win.start")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    let offline = adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title("Daemon Not Running")
        .description(OFFLINE_DESC)
        .child(&start)
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&scroll, Some("results"));
    stack.add_named(&grid_scroll, Some("grid"));
    stack.add_named(&offline, Some("offline"));
    // While drives are read from scratch (design 1c): search is locked, this shows the progress.
    let idx_sub = gtk::Label::builder().css_classes(["fa-idx-sub", "numeric"]).build();
    let idx_bar = gtk::ProgressBar::builder()
        .width_request(240)
        .halign(gtk::Align::Center)
        .margin_top(6)
        .css_classes(["fa-bar6"])
        .build();
    let indexing = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .margin_bottom(40)
        .build();
    indexing.append(&gtk::Image::builder().icon_name("fa-indexing-symbolic").pixel_size(56).css_classes(["fa-idx-icon"]).build());
    indexing.append(&gtk::Label::builder().label("Indexing your drives").css_classes(["fa-idx-title"]).build());
    indexing.append(&idx_sub);
    indexing.append(&idx_bar);
    stack.add_named(&indexing, Some("indexing"));
    let preview = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .vexpand(true)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .build();
    let preview_label = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label", "caption"])
        .selectable(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(12)
        .build();
    let preview_pane = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .width_request(300)
        .visible(false)
        .build();
    preview_pane.append(&preview);
    preview_pane.append(&preview_label);
    let paned = gtk::Paned::builder()
        .start_child(&stack)
        .end_child(&preview_pane)
        .resize_end_child(false)
        .shrink_end_child(false)
        .shrink_start_child(false)
        .vexpand(true)
        .build();
    // Status bar (design "1c", 2026-10-21): results, the folder in grid view, then one status
    // button whose popover holds the daemon, progress, drives, logging and Report an issue.
    let status = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(pango::EllipsizeMode::End)
        .css_classes(["fa-bar-text", "fa-results"])
        .build();
    let folder_label = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(pango::EllipsizeMode::Middle)
        .css_classes(["fa-bar-text"])
        .build();
    // As the design: "2 results · (folder) /the/folder", the dot fainter than the path.
    let folder = gtk::Box::builder().spacing(10).visible(false).build();
    folder.append(&gtk::Label::builder().label("·").css_classes(["fa-bar-text", "fa-bar-dot"]).build());
    let path = gtk::Box::builder().spacing(5).css_classes(["fa-bar-path"]).build();
    path.append(&gtk::Image::builder().icon_name("folder-symbolic").pixel_size(16).build());
    path.append(&folder_label);
    folder.append(&path);
    let bar = gtk::Box::builder()
        .spacing(10)
        .margin_start(12)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    bar.append(&status);
    bar.append(&folder);
    let stop = gtk::Button::builder()
        .label("Stop")
        .tooltip_text("Stop reading file contents")
        .css_classes(["flat"])
        .visible(false)
        .build();
    bar.append(&stop);
    bar.append(&gtk::Box::builder().hexpand(true).build());
    let dot = gtk::Box::builder().valign(gtk::Align::Center).css_classes(["fa-dot", "off"]).build();
    let state = gtk::Label::builder().label("Stopped").css_classes(["fa-bar-text"]).build();
    let progress_bar = gtk::ProgressBar::builder()
        .valign(gtk::Align::Center)
        .css_classes(["fa-mini"])
        .visible(false)
        .build();
    let log_icon = gtk::Image::builder().icon_name("fa-log-symbolic").pixel_size(15).css_classes(["warning"]).visible(false).tooltip_text("Debug logging on").build();
    let face = gtk::Box::builder().spacing(8).build();
    face.append(&dot);
    face.append(&state);
    face.append(&progress_bar);
    face.append(&log_icon);
    face.append(&gtk::Image::builder().icon_name("pan-up-symbolic").pixel_size(16).build());
    // The popover: a header (dot, daemon, uptime and memory, Start/Stop), then `pop_body`.
    let pop_dot = gtk::Box::builder().valign(gtk::Align::Center).css_classes(["fa-dot", "off"]).build();
    let pop_title = gtk::Label::builder().xalign(0.0).css_classes(["fa-title"]).build();
    let pop_sub = gtk::Label::builder().xalign(0.0).css_classes(["fa-sub"]).build();
    let names = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).build();
    names.append(&pop_title);
    names.append(&pop_sub);
    let toggle = gtk::Button::builder()
        .child(&adw::ButtonContent::builder().icon_name("media-playback-start-symbolic").label("Start").build())
        .action_name("win.start")
        .valign(gtk::Align::Center)
        .css_classes(["fa-head-btn"])
        .build();
    let head = gtk::Box::builder().spacing(10).build();
    head.append(&pop_dot);
    head.append(&names);
    head.append(&toggle);
    let pop_body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    // 360 px wide with 14 px around, as the design (the popover adds 6 of them).
    let pop_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .width_request(332)
        .margin_start(8)
        .margin_end(8)
        .margin_top(8)
        .margin_bottom(8)
        .css_classes(["fa-pop"])
        .build();
    pop_box.append(&head);
    pop_box.append(&pop_body);
    let pop = gtk::Popover::builder().child(&pop_box).position(gtk::PositionType::Top).build();
    let status_btn = gtk::MenuButton::builder()
        .child(&face)
        .popover(&pop)
        .direction(gtk::ArrowType::Up)
        .tooltip_text("Daemon and indexing")
        .css_classes(["fa-status-btn"])
        .build();
    bar.append(&status_btn);
    toggle.connect_clicked(glib::clone!(
        #[weak]
        pop,
        move |_| pop.popdown()
    ));
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&paned);
    body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    body.append(&bar);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&body));
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.add_top_bar(&search_row);
    let other = adw::Banner::builder().button_label("Restart Daemon").build();
    tv.add_top_bar(&other);
    tv.set_content(Some(&toasts));
    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title("Find Anything")
        .default_width(1200)
        .default_height(700)
        .content(&tv)
        .build();
    entry.set_key_capture_widget(Some(&win));

    let ui = Rc::new(Ui {
        other: other.clone(),
        win: win.clone(),
        entry: entry.clone(),
        filter: filter.clone(),
        // 0.19.0 saved "hidden path<tab>the result it was picked from"; only the path counts.
        hidden: RefCell::new(
            read_lines("hidden.txt")
                .into_iter()
                .map(|l| l.split('\t').next().unwrap_or_default().to_string())
                .collect(),
        ),
        model,
        sel,
        stack,
        status,
        dot,
        state,
        folder,
        folder_label,
        pop: pop.clone(),
        pop_dot,
        pop_title,
        pop_sub,
        pop_body,
        pop_key: RefCell::default(),
        drives_text: RefCell::default(),
        toasts,
        preview,
        preview_label,
        preview_pane,
        preview_seq: Cell::new(0),
        last: RefCell::default(),
        listed: Cell::new(0),
        sorter: view.sorter().and_downcast().unwrap(),
        total: Cell::new(0),
        searched: RefCell::default(),
        found_at: Cell::new(None),
        status_win: Default::default(),
        prefs_win: Default::default(),
        status_text: RefCell::default(),
        seq: Cell::new(0),
        offline,
        start,
        starting: Cell::new(false),
        job: RefCell::default(),
        stop: stop.clone(),
        bar: progress_bar,
        log_icon,
        idx_sub,
        idx_bar,
        toggle,
    });
    stop.connect_clicked(glib::clone!(
        #[strong]
        ui,
        move |_| {
            if let Some(j) = ui.job.borrow().as_ref() {
                j.stop.store(true, Relaxed);
            }
        }
    ));

    // Opening the status popover fetches the drives at once (then with every refresh while open).
    pop.connect_show(glib::clone!(
        #[strong]
        ui,
        move |_| {
            ui.pop_key.take();
            ui.refresh()
        }
    ));

    // Column order: put back as saved, then saved whenever a header is dragged elsewhere.
    setting(
        &win,
        "col-order",
        "name,folder,size,type,date".to_variant(),
        |_| {},
    );
    let order: String = win
        .action_state("col-order")
        .and_then(|v| v.get())
        .unwrap_or_default();
    for (i, id) in order.split(',').enumerate() {
        if let Some(c) = [&name_col, &folder_col, &size_col, &type_col, &date_col]
            .into_iter()
            .find(|c| c.id().as_deref() == Some(id))
        {
            view.insert_column(i as u32, c);
        }
    }
    if !order.starts_with("name,") {
        view.insert_column(0, &name_col); // an order saved before Name was pinned
    }
    view.columns().connect_items_changed(glib::clone!(
        #[weak]
        win,
        move |cols, _, _, _| {
            let ids: Vec<String> = (0..cols.n_items())
                .filter_map(|i| {
                    cols.item(i)
                        .and_downcast::<gtk::ColumnViewColumn>()?
                        .id()
                        .map(String::from)
                })
                .collect();
            if ids.len() == 5 {
                win.change_action_state("col-order", &ids.join(",").to_variant());
            }
        }
    ));
    // "left-size": the Size column swaps places with the next shown column to its left.
    let mv = gio::SimpleAction::new("move-col", Some(glib::VariantTy::STRING));
    mv.connect_activate(glib::clone!(
        #[weak]
        view,
        move |_, p| {
            let Some((dir, id)) = p.and_then(|p| p.str()).and_then(|p| p.split_once('-')) else {
                return;
            };
            let cols: Vec<gtk::ColumnViewColumn> = view
                .columns()
                .iter::<gtk::ColumnViewColumn>()
                .flatten()
                .collect();
            let Some(i) = cols.iter().position(|c| c.id().as_deref() == Some(id)) else {
                return;
            };
            let shown = |j: &usize| cols[*j].is_visible();
            let to = if i == 0 {
                None // Name stays first
            } else if dir == "left" {
                (1..i).rev().find(shown)
            } else {
                (i + 1..cols.len()).find(shown)
            };
            if let Some(to) = to {
                view.insert_column(to as u32, &cols[i]);
            }
        }
    ));
    win.add_action(&mv);
    setting(
        &win,
        "folder-short",
        false.to_variant(),
        glib::clone!(
            #[strong]
            ui,
            move |v| {
                SHORT_FOLDER.set(v.get().unwrap_or(false));
                let n = ui.model.n_items();
                ui.model.items_changed(0, n, n); // fill the rows on screen again
            }
        ),
    );
    for (name, col) in [
        ("col-folder", &folder_col),
        ("col-size", &size_col),
        ("col-type", &type_col),
        ("col-date", &date_col),
    ] {
        let col = col.clone();
        setting(&win, name, true.to_variant(), move |v| {
            col.set_visible(v.get().unwrap_or(true))
        });
    }
    for (name, ..) in OPTIONS {
        setting(
            &win,
            name,
            false.to_variant(),
            glib::clone!(
                #[strong]
                ui,
                move |_| ui.refresh()
            ),
        );
    }
    setting(
        &win,
        "position",
        "anywhere".to_variant(),
        glib::clone!(
            #[strong]
            ui,
            move |_| ui.refresh()
        ),
    );
    setting(
        &win,
        "grid",
        false.to_variant(),
        glib::clone!(
            #[strong]
            ui,
            move |v| {
                col_btn.set_visible(!v.get().unwrap_or(false));
                ui.show_folder();
                if !matches!(ui.stack.visible_child_name().as_deref(), Some("offline" | "indexing")) {
                    ui.show_results();
                }
            }
        ),
    );
    setting(
        &win,
        "thumb-size",
        128i32.to_variant(),
        glib::clone!(
            #[weak]
            grid,
            move |v| {
                TILE.set(v.get::<i32>().unwrap_or(128).clamp(64, THUMB));
                let mut c = grid.first_child(); // the cells made so far; new ones size themselves
                while let Some(w) = c {
                    if let Some(cell) = w.first_child() {
                        size_cell(&cell);
                    }
                    c = w.next_sibling();
                }
            }
        ),
    );
    // Log level for the next daemon start: off, detailed or summary; the detailed log is capped at log-max-mb.
    setting(&win, "log-level", "off".to_variant(), |_| {});
    setting(&win, "log-max-mb", 5i32.to_variant(), |_| {});
    setting(&win, "search-delay", 80i32.to_variant(), |_| {});
    setting(
        &win,
        "zebra",
        true.to_variant(),
        glib::clone!(
            #[weak]
            view,
            move |v| {
                if v.get().unwrap_or(true) {
                    view.add_css_class("zebra-on")
                } else {
                    view.remove_css_class("zebra-on")
                }
            }
        ),
    );
    setting(
        &win,
        "preview",
        false.to_variant(),
        glib::clone!(
            #[strong]
            ui,
            move |v| {
                ui.preview_pane.set_visible(v.get().unwrap_or(false));
                ui.show_preview();
            }
        ),
    );

    // Search after a pause: 80 ms, but 350 ms while the box holds one or two characters (the
    // first keys of a word match millions of files, and the rest of the word is on its way).
    // Enter searches at once.
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Default::default();
    entry.connect_search_changed(glib::clone!(
        #[strong]
        ui,
        #[strong]
        pending,
        move |e| {
            diag::TYPED.set(Some(Instant::now()));
            dlog!("search box now {:?}", e.text());
            // Typing elsewhere in the window lands here (key capture), but GTK's capture never
            // passes on the space bar: take the focus so the next word can be typed.
            if !e.root().and_then(|r| r.focus()).is_some_and(|f| f.is_ancestor(e)) {
                dlog!("search box takes the focus");
                e.grab_focus();
            }
            if let Some(id) = pending.take() {
                id.remove();
            }
            // Preferences › Search Delay; one or two letters match so much that they wait longer.
            let delay = ui.number("search-delay").max(0) as u64;
            let ms = if e.text().chars().count() <= 2 {
                delay.max(350)
            } else {
                delay
            };
            let (ui, p) = (ui.clone(), pending.clone());
            *pending.borrow_mut() = Some(glib::timeout_add_local_once(
                Duration::from_millis(ms),
                move || {
                    p.take();
                    ui.refresh()
                },
            ));
        }
    ));
    filter.connect_selected_notify(glib::clone!(
        #[strong]
        ui,
        move |_| ui.refresh()
    ));
    ui.sorter.connect_changed(glib::clone!(
        #[strong]
        ui,
        move |_, _| ui.resort()
    ));
    ui.sel.connect_selection_changed(glib::clone!(
        #[strong]
        ui,
        move |_, _, _| {
            ui.show_preview();
            ui.show_folder();
        }
    ));
    // In the list or grid: Ctrl+C / Ctrl+X for Files, Delete to the Trash, Ctrl+H to hide. Not
    // window-wide, so the search box keeps its own Ctrl+C, Ctrl+X and Delete.
    for w in [view.upcast_ref::<gtk::Widget>(), grid.upcast_ref()] {
        let keys = gtk::ShortcutController::new();
        for (k, a) in [
            ("<Control>c", "win.copy-files"),
            ("<Control>x", "win.cut-files"),
            ("Delete", "win.trash"),
            ("KP_Delete", "win.trash"),
            ("<Control>h", "win.hide"),
        ] {
            keys.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(k),
                Some(gtk::NamedAction::new(a)),
            ));
        }
        w.add_controller(keys);
    }
    entry.connect_activate(glib::clone!(
        #[strong]
        ui,
        #[strong]
        pending,
        move |_| {
            if let Some(id) = pending.take() {
                id.remove();
                ui.refresh();
            }
            // Enter in the search box jumps to the results
            if let Some(v) = ui
                .stack
                .visible_child()
                .and_downcast::<gtk::ScrolledWindow>()
                .and_then(|s| s.child())
            {
                v.grab_focus();
            }
        }
    ));
    view.connect_activate(glib::clone!(
        #[strong]
        ui,
        move |_, pos| {
            ui.sel.select_item(pos, true);
            ActionGroupExt::activate_action(&ui.win, "open", None);
        }
    ));
    grid.connect_activate(glib::clone!(
        #[strong]
        ui,
        move |_, pos| {
            ui.sel.select_item(pos, true);
            ActionGroupExt::activate_action(&ui.win, "open", None);
        }
    ));
    glib::timeout_add_seconds_local(
        2,
        glib::clone!(
            #[strong]
            ui,
            move || {
                ui.refresh();
                glib::ControlFlow::Continue
            }
        ),
    );
    other.connect_button_clicked(glib::clone!(
        #[strong]
        ui,
        move |_| ui.restart(false)
    ));

    add_action(
        &win,
        "open",
        glib::clone!(
            #[strong]
            ui,
            move || {
                if let Some(p) = ui.selected() {
                    ui.open(Path::new(&p));
                }
            }
        ),
    );
    add_action(
        &win,
        "open-folder",
        glib::clone!(
            #[strong]
            ui,
            move || {
                if let Some(p) = ui.selected() {
                    gtk::FileLauncher::new(Some(&gio::File::for_path(&p))).open_containing_folder(
                        Some(&ui.win),
                        gio::Cancellable::NONE,
                        glib::clone!(
                            #[strong]
                            ui,
                            move |r| {
                                if let Err(e) = r {
                                    ui.toast(&format!("Could not open folder: {e}"))
                                }
                            }
                        ),
                    );
                }
            }
        ),
    );
    add_action(
        &win,
        "copy-path",
        glib::clone!(
            #[strong]
            ui,
            move || {
                let p = ui.picked();
                if !p.is_empty() {
                    ui.win.clipboard().set_text(&p.join("\n"));
                    ui.toast(if p.len() == 1 { "Path copied" } else { "Paths copied" });
                }
            }
        ),
    );
    for (name, cut) in [("copy-files", false), ("cut-files", true)] {
        add_action(
            &win,
            name,
            glib::clone!(
                #[strong]
                ui,
                move || ui.put_files(cut)
            ),
        );
    }
    add_action(
        &win,
        "trash",
        glib::clone!(
            #[strong]
            ui,
            move || ui.trash()
        ),
    );
    add_action(
        &win,
        "hide",
        glib::clone!(
            #[strong]
            ui,
            move || ui.hide()
        ),
    );
    add_action(
        &win,
        "start",
        glib::clone!(
            #[strong]
            ui,
            move || ui.start_daemon(false)
        ),
    );
    add_action(
        &win,
        "stop",
        glib::clone!(
            #[strong]
            ui,
            move || {
                match ask("quit\n") {
                    Ok(_) => ui.toast("Daemon stopped"),
                    Err(_) => ui.toast("Daemon is not running"),
                }
                ui.refresh();
            }
        ),
    );
    add_action(
        &win,
        "status",
        glib::clone!(
            #[strong]
            ui,
            move || {
                // Sized for every drive; one at a time.
                if let Some(w) = ui.status_win.upgrade() {
                    return w.present();
                }
                let (w, tv, header) = ui.side_window("Daemon Status", 760, 920);
                let again = gtk::Button::builder()
                    .icon_name("view-refresh-symbolic")
                    .tooltip_text("Refresh")
                    .build();
                header.pack_start(&again);
                tv.set_content(Some(&status_page(w.upcast_ref())));
                again.connect_clicked(glib::clone!(
                    #[weak]
                    w,
                    #[weak]
                    tv,
                    move |_| tv.set_content(Some(&status_page(w.upcast_ref())))
                ));
                ui.status_win.set(Some(&w));
                w.present();
            }
        ),
    );
    // Daemon Status › Rescan All: asks, then restarts the daemon with no saved index.
    add_action(
        &win,
        "rescan",
        glib::clone!(
            #[strong]
            ui,
            move || {
                let a = adw::AlertDialog::new(Some("Rescan All Drives?"), Some("The daemon stops, the saved index is deleted, and the daemon starts again (may ask for your password) to read every drive from scratch. Search is off until it is done. Settings, hidden and excluded folders and logs stay."));
                a.add_responses(&[("cancel", "Cancel"), ("rescan", "Rescan All")]);
                a.set_response_appearance("rescan", adw::ResponseAppearance::Suggested);
                a.set_close_response("cancel");
                let ui2 = ui.clone();
                a.connect_response(None, move |_, r| {
                    if r == "rescan" {
                        ui2.restart(true);
                    }
                });
                let parent = ui.status_win.upgrade().map(|w| w.upcast::<gtk::Widget>());
                a.present(Some(parent.as_ref().unwrap_or(ui.win.upcast_ref())));
            }
        ),
    );
    // Folders & Logs › Delete All Logs: the daemon's log (emptied: it keeps writing to it) and its
    // .1 and .totals, the window log, and ~/FindAnything-report-*.txt. The log folder is the
    // user's, so these go even when root wrote them.
    add_action(
        &win,
        "delete-logs",
        glib::clone!(
            #[strong]
            ui,
            move || {
                let log = ask("status\n")
                    .ok()
                    .and_then(|st| st.lines().find_map(|l| l.strip_prefix("log ").map(PathBuf::from)))
                    .unwrap_or_else(|| daemon_files().1);
                let _ = std::fs::OpenOptions::new().write(true).open(&log).and_then(|f| f.set_len(0));
                for ext in [".1", ".totals"] {
                    let mut p = log.clone().into_os_string();
                    p.push(ext);
                    // In /opt/findanything (the boot service) the folder is root's: empty them instead.
                    if std::fs::remove_file(&p).is_err() {
                        let _ = std::fs::OpenOptions::new().write(true).open(&p).and_then(|f| f.set_len(0));
                    }
                }
                diag::clear(&gui_log());
                for e in std::fs::read_dir(glib::home_dir()).into_iter().flatten().flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if n.starts_with("FindAnything-report-") && n.ends_with(".txt") {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
                ui.toast("Logs and problem reports deleted");
            }
        ),
    );
    // Also app actions: their buttons are in Daemon Status and Preferences, windows of their own.
    if let Some(app) = win.application() {
        for n in ["rescan", "delete-logs"] {
            if let Some(a) = win.lookup_action(n) {
                app.add_action(&a);
            }
        }
    }
    add_action(
        &win,
        "report",
        glib::clone!(
            #[strong]
            ui,
            move || {
                let d = adw::AlertDialog::new(
                    Some("Report a Problem"),
                    Some("Pick the file or folder that should have shown up for your search. The report says where the index lost it and re-reads that folder, so it shows afterwards. It is saved as a small text file in your home folder, ready to share."),
                );
                d.add_responses(&[
                    ("cancel", "Cancel"),
                    ("none", "No File"),
                    ("folder", "Pick a Folder…"),
                    ("file", "Pick a File…"),
                ]);
                d.set_response_appearance("file", adw::ResponseAppearance::Suggested);
                d.set_default_response(Some("file"));
                d.set_close_response("cancel");
                d.connect_response(
                    None,
                    glib::clone!(
                        #[strong]
                        ui,
                        move |_, r| {
                            if r == "none" {
                                return ui.report(None);
                            }
                            if r == "cancel" {
                                return;
                            }
                            let ui2 = ui.clone();
                            let done = move |f: Result<gio::File, glib::Error>| {
                                if let Some(p) = f.ok().and_then(|f| f.path()) {
                                    ui2.report(Some(p));
                                }
                            };
                            let fd = gtk::FileDialog::builder().title("The one that should show").modal(true).build();
                            if r == "file" {
                                fd.open(Some(&ui.win), gio::Cancellable::NONE, done);
                            } else {
                                fd.select_folder(Some(&ui.win), gio::Cancellable::NONE, done);
                            }
                        }
                    ),
                );
                d.present(Some(&ui.win));
            }
        ),
    );
    add_action(
        &win,
        "preferences",
        glib::clone!(
            #[strong]
            ui,
            move || {
                // Its own window (one at a time): Search, Search Words and Index pages, switched in the header.
                if let Some(w) = ui.prefs_win.upgrade() {
                    return w.present();
                }
                let (d, tv, header) = ui.side_window("Preferences", 760, 760);
                // As Diskopium: "Preferences" centred on a flat header, the tabs below it as pills.
                header.add_css_class("flat");
                header.set_title_widget(Some(&adw::WindowTitle::new("Preferences", "")));
                let pages = gtk::Stack::builder()
                    .transition_type(gtk::StackTransitionType::Crossfade)
                    .vexpand(true)
                    .build();
                for (name, p) in [("search", search_page(&ui)), ("words", help_page()), ("folders", folders_page(&ui))] {
                    pages.add_titled(&p, Some(name), &p.title());
                }
                let tabs = gtk::StackSwitcher::builder()
                    .stack(&pages)
                    .halign(gtk::Align::Center)
                    .margin_top(6)
                    .margin_bottom(12)
                    .margin_start(18)
                    .margin_end(18)
                    .css_classes(["fa-tabs"])
                    .build();
                tabs.remove_css_class("linked");
                let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
                body.append(&tabs);
                body.append(&pages);
                let toasts = adw::ToastOverlay::new();
                toasts.set_child(Some(&body));
                tv.set_content(Some(&toasts));
                if let Ok(name) = std::env::var("FA_GUI_PAGE") {
                    pages.set_visible_child_name(&name);
                }
                if let Some(s) = std::env::var_os("FA_GUI_END")
                    .and(pages.visible_child())
                    .and_then(|p| p.first_child())
                    .and_downcast::<gtk::ScrolledWindow>()
                {
                    // test harness: show the end of the page
                    glib::timeout_add_local_once(Duration::from_millis(500), move || {
                        let a = s.vadjustment();
                        a.set_value(a.upper());
                    });
                }
                ui.prefs_win.set(Some(&d));
                d.present();
            }
        ),
    );
    add_action(
        &win,
        "about",
        glib::clone!(
            #[strong]
            ui,
            move || {
                adw::AboutDialog::builder()
                    .application_name("Find Anything")
                    .application_icon(APP_ID)
                    .version(VERSION)
                    .developer_name("Droid")
                    .comments("Instant filename search for Linux, kept live by fanotify.")
                    .release_notes_version(VERSION)
                    .release_notes(RELEASE_NOTES)
                    .build()
                    .present(Some(&ui.win));
            }
        ),
    );
    app.set_accels_for_action("win.copy-path", &["<Ctrl><Shift>c"]);
    app.set_accels_for_action("win.preferences", &["<Ctrl>comma"]);
    app.set_accels_for_action("win.open-folder", &["<Ctrl>Return"]);
    app.set_accels_for_action("win.preview", &["<Alt>p"]);
    app.set_accels_for_action("win.grid", &["<Ctrl>g"]);
    app.set_accels_for_action("win.case", &["<Ctrl>i"]);
    app.set_accels_for_action("win.ww", &["<Ctrl>b"]);
    app.set_accels_for_action("window.close", &["<Ctrl>q", "<Ctrl>w"]);

    ui.refresh();
    diag::watch(
        win.upcast_ref(),
        glib::clone!(
            #[strong]
            ui,
            move || format!(
                "rows {} thumbs queued {} busy {}",
                ui.last.borrow().len(),
                THUMB_QUEUE.with(|q| q.borrow().len()),
                THUMB_BUSY.get()
            )
        ),
    );
    win.present();
    ui.entry.grab_focus(); // GTK would give it to the results list
    snapshot_mode(&ui, &tv);
}

// Daemon Status: the daemon, one row per drive, and the raw status folded away.
fn status_page(win: &gtk::Widget) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let daemon = adw::PreferencesGroup::builder()
        .title("Daemon")
        .description("Runs in the background and keeps running when the search window is closed.")
        .build();
    match ask("status\n") {
        Ok(st) => {
            let line = |p: &str| {
                st.lines()
                    .find_map(|l| l.strip_prefix(p))
                    .unwrap_or("")
                    .to_string()
            };
            let mem = st
                .lines()
                .next()
                .and_then(|l| l.split("memory ").nth(1))
                .and_then(|m| m.trim_end_matches(" kB").parse::<f64>().ok())
                .map_or(String::new(), |k| match k / 1024.0 {
                    mb if mb < 1024.0 => format!(", using {mb:.0} MB of memory"),
                    mb => format!(", using {:.1} GB of memory", mb / 1024.0),
                });
            let row = adw::ActionRow::builder()
                .title("Running")
                .subtitle(format!("Up {}{mem}", uptime(&st)))
                .use_markup(false)
                .build();
            row.add_prefix(
                &gtk::Image::builder()
                    .icon_name("media-record-symbolic")
                    .css_classes(["success"])
                    .build(),
            );
            daemon.add(&row);
            let dirs = st
                .lines()
                .nth(1)
                .and_then(|l| l.split(" in ").nth(1))
                .and_then(|r| r.split(' ').next())
                .unwrap_or("?");
            daemon.add(
                &adw::ActionRow::builder()
                    .title("Indexed")
                    .subtitle(format!(
                        "{} files and folders in {} folders",
                        group(entries(&st)),
                        group_num(dirs)
                    ))
                    .use_markup(false)
                    .build(),
            );
            let busy = line("busy: ");
            if !busy.is_empty() {
                daemon.add(
                    &adw::ActionRow::builder()
                        .title("Working")
                        .subtitle(busy)
                        .use_markup(false)
                        .build(),
                );
            }
            let full = line("last full re-check: ");
            if !full.is_empty() {
                let sub = format!("Every drive was re-checked {full}. Changes that came in meanwhile may have shown late.");
                daemon.add(
                    &adw::ActionRow::builder()
                        .title("Too many changes at once")
                        .subtitle(sub)
                        .use_markup(false)
                        .build(),
                );
            }
            let mut queue = line("change queue: ");
            if !queue.is_empty() {
                queue[..1].make_ascii_uppercase(); // "up to ..." -> "Up to ..."
                let waiting = line("changes waiting: ");
                if !waiting.is_empty() {
                    queue = format!("{} waiting now. {queue}", group_num(&waiting));
                }
                daemon.add(
                    &adw::ActionRow::builder()
                        .title("Change queue")
                        .subtitle(queue)
                        .use_markup(false)
                        .build(),
                );
            }
            page.add(&daemon);
            let file = adw::PreferencesGroup::builder()
                .title("Index")
                .description("Saved when the daemon stops and every 10 minutes, so a restart only checks what changed.")
                .build();
            let info = line("index file ");
            let (path, rest) = info.split_once(" (").unwrap_or((&info, ""));
            file.add(&adw::ActionRow::builder().title("Location").subtitle(path).subtitle_selectable(true).use_markup(false).build());
            if !rest.is_empty() {
                file.add(&adw::ActionRow::builder().title("Size").subtitle(rest.trim_end_matches(')')).use_markup(false).build());
            }
            let rescan = adw::ActionRow::builder()
                .title("Rescan All")
                .subtitle("Stops the daemon, deletes the saved index and starts it again (may ask for your password) to read every drive from scratch. Search is off until it is done.")
                .build();
            let b = gtk::Button::builder()
                .label("Rescan All")
                .valign(gtk::Align::Center)
                .action_name("app.rescan")
                .build();
            rescan.add_suffix(&b);
            rescan.set_activatable_widget(Some(&b));
            file.add(&rescan);
            page.add(&file);
            page.add(&drive_rows(win));
            let details = adw::PreferencesGroup::builder().title("Details").build();
            let exp = adw::ExpanderRow::builder()
                .title("Full status")
                .subtitle("Event counters and log location, for bug reports")
                .build();
            exp.add_row(
                &gtk::Label::builder()
                    .label(&st)
                    .selectable(true)
                    .wrap(true)
                    .xalign(0.0)
                    .margin_start(12)
                    .margin_end(12)
                    .margin_top(8)
                    .margin_bottom(8)
                    .css_classes(["monospace", "caption"])
                    .build(),
            );
            details.add(&exp);
            page.add(&details);
        }
        Err(_) => {
            daemon.add(
                &adw::ActionRow::builder()
                    .title("Not running")
                    .subtitle("Search is off. Start it from the menu (may ask for your password).")
                    .build(),
            );
            page.add(&daemon);
        }
    }
    page
}

// Preferences › Search: columns, hidden results.
fn search_page(ui: &Rc<Ui>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Search")
        .icon_name("system-search-symbolic")
        .build();
    let cols = adw::PreferencesGroup::builder().title("Columns").description("Also from the right-click menu on a column header, which can move columns too (or drag a header). Click a header to sort.").build();
    for (name, title, sub) in [
        (
            "col-folder",
            "Folder",
            "Where the file is; see Short Folder Path below",
        ),
        (
            "col-size",
            "Size",
            "Bytes for a file, number of items for a folder; read only for the rows on screen",
        ),
        ("col-type", "Type", "From the file name, nothing is read"),
        (
            "col-date",
            "Date Modified",
            "Read only for the rows on screen; sorting by date reads them all",
        ),
    ] {
        let r = adw::SwitchRow::builder()
            .title(title)
            .subtitle(sub)
            .active(ui.flag(name))
            .build();
        let w = ui.win.clone();
        r.connect_active_notify(move |r| w.change_action_state(name, &r.is_active().to_variant()));
        cols.add(&r);
    }
    page.add(&cols);
    let list = adw::PreferencesGroup::builder().title("List").build();
    let short = adw::SwitchRow::builder().title("Short Folder Path").subtitle("The Folder column shows only the last two folders (…/me/Documents)").active(ui.flag("folder-short")).build();
    let w = ui.win.clone();
    short.connect_active_notify(move |r| {
        w.change_action_state("folder-short", &r.is_active().to_variant())
    });
    list.add(&short);
    let z = adw::SwitchRow::builder()
        .title("Alternating Row Colors")
        .subtitle("Shade every other result row to follow a line across the columns")
        .active(ui.flag("zebra"))
        .build();
    let w = ui.win.clone();
    z.connect_active_notify(move |r| w.change_action_state("zebra", &r.is_active().to_variant()));
    list.add(&z);
    let delay = adw::SpinRow::with_range(0.0, 1000.0, 10.0);
    delay.set_title("Search Delay (ms)");
    delay.set_subtitle("How long to wait after the last key before searching. Longer: fewer searches while you type. Words of one or two letters wait at least 350 ms.");
    delay.set_value(ui.number("search-delay") as f64);
    let w = ui.win.clone();
    delay.connect_value_notify(move |r| {
        w.change_action_state("search-delay", &(r.value() as i32).to_variant())
    });
    list.add(&delay);
    page.add(&list);
    page
}

// Preferences › Folders & Logs: excluded and hidden folders, and the daemon's log.
fn folders_page(ui: &Rc<Ui>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Folders & Logs")
        .icon_name("folder-symbolic")
        .build();
    page.add(&exclude_group(ui));
    let hidden = adw::PreferencesGroup::builder()
        .title("Hidden Folders")
        .description("Still indexed, just never shown in results; nothing is read again. Add one here or right-click a result › Hide From Results. A hidden folder hides everything in it.")
        .build();
    let items = Rc::new(RefCell::new(ui.hidden.borrow().clone()));
    let changed: Rc<dyn Fn(&[String])> = Rc::new(glib::clone!(
        #[strong]
        ui,
        move |l: &[String]| {
            write_lines("hidden.txt", l);
            *ui.hidden.borrow_mut() = l.to_vec();
            ui.research();
        }
    ));
    add_folder_button(&hidden, &items, "view-reveal-symbolic", "Show in results again", &changed);
    for h in items.borrow().iter() {
        list_row(
            &hidden,
            &items,
            h,
            "view-reveal-symbolic",
            "Show in results again",
            &changed,
        );
    }
    page.add(&hidden);
    page.add(&logs_group(ui));
    page
}

// The header button of Excluded and Hidden Folders: pick a folder in the file chooser.
fn add_folder_button(
    g: &adw::PreferencesGroup,
    items: &Rc<RefCell<Vec<String>>>,
    icon: &'static str,
    tip: &'static str,
    changed: &Rc<dyn Fn(&[String])>,
) {
    let b = gtk::Button::builder()
        .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label("Add Folder…").build())
        .valign(gtk::Align::Center)
        .build();
    let (g2, items, changed) = (g.clone(), items.clone(), changed.clone());
    b.connect_clicked(move |b| {
        let win = b.root().and_downcast::<gtk::Window>();
        let (g, items, changed) = (g2.clone(), items.clone(), changed.clone());
        gtk::FileDialog::builder()
            .title("Choose a Folder")
            .modal(true)
            .build()
            .select_folder(win.as_ref(), gio::Cancellable::NONE, move |r| {
                let Some(p) = r.ok().and_then(|f| f.path()) else { return };
                let p = p.display().to_string();
                if items.borrow().contains(&p) {
                    return;
                }
                items.borrow_mut().push(p.clone());
                list_row(&g, &items, &p, icon, tip, &changed);
                changed(&items.borrow());
            });
    });
    g.set_header_suffix(Some(&b));
}

// Folders & Logs › Log: where, how much, and Delete All Logs.
fn logs_group(ui: &Rc<Ui>) -> adw::PreferencesGroup {
    let logs = adw::PreferencesGroup::builder().title("Log").build();
    let st = ask("status\n").unwrap_or_default();
    if let Some(l) = st.lines().find_map(|l| l.strip_prefix("log ")) {
        logs.add(
            &adw::ActionRow::builder()
                .title("Location")
                .subtitle(l)
                .subtitle_selectable(true)
                .use_markup(false)
                .build(),
        );
    }
    const LEVELS: [&str; 3] = ["off", "summary", "detailed"];
    let level = adw::ComboRow::builder()
        .title("Log Level")
        .subtitle("Summary: totals and one line per search, small enough to share. Detailed: every file change and search, for bug reports. Applies the next time the daemon starts.")
        .model(&gtk::StringList::new(&["Off", "Summary", "Detailed"]))
        .selected(LEVELS.iter().position(|l| *l == ui.text("log-level")).unwrap_or(0) as u32)
        .build();
    let w = ui.win.clone();
    level.connect_selected_notify(move |r| {
        w.change_action_state("log-level", &LEVELS[r.selected() as usize % 3].to_variant())
    });
    logs.add(&level);
    let cap = adw::SpinRow::with_range(1.0, 1000.0, 1.0);
    cap.set_title("Detailed Log Size Limit (MB)");
    cap.set_subtitle("When reached, the log moves to a .1 file next to it and a new one starts.");
    cap.set_value(ui.number("log-max-mb") as f64);
    let w = ui.win.clone();
    cap.connect_value_notify(move |r| {
        w.change_action_state("log-max-mb", &(r.value() as i32).to_variant())
    });
    logs.add(&cap);
    let del = adw::ActionRow::builder()
        .title("Delete All Logs")
        .subtitle("The daemon and window logs and the problem reports in your home folder. The daemon keeps running.")
        .build();
    let b = gtk::Button::builder()
        .label("Delete")
        .valign(gtk::Align::Center)
        .css_classes(["destructive-action"])
        .action_name("app.delete-logs")
        .build();
    del.add_suffix(&b);
    del.set_activatable_widget(Some(&b));
    logs.add(&del);
    logs
}

// Preferences › Search Words: what can be typed in the search box.
fn help_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Search Words")
        .icon_name("help-about-symbolic")
        .build();
    let g = adw::PreferencesGroup::builder()
        .title("Search Words")
        .description("Type these in the search box. Mix them freely.")
        .build();
    for (what, means) in [
        ("holiday photo", "both words, any order"),
        ("jpg | png", "either word"),
        ("!thumb   !BACKUP", "leave out anything with that word in its name or path, drive names included"),
        ("\"two words\"", "exact phrase"),
        ("*.mkv   IMG_????", "wildcards (* any text, ? one character)"),
        ("ext:pdf;epub", "by extension"),
        ("/mnt/drives/PHOTOS/ invoice", "only inside that folder"),
        ("parent:/home/me/Documents", "only directly in that folder"),
        ("folder:   file:   empty:", "only folders / only files / empty folders"),
        ("image:  audio:  video:  doc:  zip:  exe:", "by kind (also in the list right of the box)"),
        ("content:invoice", "also read the files for that word (slower; add a name part, e.g. ext:txt content:invoice, to keep it quick)"),
    ] {
        let row = adw::ActionRow::builder().title(what).subtitle(means).use_markup(false).title_lines(1).subtitle_lines(0).build();
        g.add(&row);
    }
    page.add(&g);
    page
}

// Preferences › Folders & Logs › Excluded Folders: kept by the daemon, which drops them from the index.
fn exclude_group(ui: &Rc<Ui>) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title("Excluded Folders")
        .description("Never indexed, so they never show and aren't read at all. Taking one off the list re-reads the drives.")
        .build();
    let Ok(list) = ask("exclude\n") else {
        g.add(
            &adw::ActionRow::builder()
                .title("Daemon not running")
                .subtitle("Start it from the menu to edit this list.")
                .build(),
        );
        return g;
    };
    let items = Rc::new(RefCell::new(
        list.lines().map(String::from).collect::<Vec<_>>(),
    ));
    let changed: Rc<dyn Fn(&[String])> = Rc::new(glib::clone!(
        #[strong]
        ui,
        move |l: &[String]| {
            match ask(&format!("exclude\t{}\n", l.join("\t"))).as_deref() {
            Ok("ok\n") => ui.toast("Excluded folders dropped from the index"),
            Ok("rebuilding\n") => ui.toast("Re-reading the drives to bring the folder back; progress shows in the status bar"),
            Ok(_) => ui.toast("Saved; the daemon is busy, so use Daemon Status › Rescan All when it's done to bring the folder back"),
            Err(_) => ui.toast("The daemon is not running"),
        }
            ui.research();
        }
    ));
    add_folder_button(&g, &items, "user-trash-symbolic", "Index this folder again", &changed);
    for i in items.borrow().iter() {
        list_row(
            &g,
            &items,
            i,
            "user-trash-symbolic",
            "Index this folder again",
            &changed,
        );
    }
    g
}

// Test harness: FA_GUI_SNAPSHOT=out.png [FA_GUI_QUERY=words] [FA_GUI_SELECT=row or rows: 0,2,5]
// [FA_GUI_QUERY2=words, replaces the list after 1.5 s] [FA_GUI_DIAG=1, write the debug log] [FA_GUI_ABOUT=1 | FA_GUI_DIALOG=status|preferences [FA_GUI_PAGE=search|words|folders] | any win action, e.g. move-col::left-size] renders the window to a
// PNG and quits; FA_GUI_POPOVER=1 opens the status button's popover and paints its contents
// instead. Settings come from $XDG_CONFIG_HOME/findanything/.
fn snapshot_mode(ui: &Rc<Ui>, content: &adw::ToolbarView) {
    let Ok(out) = std::env::var("FA_GUI_SNAPSHOT") else {
        return;
    };
    if let Ok(q) = std::env::var("FA_GUI_QUERY") {
        ui.entry.set_text(&q);
    }
    if let Ok(q) = std::env::var("FA_GUI_QUERY2") {
        // a second search replaces the first list
        let ui = ui.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
            ui.entry.set_text(&q)
        });
    }
    if let Ok(rows) = std::env::var("FA_GUI_SELECT") {
        // "3", or "0,2,5" for several
        let ui = ui.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
            for n in rows.split(',').filter_map(|n| n.trim().parse().ok()) {
                ui.sel.select_item(n, false);
            }
        });
    }
    // FA_GUI_ABOUT=1 opens About first; the dialog lives in the window, so paint the whole window.
    let dialog = std::env::var("FA_GUI_DIALOG")
        .ok()
        .or(std::env::var_os("FA_GUI_ABOUT").map(|_| "about".into()));
    let win = ui.win.clone();
    let content: gtk::Widget = if dialog.is_some() {
        win.clone().upcast()
    } else {
        content.clone().upcast()
    };
    if let Some(d) = dialog {
        // After the first refresh, so the window behind shows its real state.
        let win = win.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
            let (name, arg) = d.split_once("::").map_or((d.as_str(), None), |(n, a)| {
                (
                    n,
                    Some(glib::Variant::parse(None, a).unwrap_or_else(|_| a.to_variant())),
                )
            }); // "move-col::left-size", "thumb-size::64"
            let _ = WidgetExt::activate_action(&win, &format!("win.{name}"), arg.as_ref());
        });
    }
    let popover = std::env::var_os("FA_GUI_POPOVER").is_some();
    if popover {
        let ui = ui.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(1000), move || ui.pop.popup());
    }
    let ui = ui.clone();
    glib::timeout_add_seconds_local_once(3, move || {
        let status = ui.status_win.upgrade().or(ui.prefs_win.upgrade()); // Daemon Status and Preferences are their own windows: paint that one
        let content: gtk::Widget = status.clone().map_or(content, |w| w.upcast());
        let content = if popover { ui.pop.child().unwrap_or(content) } else { content };
        let snap = gtk::Snapshot::new();
        let bg = if adw::StyleManager::default().is_dark() {
            0.13
        } else {
            0.98
        };
        snap.append_color(
            &gdk::RGBA::new(bg, bg, bg, 1.0),
            &gtk::graphene::Rect::new(0.0, 0.0, content.width() as f32, content.height() as f32),
        );
        gtk::WidgetPaintable::new(Some(&content)).snapshot(
            &snap,
            content.width() as f64,
            content.height() as f64,
        );
        if let (Some(node), Some(r)) = (snap.to_node(), content.native().and_then(|n| n.renderer()))
        {
            let _ = r.render_texture(&node, None).save_to_png(&out);
        }
        if let Some(w) = status {
            w.destroy();
        }
        win.destroy(); // close() with a dialog open can leave the app running
    });
}

fn main() -> glib::ExitCode {
    if std::env::var_os("FA_GUI_SNAPSHOT").is_none() || std::env::var_os("FA_GUI_DIAG").is_some() {
        diag::init(&gui_log());
    }
    // One window per session: a second fa-gui only raises the first. When the first is another
    // build, this one opens anyway and asks which to keep, instead of showing the old one.
    let other = other_window();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(if other.is_some() { gio::ApplicationFlags::NON_UNIQUE } else { Default::default() })
        .build();
    app.connect_activate(move |app| {
        build(app);
        if let (Some((pid, exe)), Some(w)) = (&other, app.windows().first()) {
            other_dialog(w, *pid, exe);
        }
    });
    app.run()
}

#[cfg(test)]
mod tests {
    #[test]
    fn summary_line() {
        let st = "Find Anything 0.4.0, up 7385s, memory 1 kB\n523117 entries in 54269 directories, generation 9\nlog /x\nindex file /v (1 MB, saved 3 s ago)\n  / [ext4 /dev/sda2]: LIVE, live updates; scanned\n  /d [fuseblk /dev/sdb1]: NOT LIVE (x), checked every 5 min (last 1 min ago, took 17 s); y\n  /e [fuseblk /dev/sdc1]: NOT LIVE (x), checked every 49 min (last 2 min ago, took 292 s); y\n  /old (not mounted): kept\nevents 0\n";
        assert_eq!(super::summary(12, 12, true), "12 results");
        assert_eq!(
            super::summary(100_000, 250_000, true),
            "first 100,000 of 250,000 results (type more to narrow)"
        );
        assert_eq!(super::summary(0, 0, false), "Type to search");
        assert_eq!(super::generation(st), "9");
        assert_eq!((super::daemon_version(st), super::daemon_version("x")), ("0.4.0", "unknown"));
        assert_eq!((super::uptime(st), super::memory(st)), ("2 h 3 min".to_string(), "0 MB".to_string()));
        assert_eq!(super::memory("Find Anything 1, up 1s, memory 1712345 kB"), "1.7 GB");
        assert_eq!(super::busy_label("checking /mnt/drives/DATA (3 of 6 drives), 12 folders checked", true), "Indexing DATA · 3/6");
        assert_eq!(super::busy_label("checking /mnt/drives/DATA (3 of 6 drives), 12 folders checked", false), "Checking DATA · 3/6");
        assert_eq!(super::busy_label("rebuilding / (1 of 6 drives)", false), "Indexing System · 1/6");
        assert_eq!(super::busy_label("indexing: loading the saved index", true), "Loading the saved index");
        let b = "checking /mnt/x (2 of 4 drives), 1,500 of about 3,000 files read, 9 s on this drive";
        assert_eq!((super::drive_done(b), super::drive_fraction(b)), (Some(0.5), Some(1.5 / 4.0)));
        assert_eq!(super::drive_done("checking /mnt/x (2 of 4 drives), 1,500 files read"), None);
        assert_eq!(super::drive_done("checking /x (1 of 1 drives), 9,000 of about 3,000 files read"), Some(0.99));
        assert_eq!((super::short_num("1,717,986"), super::short_num("4,000,000"), super::short_num("250,000")), ("1.7 M".into(), "4 M".into(), "250 K".into()));
        let st = |busy: &str, startup: &str| format!("Find Anything 1, up 1s, memory 1 kB\n0 entries in 0 directories, generation 0\nstartup: {startup}\n{busy}");
        assert!(super::from_scratch(&st("busy: x\n", "started over")));
        assert!(super::from_scratch(&st("busy: x\n", "no saved index")));
        assert!(!super::from_scratch(&st("busy: x\n", "loaded the saved index")));
        assert!(!super::from_scratch(&st("busy: x\n", "started over, 7 of 7 drives read from scratch, ready in 3.0 s")));
        assert!(!super::from_scratch(&st("", "started over")));
        assert!(super::from_scratch("Find Anything 1, up 1s, starting\nindexing: loading the saved index\n"));
        assert_eq!(super::busy_drive("x: re-checking /a (b) (2 of 3 drives); y"), Some(("/a (b)", 2, 3)));
        assert_eq!(super::plain("/a/b/"), "/a/b");
        assert_eq!(super::plain("/"), "/");
        assert_eq!(super::group(1000), "1,000");
        assert_eq!(
            super::drive_fraction("checking /mnt/x (6 of 7 drives), 12 folders checked"),
            Some(5.0 / 7.0)
        );
        assert_eq!(
            super::drive_fraction("indexing: loading the saved index"),
            None
        );
    }

    #[test]
    fn columns_and_hidden() {
        assert_eq!(
            (super::name_of("/a/b/"), super::folder_of("/a/b/")),
            ("b".into(), "/a".into())
        );
        assert_eq!(
            (
                super::ext_of("/a/x.TAR.GZ"),
                super::ext_of("/a/README"),
                super::ext_of("/a/dir.d/")
            ),
            (".gz".into(), ".".into(), "".into())
        );
        let h = vec!["/a/b".to_string()];
        assert!(
            super::is_hidden(&h, "/a/b/")
                && super::is_hidden(&h, "/a/b/c.txt")
                && !super::is_hidden(&h, "/a/bc.txt")
        );
        super::SHORT_FOLDER.set(true);
        assert_eq!(
            (
                super::folder_text("/home/me/Documents/x.pdf"),
                super::folder_text("/mnt/d/"),
                super::folder_text("/x")
            ),
            ("…/me/Documents".into(), "/mnt".into(), "/".into())
        );
        super::SHORT_FOLDER.set(false);
        assert_eq!(
            super::folder_text("/home/me/Documents/x.pdf"),
            "/home/me/Documents"
        );
    }

    #[test]
    fn desktop_exec_is_quoted() {
        let e = super::desktop_entry("/home/me/My Apps/fa 100%/fa-gui");
        assert!(e.contains("Exec=\"/home/me/My Apps/fa 100%%/fa-gui\"\n"));
        assert!(super::desktop_entry("/a/$x\"/fa-gui").contains("Exec=\"/a/\\$x\\\"/fa-gui\""));
    }

    #[test]
    fn worker_sort_and_stalls() {
        let rows = || vec!["/b/file10".to_string(), "/a/file2".into(), "/c/File1".into()];
        let by = |id: &str, desc| Some((id.to_string(), desc));
        assert_eq!(super::sort_rows(rows(), None).0, rows());
        assert_eq!(super::sort_rows(rows(), by("name", false)).0, ["/c/File1", "/a/file2", "/b/file10"]);
        assert_eq!(super::sort_rows(rows(), by("name", true)).0, ["/b/file10", "/a/file2", "/c/File1"]);
        assert_eq!(super::sort_rows(rows(), by("folder", false)).0, ["/a/file2", "/b/file10", "/c/File1"]);

        let d = std::env::temp_dir().join(format!("fa-stalls-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let log = d.join("gui-debug.log");
        std::fs::write(d.join("gui-debug.log.1"), "[1] STALL: main loop silent for 900 ms\n[2] show #1\n").unwrap();
        std::fs::write(&log, "[1] show #1\n").unwrap();
        let s = crate::diag::stalls(&log);
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(s, "window stalls, last run: 1\n  [1] STALL: main loop silent for 900 ms\nwindow stalls, this run: 0\n");
    }
}
