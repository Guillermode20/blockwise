use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gpui::{
    canvas, div, fill, point, prelude::*, px, rgb, rgba, size, uniform_list, AnyElement,
    Bounds, ClipboardItem, Context, Corners, CursorStyle, Div, ElementId, FocusHandle,
    KeyDownEvent, MouseButton, Pixels, Point, Render, ScrollStrategy, SharedString, Stateful,
    UniformListScrollHandle, Window,
};

use crate::filter::Filter;
use crate::format;
use crate::model::{Node, ScanResult};
use crate::scanner::{start_scan, LiveEntry, ScanEvent};
use crate::settings::Settings;
use crate::view_model::{self, SortKey, SortSpec};
use crate::treemap::{self, ExtColors, Treemap};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewKind {
    /// Upper pane mode. The treemap is always shown below, WizTree-style.
    Tree,
    TopFiles,
    Types,
}

// Tree column widths (Name takes the rest), in WizTree's column order.
const COL_PCT: f32 = 120.;
const COL_SIZE: f32 = 84.;
const COL_ALLOC: f32 = 84.;
const COL_COUNT: f32 = 76.;
const COL_MODIFIED: f32 = 164.;
const ROW_H: f32 = 20.;
const INDENT: f32 = 12.;

/// Rows moved by PageUp/PageDown.
const PAGE_ROWS: isize = 20;

/// What a treemap bitmap was built for: (scan version, root node,
/// highlighted extension, width px, height px).
type MapKey = (u64, usize, Option<String>, u32, u32);

/// The treemap bitmap is built on a background thread and cached until its
/// `MapKey` changes. While a rebuild is in flight the previous bitmap is
/// stretched to the current bounds, so resizing never blocks a frame.
#[derive(Default)]
struct TreemapCache {
    key: Option<MapKey>,
    /// Key of the build currently running (at most one at a time; later
    /// requests coalesce into the next build when it finishes).
    pending: Option<MapKey>,
    map: Option<Treemap>,
    colors: Option<(u64, Arc<ExtColors>)>,
    bounds: Option<Bounds<Pixels>>,
    scale: f32,
    /// Scan version the view currently shows; maps from other versions are
    /// never hit-tested (their node ids may not exist any more).
    want_version: u64,
}

impl TreemapCache {
    fn current_map(&self) -> Option<&Treemap> {
        let map = self.map.as_ref()?;
        (self.key.as_ref()?.0 == self.want_version).then_some(map)
    }

    /// Window point -> map pixel coordinates (the map may be stale-sized).
    fn to_map(&self, pos: Point<Pixels>) -> Option<(f32, f32)> {
        let b = self.bounds?;
        if !b.contains(&pos) {
            return None;
        }
        let map = self.current_map()?;
        let fx = f32::from(pos.x - b.origin.x) / f32::from(b.size.width);
        let fy = f32::from(pos.y - b.origin.y) / f32::from(b.size.height);
        Some((fx * map.layout.width as f32, fy * map.layout.height as f32))
    }

    /// Node under a window-space point (deepest = a file, as in WizTree).
    fn hit(&self, pos: Point<Pixels>) -> Option<usize> {
        let (x, y) = self.to_map(pos)?;
        self.current_map()?.layout.hit_test(x, y)
    }

    fn ext_colors(&self) -> Option<Arc<ExtColors>> {
        self.colors
            .as_ref()
            .filter(|(v, _)| *v == self.want_version)
            .map(|(_, c)| c.clone())
    }
}

/// Popup menus share one overlay; only one is open at a time.
enum Menu {
    Context { pos: Point<Pixels>, id: usize },
    Recent { pos: Point<Pixels> },
}

#[derive(Clone)]
enum MenuAction {
    Open(usize),
    OpenFolder(usize),
    CopyPath(usize),
    Reveal(usize),
    Zoom(usize),
    Delete(usize),
    Scan(String),
    ClearRecent,
}

/// Dark, low-chroma palette. Colour is reserved for state (selection,
/// danger, warnings); everything else is a shade of grey.
mod theme {
    pub const BG: u32 = 0x0d0e0f;
    pub const PANEL: u32 = 0x141517;
    pub const HEADER: u32 = 0x18191b;
    pub const BORDER: u32 = 0x25272a;
    pub const BORDER_HI: u32 = 0x3a3d42;
    pub const ROW_ALT: u32 = 0x111213;
    pub const HOVER: u32 = 0x1c1e21;
    pub const PRESS: u32 = 0x26292d;
    pub const SEL: u32 = 0x232c38;
    pub const SEL_EDGE: u32 = 0x6f86a6;
    pub const TEXT: u32 = 0xc4c6c9;
    pub const TEXT_DIM: u32 = 0x74787e;
    pub const TEXT_HI: u32 = 0xe6e7e9;
    pub const BAR: u32 = 0x4b5869;
    pub const BAR_BG: u32 = 0x1a1c1f;
    pub const DANGER: u32 = 0xb46a6a;
    pub const DANGER_BG: u32 = 0x2a1a1a;
    pub const OK: u32 = 0x86a086;
    pub const WARN: u32 = 0xb4a476;
}

/// Monospace families in order of preference; the first one installed wins.
const FONT_PREFS: [&str; 7] = [
    "JetBrains Mono",
    "Iosevka",
    "Cascadia Mono",
    "Hack",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Adwaita Mono",
];

pub fn pick_font(installed: &[String]) -> SharedString {
    FONT_PREFS
        .iter()
        .find(|f| installed.iter().any(|i| i == *f))
        .copied()
        .unwrap_or("monospace")
        .into()
}

/// Flat, square toolbar button with hover/press feedback.
fn btn(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(22.))
        .px_2()
        .flex()
        .flex_none()
        .items_center()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::HEADER))
        .text_color(rgb(theme::TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::HOVER)).border_color(rgb(theme::BORDER_HI)).text_color(rgb(theme::TEXT_HI)))
        .active(|s| s.bg(rgb(theme::PRESS)))
        .child(label.into())
}

fn danger_btn(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    btn(id, label)
        .text_color(rgb(theme::DANGER))
        .hover(|s| s.bg(rgb(theme::DANGER_BG)).border_color(rgb(theme::DANGER)))
}

/// Tree columns, hidden right-to-left as the window narrows.
const COL_SHOW_PCT: u8 = 1;
const COL_SHOW_ALLOC: u8 = 2;
const COL_SHOW_COUNTS: u8 = 4;
const COL_SHOW_MODIFIED: u8 = 8;

fn columns_for_width(w: f32) -> u8 {
    let mut m = 0;
    if w >= 520. {
        m |= COL_SHOW_PCT;
    }
    if w >= 760. {
        m |= COL_SHOW_ALLOC;
    }
    if w >= 980. {
        m |= COL_SHOW_COUNTS;
    }
    if w >= 1140. {
        m |= COL_SHOW_MODIFIED;
    }
    m
}

pub struct Blockwise {
    font: SharedString,
    focus: FocusHandle,
    search_focus: FocusHandle,
    settings: Settings,
    /// Search box text; non-empty switches the tree to a flat match list.
    filter: String,
    filter_active: bool,
    /// Root children with running totals while a scan is in flight.
    live: Vec<LiveEntry>,
    live_scanned: u64,
    /// Treemap shows this folder's subtree instead of the scan root.
    zoom: Option<usize>,
    /// File Types selection: dims every other extension in the treemap.
    highlight_ext: Option<String>,
    types_sel: Option<usize>,
    menu: Option<Menu>,
    show_warnings: bool,
    dragging_split: bool,
    content_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    top_scroll: UniformListScrollHandle,
    types_scroll: UniformListScrollHandle,
    tree_state: view_model::TreeState,
    /// Background search bookkeeping: cancel flag of the running query and
    /// the (query, sort, scan version) whose results `visible` holds.
    search_cancel: Option<Arc<AtomicBool>>,
    search_done: Option<(String, SortSpec, u64)>,
    searching: bool,
    /// Row index of `selected` in `visible` when known, so arrow keys don't
    /// search the list for it.
    sel_hint: Option<usize>,
    path_cache: RefCell<Vec<(usize, u64, String)>>,
    /// `COL_SHOW_*` mask, recomputed from the window width each render.
    cols: u8,
    root_path: Option<String>,
    scanning: bool,
    progress: Option<String>,
    error: Option<String>,
    scan: Option<Arc<ScanResult>>,
    cancel: Option<Arc<AtomicBool>>,
    /// Generation counter: stale scan events from a previous scan are dropped.
    scan_gen: u64,
    view: ViewKind,
    sort: SortSpec,
    selected: Option<usize>,
    expanded: HashSet<usize>,
    visible: Vec<(usize, usize)>,
    visible_dirty: bool,
    treemap: Rc<RefCell<TreemapCache>>,
    /// Bumped whenever the scan data changes so the treemap re-renders.
    scan_version: u64,
    hovered: Option<usize>,
    tree_scroll: UniformListScrollHandle,
    pending_delete: Option<usize>,
    status: Option<String>,
    deleting: bool,
    top_files_cache: Vec<usize>,
    top_files_dirty: bool,
}

