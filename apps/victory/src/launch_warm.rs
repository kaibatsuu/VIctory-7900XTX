//! Pre-launch shader translation and pipeline warming (`victory run --execute`).
//!
//! Controlled by `VICTORY_WARM_ON_LAUNCH`: enabled by default, disabled when
//! `VICTORY_WARM_ON_LAUNCH=0`.

use std::collections::HashSet;
use std::path::Path;
use std::time::UNIX_EPOCH;

const WARM_ON_LAUNCH_ENV: &str = "VICTORY_WARM_ON_LAUNCH";

/// Whether pre-launch warming is enabled for the given `VICTORY_WARM_ON_LAUNCH` value.
pub(crate) fn warm_on_launch_enabled(val: Option<&str>) -> bool {
    val.is_none_or(|v| v.trim() != "0")
}

/// Formats the `warm-shaders.txt` marker line for `(fingerprint, len, mtime_secs)`.
fn format_marker(fingerprint: &str, len: u64, mtime_secs: u64) -> String {
    format!("{fingerprint} {len} {mtime_secs}\n")
}

/// Returns whether an existing `warm-shaders.txt` marker matches the current state.
fn marker_matches(existing: Option<&str>, fingerprint: &str, len: u64, mtime_secs: u64) -> bool {
    let expected = format!("{fingerprint} {len} {mtime_secs}");
    existing.is_some_and(|text| text.trim() == expected)
}

/// Writes `content` to `path` via a temporary file rename in the same directory.
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let res = std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Runs pre-launch shader translation and used-pipeline warming unless disabled.
pub(crate) fn warm_before_launch(cfg: &victory_core::Config, title_dir: &Path) {
    let env_val = std::env::var(WARM_ON_LAUNCH_ENV).ok();
    if !warm_on_launch_enabled(env_val.as_deref()) {
        return;
    }
    let shaders_dir = match cfg.cache_subdir("shaders") {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(error = %e, "pre-launch warming skipped: shader cache unavailable");
            return;
        }
    };
    warm_shaders_rpf(&shaders_dir, title_dir);
    warm_used_pipelines(&shaders_dir);
}

fn warm_shaders_rpf(shaders_dir: &Path, title_dir: &Path) {
    let shaders_rpf = title_dir.join("shaders.rpf");
    let Ok(meta) = std::fs::metadata(&shaders_rpf) else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    let len = meta.len();
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let fingerprint = victory_hle::modules::d3d12x::shader_cache_fingerprint();
    let marker_path = shaders_dir.join("warm-shaders.txt");
    let existing = std::fs::read_to_string(&marker_path).ok();
    if marker_matches(existing.as_deref(), &fingerprint, len, mtime_secs) {
        return;
    }
    println!(
        "warming shaders from {} before launch ({WARM_ON_LAUNCH_ENV}=0 to skip)...",
        shaders_rpf.display()
    );
    match crate::translate::precompile(vec![shaders_rpf], None, None) {
        Ok(()) => {
            let marker = format_marker(&fingerprint, len, mtime_secs);
            if let Err(e) = write_atomic(&marker_path, &marker) {
                tracing::warn!(error = %e, path = %marker_path.display(), "failed to write shader warm marker");
                eprintln!("warning: failed to write {}: {e}", marker_path.display());
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "pre-launch shader warming failed");
            eprintln!("warning: pre-launch shader warming failed: {e}");
        }
    }
}

fn warm_used_pipelines(shaders_dir: &Path) {
    let pipelines_dir = shaders_dir.with_file_name("pipelines");
    if !pipelines_dir.join("known-pipelines.bin").is_file() {
        tracing::info!(
            "pre-launch: no pipeline book yet (set VICTORY_PIPELINE_BOOK=1 on one run to enable offline pipeline warming)"
        );
        return;
    }
    let early_path = pipelines_dir.join("early-pipelines.txt");
    if !early_path.is_file() {
        return;
    }
    let keys = victory_hle::known_pipelines::keys(&pipelines_dir);
    let fingerprint = victory_hle::known_pipelines::fingerprint();
    let early_text = std::fs::read_to_string(&early_path).unwrap_or_default();
    let done_text = std::fs::read_to_string(pipelines_dir.join("warm-done.txt")).unwrap_or_default();
    let bad_text = std::fs::read_to_string(pipelines_dir.join("warm-bad.txt")).unwrap_or_default();
    let early: HashSet<&str> = early_text.lines().filter(|l| !l.is_empty()).collect();
    let done: HashSet<&str> = done_text
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(of, _)| *of == fingerprint)
        .map(|(_, key)| key.trim())
        .collect();
    let bad: HashSet<&str> = bad_text.lines().filter_map(|l| l.split(' ').next()).collect();
    let pending = keys
        .iter()
        .filter(|key| {
            let k = key.as_str();
            early.contains(k) && !done.contains(k) && !bad.contains(k)
        })
        .count();
    if pending == 0 {
        return;
    }
    println!(
        "warming {pending} used pipelines before launch ({WARM_ON_LAUNCH_ENV}=0 to skip)..."
    );
    if let Err(e) = crate::warm::run(None, None, false, true, None) {
        tracing::warn!(error = %e, "pre-launch pipeline warming failed");
        eprintln!("warning: pre-launch pipeline warming failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_on_launch_switch_defaults_on() {
        assert!(warm_on_launch_enabled(None));
        assert!(warm_on_launch_enabled(Some("1")));
        assert!(!warm_on_launch_enabled(Some("0")));
        assert!(!warm_on_launch_enabled(Some(" 0 ")));
    }

    #[test]
    fn marker_matches_only_when_fingerprint_size_and_mtime_agree() {
        let marker = format_marker("abc123", 4096, 1_700_000_000);
        assert!(marker_matches(Some(&marker), "abc123", 4096, 1_700_000_000));
        assert!(marker_matches(Some("abc123 4096 1700000000"), "abc123", 4096, 1_700_000_000));
        assert!(!marker_matches(None, "abc123", 4096, 1_700_000_000));
        assert!(!marker_matches(Some(&marker), "def456", 4096, 1_700_000_000));
        assert!(!marker_matches(Some(&marker), "abc123", 4097, 1_700_000_000));
        assert!(!marker_matches(Some(&marker), "abc123", 4096, 1_700_000_001));
    }
}
