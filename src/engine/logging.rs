// Auto-split from the former monolithic engine.rs. Child of `engine`, so
// `use super::*;` pulls in the shared types, std imports, and the sibling
// modules' items that `engine` re-exports via `pub(crate) use`.
use super::*;

/// Outcome of parsing the user config's `options` block. Only fields
/// Grinch actually acts on appear here — the others (urlShorteners,
/// logRequests, checkForUpdates, keepRunning) are still accepted at
/// parse time but discarded (see `parse_options_block`).
#[derive(Default, Debug, Clone, Copy)]
pub struct OptionsConfig {
    /// Whether the menu-bar status item should be skipped at app launch.
    /// Read once by AppDelegate during `setup_menu_bar`; reloads won't
    /// hide or re-show the icon mid-session (consistent with most macOS
    /// background apps that surface this kind of toggle).
    pub hide_icon: bool,
    /// Whether to write a per-resolve JSONL log to
    /// `~/Library/Logs/Grinch/Grinch_<timestamp>.log`. Each line is one
    /// resolve: input URL, final URL after rewrites, target browser,
    /// opener bundle ID, and a Unix timestamp. Mirrors Finicky's
    /// `options.logRequests` semantics.
    pub log_requests: bool,
    /// Rotate the request log when it grows past this many bytes.
    /// `None` (the default) disables size-based rotation. Rotation
    /// renames the current file to `<path>.<iso-timestamp>` and starts
    /// a fresh empty file, so older entries are preserved on disk for
    /// post-mortem until the user prunes them.
    pub log_rotate_bytes: Option<u64>,
    /// Rotate the request log when it has been written to for this many
    /// days (since the file was opened or most-recently rotated).
    /// `None` disables time-based rotation. Combine with
    /// `log_rotate_bytes` to get "rotate on either trigger".
    pub log_rotate_days: Option<u32>,
}

/// Per-resolve JSONL log writer used when `options.logRequests` is on.
/// Opens the destination file lazily on first `write` so an engine that
/// never resolves doesn't create an empty log file. After a write
/// failure the writer marks itself failed and stops trying — better
/// than spamming stderr per resolve.
///
/// Rotation: when either `rotate_bytes` or `rotate_days` is set and the
/// corresponding threshold is exceeded, the current file is renamed to
/// `<path>.<iso-timestamp>` and a fresh file is opened on the next write.
/// `bytes_written` is tracked in-process (initialised from the existing
/// file's size on open) so rotation decisions don't stat() per write.
pub(crate) struct LogWriter {
    path: std::path::PathBuf,
    pub(crate) file: Option<std::fs::File>,
    failed: bool,
    rotate_bytes: Option<u64>,
    rotate_days: Option<u32>,
    pub(crate) bytes_written: u64,
    pub(crate) opened_at_unix: u64,
}

impl LogWriter {
    pub(crate) fn new(
        path: std::path::PathBuf,
        rotate_bytes: Option<u64>,
        rotate_days: Option<u32>,
    ) -> Self {
        Self {
            path,
            file: None,
            failed: false,
            rotate_bytes,
            rotate_days,
            bytes_written: 0,
            opened_at_unix: 0,
        }
    }

    pub(crate) fn write(&mut self, line: &str) {
        use std::io::Write;
        if self.failed {
            return;
        }
        // newline-terminated; writeln! appends one
        let about_to_write = line.len() as u64 + 1;
        if self.should_rotate(about_to_write, now_unix()) {
            self.rotate();
        }
        if self.file.is_none() {
            match Self::open(&self.path) {
                Ok((f, size)) => {
                    self.file = Some(f);
                    self.bytes_written = size;
                    self.opened_at_unix = now_unix();
                }
                Err(e) => {
                    eprintln!(
                        "grinch: couldn't open log file {}: {e} — disabling \
                         options.logRequests for this session",
                        self.path.display()
                    );
                    self.failed = true;
                    return;
                }
            }
        }
        if let Some(f) = self.file.as_mut() {
            if let Err(e) = writeln!(f, "{line}") {
                eprintln!(
                    "grinch: write to {} failed: {e} — disabling \
                     options.logRequests for this session",
                    self.path.display()
                );
                self.failed = true;
                self.file = None;
            } else {
                self.bytes_written += about_to_write;
            }
        }
    }

    /// True when writing `extra_bytes` more would push the file past
    /// `rotate_bytes`, OR `now` is past `rotate_days` since the file
    /// was opened. Pure function so it's testable without a real fs.
    pub(crate) fn should_rotate(&self, extra_bytes: u64, now: u64) -> bool {
        if self.file.is_none() {
            return false;
        }
        if let Some(cap) = self.rotate_bytes {
            if self.bytes_written.saturating_add(extra_bytes) > cap {
                return true;
            }
        }
        if let Some(days) = self.rotate_days {
            let secs = u64::from(days).saturating_mul(86_400);
            if now.saturating_sub(self.opened_at_unix) >= secs {
                return true;
            }
        }
        false
    }

    fn rotate(&mut self) {
        // Drop the file handle so the rename can complete on platforms
        // that hold it locked (not macOS, but cheap to do everywhere).
        self.file = None;
        let stamp = iso_timestamp_for_filename();
        let rotated = self.path.with_extension(format!("log.{stamp}"));
        if let Err(e) = std::fs::rename(&self.path, &rotated) {
            // Rename can fail under very-unusual conditions (the source
            // disappeared because someone deleted it externally, or
            // permissions changed). Log once and carry on — the next
            // write will lazily re-open the path; in the worst case we
            // keep appending to a file that has grown past the cap,
            // which is still better than dropping log lines.
            eprintln!(
                "grinch: log rotation rename {} → {} failed: {e}",
                self.path.display(),
                rotated.display()
            );
        }
        self.bytes_written = 0;
        self.opened_at_unix = now_unix();
    }

