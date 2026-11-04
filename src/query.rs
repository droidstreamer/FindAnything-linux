// Search syntax, close to Everything's. Words are ANDed, `a | b` ORs, `!word` excludes,
// "quotes" keep spaces, `*` and `?` match the whole name, a word with a slash matches the full
// path, and `name:value` modifiers filter by type, extension, folder, children, or set match
// options. A bare option modifier (`case:`) applies to every word. Natural sort lives here too.

use std::borrow::Cow;
use std::cmp::Ordering;

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Opts {
    pub case: bool,    // case-sensitive
    pub word: bool,    // whole words only
    pub start: bool,   // name starts with it
    pub end: bool,     // name ends with it (start + end: whole filename)
    pub punct: bool,   // ignore punctuation
    pub accents: bool, // ignore accents: é finds e
}

#[derive(Debug, PartialEq)]
pub enum Kind {
    Text(Vec<u8>, bool), // needle (already normalized for the options); true: match the full path
    Glob(Vec<u8>, bool), // * and ? against the whole name (or full path)
    Ext(Vec<Vec<u8>>),   // lower-case extensions, files only
    File,
    Folder,
    Parent(Vec<u8>), // the containing folder is exactly this path
    Child(Vec<u8>),  // a folder holding an entry whose name contains this
    Empty,           // a folder with nothing in it
}

#[derive(Debug)]
pub struct Term {
    pub not: bool,
    pub kinds: Vec<Kind>, // all must match
    pub o: Opts,
}

#[derive(Debug, Default)]
pub struct Query {
    pub groups: Vec<Vec<Term>>, // every group must match; any term in a group will do
    pub path: bool,             // some term needs the entry's folder path
    pub kids: bool,             // some term needs a folder's children
    plain: Option<Vec<Vec<u8>>>, // only plain words, no options: matched directly (the fast path)
}

// What a query is checked against: one name in the index.
pub struct Entry<'a> {
    pub name: &'a [u8],
    pub dir: bool,
    pub folder: &'a [u8], // containing folder, ending in "/" (only filled if Query::path)
    pub full: &'a [u8],   // folder + name (+ "/" for a folder) (only filled if Query::path)
    pub kids: &'a [&'a [u8]], // a folder's entries (only filled if Query::kids)
}

fn set_opt(o: &mut Opts, m: &str) -> bool {
    match m {
        "case" => o.case = true,
        "nocase" => o.case = false,
        "ww" | "wholeword" => o.word = true,
        "startwith" => o.start = true,
        "endwith" => o.end = true,
        "wfn" | "wholefilename" => (o.start, o.end) = (true, true),
        "nopunc" | "ignorepunc" => o.punct = true,
        "noaccents" | "nodiacritics" => o.accents = true,
        _ => return false,
    }
    true
}

// Everything's type macros, also used by the window's filter list.
fn macro_ext(m: &str) -> Option<&'static str> {
    Some(match m {
        "image" | "pic" => {
            "jpg;jpeg;png;gif;webp;bmp;tif;tiff;heic;heif;avif;svg;ico;raw;cr2;nef;arw;dng"
        }
        "audio" => "mp3;flac;wav;ogg;oga;opus;m4a;aac;wma;aiff;mid;midi",
        "video" => "mp4;mkv;avi;mov;webm;wmv;flv;m4v;mpg;mpeg;ts;3gp",
        "doc" => "pdf;doc;docx;odt;rtf;txt;md;xls;xlsx;ods;ppt;pptx;odp;epub;csv",
        "zip" | "archive" => "zip;7z;rar;tar;gz;tgz;bz2;xz;zst;iso",
        "exe" | "program" => "exe;msi;bat;cmd;sh;appimage;deb;rpm;flatpakref;run;bin",
        _ => return None,
    })
}

fn ext_list(s: &str) -> Vec<Vec<u8>> {
    s.split([';', ','])
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase().into_bytes())
        .filter(|e| !e.is_empty())
        .collect()
}

