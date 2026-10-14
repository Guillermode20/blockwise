//! Small persisted UI state: recent folders, split position, window size.
//! Stored as `key=value` lines in `<config_dir>/blockwise/settings.txt`;
//! unknown or malformed lines are ignored so the format can grow.

use std::path::PathBuf;

const MAX_RECENT: usize = 10;

#[derive(Clone)]
pub struct Settings {
    /// Fraction of the content area given to the upper (list) pane.
    pub split: f32,
    pub window: Option<(f32, f32)>,
    /// Most recent first.
    pub recent: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { split: 0.5, window: None, recent: Vec::new() }
    }
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("blockwise").join("settings.txt"))
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return s;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "split" => {
                    if let Ok(f) = v.parse::<f32>() {
                        s.split = f.clamp(0.1, 0.9);
                    }
                }
                "window" => {
                    if let Some((w, h)) = v.split_once('x') {
                        if let (Ok(w), Ok(h)) = (w.parse::<f32>(), h.parse::<f32>()) {
                            if w >= 320. && h >= 240. {
                                s.window = Some((w, h));
                            }
                        }
                    }
                }
                "recent" if s.recent.len() < MAX_RECENT && !v.is_empty() => {
                    s.recent.push(v.to_string());
                }
                _ => {}
            }
        }
        s
    }

    /// Best effort: settings are a convenience, so write errors are ignored.
    pub fn save(&self) {
        let Some(p) = path() else {
            return;
        };
        let mut out = format!("split={:.3}\n", self.split);
        if let Some((w, h)) = self.window {
            out.push_str(&format!("window={}x{}\n", w.round(), h.round()));
        }
        for r in &self.recent {
            out.push_str(&format!("recent={}\n", r));
        }
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, out);
    }

    pub fn push_recent(&mut self, path: &str) {
        self.recent.retain(|r| r != path);
        self.recent.insert(0, path.to_string());
        self.recent.truncate(MAX_RECENT);
    }
}
