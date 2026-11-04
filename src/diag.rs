// Window diagnostics (0.7.3): one log file, ~/.config/findanything/gui-debug.log, new at each
// start and capped at 8 MB. It records what the window does, so a stall or a dead header button
// can be read off it: searches (timings), main-thread stalls, input events, slow frames, GLib
// warnings. No file names are logged, only search words.

use gtk::prelude::*;
use gtk::{gdk, glib};
use std::cell::Cell;
use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
static T0: OnceLock<Instant> = OnceLock::new();
static WRITTEN: AtomicUsize = AtomicUsize::new(0);
static WHAT: Mutex<&'static str> = Mutex::new("idle"); // what the main thread is doing, for the stall lines
static BEAT: AtomicU64 = AtomicU64::new(0); // ms since T0 of the main loop's last tick
static MOTION: AtomicU64 = AtomicU64::new(0);
static SCROLL: AtomicU64 = AtomicU64::new(0);
static KEYS: AtomicU64 = AtomicU64::new(0);
static BUTTONS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    pub static TYPED: Cell<Option<Instant>> = const { Cell::new(None) }; // last keystroke in the search box
    static SPLICED: Cell<Option<Instant>> = const { Cell::new(None) }; // when results were last put in the list
    static FRAME: Cell<Option<Instant>> = const { Cell::new(None) };
}

macro_rules! dlog { ($($a:tt)*) => { $crate::diag::line(&format!($($a)*)) } }
pub(crate) use dlog;

pub fn line(msg: &str) {
    let (Some(f), Some(t0)) = (FILE.get(), T0.get()) else {
        return;
    };
    let l = format!("[{:9.3}] {msg}\n", t0.elapsed().as_secs_f64());
    if WRITTEN.fetch_add(l.len(), Relaxed) < 8 << 20 {
        let _ = f.lock().unwrap().write_all(l.as_bytes());
    }
}

// The window's stalls from the last run and this one, for Report a Problem (newest last).
pub fn stalls(path: &std::path::Path) -> String {
    let mut o = String::new();
    for (p, run) in [(path.with_extension("log.1"), "last run"), (path.to_path_buf(), "this run")] {
        let s = std::fs::read_to_string(&p).unwrap_or_default();
        let l: Vec<&str> = s.lines().filter(|l| l.contains("STALL")).collect();
        o += &format!("window stalls, {run}: {}\n", l.len());
        for l in &l[l.len().saturating_sub(20)..] {
            o += &format!("  {l}\n");
        }
    }
    o
}

pub fn what(s: &'static str) {
    *WHAT.lock().unwrap() = s;
}

pub fn ms(t: Instant) -> String {
    format!("{:.1} ms", t.elapsed().as_secs_f64() * 1000.0)
}

// Results were just put in the list: the next paint is timed against this.
pub fn spliced() {
    SPLICED.set(Some(Instant::now()));
}

fn tick() -> u64 {
    T0.get().map_or(0, |t| t.elapsed().as_millis() as u64)
}

fn read(p: &str) -> String {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .trim()
        .to_string()
}

// State and CPU ticks of the main thread (its tid is the pid).
fn main_thread() -> (String, u64) {
    let s = read(&format!("/proc/{0}/task/{0}/stat", std::process::id()));
    let f: Vec<&str> = s.rsplit_once(") ").map_or("", |x| x.1).split(' ').collect();
    let ticks = f.get(11).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0)
        + f.get(12).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
    let wchan = read(&format!("/proc/{0}/task/{0}/wchan", std::process::id()));
    (
        format!(
            "{} wchan {}",
            f.first().unwrap_or(&"?"),
            if wchan.is_empty() { "-" } else { &wchan }
        ),
        ticks,
    )
}

// Start Over: this run's log starts again empty (still open), the earlier runs' go.
pub fn clear(path: &std::path::Path) {
    if let Some(f) = FILE.get() {
        let mut f = f.lock().unwrap();
        let _ = f.set_len(0);
        let _ = std::io::Seek::rewind(&mut *f);
    }
    for i in 1..=RUNS {
        let _ = std::fs::remove_file(path.with_extension(format!("log.{i}")));
    }
    dlog!("log erased by Delete All Logs, {}", now());
}

const RUNS: usize = 5; // earlier runs kept: a stall is often a few restarts back by the time the log is copied

fn now() -> glib::GString {
    glib::DateTime::now_local()
        .and_then(|d| d.format("%F %T"))
        .unwrap_or_default()
}