impl Blockwise {
    pub fn new(font: SharedString, settings: Settings, cx: &mut Context<Self>) -> Self {
        Self {
            font,
            focus: cx.focus_handle(),
            search_focus: cx.focus_handle(),
            settings,
            filter: String::new(),
            filter_active: false,
            live: Vec::new(),
            live_scanned: 0,
            zoom: None,
            highlight_ext: None,
            types_sel: None,
            menu: None,
            show_warnings: false,
            dragging_split: false,
            content_bounds: Rc::new(Cell::new(None)),
            top_scroll: UniformListScrollHandle::new(),
            types_scroll: UniformListScrollHandle::new(),
            tree_state: view_model::TreeState::default(),
            search_cancel: None,
            search_done: None,
            searching: false,
            sel_hint: None,
            path_cache: RefCell::new(Vec::new()),
            cols: 0xff,
            root_path: None,
            scanning: false,
            progress: None,
            error: None,
            scan: None,
            cancel: None,
            scan_gen: 0,
            view: ViewKind::Tree,
            sort: SortSpec {
                key: SortKey::Size,
                ascending: false,
            },
            selected: None,
            expanded: HashSet::new(),
            visible: Vec::new(),
            visible_dirty: false,
            treemap: Rc::new(RefCell::new(TreemapCache::default())),
            scan_version: 0,
            hovered: None,
            tree_scroll: UniformListScrollHandle::new(),
            pending_delete: None,
            status: None,
            deleting: false,
            top_files_cache: Vec::new(),
            top_files_dirty: true,
        }
    }

    pub fn start_scan(&mut self, path: String, cx: &mut Context<Self>) {
        if let Some(c) = self.cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        self.scan_gen = self.scan_gen.wrapping_add(1);
        let generation = self.scan_gen;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let rx = start_scan(PathBuf::from(path.clone()), cancel);

        self.settings.push_recent(&path);
        self.settings.save();
        self.root_path = Some(path);
        self.scanning = true;
        self.live.clear();
        self.live_scanned = 0;
        self.zoom = None;
        self.highlight_ext = None;
        self.types_sel = None;
        self.menu = None;
        self.show_warnings = false;
        self.progress = Some("Starting scan...".into());
        self.error = None;
        self.scan = None;
        self.selected = None;
        self.hovered = None;
        self.expanded.clear();
        self.visible.clear();
        self.visible_dirty = true;
        self.status = None;
        self.top_files_cache.clear();
        self.top_files_dirty = true;

        let weak = cx.entity().downgrade();
        cx.spawn(async move |_weak, async_cx| {
            while let Ok(event) = rx.recv_async().await {
                let weak2 = weak.clone();
                let _ = weak2.update::<gpui::AsyncApp, _>(&mut *async_cx, |app, cx| {
                    app.handle_event(event, generation, cx);
                });
            }
        })
        .detach();
    }

    fn handle_event(&mut self, event: ScanEvent, generation: u64, cx: &mut Context<Self>) {
        // Drop events from a superseded scan so a slow old thread can't
        // overwrite a newer scan.
        if generation != self.scan_gen {
            return;
        }
        match event {
            ScanEvent::Progress { scanned, current, mut top } => {
                self.progress = Some(format!(
                    "scanning  {} entries  {}",
                    format::format_number(scanned),
                    current
                ));
                top.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
                self.live = top;
                self.live_scanned = scanned;
            }
            ScanEvent::Done(result) => {
                self.scanning = false;
                self.progress = None;
                self.expanded.insert(result.root);
                self.scan = Some(Arc::new(result));
                self.scan_version = self.scan_version.wrapping_add(1);
                self.visible_dirty = true;
                self.top_files_dirty = true;
                self.live.clear();
                self.status = Some("scan complete".into());
            }
            ScanEvent::Cancelled => {
                self.scanning = false;
                self.progress = Some("Scan cancelled".into());
            }
            ScanEvent::Failed(msg) => {
                self.scanning = false;
                self.progress = None;
                self.error = Some(msg);
            }
        }
        cx.notify();
    }

    /// Rebuild the upper list. The tree is rebuilt synchronously (cheap,
    /// sorted children are cached); a search runs on the background
    /// executor and replaces `visible` when it lands, so typing never
    /// blocks a frame. The previous rows stay up in the meantime.
    fn rebuild_visible(&mut self, cx: &mut Context<Self>) {
        let Some(scan) = self.scan.clone() else {
            self.visible.clear();
            return;
        };
        let Some(filter) = Filter::parse(&self.filter) else {
            self.rebuild_tree();
            return;
        };
        if !self.filter_active {
            // Entering search mode: tree rows are not search results.
            self.visible.clear();
        }
        self.filter_active = true;
        let key = (self.filter.clone(), self.sort, self.scan_version);
        if self.search_done.as_ref() == Some(&key) {
            return;
        }
        if let Some(c) = self.search_cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.search_cancel = Some(cancel.clone());
        self.searching = true;
        let sort = self.sort;
        cx.spawn(async move |this, cx| {
            let flag = cancel.clone();
            let ids = cx
                .background_executor()
                .spawn(async move { view_model::search(&scan, &filter, sort, &flag) })
                .await;
            let Some(ids) = ids else {
                return;
            };
            let _ = this.update(cx, |app, cx| {
                // Drop results a newer query or scan has superseded.
                if cancel.load(Ordering::Relaxed) || !app.filter_active {
                    return;
                }
                app.visible.clear();
                app.visible.extend(ids.into_iter().map(|id| (id, 0)));
                app.searching = false;
                app.search_done = Some(key);
                app.sel_hint = None;
                cx.notify();
            });
        })
        .detach();
    }

    /// Synchronous tree rows (leaves search mode, cancelling any search).
    fn rebuild_tree(&mut self) {
        if let Some(c) = self.search_cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        self.filter_active = false;
        self.searching = false;
        self.search_done = None;
        self.sel_hint = None;
        match &self.scan {
            Some(scan) => view_model::build_visible(
                scan,
                self.scan_version,
                &self.expanded,
                self.sort,
                &mut self.tree_state,
                &mut self.visible,
            ),
            None => self.visible.clear(),
        }
    }

    /// Full path of `id`, memoized: the footer and treemap status line ask
    /// for the same one or two paths every frame.
    fn path_of(&self, id: usize) -> String {
        let Some(scan) = &self.scan else {
            return String::new();
        };
        let mut cache = self.path_cache.borrow_mut();
        if let Some((_, _, p)) = cache.iter().find(|(i, v, _)| *i == id && *v == self.scan_version) {
            return p.clone();
        }
        let p = scan.display_path(id);
        if cache.len() >= 4 {
            cache.remove(0);
        }
        cache.push((id, self.scan_version, p.clone()));
        p
    }

    fn ensure_top_files(&mut self) {
        if !self.top_files_dirty {
            return;
        }
        self.top_files_dirty = false;
        self.top_files_cache.clear();
        let Some(scan) = &self.scan else {
            return;
        };
        self.top_files_cache = view_model::top_files(scan);
    }

    fn request_delete(&mut self, node_id: usize, cx: &mut Context<Self>) {
        self.pending_delete = Some(node_id);
        cx.notify();
    }