// Words, with "quoted parts" kept whole; a `|` outside quotes is a word of its own.
fn words(q: &str) -> Vec<String> {
    let mut words = Vec::new();
    let (mut cur, mut quoted) = (String::new(), false);
    for c in q.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            '|' if !quoted => {
                words.push(std::mem::take(&mut cur));
                words.push("|".into());
            }
            c if c.is_whitespace() && !quoted => words.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    words.push(cur);
    words.retain(|w| !w.is_empty());
    words
}

pub fn parse(q: &str) -> Query {
    let mut words = words(q);
    let mut g = Opts::default();
    words
        .retain(|w| !(w.ends_with(':') && set_opt(&mut g, &w[..w.len() - 1].to_ascii_lowercase())));
    let mut query = Query::default();
    let mut join = false;
    for w in &words {
        if w == "|" {
            join = true;
            continue;
        }
        let Some(t) = term(w, g) else { continue };
        for k in &t.kinds {
            query.path |= matches!(
                k,
                Kind::Text(_, true) | Kind::Glob(_, true) | Kind::Parent(_)
            );
            query.kids |= matches!(k, Kind::Child(_) | Kind::Empty);
        }
        match query.groups.last_mut() {
            Some(last) if join => last.push(t),
            _ => query.groups.push(vec![t]),
        }
        join = false;
    }
    let plain = |g: &Vec<Term>| match &g[..] {
        [Term {
            not: false,
            kinds,
            o,
        }] if *o == Opts::default() => match &kinds[..] {
            [Kind::Text(n, false)] => Some(n.clone()),
            _ => None,
        },
        _ => None,
    };
    query.plain = query.groups.iter().map(plain).collect();
    query
}

fn term(w: &str, mut o: Opts) -> Option<Term> {
    let not = w.starts_with('!');
    let mut w = w.trim_start_matches('!');
    let (mut kinds, mut full) = (Vec::new(), false);
    // Modifiers stack (`file:case:abc`); a word like `c:` that isn't one stays plain text.
    while let Some((m, rest)) = w.split_once(':') {
        let m = m.to_ascii_lowercase();
        if m.is_empty() || !m.bytes().all(|b| b.is_ascii_lowercase()) {
            break;
        }
        if !set_opt(&mut o, &m) {
            match m.as_str() {
                "file" | "files" => kinds.push(Kind::File),
                "folder" | "folders" | "dir" => kinds.push(Kind::Folder),
                "path" => full = true,
                "empty" => kinds.push(Kind::Empty),
                "ext" => {
                    kinds.push(Kind::Ext(ext_list(rest)));
                    return Some(Term { not, kinds, o });
                }
                "parent" | "infolder" => {
                    let p = rest.replace('"', "");
                    let p = if p.len() > 1 {
                        p.trim_end_matches('/')
                    } else {
                        &p
                    };
                    kinds.push(Kind::Parent(p.as_bytes().to_vec()));
                    return Some(Term { not, kinds, o });
                }
                "child" => {
                    kinds.push(Kind::Child(
                        norm(rest.replace('"', "").as_bytes(), o).into_owned(),
                    ));
                    return Some(Term { not, kinds, o });
                }
                m => match macro_ext(m) {
                    Some(l) => kinds.push(Kind::Ext(ext_list(l))),
                    None => break,
                },
            }
        }
        w = rest;
    }
    if !w.is_empty() {
        let v = w.replace('"', "");
        // `!word` leaves out anything with the word anywhere in its path: the name, a folder, or
        // the drive's mount point (`!BACKUP` hides /mnt/drives/BACKUP). Wildcards stay on names.
        full |= v.contains('/');
        if !w.contains('"') && (v.contains('*') || v.contains('?')) {
            kinds.push(Kind::Glob(v.into_bytes(), full));
        } else {
            kinds.push(Kind::Text(norm(v.as_bytes(), o).into_owned(), full || not));
        }
    }
    (!kinds.is_empty()).then_some(Term { not, kinds, o })
}

