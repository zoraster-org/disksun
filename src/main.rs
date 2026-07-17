// disksun — interactive pie disk-usage viewer.
//
// A directory is drawn as a PIE: every child is a wedge radiating from the
// centre, its angle proportional to its share of the directory. Click a
// directory wedge to descend; "Up" / h / Backspace to go back. Drag any
// wedge onto the trash can (bottom-right) to move that file/folder to the
// Trash. The sidebar lists the largest children; the top bar lets you
// rescan an arbitrary path, jump to any mounted partition, or scan the
// whole disk as admin (root, via sudo in a terminal).
//
// Two run modes:
//   disksun [PATH]              GUI, scans PATH (default $HOME)
//   disksun --scan [--cross] P  headless walker: prints the tree to stdout
//                               (used by the GUI's admin scan under sudo)

use std::collections::HashSet;
use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use eframe::egui::{self, vec2, Align2, Color32, FontId, Rect, Sense, Shape, Stroke};

const MIN_WEDGE_ANGLE: f32 = 0.02;  // rad; thinner children get lumped
const MAX_RINGS: usize = 20;
const TAU: f32 = std::f32::consts::TAU;

// ---------------------------------------------------------------- mounts

/// Virtual/pseudo filesystem types — never worth scanning, always skipped.
const VIRTUAL_FS: &[&str] = &[
    "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "ramfs", "cgroup", "cgroup2",
    "bpf", "securityfs", "debugfs", "tracefs", "fusectl", "configfs", "mqueue",
    "hugetlbfs", "pstore", "efivarfs", "autofs", "binfmt_misc", "rpc_pipefs",
    "overlay", "squashfs", "nsfs", "selinuxfs",
];

fn is_virtual_fs(ty: &str) -> bool {
    VIRTUAL_FS.contains(&ty)
        || ty.starts_with("fuse")
        || ty.starts_with("nfs")
        || ty.starts_with("cifs")
        || ty == "9p"
        || ty == "smb3"
}

struct Mount {
    point: PathBuf,
    fstype: String,
    real: bool, // backed by a block device (worth offering / scanning)
}

fn read_mounts() -> Vec<Mount> {
    let mut out = Vec::new();
    let Ok(txt) = std::fs::read_to_string("/proc/mounts") else {
        return out;
    };
    for line in txt.lines() {
        let mut f = line.split_whitespace();
        let (Some(dev), Some(mp), Some(ty)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let point = PathBuf::from(mp.replace("\\040", " ")); // octal-escaped spaces
        let real = !is_virtual_fs(ty) && dev.starts_with("/dev/");
        out.push(Mount { point, fstype: ty.to_string(), real });
    }
    out
}

/// Split mounts into (always-skip virtual points, real-fs mount points).
fn mount_sets() -> (HashSet<PathBuf>, HashSet<PathBuf>) {
    let mut virtual_pts = HashSet::new();
    let mut real_pts = HashSet::new();
    for m in read_mounts() {
        if m.real {
            real_pts.insert(m.point);
        } else {
            virtual_pts.insert(m.point);
        }
    }
    (virtual_pts, real_pts)
}

// ---------------------------------------------------------------- scanning

struct Node {
    name: String,
    size: u64,
    is_dir: bool,
    children: Vec<Node>, // sorted by size, descending
}

#[derive(Default)]
struct Progress {
    files: AtomicU64,
    bytes: AtomicU64,
    done: AtomicBool,
}

struct WalkCfg {
    target: PathBuf,
    virtual_pts: HashSet<PathBuf>,
    real_pts: HashSet<PathBuf>,
    cross_real: bool, // descend into other real-fs mounts (whole-disk scan)
}

fn walk(
    path: &Path,
    cfg: &WalkCfg,
    seen: &mut HashSet<(u64, u64)>,
    prog: &Progress,
) -> Node {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let mut node = Node { name, size: 0, is_dir: true, children: Vec::new() };

    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return node, // unreadable (e.g. root-only without admin)
    };
    for entry in entries.flatten() {
        let ftype = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ftype.is_symlink() {
            continue;
        }
        let p = entry.path();
        if ftype.is_dir() {
            if cfg.virtual_pts.contains(&p) {
                continue; // virtual/network mount
            }
            if !cfg.cross_real && p != cfg.target && cfg.real_pts.contains(&p) {
                continue; // a different real volume — scan those separately
            }
            let child = walk(&p, cfg, seen, prog);
            node.size += child.size;
            node.children.push(child);
        } else {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Count hardlinked content once (the Nix store is full of these).
            if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
                continue;
            }
            let size = meta.blocks() * 512; // real disk usage, like du
            node.size += size;
            prog.files.fetch_add(1, Ordering::Relaxed);
            prog.bytes.fetch_add(size, Ordering::Relaxed);
            node.children.push(Node {
                name: entry.file_name().to_string_lossy().into_owned(),
                size,
                is_dir: false,
                children: Vec::new(),
            });
        }
    }
    node.children.sort_by(|a, b| b.size.cmp(&a.size));
    node
}

fn build_cfg(target: &Path, cross_real: bool) -> WalkCfg {
    let (virtual_pts, real_pts) = mount_sets();
    WalkCfg { target: target.to_path_buf(), virtual_pts, real_pts, cross_real }
}

/// In-process scan on a worker thread (runs as the current user).
fn start_scan(path: PathBuf, cross_real: bool, prog: Arc<Progress>) -> mpsc::Receiver<ScanResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let cfg = build_cfg(&path, cross_real);
        let mut seen = HashSet::new();
        let mut root = walk(&path, &cfg, &mut seen, &prog);
        root.name = path.to_string_lossy().into_owned();
        prog.done.store(true, Ordering::Relaxed);
        let _ = tx.send(Ok(root));
    });
    rx
}

// -------- admin scan: sudo <self --scan> in a terminal, tree via a file ----

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

/// Spawn a foot terminal running `sudo disksun --scan …`, wait for it to
/// finish, then parse the emitted tree. The user authenticates in the
/// terminal (no polkit agent needed).
fn start_admin_scan(path: PathBuf, cross_real: bool) -> mpsc::Receiver<ScanResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        let base = format!("{rt}/disksun-scan-{}", std::process::id());
        let out = PathBuf::from(format!("{base}.tree"));
        let done = PathBuf::from(format!("{base}.done"));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&done);

        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.send(Err(format!("cannot find own path: {e}")));
                return;
            }
        };
        let cross = if cross_real { "--cross" } else { "" };
        let cmd = format!(
            "echo 'disksun needs admin rights to read every file.'; \
             sudo {exe} --scan {cross} {tgt} > {out} && touch {done}; \
             ec=$?; if [ $ec -ne 0 ]; then echo FAIL > {done}; \
             echo 'scan failed — press Enter'; read _; fi",
            exe = shell_quote(&exe),
            tgt = shell_quote(&path),
            out = shell_quote(&out),
            done = shell_quote(&done),
        );
        let spawn = std::process::Command::new("foot")
            .args(["--app-id=disksun-sudo", "--title=disksun (admin scan)", "-e", "sh", "-c", &cmd])
            .spawn();
        if let Err(e) = spawn {
            let _ = tx.send(Err(format!("could not launch terminal for sudo: {e}")));
            return;
        }

        // Poll for completion (up to 15 min).
        for _ in 0..(15 * 60 * 5) {
            if done.exists() {
                if std::fs::read_to_string(&done).map(|s| s.contains("FAIL")).unwrap_or(false) {
                    let _ = tx.send(Err("admin scan failed (see the terminal)".into()));
                    let _ = std::fs::remove_file(&done);
                    return;
                }
                match std::fs::read_to_string(&out) {
                    Ok(txt) => {
                        let mut root = parse_tree(&txt);
                        root.name = path.to_string_lossy().into_owned();
                        let _ = std::fs::remove_file(&out);
                        let _ = std::fs::remove_file(&done);
                        let _ = tx.send(Ok(root));
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("cannot read scan output: {e}")));
                    }
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let _ = tx.send(Err("admin scan timed out".into()));
    });
    rx
}