    fn perform_delete(&mut self, node_id: usize, cx: &mut Context<Self>) {
        let full_path: Option<PathBuf> = self.scan.as_ref().and_then(|s| {
            if s.nodes.get(node_id).is_some() {
                Some(s.full_path(node_id))
            } else {
                None
            }
        });
        let Some(path) = full_path else {
            self.pending_delete = None;
            self.error = Some("Delete failed: invalid node".into());
            cx.notify();
            return;
        };

        self.pending_delete = None;
        self.deleting = true;
        cx.notify();

        // Filesystem work off the UI thread so large trees don't freeze.
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, async_cx| {
            let res = async_cx
                .background_executor()
                .spawn(async move {
                    let mut ok = false;
                    let mut err = String::new();
                    let is_dir_symlink_aware = path.is_dir() && !path.is_symlink();
                    if let Ok(status) = std::process::Command::new("gio")
                        .arg("trash")
                        .arg(&path)
                        .status()
                    {
                        if status.success() {
                            ok = true;
                        }
                    }
                    if !ok {
                        if let Ok(status) = std::process::Command::new("trash-put")
                            .arg(&path)
                            .status()
                        {
                            if status.success() {
                                ok = true;
                            }
                        }
                    }
                    if !ok {
                        let res = if is_dir_symlink_aware {
                            std::fs::remove_dir_all(&path)
                        } else {
                            std::fs::remove_file(&path)
                        };
                        match res {
                            Ok(()) => ok = true,
                            Err(e) => err = e.to_string(),
                        }
                    }
                    (node_id, ok, err)
                })
                .await;
            let _ = weak.update(&mut *async_cx, |app, cx| {
                app.finish_delete(res.0, res.1, res.2, cx);
            });
        })
        .detach();
    }

    fn finish_delete(&mut self, node_id: usize, ok: bool, err: String, cx: &mut Context<Self>) {
        self.deleting = false;
        if ok {
            self.apply_delete(node_id);
            self.status = Some("Deleted".into());
        } else {
            self.error = Some(format!("Delete failed: {}", err));
        }
        cx.notify();
    }

    fn apply_delete(&mut self, node_id: usize) {
        // Mark the whole subtree deleted and fix ancestor aggregates.
        // Arc::make_mut clones only on shared refs (scan is otherwise
        // treated as immutable, so this is a cheap unique-borrow in practice).
        let Some(scan) = self.scan.as_mut().map(Arc::make_mut) else {
            return;
        };
        if node_id >= scan.nodes.len() {
            return;
        }
        // Collect subtree.
        let mut subtree: Vec<usize> = Vec::new();
        let mut stack = vec![node_id];
        let mut subtree_dirs: u32 = 0;
        while let Some(id) = stack.pop() {
            if id >= scan.nodes.len() || scan.nodes[id].deleted {
                continue;
            }
            subtree.push(id);
            if scan.nodes[id].is_dir {
                subtree_dirs += 1;
            }
            stack.extend(scan.nodes[id].children());
        }
        // Subtree root's aggregates already include its descendants.
        let size = scan.nodes[node_id].size;
        let allocated = scan.nodes[node_id].allocated;
        let files = scan.nodes[node_id].file_count;
        // dir_count counts descendant dirs; deleting X removes X itself
        // (if a dir) plus its descendant dirs from every ancestor.
        // subtree_dirs already includes X when X is a dir, which is exactly
        // the amount each ancestor over-counts by.
        let dir_adjust = subtree_dirs;

        // Children are an id range, so the node stays in its parent's range;
        // every consumer skips `deleted` nodes.
        let mut cur = scan.nodes[node_id].parent();
        while let Some(pid) = cur {
            scan.nodes[pid].size = scan.nodes[pid].size.saturating_sub(size);
            scan.nodes[pid].allocated = scan.nodes[pid].allocated.saturating_sub(allocated);
            scan.nodes[pid].file_count = scan.nodes[pid].file_count.saturating_sub(files);
            scan.nodes[pid].dir_count = scan.nodes[pid].dir_count.saturating_sub(dir_adjust);
            // Keep the cached total in sync.
            if pid == scan.root {
                scan.total_size = scan.nodes[pid].size;
            }
            cur = scan.nodes[pid].parent();
        }
        // Deleting the root itself: nothing above to adjust.
        if node_id == scan.root {
            scan.total_size = 0;
        }
        let deleted_set: HashSet<usize> = subtree.iter().copied().collect();
        for id in subtree {
            scan.nodes[id].deleted = true;
        }
        if self.selected.map(|s| deleted_set.contains(&s)).unwrap_or(false) {
            self.selected = None;
        }
        if self.hovered.map(|h| deleted_set.contains(&h)).unwrap_or(false) {
            self.hovered = None;
        }
        self.expanded.retain(|id| !deleted_set.contains(id));
        self.scan_version = self.scan_version.wrapping_add(1);
        self.visible_dirty = true;
        self.top_files_dirty = true;
    }

    fn export_csv(&mut self) {
        use std::fs::File;
        use std::io::{BufWriter, Write};

        let Some(scan) = &self.scan else {
            self.status = Some("Nothing to export".into());
            return;
        };
        let Some(dest) = rfd::FileDialog::new()
            .set_file_name("blockwise-export.csv")
            .add_filter("CSV", &["csv"])
            .save_file()
        else {
            return;
        };
        let file = match File::create(&dest) {
            Ok(f) => f,
            Err(e) => {
                self.error = Some(format!("Export failed: {}", e));
                return;
            }
        };
        // Stream rows instead of building a giant String in RAM.
        let mut w = BufWriter::with_capacity(64 * 1024, file);
        if writeln!(w, "id,path,is_dir,size,files").is_err() {
            self.error = Some("Export failed: write error".into());
            return;
        }
        for (id, n) in scan.nodes.iter().enumerate() {
            if n.deleted {
                continue;
            }
            let p = scan.full_path(id).display().to_string().replace('"', "\"\"");
            if writeln!(
                w,
                "{},\"{}\",{},{},{}",
                id, p, n.is_dir, n.size, n.file_count
            )
            .is_err()
            {
                self.error = Some("Export failed: write error".into());
                return;
            }
        }
        match w.flush() {
            Ok(()) => self.status = Some(format!("Exported to {}", dest.display())),
            Err(e) => self.error = Some(format!("Export failed: {}", e)),
        }
    }

    fn open_selected(&mut self) {
        let Some(scan) = &self.scan else {
            return;
        };
        let id = self.selected.unwrap_or(scan.root);
        if id >= scan.nodes.len() {
            return;
        }
        let path = scan.full_path(id);
        let _ = open::that_detached(path);
    }

    fn toggle_sort(&mut self, key: SortKey) {
        if self.sort.key == key {
            self.sort.ascending = !self.sort.ascending;
        } else {
            self.sort.key = key;
            // Name starts A-Z; every numeric column starts largest-first.
            self.sort.ascending = key == SortKey::Name;
        }
        self.visible_dirty = true;
    }

    /// Select a node and expand its ancestors so it is visible in the tree,
    /// then scroll to it (WizTree's treemap -> tree navigation).
    fn reveal(&mut self, id: usize) {
        let Some(scan) = &self.scan else {
            return;
        };
        let mut cur = scan.nodes.get(id).and_then(|n| n.parent());
        while let Some(pid) = cur {
            self.expanded.insert(pid);
            cur = scan.nodes[pid].parent();
        }
        self.selected = Some(id);
        // Search results that don't include the node would hide it: leave
        // search mode for the tree.
        if !self.filter_active || !self.visible.iter().any(|&(v, _)| v == id) {
            self.filter.clear();
            self.rebuild_tree();
        }
        self.visible_dirty = false;
        if let Some(ix) = self.visible.iter().position(|&(v, _)| v == id) {
            self.tree_scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
    }

    fn toggle_expanded(&mut self, id: usize) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.visible_dirty = true;
    }

    fn set_filter(&mut self, text: String) {
        self.filter = text;
        self.visible_dirty = true;
        self.tree_scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    /// Folder the treemap is laid out from (zoom target if still valid).
    fn treemap_root(&self) -> Option<usize> {
        let scan = self.scan.as_ref()?;
        Some(
            self.zoom
                .filter(|&z| scan.nodes.get(z).is_some_and(|n| n.is_dir && !n.deleted))
                .unwrap_or(scan.root),
        )
    }

    /// Zoom the treemap into `id` (a file zooms into its folder).
    fn zoom_to(&mut self, id: usize) {
        let Some(scan) = &self.scan else {
            return;
        };
        let Some(n) = scan.nodes.get(id) else {
            return;
        };
        let target = if n.is_dir { id } else { n.parent().unwrap_or(scan.root) };
        self.zoom = (target != scan.root).then_some(target);
        self.hovered = None;
    }

    fn zoom_out(&mut self) {
        let (Some(scan), Some(z)) = (&self.scan, self.treemap_root()) else {
            return;
        };
        match scan.nodes[z].parent() {
            Some(p) => self.zoom_to(p),
            None => self.zoom = None,
        }
    }

    fn copy_path(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(scan) = &self.scan else {
            return;
        };
        let path = scan.display_path(id);
        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
        self.status = Some(format!("copied {}", path));
    }

    fn pick_and_scan(&mut self, cx: &mut Context<Self>) {
        if let Some(f) = rfd::FileDialog::new().pick_folder() {
            self.start_scan(f.to_string_lossy().to_string(), cx);
        }
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        match self.root_path.clone() {
            Some(p) => self.start_scan(p, cx),
            None => self.pick_and_scan(cx),
        }
    }

    fn run_menu_action(&mut self, action: MenuAction, cx: &mut Context<Self>) {
        self.menu = None;
        match action {
            MenuAction::Open(id) => {
                self.selected = Some(id);
                self.open_selected();
            }
            MenuAction::OpenFolder(id) => {
                if let Some(scan) = &self.scan {
                    let dir = match scan.nodes.get(id) {
                        Some(n) if n.is_dir => id,
                        Some(n) => n.parent().unwrap_or(scan.root),
                        None => return,
                    };
                    let _ = open::that_detached(scan.full_path(dir));
                }
            }
            MenuAction::CopyPath(id) => self.copy_path(id, cx),
            MenuAction::Reveal(id) => {
                self.view = ViewKind::Tree;
                self.reveal(id);
            }
            MenuAction::Zoom(id) => self.zoom_to(id),
            MenuAction::Delete(id) => {
                self.selected = Some(id);
                self.pending_delete = Some(id);
            }
            MenuAction::Scan(path) => self.start_scan(path, cx),
            MenuAction::ClearRecent => {
                self.settings.recent.clear();
                self.settings.save();
            }
        }
        cx.notify();
    }

    fn file_types(&self) -> Option<Arc<ExtColors>> {
        self.treemap.borrow().ext_colors()
    }

    fn set_view(&mut self, kind: ViewKind) {
        self.view = kind;
        if kind == ViewKind::TopFiles {
            self.ensure_top_files();
        }
    }

    fn select_type(&mut self, ix: Option<usize>) {
        let types = self.file_types();
        let ext = ix.and_then(|i| types.as_ref()?.ranked.get(i).map(|t| t.ext.clone()));
        if ext.is_some() && ext == self.highlight_ext {
            // Clicking the highlighted type again clears it.
            self.types_sel = None;
            self.highlight_ext = None;
        } else {
            self.types_sel = ix.filter(|_| ext.is_some());
            self.highlight_ext = ext;
        }
    }

    /// Move the selection in the active list by `delta` rows (clamped);
    /// `isize::MIN`/`MAX` jump to the ends.
    fn move_selection(&mut self, delta: isize) {
        let (len, cur) = match self.view {
            ViewKind::Tree => (
                self.visible.len(),
                self.selected.and_then(|s| {
                    self.sel_hint
                        .filter(|&h| self.visible.get(h).is_some_and(|&(v, _)| v == s))
                        .or_else(|| self.visible.iter().position(|&(v, _)| v == s))
                }),
            ),
            ViewKind::TopFiles => (
                self.top_files_cache.len(),
                self.selected.and_then(|s| self.top_files_cache.iter().position(|&v| v == s)),
            ),
            ViewKind::Types => (
                self.file_types().map(|t| t.ranked.len()).unwrap_or(0),
                self.types_sel,
            ),
        };
        if len == 0 {
            return;
        }
        let ix = match (cur, delta) {
            (_, isize::MIN) => 0,
            (_, isize::MAX) => len - 1,
            (None, d) if d < 0 => len - 1,
            (None, _) => 0,
            (Some(c), d) => (c as isize + d).clamp(0, len as isize - 1) as usize,
        };
        let strategy = if delta < 0 { ScrollStrategy::Top } else { ScrollStrategy::Bottom };
        match self.view {
            ViewKind::Tree => {
                self.selected = Some(self.visible[ix].0);
                self.sel_hint = Some(ix);
                self.tree_scroll.scroll_to_item(ix, strategy);
            }
            ViewKind::TopFiles => {
                self.selected = Some(self.top_files_cache[ix]);
                self.top_scroll.scroll_to_item(ix, strategy);
            }
            ViewKind::Types => {
                self.types_sel = Some(ix);
                self.highlight_ext = self.file_types().map(|t| t.ranked[ix].ext.clone());
                self.types_scroll.scroll_to_item(ix, strategy);
            }
        }
    }

    /// Left: collapse, or step to the parent. Right: expand, or step into
    /// the first child.
    fn step_horizontal(&mut self, right: bool) {
        let (Some(scan), Some(id)) = (self.scan.clone(), self.selected) else {
            return;
        };
        let Some(n) = scan.nodes.get(id) else {
            return;
        };
        if self.filter_active || self.view != ViewKind::Tree {
            return;
        }
        let has_kids = n.is_dir && n.children().any(|c| !scan.nodes[c].deleted);
        if right {
            if has_kids && !self.expanded.contains(&id) {
                self.toggle_expanded(id);
            } else if has_kids {
                self.move_selection(1);
            }
        } else if self.expanded.contains(&id) && has_kids {
            self.toggle_expanded(id);
        } else if let Some(p) = n.parent() {
            self.reveal(p);
        }
    }

    fn on_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        let ctrl = k.modifiers.control || k.modifiers.platform;
        let key = k.key.as_str();

        // Modal layers swallow keys first.
        if self.pending_delete.is_some() {
            match key {
                "enter" => {
                    if let Some(id) = self.pending_delete {
                        self.perform_delete(id, cx);
                    }
                }
                "escape" => self.pending_delete = None,
                _ => return,
            }
            cx.notify();
            return;
        }
        if self.menu.is_some() || self.show_warnings {
            if key == "escape" {
                self.menu = None;
                self.show_warnings = false;
                cx.notify();
            }
            return;
        }

        match (ctrl, key) {
            (true, "f") => window.focus(&self.search_focus),
            (true, "o") => self.pick_and_scan(cx),
            (true, "c") => {
                if let Some(id) = self.selected {
                    self.copy_path(id, cx);
                }
            }
            (true, "e") => self.export_csv(),
            (true, "1") => self.set_view(ViewKind::Tree),
            (true, "2") => self.set_view(ViewKind::TopFiles),
            (true, "3") => self.set_view(ViewKind::Types),
            (_, "f5") | (true, "r") => self.rescan(cx),
            (false, "up") => self.move_selection(-1),
            (false, "down") => self.move_selection(1),
            (false, "pageup") => self.move_selection(-PAGE_ROWS),
            (false, "pagedown") => self.move_selection(PAGE_ROWS),
            (false, "home") => self.move_selection(isize::MIN),
            (false, "end") => self.move_selection(isize::MAX),
            (false, "left") => self.step_horizontal(false),
            (false, "right") => self.step_horizontal(true),
            (false, "space") => {
                if let Some(id) = self.selected {
                    if self.scan.as_ref().is_some_and(|s| s.nodes.get(id).is_some_and(|n| n.is_dir)) {
                        self.toggle_expanded(id);
                    }
                }
            }
            (false, "enter") => self.open_selected(),
            (false, "delete") => {
                if let Some(id) = self.selected {
                    self.pending_delete = Some(id);
                }
            }
            (false, "z") => {
                if let Some(id) = self.selected {
                    self.zoom_to(id);
                }
            }
            (false, "backspace") => self.zoom_out(),
            (false, "escape") => {
                // Peel back one layer of state per press.
                if !self.filter.is_empty() {
                    self.set_filter(String::new());
                } else if self.highlight_ext.is_some() {
                    self.select_type(None);
                } else if self.zoom.is_some() {
                    self.zoom = None;
                } else {
                    self.selected = None;
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_search_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        let ctrl = k.modifiers.control || k.modifiers.platform;
        match k.key.as_str() {
            "escape" => {
                if self.filter.is_empty() {
                    window.focus(&self.focus);
                } else {
                    self.set_filter(String::new());
                }
            }
            "enter" | "down" | "tab" => {
                window.focus(&self.focus);
                self.view = ViewKind::Tree;
                self.selected = None;
                self.move_selection(1);
            }
            "backspace" => {
                let mut f = std::mem::take(&mut self.filter);
                if ctrl {
                    // Delete the last word.
                    let trimmed = f.trim_end().len();
                    f.truncate(f[..trimmed].rfind(' ').map(|i| i + 1).unwrap_or(0));
                } else {
                    f.pop();
                }
                self.set_filter(f);
            }
            "v" if ctrl => {
                if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    let f = format!("{}{}", self.filter, t.lines().next().unwrap_or(""));
                    self.set_filter(f);
                }
            }
            "u" if ctrl => self.set_filter(String::new()),
            _ => match &k.key_char {
                Some(ch) if !ctrl && !ch.chars().any(char::is_control) => {
                    let f = format!("{}{}", self.filter, ch);
                    self.set_filter(f);
                }
                // Let shortcuts (ctrl+f, f5, ...) bubble to the root handler.
                _ => return,
            },
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Persist window size + split; called when the window closes.
    pub fn save_settings(&mut self, window: &Window) {
        let size = window.viewport_size();
        self.settings.window = Some((f32::from(size.width), f32::from(size.height)));
        self.settings.save();
    }
}

/// Shared list row chrome: zebra stripes, hover, selection edge.
fn row_base(id: impl Into<ElementId>, ix: usize, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .h(px(ROW_H))
        .flex()
        .items_center()
        .border_l_2()
        .border_color(if selected { rgb(theme::SEL_EDGE).into() } else { gpui::transparent_black() })
        .bg(rgb(if selected {
            theme::SEL
        } else if ix % 2 == 0 {
            theme::BG
        } else {
            theme::ROW_ALT
        }))
        .when(!selected, |d| d.hover(|s| s.bg(rgb(theme::HOVER))))
}

/// Right-aligned numeric cell.
fn num_cell(w: f32, t: impl Into<SharedString>) -> Div {
    div()
        .w(px(w))
        .flex_none()
        .flex()
        .justify_end()
        .px_2()
        .whitespace_nowrap()
        .overflow_hidden()
        .child(t.into())
}

fn header_bar() -> Div {
    div()
        .w_full()
        .h(px(ROW_H + 2.))
        .flex()
        .flex_none()
        .items_center()
        .text_color(rgb(theme::TEXT_DIM))
        .bg(rgb(theme::HEADER))
        .border_b_1()
        .border_color(rgb(theme::BORDER))
}

fn placeholder(text: impl Into<SharedString>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(theme::TEXT_DIM))
        .child(text.into())
        .into_any_element()
}

/// Thin proportional bar used by "% of parent" and the live scan view.
fn bar(frac: f64, color: u32) -> Div {
    div().flex_1().h(px(4.)).bg(rgb(theme::BAR_BG)).child(
        div()
            .h_full()
            .w(gpui::relative(frac.clamp(0.0, 1.0) as f32))
            .bg(rgb(color)),
    )
}

impl Blockwise {
    fn render_tree(&self, cx: &mut Context<Self>) -> AnyElement {
        let cols = self.cols;
        let sort = self.sort;
        let border = rgb(theme::BORDER);
        let mut col_header = header_bar();
        let name_label: SharedString = match (self.filter_active, self.searching) {
            (false, _) => "NAME".into(),
            (true, true) => "MATCH  searching…".into(),
            (true, false) => format!("MATCH  {}", format::format_number(self.visible.len() as u64)).into(),
        };
        for (key, label, width, right, mask) in [
            (SortKey::Name, name_label.clone(), None, false, 0),
            (SortKey::Percent, "% PARENT".into(), Some(COL_PCT), false, COL_SHOW_PCT),
            (SortKey::Size, "SIZE".into(), Some(COL_SIZE), true, 0),
            (SortKey::Allocated, "ALLOC".into(), Some(COL_ALLOC), true, COL_SHOW_ALLOC),
            (SortKey::Items, "ITEMS".into(), Some(COL_COUNT), true, COL_SHOW_COUNTS),
            (SortKey::Files, "FILES".into(), Some(COL_COUNT), true, COL_SHOW_COUNTS),
            (SortKey::Folders, "DIRS".into(), Some(COL_COUNT), true, COL_SHOW_COUNTS),
            (SortKey::Modified, "MODIFIED".into(), Some(COL_MODIFIED), false, COL_SHOW_MODIFIED),
        ] {
            if mask != 0 && cols & mask == 0 {
                continue;
            }
            let active = sort.key == key;
            let arrow = match (active, sort.ascending) {
                (false, _) => "",
                (true, true) => " ↑",
                (true, false) => " ↓",
            };
            let mut cell = div()
                .id(SharedString::from(format!("sort-{:?}", key as u8)))
                .h_full()
                .flex()
                .items_center()
                .px_2()
                .border_r_1()
                .border_color(border)
                .whitespace_nowrap()
                .overflow_hidden()
                .cursor_pointer()
                .text_color(rgb(if active { theme::TEXT_HI } else { theme::TEXT_DIM }))
                .hover(|s| s.bg(rgb(theme::HOVER)).text_color(rgb(theme::TEXT)))
                .active(|s| s.bg(rgb(theme::PRESS)))
                .child(format!("{}{}", label, arrow))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_sort(key);
                    cx.notify();
                }));
            cell = match width {
                Some(w) => cell.w(px(w)).flex_none(),
                None => cell.flex_1().min_w_0(),
            };
            if right {
                cell = cell.justify_end();
            }
            col_header = col_header.child(cell);
        }

        let ul = uniform_list(
            "tree-rows",
            self.visible.len(),
            cx.processor(move |this, range, _window, cx| {
                let mut items = Vec::new();
                let Some(scan) = this.scan.clone() else {
                    return items;
                };
                let cols = this.cols;
                let flat = this.filter_active;
                for ix in range {
                    let Some(&(nid, depth)) = this.visible.get(ix) else {
                        continue;
                    };
                    let Some(node) = scan.nodes.get(nid) else {
                        continue;
                    };
                    let is_selected = this.selected == Some(nid);
                    let expanded = this.expanded.contains(&nid);
                    let is_dir = node.is_dir;
                    let has_kids =
                        !flat && is_dir && node.children().any(|c| !scan.nodes[c].deleted);
                    let frac = node
                        .parent()
                        .and_then(|pid| scan.nodes.get(pid))
                        .map(|p| if p.size == 0 { 0.0 } else { node.size as f64 / p.size as f64 })
                        .unwrap_or(1.0);
                    // The root row shows its full path, like "C:\" in WizTree.
                    let name = if nid == scan.root {
                        scan.root_path.display().to_string()
                    } else if is_dir {
                        format!("{}/", scan.name(nid))
                    } else {
                        scan.name(nid).to_string()
                    };
                    // Search results are flat, so show where each match lives.
                    let location = flat.then(|| {
                        let parent = node.parent().unwrap_or(scan.root);
                        let p = scan.full_path(parent);
                        let rel = p.strip_prefix(&scan.root_path).unwrap_or(&p);
                        format!("{}/", rel.display())
                    });
                    let dim = rgb(theme::TEXT_DIM);

                    let row = row_base(("tree-row", ix), ix, is_selected)
                        .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _w, cx| {
                            this.selected = Some(nid);
                            // Double-click opens/closes a folder.
                            if e.click_count() >= 2 && has_kids {
                                this.toggle_expanded(nid);
                            }
                            cx.notify();
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, e: &gpui::MouseDownEvent, _w, cx| {
                                this.selected = Some(nid);
                                this.menu = Some(Menu::Context { pos: e.position, id: nid });
                                cx.notify();
                            }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .flex()
                                .items_center()
                                .pl(px(4. + depth as f32 * INDENT))
                                .child(
                                    div()
                                        .id(("tree-expand", nid))
                                        .w(px(14.))
                                        .flex_none()
                                        .text_color(dim)
                                        .when(has_kids, |d| {
                                            d.cursor_pointer()
                                                .hover(|s| s.text_color(rgb(theme::TEXT_HI)))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.toggle_expanded(nid);
                                                    cx.notify();
                                                }))
                                        })
                                        .child(match (has_kids, expanded) {
                                            (false, _) => " ",
                                            (true, true) => "−",
                                            (true, false) => "+",
                                        }),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .max_w(gpui::relative(0.7))
                                        .truncate()
                                        .text_color(rgb(if is_dir || is_selected {
                                            theme::TEXT_HI
                                        } else {
                                            theme::TEXT
                                        }))
                                        .child(name),
                                )
                                .when_some(location, |d, loc| {
                                    d.child(div().min_w_0().pl_2().truncate().text_color(dim).child(loc))
                                }),
                        )
                        .when(cols & COL_SHOW_PCT != 0, |d| {
                            d.child(
                                div()
                                    .w(px(COL_PCT))
                                    .flex_none()
                                    .h_full()
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(bar(frac, if is_selected { theme::SEL_EDGE } else { theme::BAR }))
                                    .child(num_cell(44., format!("{:.1}", frac * 100.0)).px_0().text_color(dim)),
                            )
                        })
                        .child(num_cell(COL_SIZE, format::human_bytes(node.size)).text_color(rgb(theme::TEXT_HI)))
                        .when(cols & COL_SHOW_ALLOC != 0, |d| {
                            d.child(num_cell(COL_ALLOC, format::human_bytes(node.allocated)).text_color(dim))
                        })
                        .when(cols & COL_SHOW_COUNTS != 0, |d| {
                            let (items_s, files_s, dirs_s) = if is_dir {
                                (
                                    format::format_number(u64::from(node.file_count) + u64::from(node.dir_count)),
                                    format::format_number(node.file_count),
                                    format::format_number(node.dir_count),
                                )
                            } else {
                                (String::new(), String::new(), String::new())
                            };
                            d.child(num_cell(COL_COUNT, items_s).text_color(dim))
                                .child(num_cell(COL_COUNT, files_s).text_color(dim))
                                .child(num_cell(COL_COUNT, dirs_s).text_color(dim))
                        })
                        .when(cols & COL_SHOW_MODIFIED != 0, |d| {
                            d.child(
                                div()
                                    .w(px(COL_MODIFIED))
                                    .flex_none()
                                    .px_2()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_color(dim)
                                    .child(format::datetime(node.mtime)),
                            )
                        });
                    items.push(row);
                }
                items
            }),
        )
        .track_scroll(self.tree_scroll.clone())
        .flex_1()
        .min_h_0();

        let body = if self.filter_active && self.visible.is_empty() {
            placeholder("no matches")
        } else {
            ul.into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(col_header)
            .child(body)
            .into_any_element()
    }

    /// Streaming view while a scan runs: the root's children with sizes
    /// that grow as worker threads report in.
    fn render_live(&self) -> AnyElement {
        let max = self.live.first().map(|e| e.size).unwrap_or(0).max(1);
        let total: u64 = self.live.iter().map(|e| e.size).sum();
        let mut list = div().flex_1().min_h_0().flex().flex_col().overflow_hidden();
        for (ix, e) in self.live.iter().enumerate().take(200) {
            let dim = rgb(theme::TEXT_DIM);
            list = list.child(
                row_base(("live-row", ix), ix, false)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pl(px(18.))
                            .truncate()
                            .text_color(rgb(if e.is_dir { theme::TEXT_HI } else { theme::TEXT }))
                            .child(if e.is_dir { format!("{}/", e.name) } else { e.name.clone() }),
                    )
                    .child(
                        div()
                            .w(px(COL_PCT))
                            .flex_none()
                            .px_2()
                            .flex()
                            .items_center()
                            .child(bar(e.size as f64 / max as f64, theme::BAR)),
                    )
                    .child(num_cell(COL_SIZE, format::human_bytes(e.size)).text_color(rgb(theme::TEXT_HI)))
                    .child(num_cell(COL_COUNT, format::format_number(e.files)).text_color(dim)),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                header_bar()
                    .child(div().flex_1().px_2().child(format!(
                        "SCANNING  {} entries  {} so far",
                        format::format_number(self.live_scanned),
                        format::human_bytes(total)
                    )))
                    .child(num_cell(COL_PCT, ""))
                    .child(num_cell(COL_SIZE, "SIZE"))
                    .child(num_cell(COL_COUNT, "FILES")),
            )
            .child(if self.live.is_empty() { placeholder("reading…") } else { list.into_any_element() })
            .into_any_element()
    }

    fn render_top_files(&self, scan: &Arc<ScanResult>, cx: &mut Context<Self>) -> AnyElement {
        let total = scan.total_size;
        let show_files = self.cols & COL_SHOW_COUNTS != 0;
        let header = header_bar()
            .child(div().flex_1().min_w_0().px_2().child("PATH"))
            .child(num_cell(COL_SIZE, "SIZE"))
            .child(num_cell(64., "%"))
            .when(show_files, |d| d.child(num_cell(COL_COUNT, "FILES")));

        // Virtualized: only visible rows are built, not all 1000 eagerly.
        let ul = uniform_list(
            "top-files-list",
            self.top_files_cache.len(),
            cx.processor(move |this, range, _window, cx| {
                let mut items = Vec::new();
                let Some(s) = this.scan.clone() else {
                    return items;
                };
                for ix in range {
                    let Some(&id) = this.top_files_cache.get(ix) else {
                        continue;
                    };
                    let Some(n): Option<&Node> = s.nodes.get(id) else {
                        continue;
                    };
                    let dim = rgb(theme::TEXT_DIM);
                    let row = row_base(("top-file-row", ix), ix, this.selected == Some(id))
                        .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| {
                            this.selected = Some(id);
                            if e.click_count() >= 2 {
                                this.open_selected();
                            }
                            cx.notify();
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, e: &gpui::MouseDownEvent, _w, cx| {
                                this.selected = Some(id);
                                this.menu = Some(Menu::Context { pos: e.position, id });
                                cx.notify();
                            }),
                        )
                        .child(div().flex_1().min_w_0().px_2().truncate().child(s.display_path(id)))
                        .child(num_cell(COL_SIZE, format::human_bytes(n.size)).text_color(rgb(theme::TEXT_HI)))
                        .child(num_cell(64., format::percent(n.size, total)).text_color(dim))
                        .when(this.cols & COL_SHOW_COUNTS != 0, |d| {
                            d.child(num_cell(COL_COUNT, format::format_number(n.file_count)).text_color(dim))
                        });
                    items.push(row);
                }
                items
            }),
        )
        .track_scroll(self.top_scroll.clone())
        .flex_1()
        .min_h_0();

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(ul)
            .into_any_element()
    }

    /// File Types: every extension ranked by bytes, coloured like the
    /// treemap. Selecting one dims all other files in the treemap.
    fn render_types(&self, scan: &Arc<ScanResult>, cx: &mut Context<Self>) -> AnyElement {
        let Some(types) = self.file_types() else {
            return placeholder("computing file types…");
        };
        let total = scan.total_size;
        let header = header_bar()
            .child(div().w(px(26.)).flex_none())
            .child(div().flex_1().min_w_0().px_2().child("EXTENSION"))
            .child(num_cell(COL_SIZE, "SIZE"))
            .child(num_cell(64., "%"))
            .child(num_cell(COL_COUNT, "FILES"));
        let ul = uniform_list(
            "types-list",
            types.ranked.len(),
            cx.processor(move |this, range, _window, cx| {
                let mut items = Vec::new();
                let Some(types): Option<Arc<ExtColors>> = this.file_types() else {
                    return items;
                };
                for ix in range {
                    let Some(t): Option<&treemap::ExtStat> = types.ranked.get(ix) else {
                        continue;
                    };
                    let dim = rgb(theme::TEXT_DIM);
                    let label = if t.ext.is_empty() { "(none)".to_string() } else { format!(".{}", t.ext) };
                    let row = row_base(("type-row", ix), ix, this.types_sel == Some(ix))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_type(Some(ix));
                            cx.notify();
                        }))
                        .child(
                            div()
                                .w(px(26.))
                                .flex_none()
                                .flex()
                                .justify_center()
                                .child(div().w(px(8.)).h(px(8.)).bg(rgb(t.color))),
                        )
                        .child(div().flex_1().min_w_0().px_2().truncate().child(label))
                        .child(num_cell(COL_SIZE, format::human_bytes(t.size)).text_color(rgb(theme::TEXT_HI)))
                        .child(num_cell(64., format::percent(t.size, total)).text_color(dim))
                        .child(num_cell(COL_COUNT, format::format_number(t.count)).text_color(dim));
                    items.push(row);
                }
                items
            }),
        )
        .track_scroll(self.types_scroll.clone())
        .flex_1()
        .min_h_0();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(ul)
            .into_any_element()
    }

    fn render_treemap(&self, cx: &mut Context<Self>) -> AnyElement {
        let scan = self.scan.clone();
        let border = rgb(theme::BORDER);
        // Info line: the hovered tile (WizTree's status text), else the selection.
        let info = scan
            .as_ref()
            .and_then(|s| {
                let id = self.hovered.or(self.selected)?;
                let n = s.nodes.get(id)?;
                Some(format!("{}   {}", self.path_of(id), format::human_bytes(n.size)))
            })
            .unwrap_or_default();

        // Breadcrumbs from the scan root down to the zoomed folder.
        let mut crumbs = div().flex_1().min_w_0().flex().items_center().overflow_hidden();
        let tm_root = self.treemap_root();
        if let (Some(s), Some(z)) = (&scan, tm_root) {
            let mut chain = vec![z];
            let mut cur = s.nodes[z].parent();
            while let Some(p) = cur {
                chain.push(p);
                cur = s.nodes[p].parent();
            }
            chain.reverse();
            let last = chain.len() - 1;
            for (i, id) in chain.into_iter().enumerate() {
                let label = if id == s.root { s.root_path.display().to_string() } else { s.name(id).to_string() };
                if i > 0 {
                    crumbs = crumbs.child(div().flex_none().px_1().text_color(rgb(theme::TEXT_DIM)).child("›"));
                }
                crumbs = crumbs.child(
                    div()
                        .id(("crumb", id))
                        .flex_none()
                        .px_1()
                        .whitespace_nowrap()
                        .text_color(rgb(if i == last { theme::TEXT_HI } else { theme::TEXT_DIM }))
                        .when(i != last, |d| {
                            d.cursor_pointer()
                                .hover(|st| st.text_color(rgb(theme::TEXT_HI)).bg(rgb(theme::HOVER)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.zoom_to(id);
                                    cx.notify();
                                }))
                        })
                        .child(label),
                );
            }
        }
        let bar_top = div()
            .w_full()
            .h(px(24.))
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_1()
            .bg(rgb(theme::PANEL))
            .border_b_1()
            .border_color(border)
            .child(
                btn("zoom-out-btn", "↑")
                    .when(self.zoom.is_none(), |d| d.text_color(rgb(theme::BORDER_HI)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.zoom_out();
                        cx.notify();
                    })),
            )
            .child(crumbs)
            .when_some(self.highlight_ext.clone(), |d, ext| {
                let label = if ext.is_empty() { "type: (none)".to_string() } else { format!("type: .{}", ext) };
                d.child(btn("clear-highlight-btn", format!("{}  ×", label)).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.select_type(None);
                        cx.notify();
                    },
                )))
            });

        let version = self.scan_version;
        let highlight = self.highlight_ext.clone();
        let selected_id = self.selected;
        let cache = self.treemap.clone();
        let canvas_el = canvas(
            move |bounds, _w, _a| bounds,
            move |bounds, _prep, window, cx| {
                let mut c = cache.borrow_mut();
                let scale = window.scale_factor();
                c.bounds = Some(bounds);
                c.scale = scale;
                c.want_version = version;
                let (Some(s), Some(root)) = (&scan, tm_root) else {
                    // No scan: release the old bitmap.
                    if let Some(old) = c.map.take() {
                        let _ = window.drop_image(old.image);
                    }
                    c.key = None;
                    return;
                };
                let w = (f32::from(bounds.size.width) * scale).round() as u32;
                let h = (f32::from(bounds.size.height) * scale).round() as u32;
                if w == 0 || h == 0 {
                    return;
                }
                let key: MapKey = (version, root, highlight.clone(), w, h);
                if c.key.as_ref() != Some(&key) && c.pending.is_none() {
                    // Build off the UI thread; paint keeps the stale bitmap.
                    c.pending = Some(key.clone());
                    let colors = c.ext_colors();
                    // Same scan, root and size: only the colours changed, so
                    // the layout is reused and just the pixels are redrawn.
                    let reuse = c
                        .key
                        .as_ref()
                        .filter(|k| (k.0, k.1, k.3, k.4) == (key.0, key.1, key.3, key.4))
                        .and_then(|_| c.map.as_ref())
                        .map(|m| m.layout.clone());
                    let (s, cache, hl) = (s.clone(), cache.clone(), highlight.clone());
                    cx.spawn(async move |cx| {
                        let (colors, map) = cx
                            .background_executor()
                            .spawn(async move {
                                let colors = colors.unwrap_or_else(|| Arc::new(ExtColors::build(&s)));
                                let map = treemap::build(&s, &colors, root, hl.as_deref(), w, h, reuse);
                                (colors, map)
                            })
                            .await;
                        let old = {
                            let mut c = cache.borrow_mut();
                            c.colors = Some((key.0, colors));
                            c.pending = None;
                            c.key = Some(key);
                            c.map.replace(map)
                        };
                        let _ = cx.update(|cx| {
                            if let Some(old) = old {
                                cx.drop_image(old.image, None);
                            }
                            cx.refresh_windows();
                        });
                    })
                    .detach();
                }
                let Some(map) = &c.map else {
                    return;
                };
                let _ = window.paint_image(bounds, Corners::default(), map.image.clone(), 0, false);
                let Some(r) = selected_id.and_then(|id| c.current_map()?.layout.rect_of(id)) else {
                    return;
                };
                // Map pixels -> window; the map may be a stale size.
                let sx = f32::from(bounds.size.width) / map.layout.width as f32;
                let sy = f32::from(bounds.size.height) / map.layout.height as f32;
                let sel = Bounds {
                    origin: point(bounds.origin.x + px(r.x * sx), bounds.origin.y + px(r.y * sy)),
                    size: size(px((r.w * sx).max(3.)), px((r.h * sy).max(3.))),
                };
                window.paint_quad(
                    fill(sel, gpui::transparent_black())
                        .border_widths(px(1.))
                        .border_color(rgb(theme::TEXT_HI)),
                );
            },
        )
        .size_full();

        let cache_click = self.treemap.clone();
        let cache_right = self.treemap.clone();
        let cache_move = self.treemap.clone();
        let mut area = div()
            .id("treemap-click-area")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(rgb(theme::BG))
            .child(canvas_el)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &gpui::MouseDownEvent, _w, cx| {
                    let Some(id) = cache_click.borrow().hit(e.position) else {
                        return;
                    };
                    // Double-click zooms into the tile's folder.
                    if e.click_count >= 2 {
                        this.zoom_to(id);
                    } else {
                        this.reveal(id);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &gpui::MouseDownEvent, _w, cx| {
                    if let Some(id) = cache_right.borrow().hit(e.position) {
                        this.selected = Some(id);
                        this.menu = Some(Menu::Context { pos: e.position, id });
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(move |this, e: &gpui::MouseMoveEvent, _w, cx| {
                let hit = cache_move.borrow().hit(e.position);
                if hit != this.hovered {
                    this.hovered = hit;
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _w, cx| {
                if !*hovered && this.hovered.is_some() {
                    this.hovered = None;
                    cx.notify();
                }
            }));
        if self.scan.is_none() {
            area = area.child(
                div()
                    .absolute()
                    .inset_0()
                    .child(placeholder(if self.scanning { "treemap appears when the scan finishes" } else { "open a folder to scan  (ctrl+o)" })),
            );
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar_top)
            .child(area)
            .child(
                div()
                    .w_full()
                    .h(px(22.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_2()
                    .truncate()
                    .bg(rgb(theme::PANEL))
                    .border_t_1()
                    .border_color(border)
                    .text_color(rgb(theme::TEXT_DIM))
                    .child(info),
            )
            .into_any_element()
    }

    fn render_search(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.search_focus.is_focused(window);
        let empty = self.filter.is_empty();
        div()
            .id("search-box")
            .track_focus(&self.search_focus)
            .on_key_down(cx.listener(Self::on_search_key))
            .w(px(240.))
            .flex_shrink()
            .min_w(px(110.))
            .h(px(22.))
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .border_1()
            .border_color(rgb(if focused { theme::SEL_EDGE } else { theme::BORDER }))
            .bg(rgb(theme::BG))
            .cursor(CursorStyle::IBeam)
            .child(div().flex_none().text_color(rgb(theme::TEXT_DIM)).child("/"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .when(empty && !focused, |d| {
                        d.text_color(rgb(theme::TEXT_DIM)).child("search  *.log >10M")
                    })
                    .when(!empty, |d| d.text_color(rgb(theme::TEXT_HI)).child(self.filter.clone()))
                    .when(focused, |d| d.child(div().w(px(1.)).h(px(14.)).bg(rgb(theme::TEXT_HI)))),
            )
            .when(!empty, |d| {
                d.child(
                    div()
                        .id("search-clear")
                        .flex_none()
                        .px_1()
                        .text_color(rgb(theme::TEXT_DIM))
                        .cursor_pointer()
                        .hover(|s| s.text_color(rgb(theme::TEXT_HI)))
                        .child("×")
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.set_filter(String::new());
                            cx.notify();
                        })),
                )
            })
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_ref()?;
        let (pos, items): (Point<Pixels>, Vec<(String, Option<MenuAction>, bool)>) = match menu {
            Menu::Context { pos, id } => {
                let id = *id;
                let is_dir = self.scan.as_ref()?.nodes.get(id)?.is_dir;
                let mut v = vec![
                    ("Open".to_string(), Some(MenuAction::Open(id)), false),
                    (
                        if is_dir { "Open folder".into() } else { "Open containing folder".into() },
                        Some(MenuAction::OpenFolder(id)),
                        false,
                    ),
                    ("Copy path".into(), Some(MenuAction::CopyPath(id)), false),
                    ("Show in tree".into(), Some(MenuAction::Reveal(id)), false),
                    (
                        if is_dir { "Zoom treemap here".into() } else { "Zoom treemap to folder".into() },
                        Some(MenuAction::Zoom(id)),
                        false,
                    ),
                ];
                v.push(("Delete…".into(), Some(MenuAction::Delete(id)), true));
                (*pos, v)
            }
            Menu::Recent { pos } => {
                let mut v: Vec<_> = self
                    .settings
                    .recent
                    .iter()
                    .map(|r| (r.clone(), Some(MenuAction::Scan(r.clone())), false))
                    .collect();
                if v.is_empty() {
                    v.push(("no recent folders".into(), None, false));
                } else {
                    v.push(("Clear recent".into(), Some(MenuAction::ClearRecent), true));
                }
                (*pos, v)
            }
        };
        let mut list = div()
            .absolute()
            .left(pos.x)
            .top(pos.y)
            .min_w(px(200.))
            .max_w(px(520.))
            .py_1()
            .flex()
            .flex_col()
            .bg(rgb(theme::PANEL))
            .border_1()
            .border_color(rgb(theme::BORDER_HI))
            .shadow_md()
            // Clicks inside the menu must not reach the dismiss backdrop.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        for (i, (label, action, danger)) in items.into_iter().enumerate() {
            let enabled = action.is_some();
            list = list.child(
                div()
                    .id(("menu-item", i))
                    .h(px(22.))
                    .px_3()
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .when(danger, |d| d.mt_1().border_t_1().border_color(rgb(theme::BORDER)))
                    .text_color(rgb(match (enabled, danger) {
                        (false, _) => theme::TEXT_DIM,
                        (true, true) => theme::DANGER,
                        (true, false) => theme::TEXT,
                    }))
                    .when(enabled, |d| {
                        d.cursor_pointer().hover(|s| s.bg(rgb(theme::SEL)).text_color(rgb(theme::TEXT_HI)))
                    })
                    .child(label)
                    .when_some(action, |d, a| {
                        d.on_click(cx.listener(move |this, _, _, cx| this.run_menu_action(a.clone(), cx)))
                    }),
            );
        }
        Some(
            div()
                .id("menu-backdrop")
                .absolute()
                .inset_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }),
                )
                .child(list)
                .into_any_element(),
        )
    }

    fn render_warnings(&self, scan: &ScanResult, cx: &mut Context<Self>) -> AnyElement {
        let border = rgb(theme::BORDER);
        let mut body = div()
            .id("warnings-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .flex()
            .flex_col();
        for w in &scan.warnings {
            body = body.child(div().py(px(2.)).text_color(rgb(theme::TEXT)).child(w.clone()));
        }
        if scan.warnings_truncated > 0 {
            body = body.child(
                div()
                    .pt_2()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child(format!("… and {} more not shown", scan.warnings_truncated)),
            );
        }
        div()
            .absolute()
            .inset_0()
            .bg(rgba(0x000000b0))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(720.))
                    .max_w(gpui::relative(0.92))
                    .h(gpui::relative(0.7))
                    .bg(rgb(theme::PANEL))
                    .border_1()
                    .border_color(rgb(theme::BORDER_HI))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(border)
                            .child(
                                div()
                                    .flex_1()
                                    .text_color(rgb(theme::TEXT_HI))
                                    .child(format!(
                                        "{} paths could not be read",
                                        scan.warnings.len() + scan.warnings_truncated
                                    )),
                            )
                            .child(btn("warnings-close", "Close").on_click(cx.listener(|this, _, _, cx| {
                                this.show_warnings = false;
                                cx.notify();
                            }))),
                    )
                    .child(body),
            )
            .into_any_element()
    }
}