// `content:word` terms: the daemon only knows names, so the searcher (running as the user) reads
// the files the rest of the query finds and keeps those holding every word.
pub struct Content {
    pub words: Vec<Vec<u8>>, // lower-cased unless case
    pub case: bool,
}

pub const CONTENT_MAX: u64 = 64 << 20; // bigger files are skipped

// The query without its content terms (files only), and the content terms if there were any.
pub fn split_content(q: &str) -> (String, Option<Content>) {
    let (mut rest, mut found) = (Vec::new(), Vec::new());
    for w in words(q) {
        let u = w.replace('"', "");
        match u.get(..8).filter(|m| m.eq_ignore_ascii_case("content:")) {
            Some(_) => found.push(u[8..].to_string()),
            None => rest.push(w),
        }
    }
    if found.is_empty() {
        return (q.to_string(), None);
    }
    let case = rest.iter().any(|w| w.eq_ignore_ascii_case("case:"));
    let words = found
        .into_iter()
        .filter(|w| !w.is_empty())
        .map(|w| {
            if case {
                w.into_bytes()
            } else {
                w.to_lowercase().into_bytes()
            }
        })
        .collect();
    rest.push("file:".into());
    (rest.join(" "), Some(Content { words, case }))
}

// Does this file hold every word? Binary files (a NUL in the first 8 KB) and huge ones don't count.
// Only regular files are opened (a pipe would block, a device file may act on being opened),
// without updating their access time where allowed (O_NOATIME: files you own).
pub fn file_has(path: &str, c: &Content) -> bool {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let open = |noatime| {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | noatime)
            .open(path)
    };
    let Ok(mut f) = open(libc::O_NOATIME).or_else(|_| open(0)) else {
        return false;
    };
    if f.metadata()
        .map_or(true, |m| !m.is_file() || m.len() > CONTENT_MAX)
    {
        return false;
    }
    let mut buf = Vec::new();
    if (&mut f).take(8192).read_to_end(&mut buf).is_err()
        || buf.contains(&0)
        || f.take(CONTENT_MAX).read_to_end(&mut buf).is_err()
    {
        return false;
    }
    if !c.case {
        buf.make_ascii_lowercase();
        if !buf.is_ascii() {
            buf = String::from_utf8_lossy(&buf).to_lowercase().into_bytes();
        }
    }
    let exact = Opts {
        case: true,
        ..Opts::default()
    };
    c.words.iter().all(|w| find(&buf, w, exact))
}

impl Query {
    // Words (lower case) every match has in its name: a folder whose names lack one is skipped.
    pub fn needed(&self) -> Vec<Vec<u8>> {
        let one = self
            .groups
            .iter()
            .filter(|g| g.len() == 1 && !g[0].not && !g[0].o.punct && !g[0].o.accents);
        one.flat_map(|g| &g[0].kinds)
            .filter_map(|k| match k {
                Kind::Text(n, false) if !n.is_empty() => Some(n.to_ascii_lowercase()),
                _ => None,
            })
            .collect()
    }

    pub fn matches(&self, e: &Entry) -> bool {
        if let Some(words) = &self.plain {
            return words.iter().all(|w| {
                w.is_empty() || e.name.windows(w.len()).any(|x| x.eq_ignore_ascii_case(w))
            });
        }
        self.groups.iter().all(|g| {
            g.iter()
                .any(|t| t.not != t.kinds.iter().all(|k| kind(k, t.o, e)))
        })
    }
}