// Before the app starts: the file, the GLib handler, the environment.
pub fn init(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(path));
    // Earlier runs move up one (.1 is the last run, .5 the oldest): after a force quit the
    // stall is in one of those.
    for i in (1..RUNS).rev() {
        let _ = std::fs::rename(
            path.with_extension(format!("log.{i}")),
            path.with_extension(format!("log.{}", i + 1)),
        );
    }
    let _ = std::fs::rename(path, path.with_extension("log.1"));
    let Ok(f) = std::fs::File::create(path) else {
        return;
    };
    let _ = FILE.set(Mutex::new(f));
    let _ = T0.set(Instant::now());
    glib::log_set_default_handler(|domain, level, msg| {
        if !matches!(level, glib::LogLevel::Debug | glib::LogLevel::Info) {
            dlog!("GLIB {level:?} [{}]: {msg}", domain.unwrap_or("-"));
        }
        glib::log_default_handler(domain, level, Some(msg));
    });
    dlog!(
        "fa-gui {} diagnostic log, started {}; send this file with fa-linux-dt.log",
        env!("CARGO_PKG_VERSION"),
        now()
    );
    dlog!(
        "kernel {}  nvidia {}",
        read("/proc/sys/kernel/osrelease"),
        read("/proc/driver/nvidia/version")
            .lines()
            .next()
            .unwrap_or("-")
    );
    for k in [
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "GDK_BACKEND",
        "GSK_RENDERER",
        "GDK_DEBUG",
        "GTK_DEBUG",
        "GDK_SCALE",
        "GDK_DPI_SCALE",
        "GTK_IM_MODULE",
    ] {
        dlog!("env {k}={}", std::env::var(k).unwrap_or_default());
    }
}

// The display, monitors and input devices (after gtk is initialised).
fn display() {
    dlog!(
        "gtk {}.{}.{}  adw {}.{}.{}",
        gtk::major_version(),
        gtk::minor_version(),
        gtk::micro_version(),
        adw::major_version(),
        adw::minor_version(),
        adw::micro_version()
    );
    let Some(d) = gdk::Display::default() else {
        return;
    };
    dlog!("display {} ({})", d.name(), d.type_().name());
    for (i, m) in d.monitors().iter::<gdk::Monitor>().flatten().enumerate() {
        let g = m.geometry();
        dlog!(
            "monitor {i}: {} {}x{} at {},{} scale {} refresh {} mHz",
            m.connector().unwrap_or_default(),
            g.width(),
            g.height(),
            g.x(),
            g.y(),
            m.scale_factor(),
            m.refresh_rate()
        );
    }
    if let Some(s) = d.default_seat() {
        for dev in std::iter::once(s.pointer())
            .chain(std::iter::once(s.keyboard()))
            .flatten()
            .chain(s.devices(gdk::SeatCapabilities::ALL))
        {
            dlog!("device {:?}: source {:?}", dev.name(), dev.source());
        }
        for t in s.tools() {
            dlog!("tool {:?} serial {}", t.tool_type(), t.serial());
        }
    }
}

fn chain(w: Option<gtk::Widget>) -> String {
    let mut o = Vec::new();
    let mut w = w;
    while let Some(x) = w {
        if o.len() == 5 {
            break;
        }
        o.push(format!(
            "{}{}",
            x.css_name(),
            if x.is_sensitive() {
                ""
            } else {
                "(insensitive)"
            }
        ));
        w = x.parent();
    }
    o.join(" < ")
}