    fn open(path: &std::path::Path) -> std::io::Result<(std::fs::File, u64)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        Ok((f, size))
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One JSONL entry per resolve. Schema:
///
/// - `ts`: unix seconds with millisecond fractional precision
/// - `url` / `final`: input URL and post-rewrite URL (equal when no
///   rewriter fired)
/// - `rewritten`: bool — true iff `url != final`. Pre-computed so log
///   consumers don't have to string-compare.
/// - `browser` / `args`: target bundle id and launch args. Empty
///   `browser` (== suppressed) is emitted as-is so callers can
///   distinguish a hit from "open: null".
/// - `opener`: `{bundleId, name, pid}` of the app that sent the URL.
///   Bundle id is empty when neither the sender PID nor the frontmost
///   snapshot identified one (rare).
/// - `modifiers`: `{shift, option, command, control}` at resolve time —
///   the four keys Grinch's rules actually expose to JS.
/// - `matchedRule`: `{index, name}` for the rule whose matcher fired, where
///   `name` is the user-supplied `name:` field when present, otherwise an
///   auto-derived label (string pattern, `domain:foo,bar`, or first line of
///   the fn source for fn matchers). `null` when the URL fell through to
///   the default browser.
pub(crate) fn format_log_entry(
    input_url: &str,
    opener: &Opener,
    modifiers: ModifierFlags,
    res: &Resolution<'_>,
    matched: Option<(usize, &str)>,
) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let final_url = res.url.as_ref();
    let matched_json = matched.map(|(idx, name)| serde_json::json!({"index": idx, "name": name}));
    let entry = serde_json::json!({
        "ts": ts,
        "url": input_url,
        "final": final_url,
        "rewritten": final_url != input_url,
        "browser": res.browser.bundle_id,
        "args": res.browser.args,
        "opener": {
            "bundleId": opener.bundle_id,
            "name": opener.name,
            "pid": opener.pid,
        },
        "modifiers": {
            "shift": modifiers.shift,
            "option": modifiers.option,
            "command": modifiers.command,
            "control": modifiers.control,
        },
        "matchedRule": matched_json,
    });
    entry.to_string()
}

/// Build a per-launch log path under `~/Library/Logs/Grinch/`. Falls back
/// to `/tmp/Grinch_<ts>.log` if `$HOME` isn't set (rare on macOS but
/// possible under sandboxed test runners). Filename uses an ISO-style
/// timestamp with colons replaced by dashes for filesystem safety.
pub(crate) fn log_file_path() -> std::path::PathBuf {
    let stem = format!("Grinch_{}.log", iso_timestamp_for_filename());
    let base = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Library/Logs/Grinch"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    base.join(stem)
}

/// Format the current local time as `YYYY-MM-DDTHH-MM-SS` for use in
/// log filenames. Avoids colons (which some macOS Finder pickers
/// remap) and keeps things human-readable.
fn iso_timestamp_for_filename() -> String {
    use std::ffi::CStr;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    let mut buf = [0i8; 64];
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr(),
            buf.len(),
            c"%Y-%m-%dT%H-%M-%S".as_ptr(),
            &tm,
        )
    };
    if n == 0 {
        return secs.to_string();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// Parse Finicky v4's `options` block. The five known keys are accepted
/// without error so a copied-over Finicky config doesn't break:
///
/// | Key | Grinch behaviour |
/// |---|---|
/// | `urlShorteners` | silently ignored — Finicky's hard-coded list isn't user-configurable there either; Grinch expects external expansion (see `examples/expand-shortener.sh`) |
/// | `logRequests`   | **honoured** — writes per-resolve JSONL to `~/Library/Logs/Grinch/Grinch_<timestamp>.log` |
/// | `checkForUpdates` | silently ignored — Grinch doesn't poll for updates |
/// | `keepRunning`   | silently ignored — Grinch is always resident |
/// | `hideIcon`      | **honoured** — propagated through `OptionsConfig` to AppDelegate, which skips menu-bar status item creation when set |
///
/// Unknown keys log a one-line warning so users can spot typos.
pub(crate) fn parse_options_block(opts: &JSValue) -> OptionsConfig {
    const KNOWN: &[&str] = &[
        "urlShorteners",
        "logRequests",
        "logRotateBytes",
        "logRotateDays",
        "checkForUpdates",
        "keepRunning",
        "hideIcon",
    ];
    let mut out = OptionsConfig::default();
    for (k, v) in iter_object(opts) {
        match k.as_str() {
            "hideIcon" => {
                out.hide_icon = unsafe { v.toBool() };
            }
            "logRequests" => {
                out.log_requests = unsafe { v.toBool() };
            }
            "logRotateBytes" => {
                // JS numbers are doubles; coerce to u64 with bounds-check
                // so a negative/NaN/infinity value disables rotation
                // rather than silently producing a giant cap.
                let n = unsafe { v.toDouble() };
                if n.is_finite() && n > 0.0 && n <= u64::MAX as f64 {
                    out.log_rotate_bytes = Some(n as u64);
                }
            }
            "logRotateDays" => {
                let n = unsafe { v.toDouble() };
                if n.is_finite() && n > 0.0 && n <= u32::MAX as f64 {
                    out.log_rotate_days = Some(n as u32);
                }
            }
            other if !KNOWN.contains(&other) => {
                eprintln!(
                    "grinch: unknown options.{other} — accepted keys are {}",
                    KNOWN.join(", ")
                );
            }
            // Known but inert keys: accept silently.
            _ => {}
        }
    }
    out
}
