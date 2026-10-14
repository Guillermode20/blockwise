# AGENTS.md

Single-crate Rust GPUI desktop app **blockwise**: WizTree-style disk usage visualizer (tree + treemap). No workspace.

## Commands

- `cargo run -- [PATH]` — launch GUI; `PATH` optional, or set `BLOCKWISE_ROOT` env (`src/main.rs:15`).
- `cargo test` — headless-safe (scanner + squarify only, uses temp dirs under `$TMPDIR/blockwise-verify-*`); 5 tests (+1 ignored bench), runs in <1s.
- `cargo clippy -- -D warnings` before finishing. `BENCH_ROOT=/usr cargo test --release bench -- --ignored --nocapture` times scan, memory, ext colors, treemap build/highlight, hit tests, search, tree rows. Run on /usr and $HOME before/after perf changes.

GUI needs a display; tests don't. `cargo build` already verified in `target/`.

## Architecture (`src/`)

- `main.rs` — entrypoint, opens 1200x800 window, optional initial scan.
- `scanner.rs` — `start_scan(root, cancel) -> flume::Receiver<ScanEvent>`; `Progress` also carries `top: Vec<LiveEntry>` (root children with running sizes, throttled to every 100 ms) for the live view. Spawns a thread that walks directories in parallel on a rayon pool (`read_tree`, lstat only, symlinks not followed), into per-directory `DirBlock`s (names packed per dir), then flattens so each directory's entries get one contiguous id range. Mount points of kernel pseudo-filesystems (procfs, sysfs, cgroup, ...; `is_virtual_fs`, statfs magic) are not descended and are reported as warnings. Then sends `Done`/`Cancelled`/`Failed`. Warnings capped at 100 (`MAX_WARNINGS`).
- `filter.rs` — search syntax (`>10M`, `<1G`, `*.log` globs, substrings; all terms ANDed). Non-empty query turns the tree into a flat, sorted match list (`filter_active`), computed on the background executor (`rebuild_visible` → `view_model::search`), cancelled via `search_cancel` when superseded; stale rows stay visible until results land.
- `settings.rs` — recent folders, split fraction, window size in `<config_dir>/blockwise/settings.txt` (saved on scan start, split drag end, window close).
- `model.rs` — compact `Node` (56 B): no name/id fields — names live packed in `ScanResult::names` (use `scan.name(id)`), children are the contiguous id range `node.children()`, `parent()` is a u32 sentinel, counts are u32, `mtime` is i64 secs (`NO_MTIME`). Full paths rebuilt via `full_path(id)` — O(depth); app memoizes hot ones in `path_of`.
- `view_model.rs` — GPUI-free list logic: `cmp_nodes`/`cmp_names` (case-insensitive, no stored lowercase), `build_visible` with `TreeState` cache of sorted children (keyed on sort + scan version), parallel cancellable `search`, `top_files`.
- `app.rs` — `Blockwise` GPUI view: scan state, Tree / Top Files / Types panes, live scan view, treemap pane with zoom breadcrumbs, context + recent menus, warnings panel, draggable split, delete/export/open. All keyboard handling is in `on_key` (root focus) and `on_search_key` (search box focus; unhandled keys bubble to root). Scan result held as `Arc<ScanResult>`; mutated only via `Arc::make_mut` in `apply_delete`.
- `treemap.rs` — squarified layout + WinDirStat-style cushion bitmap rendered to `gpui::RenderImage` (BGRA buffer), rasterized in parallel 16-row bands.
- `format.rs` — `human_bytes` (1024-based), `format_number`, `percent`, `datetime` (via `libc::localtime_r`).
- `verify_test.rs` — scanner integration tests, the executable spec for aggregates/paths.

## Gotchas

- Scanner invariants: parent ids are always < child ids, and each node's children are one contiguous id range. Deleting never removes ids from ranges — every consumer must skip `deleted` nodes. Aggregation iterates ids in reverse instead of sorting by depth — preserve this.
- Stale-scan guard: `app.rs` `scan_gen` counter drops events from superseded scans. New scans cancel the old `AtomicBool` first. Keep this when touching `start_scan`/`handle_event`.
- Treemap cache (`TreemapCache` in `app.rs`): keyed on `MapKey = (scan_version, root, highlight_ext, w, h)`. Builds (`ExtColors` + layout + raster) run on the background executor; `treemap::Layout` (rects, hit grid, leaves) is reused via `Arc` when only the highlight changes. Layout culls children that would get < `MIN_AREA` px² before sorting (`squarify_total` keeps the parent total so proportions stay true), one at a time (`pending`); paint stretches the stale bitmap meanwhile. Replaced images are freed via `cx.drop_image`. Never hit-test a map whose version != `want_version` (stale node ids). Hit-testing uses a 32px leaf grid, falling back to a reverse rect scan for folders; folders are never painted, they only add cushion ridges.
- Delete (`app.rs:perform_delete`): runs off the UI thread, tries `gio trash` → `trash-put` → permanent `remove_file`/`remove_dir_all`. `apply_delete` marks the subtree deleted and subtracts size/allocated/file/dir counts from all ancestors with `saturating_sub`.
- Export CSV asks for a destination via `rfd` save dialog, skips deleted nodes.
- Portability: `allocated()` in `scanner.rs` and `format::datetime` have `cfg(unix)` / fallback versions; `libc` is a unix-only dependency. Only Linux is actually built/tested. Delete still tries `gio`/`trash-put` before permanent removal.
- GPUI list rows use `uniform_list` with ids `"tree-rows"` / `"top-files-list"` and `cx.processor` closures cloning `Arc<ScanResult>` — don't build all rows eagerly; Top Files list is capped at 1000 via partial `select_nth_unstable`.
- UI styling: all colours live in `app.rs` `mod theme` (muted greys; colour only for selection/danger/warn). Buttons via `btn`/`danger_btn`. Font is the first installed family in `FONT_PREFS` (picked in `main.rs`). Tree columns hide by window width via `columns_for_width` → `self.cols` mask, read by the row processor. Treemap palette is desaturated by `treemap::mute`.
