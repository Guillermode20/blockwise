//! Search box syntax. Whitespace-separated terms, all of which must match:
//! - `>10M` / `<1G`: size bounds (K/M/G/T, 1024-based; plain number = bytes)
//! - a term containing `*` or `?`: case-insensitive glob on the name
//! - anything else: case-insensitive substring of the name

enum Term {
    MinSize(u64),
    MaxSize(u64),
    Glob(Vec<u8>),
    Contains(String),
}

pub struct Filter {
    terms: Vec<Term>,
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_suffix('b').unwrap_or(&s);
    let (num, mul) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1u64 << 10),
        'm' => (&s[..s.len() - 1], 1 << 20),
        'g' => (&s[..s.len() - 1], 1 << 30),
        't' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    let v: f64 = num.parse().ok()?;
    (v >= 0.0).then(|| (v * mul as f64) as u64)
}

impl Filter {
    /// `None` when the query has no terms (show the normal tree).
    pub fn parse(q: &str) -> Option<Self> {
        let terms: Vec<Term> = q
            .split_whitespace()
            .map(|t| {
                if let Some(n) = t.strip_prefix('>').and_then(parse_size) {
                    Term::MinSize(n)
                } else if let Some(n) = t.strip_prefix('<').and_then(parse_size) {
                    Term::MaxSize(n)
                } else if t.contains(['*', '?']) {
                    Term::Glob(t.to_lowercase().into_bytes())
                } else {
                    Term::Contains(t.to_lowercase())
                }
            })
            .collect();
        (!terms.is_empty()).then_some(Self { terms })
    }

    /// `buf` is scratch space for the lowercased name, reused across calls
    /// so matching never allocates per node.
    pub fn matches(&self, name: &str, size: u64, buf: &mut String) -> bool {
        let mut lowered = false;
        self.terms.iter().all(|t| {
            let mut lower = || {
                if !lowered {
                    lowered = true;
                    buf.clear();
                    if name.is_ascii() {
                        buf.push_str(name);
                        buf.make_ascii_lowercase();
                    } else {
                        buf.extend(name.chars().flat_map(char::to_lowercase));
                    }
                }
            };
            match t {
                Term::MinSize(s) => size >= *s,
                Term::MaxSize(s) => size <= *s,
                Term::Glob(p) => {
                    lower();
                    glob(p, buf.as_bytes())
                }
                Term::Contains(s) => {
                    lower();
                    buf.contains(s.as_str())
                }
            }
        })
    }
}

/// Iterative `*`/`?` matcher with single-star backtracking (linear-ish).
fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_and_sizes() {
        assert!(glob(b"*.log", b"syslog.log"));
        assert!(!glob(b"*.log", b"syslog.log.1"));
        assert!(glob(b"a?c*", b"abcdef"));
        assert!(glob(b"*", b""));
        assert_eq!(parse_size("10M"), Some(10 << 20));
        assert_eq!(parse_size("1.5k"), Some(1536));
        assert_eq!(parse_size("2gb"), Some(2 << 30));
        assert_eq!(parse_size("x"), None);
        assert!(Filter::parse("   ").is_none());
    }
}
