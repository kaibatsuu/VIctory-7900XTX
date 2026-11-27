//! Diagnostics: the frame times of one run, one line per title present, for
//! A/B runs. `VICTORY_FRAMETIME_DUMP=<file>` names the CSV file (outside the
//! repository); it is created afresh each run, with the columns `ms,frame_ms`:
//! the time since the first present and the time since the present before.
//! Unset, nothing is written.

use std::io::{BufWriter, Write};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct Dump {
    file: BufWriter<std::fs::File>,
    started: Instant,
    last: Instant,
}

static DUMP: Mutex<Option<Dump>> = Mutex::new(None);

/// The file named by `VICTORY_FRAMETIME_DUMP`, if any.
fn target() -> Option<&'static str> {
    static TARGET: OnceLock<Option<String>> = OnceLock::new();
    TARGET
        .get_or_init(|| std::env::var("VICTORY_FRAMETIME_DUMP").ok().filter(|s| !s.trim().is_empty()))
        .as_deref()
}

/// The title's present ends on this thread.
pub(crate) fn presented() {
    let Some(path) = target() else {
        return;
    };
    let now = Instant::now();
    let mut guard = DUMP.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dump) = guard.as_mut() {
        let at = (now - dump.started).as_secs_f64() * 1000.0;
        let frame = (now - dump.last).as_secs_f64() * 1000.0;
        dump.last = now;
        if writeln!(dump.file, "{at:.3},{frame:.3}").and_then(|()| dump.file.flush()).is_err() {
            tracing::warn!("frame times: writing {path} failed; no more lines");
            *guard = None;
        }
        return;
    }
    match std::fs::File::create(path) {
        Ok(file) => {
            let mut file = BufWriter::new(file);
            if writeln!(file, "ms,frame_ms").is_ok() {
                tracing::info!("frame times written to {path} (VICTORY_FRAMETIME_DUMP)");
                *guard = Some(Dump { file, started: now, last: now });
            }
        }
        Err(e) => tracing::warn!("frame times: cannot create {path}: {e}"),
    }
}
