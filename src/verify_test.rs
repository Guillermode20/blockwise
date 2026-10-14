use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crate::scanner::{start_scan, ScanEvent};

fn drain(root: std::path::PathBuf) -> crate::model::ScanResult {
    let cancel = Arc::new(AtomicBool::new(false));
    let rx = start_scan(root, cancel);
    loop {
        match rx.recv_timeout(Duration::from_secs(300)).expect("scan hung") {
            ScanEvent::Done(r) => return r,
            ScanEvent::Failed(m) => panic!("scan failed: {}", m),
            ScanEvent::Cancelled => panic!("cancelled unexpectedly"),
            ScanEvent::Progress { .. } => continue,
        }
    }
}

#[test]
fn verify_aggregates_paths_and_order() {
    let base = std::env::temp_dir().join(format!("blockwise-verify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("sub/nested")).unwrap();
    std::fs::write(base.join("a.bin"), vec![1u8; 100]).unwrap();
    std::fs::write(base.join("sub/b.bin"), vec![2u8; 200]).unwrap();
    std::fs::write(base.join("sub/nested/c.bin"), vec![3u8; 300]).unwrap();
    std::fs::write(base.join("sub/nested/d.bin"), vec![4u8; 50]).unwrap();

    let r = drain(base.clone());
    assert_eq!(r.total_size, 650, "aggregate size");
    assert_eq!(r.nodes[r.root].file_count, 4, "file count");
    assert_eq!(r.nodes[r.root].dir_count, 2, "dir count (sub + nested)");
    for (id, n) in r.nodes.iter().enumerate() {
        let p = r.full_path(id);
        assert!(p.starts_with(&base), "path under root: {}", p.display());
        assert!(!r.name(id).contains('/'), "name only, no prefix: {}", r.name(id));
        if id != r.root {
            let pid = n.parent().unwrap();
            assert!(pid < id, "stack order: parent id < child id");
            assert!(r.children_of(pid).contains(&id), "child in parent's range");
        }
        for c in n.children() {
            assert_eq!(r.nodes[c].parent(), Some(id), "children range is exact");
        }
    }
    // spot-check a nested path
    let nested_file = (0..r.nodes.len()).find(|&i| r.name(i) == "c.bin").unwrap();
    assert_eq!(r.full_path(nested_file), base.join("sub/nested/c.bin"));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn verify_single_file_root() {
    let base = std::env::temp_dir().join(format!("blockwise-verify-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let f = base.join("only.bin");
    std::fs::write(&f, vec![9u8; 123]).unwrap();
    let r = drain(f.clone());
    assert_eq!(r.total_size, 123);
    assert_eq!(r.nodes.len(), 1);
    assert_eq!(r.full_path(r.root), f);
    let _ = std::fs::remove_dir_all(&base);
}

fn rss_mb() -> f64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: f64 = statm.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    pages * 4096.0 / (1024.0 * 1024.0)
}

/// Perf harness: `BENCH_ROOT=/usr cargo test --release bench -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_pipeline() {
    use crate::filter::Filter;
    use crate::view_model::{self, SortKey, SortSpec};
    use std::collections::HashSet;
    use std::time::Instant;
    let root = std::env::var("BENCH_ROOT").unwrap_or_else(|_| "/usr".into());
    let rss0 = rss_mb();
    let t = Instant::now();
    let r = drain(root.into());
    println!("scan: {} nodes in {:?}", r.nodes.len(), t.elapsed());
    println!(
        "memory: node {} B, rss +{:.0} MB",
        std::mem::size_of::<crate::model::Node>(),
        rss_mb() - rss0
    );
    let t = Instant::now();
    let colors = crate::treemap::ExtColors::build(&r);
    println!("ext colors: {:?}", t.elapsed());
    let t = Instant::now();
    let map = crate::treemap::build(&r, &colors, r.root, None, 2400, 1000, None);
    println!("treemap build 2400x1000: {:?} ({} rects)", t.elapsed(), map.layout.rects.len());
    let t = Instant::now();
    let _ = crate::treemap::build(&r, &colors, r.root, Some("so"), 2400, 1000, Some(map.layout.clone()));
    println!("treemap highlight rebuild: {:?}", t.elapsed());
    let t = Instant::now();
    let mut hits = 0usize;
    for i in 0..1000 {
        let x = (i * 37 % 2400) as f32;
        let y = (i * 53 % 1000) as f32;
        hits += map.layout.hit_test(x, y).unwrap_or(0) & 1;
    }
    println!("1000 hit tests: {:?} ({hits})", t.elapsed());

    let size_desc = SortSpec { key: SortKey::Size, ascending: false };
    for q in ["a", "*.so", ">10M", "zzzqqq"] {
        let f = Filter::parse(q).unwrap();
        let t = Instant::now();
        let n = view_model::search(&r, &f, size_desc, &AtomicBool::new(false)).unwrap().len();
        println!("search {:>7}: {:?} ({} hits)", q, t.elapsed(), n);
    }

    // Tree with every folder up to depth 3 open (a busy but realistic view).
    let mut expanded = HashSet::new();
    let mut stack = vec![(r.root, 0)];
    while let Some((id, d)) = stack.pop() {
        if r.nodes[id].is_dir && d <= 3 {
            expanded.insert(id);
            for c in r.children_of(id) {
                stack.push((c, d + 1));
            }
        }
    }
    let mut vis = Vec::new();
    let mut state = view_model::TreeState::default();
    let t = Instant::now();
    view_model::build_visible(&r, 0, &expanded, size_desc, &mut state, &mut vis);
    println!("tree rows ({} open dirs): {:?} ({} rows)", expanded.len(), t.elapsed(), vis.len());
    let t = Instant::now();
    view_model::build_visible(&r, 0, &expanded, size_desc, &mut state, &mut vis);
    println!("tree rows again (expand/collapse): {:?}", t.elapsed());
    let t = Instant::now();
    let name_asc = SortSpec { key: SortKey::Name, ascending: true };
    view_model::build_visible(&r, 0, &expanded, name_asc, &mut state, &mut vis);
    println!("tree rows, sort by name: {:?}", t.elapsed());

    let all = view_model::search(&r, &Filter::parse("*").unwrap(), size_desc, &AtomicBool::new(false)).unwrap();
    let target = *all.last().unwrap();
    let t = Instant::now();
    let pos = all.iter().position(|&v| v == target);
    println!("selection lookup in {} rows: {:?} ({:?})", all.len(), t.elapsed(), pos.is_some());
}