// ---- headless --scan mode: emit the tree as `depth\tis_dir\tsize\tname` ----

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t")
}
fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn emit(node: &Node, depth: usize, w: &mut impl Write) {
    let _ = writeln!(w, "{depth}\t{}\t{}\t{}", node.is_dir as u8, node.size, esc(&node.name));
    for c in &node.children {
        emit(c, depth + 1, w);
    }
}

fn parse_tree(txt: &str) -> Node {
    let mut stack: Vec<Node> = Vec::new();
    for line in txt.lines() {
        let mut f = line.splitn(4, '\t');
        let (Some(d), Some(dir), Some(sz), Some(name)) = (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        let depth: usize = d.parse().unwrap_or(0);
        let node = Node {
            name: unesc(name),
            size: sz.parse().unwrap_or(0),
            is_dir: dir == "1",
            children: Vec::new(),
        };
        while stack.len() > depth {
            let child = stack.pop().unwrap();
            if let Some(top) = stack.last_mut() {
                top.children.push(child);
            } else {
                stack.push(child); // pathological; keep as root
                break;
            }
        }
        stack.push(node);
    }
    while stack.len() > 1 {
        let child = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(child);
    }
    stack.pop().unwrap_or(Node { name: "(empty)".into(), size: 0, is_dir: true, children: Vec::new() })
}

fn run_scan_mode(args: &[String]) {
    let mut cross = false;
    let mut path = None;
    for a in args {
        if a == "--cross" {
            cross = true;
        } else if !a.starts_with("--") {
            path = Some(PathBuf::from(a));
        }
    }
    let path = path.unwrap_or_else(|| PathBuf::from("/"));
    let cfg = build_cfg(&path, cross);
    let mut seen = HashSet::new();
    let prog = Progress::default();
    let mut root = walk(&path, &cfg, &mut seen, &prog);
    root.name = path.to_string_lossy().into_owned();
    let stdout = std::io::stdout();
    let mut w = BufWriter::new(stdout.lock());
    emit(&root, 0, &mut w);
    let _ = w.flush();
}

// ---------------------------------------------------------------- helpers

fn disk_free(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut vfs) } != 0 {
        return None;
    }
    let frsize = vfs.f_frsize as u64;
    Some((vfs.f_blocks as u64 * frsize, vfs.f_bavail as u64 * frsize))
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// XDG trash content dirs that exist: the home trash plus per-volume
/// trashes ($topdir/.Trash/$uid and $topdir/.Trash-$uid) on real mounts —
/// the same set `gio trash --empty` empties.
fn trash_files_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(data) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")))
    {
        out.push(data.join("Trash/files"));
    }
    let uid = home.as_ref().and_then(|h| std::fs::metadata(h).ok()).map(|m| m.uid());
    if let Some(uid) = uid {
        for m in read_mounts() {
            if m.real {
                out.push(m.point.join(format!(".Trash/{uid}/files")));
                out.push(m.point.join(format!(".Trash-{uid}/files")));
            }
        }
    }
    out.retain(|p| p.is_dir());
    out
}

/// Total disk usage of everything in the given trash `files/` dirs.
fn trash_size_of(dirs: &[PathBuf]) -> u64 {
    fn dir_size(p: &Path) -> u64 {
        let mut sum = 0;
        if let Ok(entries) = std::fs::read_dir(p) {
            for e in entries.flatten() {
                let Ok(meta) = e.metadata() else { continue };
                if meta.file_type().is_symlink() {
                    continue;
                } else if meta.is_dir() {
                    sum += dir_size(&e.path());
                } else {
                    sum += meta.blocks() * 512; // real usage, like the scanner
                }
            }
        }
        sum
    }
    dirs.iter().map(|d| dir_size(d)).sum()
}

/// Empty the given trash roots per the XDG trash spec: everything under
/// `files/`, the matching `info/*.trashinfo`, and the `directorysizes`
/// cache. Done directly rather than via `gio trash --empty`, which needs
/// the gvfs daemons that minimal (wlroots) sessions don't run.
fn empty_trash_of(dirs: &[PathBuf]) -> bool {
    let mut ok = true;
    for files in dirs {
        if let Ok(entries) = std::fs::read_dir(files) {
            for e in entries.flatten() {
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink());
                ok &= if is_dir {
                    std::fs::remove_dir_all(e.path()).is_ok()
                } else {
                    std::fs::remove_file(e.path()).is_ok()
                };
            }
        }
        let Some(root) = files.parent() else { continue };
        if let Ok(entries) = std::fs::read_dir(root.join("info")) {
            for e in entries.flatten() {
                ok &= std::fs::remove_file(e.path()).is_ok();
            }
        }
        let _ = std::fs::remove_file(root.join("directorysizes"));
    }
    ok
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

// ------------------------------------------------------------------- app

/// One ring segment, for hit-testing and drawing.
struct Wedge {
    ring: usize, // 0 = innermost ring, hugging the hub
    a0: f32,
    a1: f32,
    hue: f32,
    is_dir: bool,
    lump: bool, // the grey "(N small items)" aggregate
    name: String,
    size: u64,
    frac: f32,            // share of the displayed directory
    path: Vec<usize>,     // child-index path from the displayed root
    abs: Option<PathBuf>, // absolute path (None for lumps)
}

type ScanResult = Result<Node, String>;

/// Sidebar rows: real items, "(N smaller items)" pseudo-folders, or notes.
enum SideKind {
    Item { path: Vec<usize>, is_dir: bool },
    /// Navigates into the smaller-items view; key = parent path ([] = shown dir).
    Toggle { key: Vec<usize> },
    Note,
}

struct SideRow {
    label: String,
    color: Color32,
    indent: u8,
    kind: SideKind,
}

fn row_label(c: &Node, total: u64) -> String {
    // full name — the sidebar wraps long names instead of truncating
    format!(
        "{:>9}  {:>5.1}%  {}{}",
        human(c.size),
        c.size as f64 * 100.0 / total.max(1) as f64,
        c.name,
        if c.is_dir { "/" } else { "" },
    )
}

/// Zoom transition: a clicked slice expands to fill the chart (dir_in),
/// or the chart collapses back into the parent's slice (going up).
struct Anim {
    start: f64,
    dur: f32,
    span: (f32, f32), // the slice's angles in the *parent* layout
    shift: usize,     // how many rings the new layout slides inward
    dir_in: bool,
    old: Vec<Wedge>,  // outgoing layout, faded out (zoom-in only)
}

enum State {
    Scanning { rx: mpsc::Receiver<ScanResult>, admin: bool },
    Ready(Node),
    Error(String),
}

