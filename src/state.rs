use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::files::{atomic_write, home_dir, now_unix};

const MAX_RECORDS: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageRecord {
    pub path: String,
    pub prompt: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub images: Vec<ImageRecord>,
}

/// Session state lives under XDG state (`~/.local/state/fuckinggen/state.json`).
pub fn state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("fuckinggen").join("state.json")
}

pub fn load() -> State {
    load_from(&state_path())
}

pub fn load_from(path: &Path) -> State {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => State::default(),
    }
}

pub fn save(state: &State) -> Result<()> {
    save_to(&state_path(), state)
}

pub fn save_to(path: &Path, state: &State) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(state)?;
    atomic_write(path, text.as_bytes())
}

/// Remember a generated image, newest first, deduplicated by path.
pub fn record(path: &Path, prompt: &str) -> Result<()> {
    record_in(&state_path(), path, prompt)
}

pub fn record_in(state_path: &Path, path: &Path, prompt: &str) -> Result<()> {
    let mut state = load_from(state_path);
    let entry = path.display().to_string();
    state.images.retain(|record| record.path != entry);
    state.images.insert(
        0,
        ImageRecord {
            path: entry,
            prompt: prompt.to_string(),
            created_at: now_unix(),
        },
    );
    state.images.truncate(MAX_RECORDS);
    save_to(state_path, &state)
}

pub fn forget(path: &Path) -> Result<()> {
    forget_in(&state_path(), path)
}

pub fn forget_in(state_path: &Path, path: &Path) -> Result<()> {
    let mut state = load_from(state_path);
    let entry = path.display().to_string();
    state.images.retain(|record| record.path != entry);
    save_to(state_path, &state)
}

/// Newest first.
pub fn recent(count: usize) -> Vec<ImageRecord> {
    recent_from(&state_path(), count)
}

pub fn recent_from(state_path: &Path, count: usize) -> Vec<ImageRecord> {
    let state = load_from(state_path);
    state.images.into_iter().take(count).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_dedupe_and_recent_order() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");

        record_in(&path, Path::new("/tmp/a.png"), "first").unwrap();
        record_in(&path, Path::new("/tmp/b.png"), "second").unwrap();
        record_in(&path, Path::new("/tmp/a.png"), "first again").unwrap();

        let recent = recent_from(&path, 10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].path, "/tmp/a.png");
        assert_eq!(recent[0].prompt, "first again");
        assert_eq!(recent[1].path, "/tmp/b.png");
    }

    #[test]
    fn forget_removes_only_that_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        record_in(&path, Path::new("/tmp/a.png"), "a").unwrap();
        record_in(&path, Path::new("/tmp/b.png"), "b").unwrap();
        forget_in(&path, Path::new("/tmp/a.png")).unwrap();
        let recent = recent_from(&path, 10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].path, "/tmp/b.png");
    }

    #[test]
    fn caps_retained_records() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        for index in 0..(MAX_RECORDS + 5) {
            record_in(&path, Path::new(&format!("/tmp/{index}.png")), "x").unwrap();
        }
        let recent = recent_from(&path, MAX_RECORDS + 10);
        assert_eq!(recent.len(), MAX_RECORDS);
        assert_eq!(recent[0].path, format!("/tmp/{}.png", MAX_RECORDS + 4));
    }

    #[test]
    fn missing_state_file_is_empty_state() {
        let tmp = tempfile::tempdir().unwrap();
        let state = load_from(&tmp.path().join("nope.json"));
        assert!(state.images.is_empty());
    }
}