impl Render for Blockwise {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.visible_dirty {
            self.rebuild_visible(cx);
            self.visible_dirty = false;
        }
        if self.top_files_dirty && self.view == ViewKind::TopFiles {
            self.ensure_top_files();
        }
        // Keep keyboard focus inside the app (startup, after dialogs).
        if window.focused(cx).is_none() {
            window.focus(&self.focus);
        }
        let vw = f32::from(window.viewport_size().width);
        self.cols = columns_for_width(vw);
        let narrow = vw < 760.;

        // Modal delete target name (single path reconstruction, not per frame per row).
        let delete_name = self.pending_delete.and_then(|id| {
            let s = self.scan.as_ref()?;
            s.nodes.get(id).map(|_| self.path_of(id))
        });

        let border = rgb(theme::BORDER);
        let sep = || div().w(px(1.)).h(px(16.)).flex_none().bg(rgb(theme::BORDER));

        let mut root = div()
            .id("root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT))
            .font_family(self.font.clone())
            .text_size(px(12.))
            .line_height(px(16.))
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _w, cx| {
                if !this.dragging_split {
                    return;
                }
                if e.pressed_button != Some(MouseButton::Left) {
                    this.dragging_split = false;
                    this.settings.save();
                    return;
                }
                if let Some(b) = this.content_bounds.get() {
                    let f = f32::from(e.position.y - b.origin.y) / f32::from(b.size.height);
                    this.settings.split = f.clamp(0.15, 0.85);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    if this.dragging_split {
                        this.dragging_split = false;
                        this.settings.save();
                    }
                }),
            );

