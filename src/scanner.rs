use std::path::PathBuf;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use std::thread;
use rayon::prelude::*;

use crate::model::{Node, ScanResult};

/// Running total for one top-level entry, streamed while the scan runs.
#[derive(Clone)]
pub struct LiveEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub files: u64,
}

pub enum ScanEvent {
    /// Sent at most every `PROGRESS_MS`; `top` is a snapshot of the root's
    /// children with sizes accumulated so far.
    Progress { scanned: u64, current: String, top: Vec<LiveEntry> },
    Done(ScanResult),
    Cancelled,
    Failed(String),
}

/// Cap warnings so a permission-denied storm can't eat RAM.
const MAX_WARNINGS: usize = 100;
const PROGRESS_MS: u64 = 100;

/// Bytes allocated on disk. Unix reports real blocks; elsewhere round the
/// logical size up to a 4 KiB cluster as an approximation.
#[cfg(unix)]
fn allocated(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.blocks() * 512
}
#[cfg(not(unix))]
fn allocated(m: &std::fs::Metadata) -> u64 {
    m.len().div_ceil(4096) * 4096
}

pub fn start_scan(root: PathBuf, cancel: Arc<AtomicBool>) -> flume::Receiver<ScanEvent> {
    let (tx, rx) = flume::unbounded();
    thread::spawn(move || run_scan(root, cancel, tx));
    rx
}

fn run_scan(root: PathBuf, cancel: Arc<AtomicBool>, tx: flume::Sender<ScanEvent>) {
    // Single stat on the root instead of exists() + is_dir() + metadata().
    let root_meta = std::fs::symlink_metadata(&root);
    let Ok(root_meta) = root_meta else {
        let _ = tx.send(ScanEvent::Failed(format!(
            "Path does not exist: {}",
            root.display()
        )));
        return;
    };
    let root_is_file = root_meta.is_file();

    let root_name = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| root.display().to_string());

    let mut names = String::new();
    names.push_str(&root_name);
    let mut nodes: Vec<Node> = vec![Node::new(
        0..names.len(),
        !root_is_file && root_meta.is_dir(),
        if root_is_file { root_meta.len() } else { 0 },
        if root_is_file { allocated(&root_meta) } else { 0 },
        mtime_secs(&root_meta),
        None,
    )];
    if !root_is_file {
        nodes[0].file_count = 0;
    }

    // Root is a single file: nothing to walk.
    if root_is_file {
        let total_size = nodes[0].size;
        let _ = tx.send(ScanEvent::Done(ScanResult {
            nodes,
            names,
            root: 0,
            total_size,
            root_path: root,
            warnings: Vec::new(),
            warnings_truncated: 0,
        }));
        return;
    }

    // Parallel walk: each directory is read on the rayon pool into a
    // `DirBlock` (names packed per directory), subdirectories fan out
    // recursively.
    let ctx = WalkCtx {
        cancel: &cancel,
        tx: &tx,
        scanned: AtomicU64::new(0),
        name_bytes: AtomicU64::new(0),
        warnings: Mutex::new((Vec::new(), 0)),
        top: OnceLock::new(),
        start: Instant::now(),
        last_emit_ms: AtomicU64::new(0),
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .stack_size(16 * 1024 * 1024)
        .build()
        .expect("scan thread pool");
    let block = pool.install(|| read_tree(&root, device(&root_meta), &ctx, None));
    if cancel.load(Ordering::Relaxed) {
        let _ = tx.send(ScanEvent::Cancelled);
        return;
    }
    let (warnings, warnings_truncated) = ctx.warnings.into_inner().unwrap_or_default();

    // Flatten: each directory's entries get one contiguous id range (so
    // children are a range, not a Vec), assigned before any of their
    // descendants -- parent id < child id, as the aggregation needs.
    nodes.reserve_exact(ctx.scanned.load(Ordering::Relaxed) as usize);
    names.reserve_exact(ctx.name_bytes.load(Ordering::Relaxed) as usize);
    let mut stack: Vec<(usize, DirBlock)> = vec![(0, block)];
    while let Some((pid, block)) = stack.pop() {
        let base = nodes.len();
        let name_base = names.len();
        names.push_str(&block.names);
        nodes.extend(block.entries.iter().map(|e| {
            let start = name_base + e.name_off as usize;
            Node::new(
                start..start + e.name_len as usize,
                e.is_dir,
                e.size,
                e.allocated,
                e.mtime,
                Some(pid),
            )
        }));
        let end = nodes.len();
        nodes[pid].set_children(base..end);
        for (i, sub) in block.subdirs.into_iter().rev() {
            stack.push((base + i, sub));
        }
    }

    // Bottom-up aggregate. Child ids are always > parent ids (parents are
    // created first), so reverse id order visits children before parents
    // with no O(N log N) sort by depth.
    for id in (0..nodes.len()).rev() {
        if let Some(pid) = nodes[id].parent() {
            let size = nodes[id].size;
            let allocated = nodes[id].allocated;
            let files = nodes[id].file_count;
            // Recursive totals: child's own dir flag plus its descendant dirs.
            let dirs = u32::from(nodes[id].is_dir) + nodes[id].dir_count;
            nodes[pid].size = nodes[pid].size.saturating_add(size);
            nodes[pid].allocated = nodes[pid].allocated.saturating_add(allocated);
            nodes[pid].file_count = nodes[pid].file_count.saturating_add(files);
            nodes[pid].dir_count = nodes[pid].dir_count.saturating_add(dirs);
        }
    }

    if cancel.load(Ordering::Relaxed) {
        let _ = tx.send(ScanEvent::Cancelled);
        return;
    }

    let total_size = nodes.first().map(|n| n.size).unwrap_or(0);
    let _ = tx.send(ScanEvent::Done(ScanResult {
        nodes,
        names,
        root: 0,
        total_size,
        root_path: root,
        warnings,
        warnings_truncated,
    }));
}

