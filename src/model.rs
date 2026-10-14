use std::ops::Range;
use std::path::PathBuf;

/// Sentinel for "no parent" in the compact u32 id fields.
pub const NO_PARENT: u32 = u32::MAX;
/// Sentinel `mtime` for "modification time unknown". Sorts oldest.
pub const NO_MTIME: i64 = i64::MIN;

/// One file or folder, kept small (56 bytes) because big scans hold
/// millions. Names live in `ScanResult::names`; a node's children are the
/// contiguous id range `first_child..first_child + child_count`.
#[derive(Clone)]
pub struct Node {
    pub size: u64,
    /// Bytes actually allocated on disk (st_blocks * 512), aggregated for folders.
    pub allocated: u64,
    /// Seconds since the Unix epoch, or `NO_MTIME`.
    pub mtime: i64,
    name_off: u32,
    pub file_count: u32,
    pub dir_count: u32,
    parent: u32,
    first_child: u32,
    child_count: u32,
    name_len: u16,
    pub is_dir: bool,
    pub deleted: bool,
}

impl Node {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: Range<usize>,
        is_dir: bool,
        size: u64,
        allocated: u64,
        mtime: i64,
        parent: Option<usize>,
    ) -> Self {
        Self {
            size,
            allocated,
            mtime,
            name_off: u32::try_from(name.start).expect("names buffer exceeds 4 GiB"),
            name_len: u16::try_from(name.len()).unwrap_or(u16::MAX),
            file_count: u32::from(!is_dir),
            dir_count: 0,
            parent: parent.map_or(NO_PARENT, |p| p as u32),
            first_child: 0,
            child_count: 0,
            is_dir,
            deleted: false,
        }
    }

    pub fn parent(&self) -> Option<usize> {
        (self.parent != NO_PARENT).then_some(self.parent as usize)
    }

    /// Child ids (including deleted ones; callers filter).
    pub fn children(&self) -> Range<usize> {
        let a = self.first_child as usize;
        a..a + self.child_count as usize
    }

    pub fn set_children(&mut self, ids: Range<usize>) {
        self.first_child = ids.start as u32;
        self.child_count = ids.len() as u32;
    }

    fn name_range(&self) -> Range<usize> {
        let a = self.name_off as usize;
        a..a + self.name_len as usize
    }
}

#[derive(Clone)]
pub struct ScanResult {
    pub nodes: Vec<Node>,
    /// Every node's name, concatenated (see `ScanResult::name`).
    pub names: String,
    pub root: usize,
    pub total_size: u64,
    pub root_path: PathBuf,
    pub warnings: Vec<String>,
    /// Number of warnings dropped because of the cap.
    pub warnings_truncated: usize,
}

impl ScanResult {
    /// File or folder name of `id` (no parent prefix).
    pub fn name(&self, id: usize) -> &str {
        &self.names[self.nodes[id].name_range()]
    }

    /// Child ids of `id` (including deleted ones; callers filter).
    pub fn children_of(&self, id: usize) -> Range<usize> {
        self.nodes[id].children()
    }

    /// Reconstruct the full path by walking parents and joining names onto `root_path`.
    /// Cost is O(depth); only call when a concrete path is needed (delete/open/export).
    pub fn full_path(&self, id: usize) -> PathBuf {
        if id >= self.nodes.len() || id == self.root {
            return self.root_path.clone();
        }
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = Some(id);
        while let Some(cid) = cur {
            if cid == self.root {
                break;
            }
            let Some(n) = self.nodes.get(cid) else {
                break;
            };
            parts.push(self.name(cid));
            cur = n.parent();
        }
        let mut p = self.root_path.clone();
        for part in parts.into_iter().rev() {
            p.push(part);
        }
        p
    }

    pub fn display_path(&self, id: usize) -> String {
        self.full_path(id).display().to_string()
    }
}