        // Toolbar
        let mut toolbar = div()
            .w_full()
            .h(px(34.))
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_2()
            .bg(rgb(theme::PANEL))
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(theme::TEXT_HI))
                    .child(if narrow { "bw" } else { "blockwise" }),
            )
            .child(sep())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .child(btn("choose-folder-btn", "Open…").on_click(cx.listener(|this, _, _, cx| {
                        this.pick_and_scan(cx);
                    })))
                    .child(
                        btn("recent-btn", "▾")
                            .border_l_0()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    let pos = point(e.position.x - px(60.), px(30.));
                                    this.menu = Some(Menu::Recent { pos });
                                    cx.notify();
                                }),
                            ),
                    ),
            )
            .child(btn("rescan-btn", "Rescan").on_click(cx.listener(|this, _, _, cx| this.rescan(cx))));
        if self.scanning {
            toolbar = toolbar.child(danger_btn("cancel-scan-btn", "Cancel").on_click(cx.listener(
                |this, _, _, _cx| {
                    if let Some(c) = &this.cancel {
                        c.store(true, Ordering::Relaxed);
                    }
                },
            )));
        }
        toolbar = toolbar
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_1()
                    .truncate()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child(self.root_path.clone().unwrap_or_else(|| "no folder selected".into())),
            )
            .child(self.render_search(window, cx))
            .child({
                // Segmented upper-pane toggle (treemap is always shown below).
                let mut tabs = div().flex().flex_none().border_1().border_color(border);
                for (kind, label, short, tab_id) in [
                    (ViewKind::Tree, "Tree", "Tree", 0u64),
                    (ViewKind::TopFiles, "Top Files", "Top", 1u64),
                    (ViewKind::Types, "Types", "Ext", 2u64),
                ] {
                    let active = self.view == kind;
                    tabs = tabs.child(
                        div()
                            .id(("view-tab", tab_id))
                            .h(px(20.))
                            .px_2()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .when(tab_id > 0, |d| d.border_l_1().border_color(border))
                            .bg(rgb(if active { theme::PRESS } else { theme::HEADER }))
                            .text_color(rgb(if active { theme::TEXT_HI } else { theme::TEXT_DIM }))
                            .when(!active, |d| d.hover(|s| s.bg(rgb(theme::HOVER)).text_color(rgb(theme::TEXT))))
                            .child(if narrow { short } else { label })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_view(kind);
                                cx.notify();
                            })),
                    );
                }
                tabs
            })
            .when(!narrow, |d| {
                d.child(btn("export-csv-btn", "Export").on_click(cx.listener(|this, _, _, cx| {
                    this.export_csv();
                    cx.notify();
                })))
            });
        root = root.child(toolbar);

        // Progress / error strips
        if let Some(p) = &self.progress {
            root = root.child(
                div()
                    .w_full()
                    .flex_none()
                    .px_2()
                    .py(px(3.))
                    .truncate()
                    .bg(rgb(theme::PANEL))
                    .border_b_1()
                    .border_color(border)
                    .text_color(rgb(theme::TEXT_DIM))
                    .child(p.clone()),
            );
        }
        if let Some(err) = &self.error {
            root = root.child(
                div()
                    .w_full()
                    .flex_none()
                    .px_2()
                    .py(px(3.))
                    .truncate()
                    .bg(rgb(theme::DANGER_BG))
                    .border_b_1()
                    .border_color(border)
                    .text_color(rgb(theme::DANGER))
                    .child(err.clone()),
            );
        }

        // Main content: list pane above, treemap below, draggable divider.
        let scan_ref = self.scan.clone();
        let content_upper: AnyElement = match (&scan_ref, self.view) {
            (None, _) if self.scanning => self.render_live(),
            (None, _) => placeholder("open a folder to scan  (ctrl+o)"),
            (Some(_), ViewKind::Tree) => self.render_tree(cx),
            (Some(s), ViewKind::TopFiles) => self.render_top_files(s, cx),
            (Some(s), ViewKind::Types) => self.render_types(s, cx),
        };
        let lower = self.render_treemap(cx);
        let bounds_cell = self.content_bounds.clone();
        let dragging = self.dragging_split;
        let content = div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            .child(
                canvas(move |b, _, _| bounds_cell.set(Some(b)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(
                div()
                    .w_full()
                    .h(gpui::relative(self.settings.split))
                    .min_h(px(80.))
                    .flex_none()
                    .child(content_upper),
            )
            .child(
                div()
                    .id("split-handle")
                    .w_full()
                    .h(px(5.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .cursor(CursorStyle::ResizeRow)
                    .bg(rgb(if dragging { theme::PRESS } else { theme::PANEL }))
                    .hover(|s| s.bg(rgb(theme::HOVER)))
                    .border_t_1()
                    .border_b_1()
                    .border_color(border)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dragging_split = true;
                            cx.notify();
                        }),
                    ),
            )
            .child(div().flex_1().min_h(px(80.)).child(lower));
        root = root.child(content);

        // Footer / status bar
        let mut footer = div()
            .w_full()
            .h(px(28.))
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_2()
            .bg(rgb(theme::PANEL))
            .border_t_1()
            .border_color(border);
        let selected_node = self
            .selected
            .and_then(|id| scan_ref.as_ref().and_then(|s| s.nodes.get(id).map(|n| (id, s, n))));
        if let Some((id, _, n)) = selected_node {
            footer = footer
                .child(div().flex_1().min_w_0().truncate().child(self.path_of(id)))
                .child(div().flex_none().text_color(rgb(theme::TEXT_HI)).child(format::human_bytes(n.size)))
                .child(sep())
                .child(btn("open-selected-btn", "Open").on_click(cx.listener(|this, _, _, _| {
                    this.open_selected();
                })))
                .child(
                    danger_btn("delete-selected-btn", if self.deleting { "Deleting…" } else { "Delete" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(sel) = this.selected {
                                this.request_delete(sel, cx);
                            }
                        })),
                );
        } else {
            let hint = if narrow {
                "no selection"
            } else {
                "↑↓ move  ←→ fold  enter open  z zoom  del delete  ctrl+f search"
            };
            footer = footer.child(div().flex_1().min_w_0().truncate().text_color(rgb(theme::TEXT_DIM)).child(hint));
        }
        if let Some(st) = &self.status {
            footer = footer
                .child(sep())
                .child(div().flex_none().max_w(px(320.)).truncate().text_color(rgb(theme::OK)).child(st.clone()));
        }
        if let Some(scan) = scan_ref.as_ref() {
            let w = scan.warnings.len() + scan.warnings_truncated;
            if w > 0 {
                footer = footer.child(sep()).child(
                    div()
                        .id("warnings-btn")
                        .flex_none()
                        .px_1()
                        .text_color(rgb(theme::WARN))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::HOVER)))
                        .child(format!("{} warnings", w))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_warnings = true;
                            cx.notify();
                        })),
                );
            }
        }
        root = root.child(footer);

        if let (true, Some(scan)) = (self.show_warnings, scan_ref.as_ref()) {
            root = root.child(self.render_warnings(scan, cx));
        }
        if let Some(menu) = self.render_menu(cx) {
            root = root.child(menu);
        }

        // Delete confirmation modal
        if let Some(name) = delete_name {
            let modal = div()
                .absolute()
                .inset_0()
                .bg(rgba(0x000000b0))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(460.))
                        .max_w(gpui::relative(0.9))
                        .bg(rgb(theme::PANEL))
                        .border_1()
                        .border_color(rgb(theme::BORDER_HI))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(border)
                                .text_color(rgb(theme::TEXT_HI))
                                .child("Move to trash?"),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_3()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(div().child(name))
                                .child(
                                    div()
                                        .text_color(rgb(theme::TEXT_DIM))
                                        .child("Tries trash first, then permanent delete.  enter / esc"),
                                ),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .flex()
                                .gap_2()
                                .justify_end()
                                .border_t_1()
                                .border_color(border)
                                .child(btn("modal-cancel-btn", "Cancel").on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.pending_delete = None;
                                        cx.notify();
                                    },
                                )))
                                .child(danger_btn("modal-delete-btn", "Delete").on_click(cx.listener(
                                    |this, _, _, cx| {
                                        if let Some(id) = this.pending_delete {
                                            this.perform_delete(id, cx);
                                        }
                                    },
                                ))),
                        ),
                );
            root = root.child(modal);
        }

        root
    }
}