/// One directory's listing. Names are packed into one buffer per
/// directory instead of a `String` per entry.
#[derive(Default)]
struct DirBlock {
    names: String,
    entries: Vec<Entry>,
    /// (entry index, listing) for each subdirectory that was descended.
    subdirs: Vec<(usize, DirBlock)>,
}

struct Entry {
    name_off: u32,
    name_len: u16,
    is_dir: bool,
    size: u64,
    allocated: u64,
    mtime: i64,
}

#[cfg(unix)]
fn mtime_secs(m: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    m.mtime()
}
#[cfg(not(unix))]
fn mtime_secs(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(crate::model::NO_MTIME, |d| d.as_secs() as i64)
}

#[cfg(unix)]
fn device(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.dev()
}
#[cfg(not(unix))]
fn device(_m: &std::fs::Metadata) -> u64 {
    0
}

/// Kernel pseudo-filesystems (procfs, sysfs, cgroups, ...) report huge or
/// meaningless sizes, so mount points of these types are not descended.
/// Only checked where the device id changes, i.e. at mount points.
#[cfg(target_os = "linux")]
fn is_virtual_fs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const VIRTUAL: [u64; 19] = [
        0x9fa0,     // proc
        0x6265_6572, // sysfs
        0x1cd1,     // devpts
        0x0027_e0eb, // cgroup
        0x6367_7270, // cgroup2
        0x6462_6720, // debugfs
        0x7472_6163, // tracefs
        0x7363_6673, // securityfs
        0x6165_676c, // pstore
        0xcafe_4a11, // bpf
        0x6265_6570, // configfs
        0x6573_5543, // fusectl
        0x1980_0202, // mqueue
        0xde5e_81e4, // efivarfs
        0x0187,     // autofs
        0x4249_4e4d, // binfmt_misc
        0xf97c_ff8c, // selinuxfs
        0x9584_58f6, // hugetlbfs
        0x6e73_6673, // nsfs
    ];
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: statfs only writes into the zeroed struct we pass.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    VIRTUAL.contains(&((st.f_type as u64) & 0xffff_ffff))
}
#[cfg(not(target_os = "linux"))]
fn is_virtual_fs(_path: &Path) -> bool {
    false
}

struct WalkCtx<'a> {
    cancel: &'a AtomicBool,
    tx: &'a flume::Sender<ScanEvent>,
    scanned: AtomicU64,
    /// Total name bytes, so the flattened names buffer is allocated once.
    name_bytes: AtomicU64,
    warnings: Mutex<(Vec<String>, usize)>,
    /// Root children with live byte/file totals: (name, is_dir, bytes, files).
    top: OnceLock<Vec<(String, bool, AtomicU64, AtomicU64)>>,
    start: Instant,
    last_emit_ms: AtomicU64,
}