fn kind(k: &Kind, o: Opts, e: &Entry) -> bool {
    let full = e.full;
    match k {
        Kind::Text(n, false) => find(&norm(e.name, o), n, o),
        // Position options are about names, not paths.
        Kind::Text(n, true) => find(
            &norm(full, o),
            n,
            Opts {
                start: false,
                end: false,
                ..o
            },
        ),
        Kind::Glob(p, f) => glob(if *f { full } else { e.name }, p, o.case),
        Kind::Ext(l) => {
            !e.dir
                && e.name
                    .iter()
                    .rposition(|&b| b == b'.')
                    .is_some_and(|i| l.iter().any(|x| x.eq_ignore_ascii_case(&e.name[i + 1..])))
        }
        Kind::File => !e.dir,
        Kind::Folder => e.dir,
        Kind::Parent(p) => {
            let f = if e.folder.len() > 1 {
                &e.folder[..e.folder.len() - 1]
            } else {
                e.folder
            };
            if o.case {
                f == &p[..]
            } else {
                f.eq_ignore_ascii_case(p)
            }
        }
        Kind::Child(n) => {
            e.dir
                && e.kids.iter().any(|c| {
                    find(
                        &norm(c, o),
                        n,
                        Opts {
                            start: false,
                            end: false,
                            word: false,
                            ..o
                        },
                    )
                })
        }
        Kind::Empty => e.dir && e.kids.is_empty(),
    }
}

// Is `needle` in `hay`, where the options allow? Case folding is ASCII only, like the index.
pub fn find(hay: &[u8], needle: &[u8], o: Opts) -> bool {
    let n = needle.len();
    if n > hay.len() {
        return false;
    }
    let eq = |w: &[u8]| {
        if o.case {
            w == needle
        } else {
            w.eq_ignore_ascii_case(needle)
        }
    };
    let alnum = |b: u8| b.is_ascii_alphanumeric() || b >= 0x80;
    let fits = |i: usize| {
        (!o.start || i == 0)
            && (!o.end || i + n == hay.len())
            && (!o.word
                || ((i == 0 || !alnum(hay[i - 1])) && (i + n == hay.len() || !alnum(hay[i + n]))))
    };
    if o.start {
        return fits(0) && eq(&hay[..n]);
    }
    if o.end {
        let i = hay.len() - n;
        return fits(i) && eq(&hay[i..]);
    }
    if n == 0 {
        return true;
    }
    if !o.word {
        // The common case, kept to the plain form the compiler optimizes best.
        return if o.case {
            hay.windows(n).any(|w| w == needle)
        } else {
            hay.windows(n).any(|w| w.eq_ignore_ascii_case(needle))
        };
    }
    (0..=hay.len() - n).any(|i| eq(&hay[i..i + n]) && fits(i))
}