struct App {
    scan_root: PathBuf,
    path_input: String,
    state: State,
    prog: Arc<Progress>,
    nav: Vec<usize>,
    mounts: Vec<Mount>,
    wedges: Vec<Wedge>,
    hovered: Option<usize>,
    dragging: Option<usize>,          // index into wedges
    pending_trash: Option<(PathBuf, String)>,
    pending_empty: bool,              // "Empty Trash?" confirm modal is up
    side_sel: Option<usize>,          // keyboard cursor: index into the sidebar rows
    pending_g: bool,                  // first g of a gg chord was pressed
    trash_size: Option<u64>,          // None until the first background measure lands
    // (new size, Some(ok) if this refresh also emptied the trash)
    trash_rx: Option<mpsc::Receiver<(u64, Option<bool>)>>,
    toast: Option<(String, f64)>,     // (message, expiry time)
    anim: Option<Anim>,
    pending_up: Option<usize>,  // child index we just came up out of
    side_hover: Option<Vec<usize>>, // wedge path of the sidebar row under the pointer
    /// Focused on the "(smaller items)" pseudo-folder of the current dir:
    /// the chart and sidebar show only children under 2% of the parent.
    small_focus: bool,
}

impl App {
    fn new(path: PathBuf, cross_real: bool) -> Self {
        let prog = Arc::new(Progress::default());
        let rx = start_scan(path.clone(), cross_real, prog.clone());
        let mut app = Self {
            path_input: path.to_string_lossy().into_owned(),
            scan_root: path,
            state: State::Scanning { rx, admin: false },
            prog,
            nav: Vec::new(),
            mounts: read_mounts(),
            wedges: Vec::new(),
            hovered: None,
            dragging: None,
            pending_trash: None,
            pending_empty: false,
            side_sel: None,
            pending_g: false,
            trash_size: None,
            trash_rx: None,
            toast: None,
            anim: None,
            pending_up: None,
            side_hover: None,
            small_focus: false,
        };
        app.refresh_trash(false);
        app
    }

    /// Re-measure the trash on a worker thread; `empty_first` empties it
    /// before measuring.
    fn refresh_trash(&mut self, empty_first: bool) {
        let (tx, rx) = mpsc::channel();
        self.trash_rx = Some(rx);
        std::thread::spawn(move || {
            let dirs = trash_files_dirs();
            let emptied = empty_first.then(|| empty_trash_of(&dirs));
            let _ = tx.send((trash_size_of(&dirs), emptied));
        });
    }

    /// Descend into the directory behind wedge `wi`, zoom-animated.
    fn zoom_into(&mut self, wi: usize, now: f64) {
        let w = &self.wedges[wi];
        if !w.is_dir || w.path.is_empty() {
            return;
        }
        let path = w.path.clone();
        self.anim = Some(Anim {
            start: now,
            dur: 0.35,
            span: (w.a0, w.a1),
            shift: w.ring + 1,
            dir_in: true,
            old: std::mem::take(&mut self.wedges),
        });
        self.nav.extend_from_slice(&path);
        self.small_focus = false;
    }

    /// Go up one level: leave the smaller-items focus first if active,
    /// otherwise pop a directory (zoom-animated).
    fn go_up(&mut self) {
        if self.small_focus {
            self.small_focus = false;
        } else if let Some(pi) = self.nav.pop() {
            self.pending_up = Some(pi);
        }
        self.side_sel = None; // new view, new keyboard cursor
    }

    fn begin_scan(&mut self, path: PathBuf, admin: bool, cross_real: bool) {
        self.mounts = read_mounts();
        self.prog = Arc::new(Progress::default());
        self.scan_root = path.clone();
        self.path_input = path.to_string_lossy().into_owned();
        self.nav.clear();
        self.wedges.clear();
        self.small_focus = false;
        let rx = if admin {
            start_admin_scan(path, cross_real)
        } else {
            start_scan(path, cross_real, self.prog.clone())
        };
        self.state = State::Scanning { rx, admin };
    }