impl WalkCtx<'_> {
    /// Throttled progress: at most one event per `PROGRESS_MS` across threads.
    fn maybe_emit(&self, dir: &Path) {
        let now = self.start.elapsed().as_millis() as u64;
        let last = self.last_emit_ms.load(Ordering::Relaxed);
        if now < last + PROGRESS_MS
            || self
                .last_emit_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let top = self
            .top
            .get()
            .map(|t| {
                t.iter()
                    .map(|(name, is_dir, b, f)| LiveEntry {
                        name: name.clone(),
                        is_dir: *is_dir,
                        size: b.load(Ordering::Relaxed),
                        files: f.load(Ordering::Relaxed),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let _ = self.tx.send(ScanEvent::Progress {
            scanned: self.scanned.load(Ordering::Relaxed),
            current: dir.display().to_string(),
            top,
        });
    }

    fn warn(&self, path: &Path, err: impl std::fmt::Display) {
        let mut w = self.warnings.lock().unwrap();
        if w.0.len() < MAX_WARNINGS {
            w.0.push(format!("{}: {}", path.display(), err));
        } else {
            w.1 += 1;
        }
    }
}

/// Read `dir` and, in parallel, all of its subdirectories. Symlinks are
/// never followed (entries are lstat'ed via `DirEntry::metadata`).
/// `dev` is `dir`'s device id; `top` is the index of the root child this
/// directory lives under (`None` only for the root itself).
fn read_tree(dir: &Path, dev: u64, ctx: &WalkCtx, top: Option<usize>) -> DirBlock {
    let mut block = DirBlock::default();
    if ctx.cancel.load(Ordering::Relaxed) {
        return block;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) => {
            ctx.warn(dir, err);
            return block;
        }
    };
    // Subdirectories to descend: (entry index, raw name, device id).
    let mut subdirs: Vec<(usize, std::ffi::OsString, u64)> = Vec::new();
    for de in rd {
        let de = match de {
            Ok(de) => de,
            Err(err) => {
                ctx.warn(dir, err);
                continue;
            }
        };
        let is_dir = de.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let md = de.metadata().ok();
        let (size, allocated) = match (&md, is_dir) {
            (Some(m), false) => (m.len(), allocated(m)),
            _ => (0, 0),
        };
        let os_name = de.file_name();
        let name_off = block.names.len() as u32;
        block.names.push_str(&os_name.to_string_lossy());
        let name_len = (block.names.len() - name_off as usize) as u16;
        if is_dir {
            let child_dev = md.as_ref().map_or(dev, device);
            let path = dir.join(&os_name);
            if child_dev != dev && is_virtual_fs(&path) {
                ctx.warn(&path, "skipped virtual filesystem");
            } else {
                subdirs.push((block.entries.len(), os_name, child_dev));
            }
        }
        block.entries.push(Entry {
            name_off,
            name_len,
            is_dir,
            size,
            allocated,
            mtime: md.as_ref().map_or(crate::model::NO_MTIME, mtime_secs),
        });
    }
    ctx.scanned.fetch_add(block.entries.len() as u64, Ordering::Relaxed);
    ctx.name_bytes.fetch_add(block.names.len() as u64, Ordering::Relaxed);
    match top {
        None => {
            let _ = ctx.top.set(
                block
                    .entries
                    .iter()
                    .map(|e| {
                        let name = &block.names[e.name_off as usize..][..e.name_len as usize];
                        let files = u64::from(!e.is_dir);
                        (name.to_string(), e.is_dir, AtomicU64::new(e.size), AtomicU64::new(files))
                    })
                    .collect(),
            );
        }
        Some(i) => {
            if let Some(t) = ctx.top.get().and_then(|t| t.get(i)) {
                let (bytes, files) = block
                    .entries
                    .iter()
                    .filter(|e| !e.is_dir)
                    .fold((0, 0), |(b, f), e| (b + e.size, f + 1));
                t.2.fetch_add(bytes, Ordering::Relaxed);
                t.3.fetch_add(files, Ordering::Relaxed);
            }
        }
    }
    ctx.maybe_emit(dir);
    block.subdirs = subdirs
        .into_par_iter()
        .map(|(i, os_name, child_dev)| {
            let sub = read_tree(&dir.join(&os_name), child_dev, ctx, Some(top.unwrap_or(i)));
            (i, sub)
        })
        .collect();
    block
}