// Hooks on the window: input events, stalls, frames, state changes, a 30 s heartbeat.
pub fn watch(win: &gtk::Window, rows: impl Fn() -> String + 'static) {
    if FILE.get().is_none() {
        return;
    }
    // Every button, touch, pen, focus, enter/leave and grab event the window gets, before any
    // widget sees it: if a click never shows here it never reached the app.
    let ev = gtk::EventControllerLegacy::new();
    ev.set_propagation_phase(gtk::PropagationPhase::Capture);
    ev.connect_event(|c, e| {
        use gdk::EventType as T;
        let t = e.event_type();
        match t {
            T::MotionNotify => {
                MOTION.fetch_add(1, Relaxed);
            }
            T::Scroll => {
                SCROLL.fetch_add(1, Relaxed);
            }
            T::KeyPress | T::KeyRelease => {
                KEYS.fetch_add(1, Relaxed);
            }
            T::ButtonPress
            | T::ButtonRelease
            | T::TouchBegin
            | T::TouchEnd
            | T::TouchCancel
            | T::GrabBroken
            | T::ProximityIn
            | T::ProximityOut
            | T::PadButtonPress
            | T::FocusChange
            | T::EnterNotify
            | T::LeaveNotify
            | T::TouchpadHold => {
                let dev = e
                    .device()
                    .map(|d| format!("{:?}/{:?}", d.name(), d.source()))
                    .unwrap_or_default();
                let tool = e
                    .device_tool()
                    .map(|t| format!(" tool {:?}", t.tool_type()))
                    .unwrap_or_default();
                let (x, y) = e.position().unwrap_or((-1.0, -1.0));
                let btn = e
                    .downcast_ref::<gdk::ButtonEvent>()
                    .map(|b| format!(" button {}", b.button()))
                    .unwrap_or_default();
                let at = c
                    .widget()
                    .and_then(|w| w.pick(x, y, gtk::PickFlags::DEFAULT))
                    .map(|w| chain(Some(w)))
                    .unwrap_or_default();
                if matches!(t, T::ButtonPress | T::ButtonRelease) {
                    BUTTONS.fetch_add(1, Relaxed);
                }
                dlog!("event {t:?}{btn} at {x:.0},{y:.0} dev {dev}{tool} -> {at}");
            }
            _ => {}
        }
        glib::Propagation::Proceed
    });
    win.add_controller(ev);
    // The same press as the widgets see it (a header button, the title bar's drag).
    let g = gtk::GestureClick::builder()
        .button(0)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    g.connect_pressed(|g, n, x, y| {
        dlog!(
            "gesture pressed button {} n={n} at {x:.0},{y:.0} widget {}",
            g.current_button(),
            chain(
                g.widget()
                    .and_then(|w| w.pick(x, y, gtk::PickFlags::DEFAULT))
            )
        )
    });
    g.connect_released(|g, n, x, y| {
        dlog!(
            "gesture released button {} n={n} at {x:.0},{y:.0}",
            g.current_button()
        )
    });
    g.connect_cancel(|g, _| dlog!("gesture CANCELLED button {}", g.current_button()));
    g.connect_stopped(|g| dlog!("gesture stopped button {}", g.current_button()));
    win.add_controller(g);

    for p in [
        "is-active",
        "maximized",
        "fullscreened",
        "focus-widget",
        "modal",
        "scale-factor",
        "default-width",
        "default-height",
    ] {
        win.connect_notify_local(Some(p), move |w, _| {
            let v = w.property_value(p);
            let v = v
                .get::<bool>()
                .map(|b| b.to_string())
                .or_else(|_| v.get::<i32>().map(|i| i.to_string()))
                .unwrap_or_else(|_| {
                    GtkWindowExt::focus(w)
                        .map(|f| chain(Some(f)))
                        .unwrap_or_default()
                });
            dlog!("window {p} = {v}");
        });
    }
    win.connect_close_request(|_| {
        dlog!("close requested");
        glib::Propagation::Proceed
    });
    win.connect_realize(|w| {
        display();
        if let Some(s) = w.surface() {
            dlog!("surface {} scale {}", s.type_().name(), s.scale());
            s.connect_notify_local(Some("state"), |s, _| {
                if let Some(t) = s.downcast_ref::<gdk::Toplevel>() {
                    dlog!("toplevel state {:?}", t.state());
                }
            });
        }
        dlog!(
            "renderer {}",
            w.native()
                .and_then(|n| n.renderer())
                .map_or("none".into(), |r| r.type_().name().to_string())
        );
        if let Some(c) = w.frame_clock() {
            c.connect_before_paint(|_| FRAME.set(Some(Instant::now())));
            c.connect_after_paint(|_| {
                if let Some(t) = FRAME
                    .take()
                    .filter(|t| t.elapsed() > Duration::from_millis(40))
                {
                    dlog!("slow frame {}", ms(t));
                }
                if let Some(t) = SPLICED.take() {
                    dlog!("first paint after new results: {}", ms(t));
                }
            });
        }
    });

    // The main loop's pulse, and a thread that notices when it stops.
    BEAT.store(tick(), Relaxed);
    glib::timeout_add_local(Duration::from_millis(50), || {
        BEAT.store(tick(), Relaxed);
        glib::ControlFlow::Continue
    });
    std::thread::spawn(|| {
        let mut stalled: Option<(u64, u64)> = None; // (when it began, main thread ticks then)
        let mut said = 0;
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let lag = tick().saturating_sub(BEAT.load(Relaxed));
            match stalled {
                None if lag > 200 => {
                    let (st, ticks) = main_thread();
                    dlog!(
                        "STALL: main loop silent for {lag} ms; doing: {}; main thread {st}",
                        WHAT.lock().unwrap()
                    );
                    stalled = Some((tick() - lag, ticks));
                    said = lag;
                }
                Some((from, ticks)) if lag > said + 1000 => {
                    let (st, now) = main_thread();
                    dlog!("STALL continues: {} ms; doing: {}; main thread {st}, {} ticks of CPU since it began", tick() - from, WHAT.lock().unwrap(), now.saturating_sub(ticks));
                    said = lag;
                }
                Some((from, ticks)) if lag < 100 => {
                    let (_, now) = main_thread();
                    dlog!("STALL over: about {} ms, {} ticks (10 ms each) of main thread CPU; doing: {}", tick() - from, now.saturating_sub(ticks), WHAT.lock().unwrap());
                    stalled = None;
                }
                _ => {}
            }
        }
    });
    glib::timeout_add_seconds_local(30, move || {
        let st = read("/proc/self/status");
        let f = |k: &str| {
            st.lines()
                .find_map(|l| l.strip_prefix(k))
                .unwrap_or("?")
                .trim()
                .to_string()
        };
        dlog!(
            "alive: rss {} threads {} | in 30 s: motion {} scroll {} keys {} buttons {} | {}",
            f("VmRSS:"),
            f("Threads:"),
            MOTION.swap(0, Relaxed),
            SCROLL.swap(0, Relaxed),
            KEYS.swap(0, Relaxed),
            BUTTONS.swap(0, Relaxed),
            rows()
        );
        glib::ControlFlow::Continue
    });
}
