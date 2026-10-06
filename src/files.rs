use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn home_dir() -> Option<PathBuf> {
    home_from(|key| std::env::var_os(key))
}

/// `$HOME` everywhere, `%USERPROFILE%` (or `%HOMEDRIVE%%HOMEPATH%`) on Windows.
/// Split out from [`home_dir`] so the lookup order is testable.
fn home_from(get: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = get(key).filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(value));
        }
    }
    match (
        get("HOMEDRIVE").filter(|value| !value.is_empty()),
        get("HOMEPATH").filter(|value| !value.is_empty()),
    ) {
        (Some(drive), Some(path)) => {
            // Plain string concatenation on purpose: `%HOMEPATH%` starts with a
            // separator, and path joins would treat it as absolute on Unix.
            let mut home = drive.to_string_lossy().into_owned();
            let path = path.to_string_lossy();
            if !path.starts_with(['\\', '/']) {
                home.push('\\');
            }
            home.push_str(&path);
            Some(PathBuf::from(home))
        }
        _ => None,
    }
}

/// Where scratch files live: the TUI stages generations here until the user
/// decides what to keep. `$XDG_CACHE_HOME/fuckinggen`, `~/.cache/fuckinggen`,
/// or `%LOCALAPPDATA%\fuckinggen` on Windows.
pub fn cache_dir() -> PathBuf {
    for key in ["XDG_CACHE_HOME", "LOCALAPPDATA"] {
        if let Some(value) = std::env::var_os(key).filter(|value| !value.is_empty()) {
            return PathBuf::from(value).join("fuckinggen");
        }
    }
    home_dir()
        .map(|home| home.join(".cache").join("fuckinggen"))
        .unwrap_or_else(std::env::temp_dir)
}

/// Move a file, falling back to copy+delete when the destination is on another
/// filesystem (`rename` fails across mounts, and Windows refuses some moves).
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).with_context(|| format!("copying to {}", to.display()))?;
    std::fs::remove_file(from).with_context(|| format!("removing {}", from.display()))?;
    Ok(())
}

/// Move renders a previous session left behind into `session_dir`, so a crashed
/// window or a forced quit never costs the user a picture they paid for: the
/// next session shows them in the gallery like anything else. Returns the moved
/// files with a prompt guessed from their name.
pub fn adopt_stale_sessions(session_dir: &Path) -> Vec<(PathBuf, String)> {
    let Some(parent) = session_dir.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let ours = session_dir.file_name();
    let mut adopted = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if !path.is_dir()
            || name.to_string_lossy().starts_with('.')
            || Some(name.as_os_str()) == ours
        {
            continue;
        }
        if !name.to_string_lossy().starts_with("session-") {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&path) else {
            continue;
        };
        for file in files.flatten() {
            let source = file.path();
            if !is_image_path(&source) {
                continue;
            }
            let Some(file_name) = source.file_name() else {
                continue;
            };
            let target = session_dir.join(file_name);
            if move_file(&source, &target).is_err() {
                continue;
            }
            let prompt = Path::new(file_name)
                .file_stem()
                .map(|stem| stem.to_string_lossy().replace('-', " "))
                .unwrap_or_default();
            adopted.push((target, prompt));
        }
        // Only remove the old session when nothing is left in it.
        let _ = std::fs::remove_dir(&path);
    }
    adopted
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .is_some_and(|extension| {
            matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif")
        })
}

pub fn expand_tilde(input: &str) -> String {
    if input == "~" {
        return home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| input.to_string());
    }
    if let Some(rest) = input.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return format!("{}/{}", home.to_string_lossy(), rest);
    }
    input.to_string()
}

/// Where generated files land when the caller does not say.
pub fn default_out_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FUCKINGGEN_OUT_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(expand_tilde(&dir.to_string_lossy()));
    }
    if let Some(home) = home_dir() {
        return home.join("Downloads");
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Prompt to short, filesystem-safe file stem.
pub fn slug(prompt: &str, max: usize) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in prompt.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = true;
        }
        if out.len() >= max {
            break;
        }
    }
    out.truncate(max);
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "image".to_string()
    } else {
        out
    }
}

/// Resolve the *requested* output path: an explicit file, a directory, or a
/// prompt-derived name under `out_dir` (relative paths resolve against `cwd`).
pub fn resolve_out_path(
    out: Option<&str>,
    out_dir: Option<&Path>,
    prompt: &str,
    cwd: &Path,
) -> PathBuf {
    let derived_name = format!("{}.png", slug(prompt, 48));
    let requested = if let Some(raw) = out {
        let expanded = expand_tilde(raw);
        let path = PathBuf::from(&expanded);
        let treated_as_dir = raw.ends_with('/') || path.is_dir();
        if treated_as_dir {
            path.join(&derived_name)
        } else if path.extension().is_none() {
            path.with_extension("png")
        } else {
            path
        }
    } else {
        let dir = match out_dir {
            Some(dir) => dir.to_path_buf(),
            None => default_out_dir(),
        };
        dir.join(&derived_name)
    };
    if requested.is_absolute() {
        requested
    } else {
        cwd.join(requested)
    }
}