    fn displayed<'a>(root: &'a Node, nav: &[usize]) -> &'a Node {
        let mut n = root;
        for &i in nav {
            if i < n.children.len() {
                n = &n.children[i];
            }
        }
        n
    }

    fn displayed_path(scan_root: &Path, root: &Node, nav: &[usize]) -> PathBuf {
        let mut p = scan_root.to_path_buf();
        let mut n = root;
        for &i in nav {
            if i < n.children.len() {
                n = &n.children[i];
                p.push(&n.name);
            }
        }
        p
    }

    /// Recursively lay out the rings: each ring is one level deeper,
    /// children inherit their parent's hue with a slight drift so a whole
    /// subtree reads as one color family. Operates on a child slice so the
    /// smaller-items focus view can lay out just the small tail;
    /// `idx_offset` keeps wedge paths valid against the real tree.
    #[allow(clippy::too_many_arguments)]
    fn layout(
        children: &[Node],
        idx_offset: usize,
        parent_size: u64,
        node_abs: &Path,
        node_path: Vec<usize>,
        a0: f32,
        a1: f32,
        ring: usize,
        root_size: u64,
        parent_hue: Option<f32>,
        out: &mut Vec<Wedge>,
    ) {
        if parent_size == 0 || ring >= MAX_RINGS {
            return;
        }
        let span = a1 - a0;
        let mut a = a0;
        for (i, child) in children.iter().enumerate() {
            let sweep = span * (child.size as f64 / parent_size as f64) as f32;
            if sweep < MIN_WEDGE_ANGLE {
                // size-sorted: everything from here on is a sliver
                let rest: u64 = children[i..].iter().map(|c| c.size).sum();
                if rest > 0 && a1 - a > 0.002 {
                    out.push(Wedge {
                        ring,
                        a0: a,
                        a1,
                        hue: 0.0,
                        is_dir: false,
                        lump: true,
                        name: format!("({} small items)", children.len() - i),
                        size: rest,
                        frac: rest as f32 / root_size as f32,
                        path: Vec::new(),
                        abs: None,
                    });
                }
                break;
            }
            let mid = a + sweep * 0.5;
            // Children keep the family recognizable but rotate the hue:
            // a warm-ward step per level (green -> yellow -> orange, like
            // the reference palette) plus a spread across siblings.
            let hue = match parent_hue {
                None => (mid / TAU).fract(),
                Some(h) => {
                    (h - 0.045 + 0.11 * ((mid - a0) / span - 0.5)).rem_euclid(1.0)
                }
            };
            let mut path = node_path.clone();
            path.push(idx_offset + i);
            let abs = node_abs.join(&child.name);
            out.push(Wedge {
                ring,
                a0: a,
                a1: a + sweep,
                hue,
                is_dir: child.is_dir,
                lump: false,
                name: child.name.clone(),
                size: child.size,
                frac: child.size as f32 / root_size as f32,
                path: path.clone(),
                abs: Some(abs.clone()),
            });
            if child.is_dir {
                Self::layout(
                    &child.children, 0, child.size,
                    &abs, path, a, a + sweep, ring + 1, root_size, Some(hue), out,
                );
            }
            a += sweep;
        }
    }

    /// Ring radii: the rings get thinner toward the rim, but never
    /// below MIN_T — deep rings clamp there (still visible) and the space
    /// they claim is redistributed from the thicker inner rings, so all
    /// MAX_RINGS levels fit on screen.
    fn ring_bounds(hub_r: f32, max_r: f32) -> Vec<(f32, f32)> {
        const SHRINK: f32 = 0.68;
        const GAP: f32 = 2.0;
        const MIN_T: f32 = 3.0;
        let avail = (max_r - hub_r).max(1.0);
        let mut weights = Vec::with_capacity(MAX_RINGS);
        let mut w = 1.0f32;
        for _ in 0..MAX_RINGS {
            weights.push(w);
            w *= SHRINK;
        }
        let mut t = vec![0.0f32; MAX_RINGS];
        let mut clamped = vec![false; MAX_RINGS];
        loop {
            let clamped_sum: f32 = t.iter().zip(&clamped).filter(|(_, c)| **c).map(|(v, _)| *v).sum();
            let free_w: f32 = weights.iter().zip(&clamped).filter(|(_, c)| !**c).map(|(v, _)| *v).sum();
            let mut changed = false;
            for i in 0..MAX_RINGS {
                if !clamped[i] {
                    let ti = (avail - clamped_sum).max(0.0) * weights[i] / free_w.max(1e-6);
                    if ti < MIN_T {
                        t[i] = MIN_T;
                        clamped[i] = true;
                        changed = true;
                    } else {
                        t[i] = ti;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // tiny windows: everything clamped and overflowing — scale to fit
        let sum: f32 = t.iter().sum();
        if sum > avail {
            for v in &mut t {
                *v *= avail / sum;
            }
        }
        let mut out = Vec::with_capacity(MAX_RINGS);
        let mut r = hub_r;
        for ti in t {
            let gap = (ti * 0.25).min(GAP);
            out.push((r + gap, r + ti));
            r += ti;
        }
        out
    }

    /// Inner/outer colors of a segment's radial gradient (glossy, brighter
    /// toward the rim).
    fn wedge_fill(w: &Wedge, lit: bool) -> (Color32, Color32) {
        if w.lump {
            // dark charcoal for the lumped "smaller objects"
            let v: u8 = if lit { 84 } else { 60 };
            return (Color32::from_gray(v - 22), Color32::from_gray(v));
        }
        // Each ring outward is a visibly lighter shade of the parent's color
        // family (less saturated, brighter).
        let ringf = w.ring as f32;
        let sat_base = if w.is_dir { 0.88 } else { 0.22 };
        let sat = (sat_base * (1.0 - 0.16 * ringf)).max(0.12);
        let boost = if lit { 0.14 } else { 0.0 };
        let v_out = (0.78 + 0.055 * ringf + boost).min(1.0);
        let v_in = v_out * 0.72;
        let inner: Color32 = egui::ecolor::Hsva::new(w.hue, sat, v_in, 1.0).into();
        let outer: Color32 = egui::ecolor::Hsva::new(w.hue, sat * 0.90, v_out, 1.0).into();
        (inner, outer)
    }

    fn do_trash(&mut self, path: &Path, ctx: &egui::Context) {
        let res = std::process::Command::new("gio").arg("trash").arg("--").arg(path).output();
        let msg = match res {
            Ok(o) if o.status.success() => format!("Moved to Trash: {}", path.display()),
            Ok(o) => format!("Trash failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => format!("Trash failed (need `gio`): {e}"),
        };
        self.toast = Some((msg, ctx.input(|i| i.time) + 5.0));
        // rescan current volume to reflect the deletion
        let root = self.scan_root.clone();
        let admin = matches!(self.state, State::Scanning { admin: true, .. });
        self.begin_scan(root, admin, false);
        self.refresh_trash(false); // the bin just gained content
    }

    fn draw_trash_can(painter: &egui::Painter, rect: Rect, hot: bool) {
        let col = if hot { Color32::from_rgb(240, 90, 90) } else { Color32::from_gray(150) };
        let s = Stroke::new((rect.height() * 0.04).clamp(1.2, 2.2), col);
        let c = rect.center();
        let w = rect.width() * 0.5;
        let h = rect.height() * 0.5;
        // lid
        painter.line_segment([c + vec2(-w, -h * 0.55), c + vec2(w, -h * 0.55)], s);
        painter.line_segment([c + vec2(-w * 0.35, -h * 0.55), c + vec2(-w * 0.35, -h * 0.85)], s);
        painter.line_segment([c + vec2(w * 0.35, -h * 0.55), c + vec2(w * 0.35, -h * 0.85)], s);
        painter.line_segment([c + vec2(-w * 0.35, -h * 0.85), c + vec2(w * 0.35, -h * 0.85)], s);
        // body
        painter.line_segment([c + vec2(-w * 0.8, -h * 0.55), c + vec2(-w * 0.62, h)], s);
        painter.line_segment([c + vec2(w * 0.8, -h * 0.55), c + vec2(w * 0.62, h)], s);
        painter.line_segment([c + vec2(-w * 0.62, h), c + vec2(w * 0.62, h)], s);
        // ribs
        for dx in [-0.3f32, 0.0, 0.3] {
            painter.line_segment([c + vec2(w * dx, -h * 0.3), c + vec2(w * dx, h * 0.75)], s);
        }
        painter.text(
            rect.center_bottom() + vec2(0.0, rect.height() * 0.27),
            Align2::CENTER_TOP,
            "drop to Trash",
            FontId::proportional((rect.height() * 0.23).clamp(8.0, 12.0)),
            col,
        );
    }

    /// One gradient ring segment (annular sector) with angular gaps.
    #[allow(clippy::too_many_arguments)]
    fn draw_segment(
        painter: &egui::Painter,
        center: egui::Pos2,
        a0: f32,
        a1: f32,
        r0: f32,
        r1: f32,
        ci: Color32,
        co: Color32,
    ) {
        use egui::epaint::Mesh;
        let sweep = a1 - a0;
        if sweep <= 0.0 || r1 - r0 < 0.5 {
            return;
        }
        let gap = 0.006f32.min(sweep * 0.18);
        let (a0, a1) = (a0 + gap, a1 - gap);
        let steps = ((sweep / 0.03).ceil() as usize).clamp(2, 256);
        let mut mesh = Mesh::default();
        for s in 0..=steps {
            let a = a0 + (a1 - a0) * s as f32 / steps as f32;
            let dir = vec2(a.cos(), a.sin());
            mesh.colored_vertex(center + dir * r0, ci);
            mesh.colored_vertex(center + dir * r1, co);
        }
        for s in 0..steps as u32 {
            let i = 2 * s;
            mesh.add_triangle(i, i + 1, i + 2);
            mesh.add_triangle(i + 1, i + 3, i + 2);
        }
        painter.add(Shape::mesh(mesh));
    }

    fn draw_daisy(&mut self, ui: &mut egui::Ui, shown_size: u64, shown_name: &str) {
        let avail = ui.available_size();
        let (resp, painter) = ui.allocate_painter(avail, Sense::click_and_drag());
        let rect = resp.rect;
        let center = rect.center();
        let max_r = rect.width().min(rect.height()) * 0.5 - 26.0;
        if max_r < 60.0 {
            return;
        }
        let hub_r = max_r * 0.18;
        let rings = Self::ring_bounds(hub_r, max_r);
        // trash can scales with the window (clamped so it stays usable)
        let ts = (rect.width().min(rect.height()) * 0.085).clamp(20.0, 52.0);
        let trash = Rect::from_center_size(
            rect.right_bottom() + vec2(-(ts * 0.7 + 12.0), -(ts * 0.8 + 16.0)),
            vec2(ts, ts),
        );

        // zoom animation progress (smoothstep-eased)
        let now = ui.ctx().input(|i| i.time);
        let mut p = 1.0f32;
        let mut finished = false;
        if let Some(a) = &self.anim {
            let t = (((now - a.start) as f32) / a.dur).clamp(0.0, 1.0);
            p = t * t * (3.0 - 2.0 * t);
            if t >= 1.0 {
                finished = true;
            } else {
                ui.ctx().request_repaint();
            }
        }
        if finished {
            self.anim = None;
        }
        let animating = self.anim.is_some();

        // hover / drag hit-test in polar coordinates (idle only)
        let ptr = resp.hover_pos().or_else(|| ui.ctx().pointer_latest_pos());
        self.hovered = None;
        let mut hover_hub = false;
        if let Some(mp) = resp.hover_pos().filter(|_| !animating) {
            let d = mp - center;
            let r = d.length();
            if r < hub_r {
                hover_hub = !self.nav.is_empty();
            } else {
                let mut ang = d.y.atan2(d.x);
                if ang < 0.0 {
                    ang += TAU;
                }
                if let Some(ring) = rings.iter().position(|&(r0, r1)| r >= r0 && r <= r1) {
                    self.hovered = self
                        .wedges
                        .iter()
                        .position(|w| w.ring == ring && ang >= w.a0 && ang < w.a1);
                }
            }
        }
        // sidebar hover lights the matching slice exactly like chart hover
        // (an empty path is the "(smaller items)" row -> the lump wedge)
        if self.hovered.is_none() && !animating {
            if let Some(p) = &self.side_hover {
                self.hovered = if p.is_empty() {
                    self.wedges.iter().position(|w| w.lump && w.ring == 0)
                } else {
                    self.wedges.iter().position(|w| w.ring + 1 == p.len() && w.path == *p)
                };
            }
        }

        if resp.drag_started() {
            self.dragging = self.hovered;
        }
        let over_trash = ptr.map(|p| trash.contains(p)).unwrap_or(false);

        // pointing finger over anything clickable, grabbing while dragging
        if self.dragging.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if resp.hover_pos().is_some() {
            let on_wedge = self
                .hovered
                .and_then(|h| self.wedges.get(h))
                .is_some_and(|w| w.is_dir || w.lump || w.abs.is_some());
            if hover_hub || on_wedge {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }

        // outgoing layout during zoom-in: fades away; the zoomed subtree's
        // deeper rings are skipped (the incoming layout redraws them
        // expanding), while the clicked slice itself fades in place
        if let Some(a) = &self.anim {
            if a.dir_in {
                let alpha = (1.0 - p) * (1.0 - p);
                for w in &a.old {
                    let in_span = w.a0 >= a.span.0 - 1e-4 && w.a1 <= a.span.1 + 1e-4;
                    if in_span && w.ring >= a.shift {
                        continue;
                    }
                    let (r0, r1) = rings[w.ring];
                    let (ci, co) = Self::wedge_fill(w, false);
                    Self::draw_segment(
                        &painter, center, w.a0, w.a1, r0, r1,
                        ci.gamma_multiply(alpha), co.gamma_multiply(alpha),
                    );
                }
            }
        }

        // ring segments: radial-gradient meshes, the whole hovered subtree
        // lit, the hovered segment lifted outward slightly; while a zoom
        // runs, geometry is interpolated between the slice and full circle
        for (wi, w) in self.wedges.iter().enumerate() {
            let (ta0, ta1, band, alpha) = match &self.anim {
                Some(a) if a.dir_in => {
                    // expand: from the clicked slice's arc out to the circle
                    let f = (a.span.1 - a.span.0) / TAU;
                    let m0 = a.span.0 + w.a0 * f;
                    let m1 = a.span.0 + w.a1 * f;
                    let src = rings.get(w.ring + a.shift).copied().unwrap_or((max_r, max_r));
                    let dst = rings[w.ring];
                    (
                        egui::lerp(m0..=w.a0, p),
                        egui::lerp(m1..=w.a1, p),
                        (egui::lerp(src.0..=dst.0, p), egui::lerp(src.1..=dst.1, p)),
                        1.0,
                    )
                }
                Some(a) => {
                    // collapse back: the ex-root's subtree shrinks into its
                    // slice; everything outside it fades in
                    let in_span = w.a0 >= a.span.0 - 1e-4 && w.a1 <= a.span.1 + 1e-4;
                    if in_span {
                        let f = TAU / (a.span.1 - a.span.0);
                        let e0 = (w.a0 - a.span.0) * f;
                        let e1 = (w.a1 - a.span.0) * f;
                        let src = if w.ring >= a.shift {
                            rings[w.ring - a.shift]
                        } else {
                            (hub_r, hub_r)
                        };
                        let dst = rings[w.ring];
                        (
                            egui::lerp(e0..=w.a0, p),
                            egui::lerp(e1..=w.a1, p),
                            (egui::lerp(src.0..=dst.0, p), egui::lerp(src.1..=dst.1, p)),
                            1.0,
                        )
                    } else {
                        (w.a0, w.a1, rings[w.ring], p * p)
                    }
                }
                None => (w.a0, w.a1, rings[w.ring], 1.0),
            };
            let (r0, mut r1) = band;
            let lit = !animating
                && (self.hovered == Some(wi)
                    || self.dragging == Some(wi)
                    || matches!(self.hovered, Some(h)
                        if !w.path.is_empty() && self.wedges[h].path.starts_with(&w.path)));
            if !animating && (self.hovered == Some(wi) || self.dragging == Some(wi)) {
                r1 += 3.0;
            }
            let (ci, co) = Self::wedge_fill(w, lit);
            Self::draw_segment(
                &painter, center, ta0, ta1, r0, r1,
                ci.gamma_multiply(alpha), co.gamma_multiply(alpha),
            );

            // label the first three rings wherever the segment has room:
            // the arc must be long enough and the ring thick enough. Font
            // scales down with the chart (small windows), and the room
            // checks scale with the font — fixed pixel thresholds made
            // most labels vanish as soon as the window shrank.
            if !animating && w.ring <= 2 {
                let mid_r = (r0 + r1) * 0.5;
                let arc_len = (w.a1 - w.a0) * mid_r;
                let scale = (max_r / 320.0).clamp(0.65, 1.0);
                let font = ((if w.ring == 0 { 12.5 } else { 11.0 }) * scale).max(8.0);
                if arc_len > font * 6.0 && r1 - r0 > font * 1.9 {
                    let mid = (w.a0 + w.a1) * 0.5;
                    let lp = center + vec2(mid.cos(), mid.sin()) * mid_r;
                    // black on light fills, white on dark ones (grey lumps)
                    let lum = 0.299 * (ci.r() as f32 + co.r() as f32) * 0.5
                        + 0.587 * (ci.g() as f32 + co.g() as f32) * 0.5
                        + 0.114 * (ci.b() as f32 + co.b() as f32) * 0.5;
                    let text_col = if lum > 140.0 {
                        Color32::from_gray(15)
                    } else {
                        Color32::from_gray(235)
                    };
                    painter.text(
                        lp,
                        Align2::CENTER_CENTER,
                        format!("{}\n{}", truncate(&w.name, 14), human(w.size)),
                        FontId::proportional(font),
                        text_col,
                    );
                }
            }
        }

        // hub: dark disc with the current folder + size (click = up)
        let hub_fill = if hover_hub { Color32::from_gray(60) } else { Color32::from_gray(42) };
        painter.circle(center, hub_r - 3.0, hub_fill, Stroke::new(1.0, Color32::from_gray(72)));
        // small hub: size front and centre, name below it
        painter.text(
            center - vec2(0.0, 8.0),
            Align2::CENTER_CENTER,
            human(shown_size),
            FontId::proportional(15.0),
            Color32::WHITE,
        );
        let title = if self.nav.is_empty() {
            truncate(shown_name, 14)
        } else {
            format!("⬆ {}", truncate(shown_name, 12))
        };
        painter.text(
            center + vec2(0.0, 10.0),
            Align2::CENTER_CENTER,
            title,
            FontId::proportional(11.0),
            Color32::from_gray(190),
        );

        Self::draw_trash_can(&painter, trash, over_trash && self.dragging.is_some());

        // Empty Trash: size readout + button above the bin. human() picks
        // the unit (B / KiB / MiB / GiB / TiB) to match the size.
        if let Some(sz) = self.trash_size {
            let btn_rect = Rect::from_min_max(
                egui::pos2(rect.right() - 170.0, trash.top() - 36.0),
                egui::pos2(rect.right() - 10.0, trash.top() - 12.0),
            );
            if sz == 0 {
                painter.text(
                    btn_rect.right_center(),
                    Align2::RIGHT_CENTER,
                    "Trash is empty",
                    FontId::proportional(11.0),
                    Color32::from_gray(120),
                );
            } else {
                let r = ui
                    .put(
                        btn_rect,
                        egui::Button::new(
                            egui::RichText::new(format!("Empty Trash · {}", human(sz)))
                                .size(11.0),
                        ),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Permanently delete everything in the Trash");
                if r.clicked() {
                    self.pending_empty = true;
                }
            }
        }


        // dragging chip
        if let (Some(di), Some(p)) = (self.dragging, ptr) {
            if let Some(w) = self.wedges.get(di) {
                let txt = truncate(&w.name, 24);
                let galley = painter.layout_no_wrap(
                    txt.clone(),
                    FontId::proportional(13.0),
                    Color32::WHITE,
                );
                let pad = vec2(8.0, 5.0);
                let r = Rect::from_min_size(p + vec2(12.0, 12.0), galley.size() + pad * 2.0);
                painter.rect_filled(r, 4.0, Color32::from_rgb(60, 60, 70));
                painter.galley(r.min + pad, galley, Color32::WHITE);
            }
        }

        // tooltip (only when not dragging, and only for real chart hover —
        // sidebar-driven highlights already show their info in the row)
        if self.dragging.is_none() && resp.hover_pos().is_some() {
            if let Some(wi) = self.hovered {
                let w = &self.wedges[wi];
                let text = format!(
                    "{}\n{}  ·  {:.1}%{}",
                    w.name,
                    human(w.size),
                    w.frac * 100.0,
                    if w.is_dir { "\nclick to open · drag to Trash" }
                    else if w.abs.is_some() { "\ndrag to Trash" }
                    else { "" },
                );
                egui::show_tooltip_at_pointer(
                    ui.ctx(),
                    ui.layer_id(),
                    egui::Id::new("tt"),
                    |ui: &mut egui::Ui| {
                        ui.set_max_width(340.0);
                        ui.label(text);
                    },
                );
            }
        }

        // release: trash, or descend / go up
        if resp.drag_stopped() {
            if over_trash {
                if let Some(di) = self.dragging {
                    if let Some(abs) = self.wedges.get(di).and_then(|w| w.abs.clone()) {
                        let name = self.wedges[di].name.clone();
                        self.pending_trash = Some((abs, name));
                    }
                }
            }
            self.dragging = None;
        }
        if resp.clicked() && !animating {
            if hover_hub {
                self.go_up();
            } else if let Some(wi) = self.hovered {
                if self.wedges[wi].lump && self.wedges[wi].ring == 0 && !self.small_focus {
                    self.small_focus = true; // dive into the smaller-items view
                } else {
                    self.zoom_into(wi, now);
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // keys are app-wide shortcuts only while no text field has focus
        let typing = ctx.wants_keyboard_input();
        if !typing && ctx.input(|i| i.key_pressed(egui::Key::H) || i.key_pressed(egui::Key::Backspace)) {
            self.go_up();
        }
        if !typing && ctx.input(|i| i.key_pressed(egui::Key::Q)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // collect a finished scan
        let mut incoming: Option<ScanResult> = None;
        if let State::Scanning { rx, .. } = &self.state {
            match rx.try_recv() {
                Ok(r) => incoming = Some(r),
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(120));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    incoming = Some(Err("scan thread died".into()));
                }
            }
        }
        if let Some(r) = incoming {
            self.state = match r {
                Ok(n) => State::Ready(n),
                Err(e) => State::Error(e),
            };
        }

        // ---- top bar ----
        // breadcrumb data: scan root plus each navigated directory name
        let crumbs: Option<Vec<String>> = if let State::Ready(root) = &self.state {
            let mut v = vec![self.scan_root.to_string_lossy().into_owned()];
            let mut n: &Node = root;
            for &i in &self.nav {
                if i < n.children.len() {
                    n = &n.children[i];
                    v.push(n.name.clone());
                }
            }
            Some(v)
        } else {
            None
        };
        let mut crumb_clicked: Option<usize> = None;

        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.label("Path:");
                let edit = ui.text_edit_singleline(&mut self.path_input);
                let go = ui.button("Rescan").clicked()
                    || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if go {
                    let p = PathBuf::from(self.path_input.trim());
                    if p.is_dir() {
                        self.begin_scan(p, false, false);
                    }
                }
                if ui
                    .button("Scan whole disk")
                    .on_hover_text("All mounted partitions, as your user. Only a few root-only corners (/root, bits of /etc and /var) are invisible.")
                    .clicked()
                {
                    self.begin_scan(PathBuf::from("/"), false, true);
                }
                if ui
                    .button("🛡 as admin")
                    .on_hover_text("Same, via sudo in a terminal — also reads the root-only corners")
                    .clicked()
                {
                    self.begin_scan(PathBuf::from("/"), true, true);
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Partitions:");
                let reals: Vec<(PathBuf, String)> = self
                    .mounts
                    .iter()
                    .filter(|m| m.real)
                    .map(|m| (m.point.clone(), m.fstype.clone()))
                    .collect();
                for (mp, ty) in reals {
                    let label = format!("{} ({ty})", mp.display());
                    if ui.button(label).clicked() {
                        self.begin_scan(mp, false, false);
                    }
                }
                if let Some((total, free)) = disk_free(&self.scan_root) {
                    ui.separator();
                    ui.label(format!("{} free of {}", human(free), human(total)));
                }
            });
            // breadcrumb of where you are; segments jump back
            if let Some(crumbs) = &crumbs {
                ui.horizontal_wrapped(|ui| {
                    for (idx, name) in crumbs.iter().enumerate() {
                        if idx > 0 {
                            ui.label(egui::RichText::new("›").weak());
                        }
                        let last = idx + 1 == crumbs.len() && !self.small_focus;
                        let text = truncate(name, 28);
                        if last {
                            ui.strong(text);
                        } else if ui.link(text).clicked() {
                            crumb_clicked = Some(idx);
                        }
                    }
                    if self.small_focus {
                        ui.label(egui::RichText::new("›").weak());
                        ui.strong("smaller items");
                    }
                });
            }
            ui.add_space(3.0);
        });
        if let Some(idx) = crumb_clicked {
            self.nav.truncate(idx);
            self.small_focus = false;
            self.anim = None;
        }

        // ---- toast ----
        if let Some((msg, exp)) = self.toast.clone() {
            if ctx.input(|i| i.time) < exp {
                egui::TopBottomPanel::bottom("toast").show(ctx, |ui| {
                    ui.label(msg);
                });
            } else {
                self.toast = None;
            }
        }

        // ---- states ----
        match &self.state {
            State::Scanning { admin, .. } => {
                let admin = *admin;
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.4);
                        ui.spinner();
                        if admin {
                            ui.label(format!(
                                "Scanning {} as admin…\nAuthenticate in the terminal window.",
                                self.scan_root.display()
                            ));
                        } else {
                            ui.label(format!(
                                "Scanning {} …\n{} files · {}",
                                self.scan_root.display(),
                                self.prog.files.load(Ordering::Relaxed),
                                human(self.prog.bytes.load(Ordering::Relaxed)),
                            ));
                        }
                    });
                });
                return;
            }
            State::Error(e) => {
                let e = e.clone();
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.4);
                        ui.colored_label(Color32::LIGHT_RED, format!("⚠ {e}"));
                    });
                });
                return;
            }
            State::Ready(_) => {}
        }

        // ---- ready: precompute, then draw ----
        const SMALL_FRAC: f64 = 0.02;
        let (shown_size, shown_name, wedges, side_rows) = {
            let State::Ready(root) = &self.state else { unreachable!() };
            let shown = Self::displayed(root, &self.nav);
            let base = Self::displayed_path(&self.scan_root, root, &self.nav);
            // first child under 2% of the parent (children are size-sorted)
            let cut = shown
                .children
                .iter()
                .position(|c| (c.size as f64) < shown.size.max(1) as f64 * SMALL_FRAC)
                .unwrap_or(shown.children.len());
            let focus = self.small_focus && cut < shown.children.len();
            let (kids, offset, total) = if focus {
                let small_sum: u64 = shown.children[cut..].iter().map(|c| c.size).sum();
                (&shown.children[cut..], cut, small_sum.max(1))
            } else {
                (&shown.children[..], 0, shown.size.max(1))
            };
            let mut wedges = Vec::new();
            Self::layout(kids, offset, total, &base, Vec::new(), 0.0, TAU, 0, total, None, &mut wedges);
            let name = if focus {
                format!("{} · smaller items", shown.name)
            } else {
                shown.name.clone()
            };
            // sidebar rows over the same child set as the chart; in the
            // normal view, children under 2% of the parent collapse into a
            // clickable "(N smaller items)" pseudo-folder (top level and
            // sub-layer alike) that navigates into a view of just them
            const MAX_TOP: usize = 30;
            const MAX_SUB: usize = 10;
            const MAX_FOCUS_ROWS: usize = 400;
            let mut wedge_col: std::collections::HashMap<&[usize], Color32> =
                std::collections::HashMap::new();
            for w in &wedges {
                if w.ring <= 1 && !w.path.is_empty() {
                    wedge_col.insert(w.path.as_slice(), Self::wedge_fill(w, false).1);
                }
            }
            let grey = Color32::from_gray(96);
            let toggle_label = |n: usize, size: u64| {
                format!(
                    "({n} smaller items)  {}  {:.1}%",
                    human(size),
                    size as f64 * 100.0 / total as f64
                )
            };
            let mut side: Vec<SideRow> = Vec::new();
            for (k, c) in kids.iter().enumerate() {
                let i = offset + k;
                if focus && k >= MAX_FOCUS_ROWS {
                    side.push(SideRow {
                        label: format!("…and {} more", kids.len() - k),
                        color: grey,
                        indent: 0,
                        kind: SideKind::Note,
                    });
                    break;
                }
                let c_small = (c.size as f64) < total as f64 * SMALL_FRAC;
                if !focus && (c_small || k >= MAX_TOP) {
                    let rest = &kids[k..];
                    let rest_size: u64 = rest.iter().map(|n| n.size).sum();
                    if rest_size > 0 {
                        side.push(SideRow {
                            label: toggle_label(rest.len(), rest_size),
                            color: grey,
                            indent: 0,
                            kind: SideKind::Toggle { key: Vec::new() },
                        });
                    }
                    break;
                }
                side.push(SideRow {
                    label: row_label(c, total),
                    color: wedge_col.get(&[i][..]).copied().unwrap_or(grey),
                    indent: 0,
                    kind: SideKind::Item { path: vec![i], is_dir: c.is_dir },
                });
                for (j, gc) in c.children.iter().enumerate() {
                    let g_small = (gc.size as f64) < c.size.max(1) as f64 * SMALL_FRAC;
                    if g_small || gc.size == 0 || j >= MAX_SUB {
                        let rest = &c.children[j..];
                        let rest_size: u64 = rest.iter().map(|n| n.size).sum();
                        if rest_size > 0 {
                            side.push(SideRow {
                                label: toggle_label(rest.len(), rest_size),
                                color: grey,
                                indent: 1,
                                kind: SideKind::Toggle { key: vec![i] },
                            });
                        }
                        break;
                    }
                    side.push(SideRow {
                        label: row_label(gc, total),
                        color: wedge_col.get(&[i, j][..]).copied().unwrap_or(grey),
                        indent: 1,
                        kind: SideKind::Item { path: vec![i, j], is_dir: gc.is_dir },
                    });
                }
            }
            (total, name, wedges, side)
        };
        self.wedges = wedges;

        // an "up" navigation resolves once the parent layout exists: animate
        // the chart collapsing back into the slice we came out of
        if let Some(pi) = self.pending_up.take() {
            if let Some(w) = self.wedges.iter().find(|w| w.ring == 0 && w.path == [pi]) {
                self.anim = Some(Anim {
                    start: ctx.input(|i| i.time),
                    dur: 0.35,
                    span: (w.a0, w.a1),
                    shift: 1,
                    dir_in: false,
                    old: Vec::new(),
                });
            }
        }

        // rows light when they're an ancestor (or exact match) of the
        // hovered wedge — so a deep hover lights its parent row and sub-row
        let hovered_path: Option<Vec<usize>> = self
            .hovered
            .and_then(|h| self.wedges.get(h))
            .map(|w| w.path.clone())
            .filter(|p| !p.is_empty());
        let hovered_lump = self
            .hovered
            .and_then(|h| self.wedges.get(h))
            .is_some_and(|w| w.lump && w.ring == 0);

        // exactly one sidebar highlight at a time: a hovered wedge lights
        // only its deepest matching row; the keyboard cursor shows only
        // while nothing in the chart is hovered
        let hover_row: Option<usize> = if hovered_lump {
            side_rows
                .iter()
                .position(|r| matches!(&r.kind, SideKind::Toggle { key } if key.is_empty()))
        } else {
            hovered_path.as_ref().and_then(|hp| {
                side_rows
                    .iter()
                    .enumerate()
                    .filter_map(|(i, r)| match &r.kind {
                        SideKind::Item { path, .. } if hp.starts_with(path) => {
                            Some((i, path.len()))
                        }
                        _ => None,
                    })
                    .max_by_key(|&(_, len)| len)
                    .map(|(i, _)| i)
            })
        };

        let mut side_clicked: Option<Vec<usize>> = None;
        let mut small_toggled: Option<Vec<usize>> = None;

        // ---- vim keys for the sidebar: j/k move, gg/G first/last,
        // Enter/l activate (h above already goes back). The cursor skips
        // non-interactive note rows.
        let mut key_scroll = false; // scroll the cursor row into view this frame
        let modal_open = self.pending_trash.is_some() || self.pending_empty;
        if !typing && !modal_open && !side_rows.is_empty() {
            let interactive = |i: usize| !matches!(side_rows[i].kind, SideKind::Note);
            let first = (0..side_rows.len()).find(|&i| interactive(i));
            let last = (0..side_rows.len()).rev().find(|&i| interactive(i));
            let step = |from: Option<usize>, down: bool| -> Option<usize> {
                let Some(cur) = from else {
                    return if down { first } else { last };
                };
                let it: Box<dyn Iterator<Item = usize>> = if down {
                    Box::new(cur + 1..side_rows.len())
                } else {
                    Box::new((0..cur).rev())
                };
                it.filter(|&i| interactive(i)).next().or(from)
            };
            let (j, k, g, sg, act) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::J),
                    i.key_pressed(egui::Key::K),
                    i.key_pressed(egui::Key::G) && !i.modifiers.shift,
                    i.key_pressed(egui::Key::G) && i.modifiers.shift,
                    i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::L),
                )
            });
            let before = self.side_sel;
            if j {
                self.side_sel = step(self.side_sel, true);
            } else if k {
                self.side_sel = step(self.side_sel, false);
            } else if sg {
                self.side_sel = last;
            } else if g {
                if self.pending_g {
                    self.side_sel = first;
                }
                self.pending_g = !self.pending_g;
            }
            if j || k || sg {
                self.pending_g = false;
            }
            key_scroll = self.side_sel != before && self.side_sel.is_some();
            if act {
                self.pending_g = false;
                if let Some(s) = self.side_sel.filter(|&s| s < side_rows.len()) {
                    match &side_rows[s].kind {
                        SideKind::Item { path, is_dir } => {
                            if *is_dir {
                                side_clicked = Some(path.clone());
                            }
                        }
                        SideKind::Toggle { key } => small_toggled = Some(key.clone()),
                        SideKind::Note => {}
                    }
                }
            }
        }
        // rows change with every scan/navigation: keep the cursor in range,
        // and park it on the first row of a fresh view so it's visible
        // before any j/k is pressed
        if self.side_sel.is_none() || self.side_sel.is_some_and(|s| s >= side_rows.len()) {
            self.side_sel =
                (0..side_rows.len()).find(|&i| !matches!(side_rows[i].kind, SideKind::Note));
        }

        egui::SidePanel::left("list").resizable(false).exact_width(320.0).show(ctx, |ui| {
            // Everything in this panel must wrap, header included: a single
            // un-wrapped label that overflows the panel is unioned into the
            // Ui's max_rect (egui Region::expand_to_include_rect), after
            // which all rows below wrap at the inflated width and spill
            // past the panel edge instead of wrapping at 320.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if (!self.nav.is_empty() || self.small_focus)
                    && ui
                        .button("⬆ Up")
                        .on_hover_text("Go to parent directory")
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                {
                    self.go_up();
                }
                let base = shown_name.trim_end_matches(" · smaller items");
                ui.strong(if self.small_focus {
                    format!("Smaller items in {base}")
                } else {
                    format!("Items in {base} directory")
                });
            });
            ui.separator();
            let mut row_hover = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (ri, row) in side_rows.iter().enumerate() {
                    // one highlight only: hovered wedge's row, else the cursor
                    let hl = hover_row == Some(ri)
                        || (hover_row.is_none() && self.side_sel == Some(ri));
                    let cursor = self.side_sel == Some(ri); // for scroll-into-view
                    ui.horizontal(|ui| {
                        // wrap long names within the fixed panel width
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                        ui.add_space(20.0 * row.indent as f32);
                        ui.colored_label(row.color, if row.indent > 0 { "•" } else { "⏺" });
                        match &row.kind {
                            SideKind::Item { path, is_dir } => {
                                let mut r = ui.selectable_label(hl, &row.label);
                                if *is_dir {
                                    r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
                                }
                                if r.hovered() {
                                    row_hover = Some(path.clone());
                                }
                                if r.clicked() && *is_dir {
                                    side_clicked = Some(path.clone());
                                }
                                if cursor && key_scroll {
                                    r.scroll_to_me(None);
                                }
                            }
                            SideKind::Toggle { key } => {
                                // '›' — egui's bundled fonts lack U+25B8 '▸'
                                let r = ui
                                    .selectable_label(hl, format!("› {}", row.label))
                                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                                if r.hovered() && key.is_empty() {
                                    row_hover = Some(Vec::new()); // = the lump wedge
                                }
                                if r.clicked() {
                                    small_toggled = Some(key.clone());
                                }
                                if cursor && key_scroll {
                                    r.scroll_to_me(None);
                                }
                            }
                            SideKind::Note => {
                                ui.weak(&row.label);
                            }
                        }
                    });
                }
            });
            self.side_hover = row_hover;
        });
        if let Some(key) = small_toggled {
            // enter the smaller-items view (of the shown dir, or of a child)
            if let Some(&child) = key.first() {
                self.nav.push(child);
            }
            self.small_focus = true;
            self.side_sel = None;
        }
        if let Some(path) = side_clicked {
            // zoom via the matching wedge when it has one (children inside
            // the grey lumps don't; those jump without animation)
            if let Some(wi) = self
                .wedges
                .iter()
                .position(|w| w.ring + 1 == path.len() && w.path == path)
            {
                self.zoom_into(wi, ctx.input(|inp| inp.time));
            } else {
                self.nav.extend_from_slice(&path);
            }
            self.side_sel = None;
        }

        // background trash-measure result (and possible empty outcome)
        if let Some(rx) = &self.trash_rx {
            match rx.try_recv() {
                Ok((sz, emptied)) => {
                    self.trash_size = Some(sz);
                    self.trash_rx = None;
                    let msg = match emptied {
                        Some(true) => Some("Trash emptied"),
                        Some(false) => Some("Couldn't empty some Trash items"),
                        None => None,
                    };
                    if let Some(m) = msg {
                        self.toast = Some((m.into(), ctx.input(|i| i.time) + 5.0));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(300));
                }
                Err(mpsc::TryRecvError::Disconnected) => self.trash_rx = None,
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            self.draw_daisy(ui, shown_size, &shown_name);
        });

        // ---- trash confirm modal ----
        if let Some((path, name)) = self.pending_trash.clone() {
            let mut close = false;
            egui::Window::new("Move to Trash?")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("Move “{name}” to the Trash?"));
                    ui.label(egui::RichText::new(path.to_string_lossy()).weak());
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Move to Trash").clicked() {
                            self.do_trash(&path, ctx);
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if close {
                self.pending_trash = None;
            }
        }

        // ---- empty-trash confirm modal ----
        if self.pending_empty {
            let mut close = false;
            let sz = self.trash_size.unwrap_or(0);
            egui::Window::new("Empty Trash?")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Permanently delete everything in the Trash ({})?",
                        human(sz)
                    ));
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("This cannot be undone.")
                            .color(Color32::from_rgb(240, 90, 90))
                            .strong(),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let empty_btn = ui.button(
                            egui::RichText::new("Empty Trash")
                                .color(Color32::from_rgb(240, 90, 90)),
                        );
                        if empty_btn.clicked() {
                            self.refresh_trash(true);
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if close {
                self.pending_empty = false;
            }
        }
    }
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|s| s == "--scan").unwrap_or(false) {
        run_scan_mode(&args[1..]);
        return Ok(());
    }

    // No argument: whole-disk view (all real partitions, as the user) —
    // the default. An explicit path scans just that tree.
    let (path, cross) = match args.iter().find(|a| !a.starts_with("--")) {
        Some(p) => (PathBuf::from(p), false),
        None => (PathBuf::from("/"), true),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("disksun")
            .with_inner_size([1150.0, 760.0])
            .with_title("disksun"),
        ..Default::default()
    };
    eframe::run_native(
        "disksun",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(path, cross)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_trash_clears_measured_dirs() {
        let root = std::env::temp_dir().join(format!("disksun-test-{}", std::process::id()));
        let files = root.join("Trash/files");
        let info = root.join("Trash/info");
        std::fs::create_dir_all(files.join("adir")).unwrap();
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(files.join("a.dat"), vec![0u8; 8192]).unwrap();
        std::fs::write(files.join("adir/b.dat"), vec![0u8; 8192]).unwrap();
        std::fs::write(info.join("a.dat.trashinfo"), "x").unwrap();
        std::fs::write(root.join("Trash/directorysizes"), "x").unwrap();

        let dirs = vec![files.clone()];
        assert!(trash_size_of(&dirs) >= 16384);
        assert!(empty_trash_of(&dirs));
        assert_eq!(trash_size_of(&dirs), 0);
        assert!(std::fs::read_dir(&files).unwrap().next().is_none());
        assert!(std::fs::read_dir(&info).unwrap().next().is_none());
        assert!(!root.join("Trash/directorysizes").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
