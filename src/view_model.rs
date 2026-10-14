//! Pure list logic behind the GPUI view (sorting, visible tree rows,
//! search, Top Files). No GPUI types, so it can be benchmarked headlessly.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use rayon::prelude::*;

use crate::filter::Filter;
use crate::model::ScanResult;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    /// "% of Parent" sorts the same as Size, but highlights its own header.
    Percent,
    Size,
    Allocated,
    Items,
    Files,
    Folders,
    Modified,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SortSpec {
    pub key: SortKey,
    pub ascending: bool,
}

/// Cap for the Top Files list so sorting stays bounded.
pub const TOP_FILES_LIMIT: usize = 1000;

/// Case-insensitive name order without allocating. Single pass over the
/// bytes; falls back to Unicode lowercasing only at a non-ASCII byte.
pub fn cmp_names(a: &str, b: &str) -> Ordering {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    for (&p, &q) in x.iter().zip(y) {
        if p == q {
            if p < 0x80 {
                continue;
            }
        } else if p < 0x80 && q < 0x80 {
            let (lp, lq) = (p.to_ascii_lowercase(), q.to_ascii_lowercase());
            if lp != lq {
                return lp.cmp(&lq);
            }
            continue;
        }
        return a
            .chars()
            .flat_map(char::to_lowercase)
            .cmp(b.chars().flat_map(char::to_lowercase));
    }
    x.len().cmp(&y.len())
}

pub fn cmp_nodes(scan: &ScanResult, a: usize, b: usize, sort: SortSpec) -> Ordering {
    let (na, nb) = (&scan.nodes[a], &scan.nodes[b]);
    let by_name = || {
        let (sa, sb) = (scan.name(a), scan.name(b));
        cmp_names(sa, sb).then_with(|| sa.cmp(sb))
    };
    let base = match sort.key {
        SortKey::Name => by_name(),
        SortKey::Size | SortKey::Percent => nb.size.cmp(&na.size),
        SortKey::Allocated => nb.allocated.cmp(&na.allocated),
        SortKey::Items => (u64::from(nb.file_count) + u64::from(nb.dir_count))
            .cmp(&(u64::from(na.file_count) + u64::from(na.dir_count))),
        SortKey::Files => nb.file_count.cmp(&na.file_count),
        SortKey::Folders => nb.dir_count.cmp(&na.dir_count),
        SortKey::Modified => nb.mtime.cmp(&na.mtime),
    };
    // Numeric ties fall back to name (only evaluated on a tie).
    let base = if sort.key == SortKey::Name { base } else { base.then_with(by_name) };
    // Numeric keys are written largest-first and Name A-Z, so flip
    // whenever the requested direction disagrees with that default.
    let natural_ascending = sort.key == SortKey::Name;
    if sort.ascending == natural_ascending {
        base
    } else {
        base.reverse()
    }
}

/// Live children of `id`, sorted.
fn children_sorted(scan: &ScanResult, id: usize, sort: SortSpec) -> Vec<u32> {
    let mut v: Vec<u32> = scan
        .children_of(id)
        .filter(|&c| !scan.nodes[c].deleted)
        .map(|c| c as u32)
        .collect();
    if sort.key == SortKey::Name {
        // Lowercase each name once instead of on every comparison; the
        // sort is stable, so names equal ignoring case keep listing order.
        v.sort_by_cached_key(|&c| {
            let n = scan.name(c as usize);
            if n.is_ascii() { n.to_ascii_lowercase() } else { n.to_lowercase() }
        });
        if !sort.ascending {
            v.reverse();
        }
    } else {
        v.sort_by(|&a, &b| cmp_nodes(scan, a as usize, b as usize, sort));
    }
    v
}

/// Caches reused across `build_visible` calls: each open folder's sorted
/// children, so expanding one folder doesn't re-sort every other one.
/// Invalidated when the sort or the scan version changes.
#[derive(Default)]
pub struct TreeState {
    key: Option<(SortSpec, u64)>,
    sorted: HashMap<u32, Vec<u32>>,
}

/// Pre-order rows `(node, depth)` of the tree with `expanded` folders open.
pub fn build_visible(
    scan: &ScanResult,
    version: u64,
    expanded: &HashSet<usize>,
    sort: SortSpec,
    state: &mut TreeState,
    out: &mut Vec<(usize, usize)>,
) {
    out.clear();
    let nodes = &scan.nodes;
    if nodes.is_empty() {
        return;
    }
    if state.key != Some((sort, version)) {
        state.key = Some((sort, version));
        state.sorted.clear();
    }
    // Iterative pre-order traversal (no recursion -> no stack overflow).
    let mut stack: Vec<(usize, usize)> = vec![(scan.root, 0)];
    while let Some((id, depth)) = stack.pop() {
        if nodes[id].deleted {
            continue;
        }
        out.push((id, depth));
        if expanded.contains(&id) {
            let kids = state
                .sorted
                .entry(id as u32)
                .or_insert_with(|| children_sorted(scan, id, sort));
            stack.extend(kids.iter().rev().map(|&c| (c as usize, depth + 1)));
        }
    }
}

/// Every live node (except the root) matching `filter`, sorted. Runs on
/// all cores; each worker reuses one lowercase scratch buffer. Returns
/// `None` if `cancel` was set (a newer query superseded this one).
pub fn search(
    scan: &ScanResult,
    filter: &Filter,
    sort: SortSpec,
    cancel: &AtomicBool,
) -> Option<Vec<usize>> {
    let mut ids: Vec<usize> = scan
        .nodes
        .par_iter()
        .enumerate()
        .with_min_len(4096)
        .map_init(String::new, |buf, (id, n)| {
            if cancel.load(AtomicOrdering::Relaxed) {
                return None;
            }
            (!n.deleted && id != scan.root && filter.matches(scan.name(id), n.size, buf))
                .then_some(id)
        })
        .flatten()
        .collect();
    if cancel.load(AtomicOrdering::Relaxed) {
        return None;
    }
    ids.par_sort_unstable_by(|&a, &b| cmp_nodes(scan, a, b, sort));
    Some(ids)
}

/// The `TOP_FILES_LIMIT` largest live files, largest first.
pub fn top_files(scan: &ScanResult) -> Vec<usize> {
    let mut ids: Vec<usize> = scan
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| !n.is_dir && !n.deleted)
        .map(|(id, _)| id)
        .collect();
    // Partial sort is enough: we only display the top N.
    if ids.len() > TOP_FILES_LIMIT {
        ids.select_nth_unstable_by(TOP_FILES_LIMIT, |&a, &b| {
            scan.nodes[b].size.cmp(&scan.nodes[a].size)
        });
        ids.truncate(TOP_FILES_LIMIT);
    }
    ids.sort_unstable_by(|&a, &b| scan.nodes[b].size.cmp(&scan.nodes[a].size));
    ids
}

#[cfg(test)]
mod tests {
    use super::cmp_names;
    use std::cmp::Ordering::*;

    #[test]
    fn names_case_insensitive() {
        assert_eq!(cmp_names("abc", "ABD"), Less);
        assert_eq!(cmp_names("Zeta", "alpha"), Greater);
        assert_eq!(cmp_names("abc", "ABC"), Equal);
        assert_eq!(cmp_names("ab", "abc"), Less);
        assert_eq!(cmp_names("Éclair", "éclair"), Equal);
        assert_eq!(cmp_names("é", "z"), Greater);
    }
}