// Whole-string match with `*` (anything) and `?` (one character).
pub fn glob(s: &[u8], p: &[u8], case: bool) -> bool {
    let next = |i: usize| {
        (i + 1..s.len())
            .find(|&j| s[j] & 0xC0 != 0x80)
            .unwrap_or(s.len())
    }; // past one UTF-8 char
    let eq = |a: u8, b: u8| {
        if case {
            a == b
        } else {
            a.eq_ignore_ascii_case(&b)
        }
    };
    let (mut si, mut pi, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'?' {
            si = next(si);
            pi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < p.len() && eq(p[pi], s[si]) {
            si += 1;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark = next(mark);
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

// ponytail: Latin-1 and common Latin Extended-A letters only; add a Unicode table if names in other scripts need it.
const ACCENTED: &str = "ÀÁÂÃÄÅÇÈÉÊËÌÍÎÏÑÒÓÔÕÖØÙÚÛÜÝàáâãäåçèéêëìíîïñòóôõöøùúûüýÿĀāĂăĄąĆćČčĎďĒēĖėĘęĚěĞğĪīĮįİıŁłŃńŇňŌōŐőŘřŚśŞşŠšŢţŤťŪūŮůŰűŲųŸŹźŻżŽž";
const PLAIN: &str = "AAAAAACEEEEIIIINOOOOOOUUUUYaaaaaaceeeeiiiinoooooouuuuyyAaAaAaCcCcDdEeEeEeEeGgIiIiIiLlNnNnOoOoRrSsSsSsTtTtUuUuUuUuYZzZzZz";

// The name as the options compare it: without punctuation and/or accents. Borrowed when unchanged.
fn norm(s: &[u8], o: Opts) -> Cow<'_, [u8]> {
    let accents = o.accents && !s.is_ascii();
    if !accents && !(o.punct && s.iter().any(|b| b.is_ascii_punctuation())) {
        return Cow::Borrowed(s);
    }
    let mut out = String::new();
    for c in String::from_utf8_lossy(s).chars() {
        let c = if accents && !c.is_ascii() {
            ACCENTED
                .chars()
                .position(|a| a == c)
                .and_then(|i| PLAIN.chars().nth(i))
                .unwrap_or(c)
        } else {
            c
        };
        if !(o.punct && !c.is_alphanumeric() && !c.is_whitespace()) {
            out.push(c);
        }
    }
    Cow::Owned(out.into_bytes())
}

// Natural order, ignoring ASCII case: file2 before file10.
pub fn natural(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let zeros = |s: &[u8]| s.iter().position(|&c| c != b'0').unwrap_or(s.len());
            let (x, y) = (&a[si..i][zeros(&a[si..i])..], &b[sj..j][zeros(&b[sj..j])..]);
            let o = x.len().cmp(&y.len()).then(x.cmp(y));
            if o != Ordering::Equal {
                return o;
            }
        } else {
            let o = a[i].to_ascii_lowercase().cmp(&b[j].to_ascii_lowercase());
            if o != Ordering::Equal {
                return o;
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Does `q` match `name` (in /home/user/, with these children if it's a folder)?
    fn m(q: &str, name: &str) -> bool {
        let dir = name.ends_with('/');
        let name = name.trim_end_matches('/');
        let kids: &[&[u8]] = if name == "proj" {
            &[b"Cargo.toml", b"src"]
        } else {
            &[]
        };
        let full = [
            &b"/home/user/"[..],
            name.as_bytes(),
            if dir { b"/" } else { b"" },
        ]
        .concat();
        parse(q).matches(&Entry {
            name: name.as_bytes(),
            dir,
            folder: b"/home/user/",
            full: &full,
            kids,
        })
    }

    #[test]
    fn syntax() {
        assert!(m("photo", "Holiday PHOTO.jpg"));
        assert!(m("hol jpg", "Holiday PHOTO.jpg") && !m("hol png", "Holiday PHOTO.jpg"));
        assert!(
            m("\"day photo\"", "Holiday PHOTO.jpg") && !m("\"day  photo\"", "Holiday PHOTO.jpg")
        );
        assert!(m("!png", "a.jpg") && !m("!jpg", "a.jpg"));
        assert!(!m("a !USER", "a.txt") && !m("!home", "src/") && m("!backup", "a.txt")); // the path counts too
        assert!(!m("!*.txt", "a.txt") && m("!*user*", "a.txt")); // wildcards: the name only
        assert!(m("png | jpg", "a.jpg") && m("png|jpg", "a.jpg") && !m("png|gif", "a.jpg"));
        assert!(m("a png|jpg", "a.jpg") && !m("x png|jpg", "a.jpg")); // OR binds tighter than AND
        assert!(m("*.png", "shot.PNG") && !m("*.png", "shot.png.txt"));
        assert!(
            m("IMG_????", "IMG_1234") && !m("IMG_????", "IMG_12345") && m("IMG_????", "IMG_12é4")
        );
        assert!(m("ext:png;jpg", "a.JPG") && !m("ext:png", "a.jpg") && !m("ext:png", "png/"));
        assert!(
            m("file:", "a.txt")
                && !m("file:", "src/")
                && m("folder:", "src/")
                && m("folder:sr", "src/")
                && !m("folder:sr", "src")
        );
        assert!(m("/home/user/", "a.txt") && !m("!/home/", "a.txt") && m("user/src/", "src/"));
        assert!(m("/home/*/a.*", "a.txt") && m("path:user", "a.txt"));
        assert!(
            m("parent:/home/user", "a.txt")
                && m("parent:/home/user/", "a.txt")
                && !m("parent:/home", "a.txt")
        );
        assert!(
            m("child:cargo", "proj/") && !m("child:cargo", "other/") && !m("child:cargo", "proj")
        );
        assert!(m("empty:", "other/") && !m("empty:", "proj/") && !m("empty:", "a.txt"));
        assert!(
            m("image:", "a.webp")
                && m("image:cat", "cat.png")
                && !m("image:dog", "cat.png")
                && m("audio:", "x.flac")
        );
        assert!(m("c:", "c:") && m("http://x", "a") == false && m("re:x", "re:xy")); // not modifiers: plain text
        assert!(parse("").groups.is_empty() && parse("case:").groups.is_empty());
    }

    #[test]
    fn options() {
        assert!(
            m("photo", "PHOTO")
                && !m("case: photo", "PHOTO")
                && m("case:PHO", "PHOTO")
                && !m("case:pho", "PHOTO")
        );
        assert!(
            m("ww: cat", "my cat.png")
                && !m("ww: cat", "concat.png")
                && m("ww:cat", "cat")
                && !m("ww:cat", "cats")
        );
        assert!(m("startwith: hol", "Holiday") && !m("startwith: day", "Holiday"));
        assert!(m("endwith: day", "Holiday") && !m("endwith: hol", "Holiday"));
        assert!(m("wfn: holiday", "Holiday") && !m("wfn: holi", "Holiday"));
        assert!(
            m("nopunc: dont", "don't stop.txt")
                && !m("dont", "don't stop.txt")
                && m("nopunc: don't", "dont.txt")
        );
        assert!(
            m("noaccents: cafe", "Café.txt")
                && !m("cafe", "Café.txt")
                && m("noaccents: café", "cafe.txt")
        );
        assert!(m("noaccents: zolc", "żółć") && m("noaccents: Ecole", "École"));
        assert_eq!(ACCENTED.chars().count(), PLAIN.chars().count());
        assert!(m("case: ww: Cat", "a Cat b") && !m("case: ww: Cat", "a cat b"));
    }

    #[test]
    fn needed_words() {
        let n = |q: &str| {
            parse(q)
                .needed()
                .into_iter()
                .map(|w| String::from_utf8(w).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(n("HellBoy png !217"), ["hellboy", "png"]);
        assert_eq!(n("case: Abc ww:def"), ["abc", "def"]);
        assert!(
            n("a | b").is_empty()
                && n("*.png").is_empty()
                && n("/x/ y") == ["y"]
                && n("nopunc: ab").is_empty()
        );
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["file10", "File2", "file1", "file02b", "a", "file2"];
        v.sort_by(|a, b| natural(a.as_bytes(), b.as_bytes()));
        assert_eq!(v, ["a", "file1", "File2", "file2", "file02b", "file10"]);
    }

    #[test]
    fn content() {
        let (q, c) = split_content("ext:txt content:Invoice \"content:due date\" case:");
        assert_eq!(q, "ext:txt case: file:");
        let c = c.unwrap();
        assert!(c.case && c.words == [b"Invoice".to_vec(), b"due date".to_vec()]);
        assert!(split_content("plain words").1.is_none());
        let d = std::env::temp_dir().join(format!("fa-content-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (t, b) = (d.join("a.txt"), d.join("b.bin"));
        std::fs::write(&t, "Ünïcode INVOICE, due Date\n").unwrap();
        std::fs::write(&b, b"invoice\0due date").unwrap();
        let (_, c) = split_content("content:invoice content:\"due date\" content:ünï");
        let c = c.unwrap();
        assert!(file_has(t.to_str().unwrap(), &c) && !file_has(b.to_str().unwrap(), &c));
        assert!(!file_has(
            t.to_str().unwrap(),
            &split_content("case: content:invoice").1.unwrap()
        ));
        std::fs::remove_dir_all(d).unwrap();
    }
}