/// Never overwrite: `img.png` occupied -> `img-v2.png`, then `-v3`, ...
pub fn pick_non_overwrite(requested: &Path, max_version: u32) -> Result<PathBuf> {
    if !requested.exists() {
        return Ok(requested.to_path_buf());
    }
    let dir = requested
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = requested
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_string());
    let ext = requested
        .extension()
        .map(|e| e.to_string_lossy().into_owned());
    for n in 2..=max_version {
        let candidate = match &ext {
            Some(ext) => dir.join(format!("{stem}-v{n}.{ext}")),
            None => dir.join(format!("{stem}-v{n}")),
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!(
        "no free filename next to {} (tried -v2 .. -v{max_version})",
        requested.display()
    )
}

/// Create parent directories and pick a non-conflicting final path.
pub fn prepare_path(requested: &Path) -> Result<PathBuf> {
    if let Some(parent) = requested
        .parent()
        .filter(|p| !p.as_os_str().is_empty() && !p.exists())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    pick_non_overwrite(requested, 999)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".to_string());
    let tmp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("moving into place: {}", path.display()))?;
    Ok(())
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// UTC RFC 3339 without pulling in a date library.
pub fn rfc3339(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_dir_lookup_order_covers_windows_and_unix() {
        use std::ffi::OsString;
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| OsString::from(*value))
            }
        };

        // Unix and Windows both set $HOME these days; it wins.
        assert_eq!(
            home_from(env(&[
                ("HOME", "/home/you"),
                ("USERPROFILE", r"C:\Users\you")
            ])),
            Some(PathBuf::from("/home/you"))
        );
        // Windows without $HOME.
        assert_eq!(
            home_from(env(&[("USERPROFILE", r"C:\Users\you")])),
            Some(PathBuf::from(r"C:\Users\you"))
        );
        // Older Windows: drive + path.
        assert_eq!(
            home_from(env(&[("HOMEDRIVE", "C:"), ("HOMEPATH", r"\Users\you")])),
            Some(PathBuf::from(r"C:\Users\you"))
        );
        // Empty values are ignored, and a missing drive is not a home.
        assert_eq!(home_from(env(&[("HOME", ""), ("HOMEPATH", r"\you")])), None);
        assert_eq!(home_from(env(&[])), None);
    }

    #[test]
    fn earlier_sessions_are_adopted_not_lost() {
        let cache = tempfile::tempdir().unwrap();
        let ours = cache.path().join("session-222-2");
        std::fs::create_dir(&ours).unwrap();
        // A crashed session with a render in it, plus an empty one.
        let stale = cache.path().join("session-111-1");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("a-girl-holding-a-cup.png"), b"png").unwrap();
        std::fs::write(stale.join("notes.txt"), b"ignore me").unwrap();
        let empty = cache.path().join("session-333-3");
        std::fs::create_dir(&empty).unwrap();

        let adopted = adopt_stale_sessions(&ours);
        assert_eq!(adopted.len(), 1, "only images are picked up");
        let (path, prompt) = &adopted[0];
        assert_eq!(path, &ours.join("a-girl-holding-a-cup.png"));
        assert_eq!(prompt, "a girl holding a cup");
        assert!(path.is_file(), "the render moved into the live session");
        assert!(!stale.join("a-girl-holding-a-cup.png").exists());
        assert!(!empty.exists(), "empty session directories are cleaned up");
        // A directory with a non-image left in it survives, so nothing is eaten.
        assert!(stale.exists());
    }

    #[test]
    fn slug_shapes() {
        assert_eq!(slug("Hello, World! 2026", 48), "hello-world-2026");
        assert_eq!(slug("   ", 48), "image");
        assert_eq!(slug("hello w rld", 48), "hello-w-rld");
        assert_eq!(slug(&"x".repeat(100), 48).len(), 48);
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(rfc3339(1_767_225_600), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn resolve_default_name() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("Downloads");
        let p = resolve_out_path(None, Some(&out_dir), "Hello, World! 2026", tmp.path());
        assert_eq!(p, out_dir.join("hello-world-2026.png"));
    }

    #[test]
    fn resolve_explicit_file_and_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let file = resolve_out_path(Some("shot"), None, "x", tmp.path());
        assert_eq!(file, tmp.path().join("shot.png"));

        let dir = tmp.path().join("sub");
        std::fs::create_dir_all(&dir).unwrap();
        let inside = resolve_out_path(Some(dir.to_str().unwrap()), None, "Red Square", tmp.path());
        assert_eq!(inside, dir.join("red-square.png"));
    }

    #[test]
    fn resolve_relative_against_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let rel = resolve_out_path(Some("out/hero.png"), None, "x", tmp.path());
        assert_eq!(rel, tmp.path().join("out/hero.png"));
    }

    #[test]
    fn versioning_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("img.png");
        std::fs::write(&target, b"x").unwrap();
        let v2 = pick_non_overwrite(&target, 999).unwrap();
        assert_eq!(v2.file_name().unwrap(), "img-v2.png");
        std::fs::write(&v2, b"x").unwrap();
        let v3 = pick_non_overwrite(&target, 999).unwrap();
        assert_eq!(v3.file_name().unwrap(), "img-v3.png");
    }
}
