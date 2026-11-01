// Auto-split from the former monolithic engine.rs. Child of `engine`, so
// `use super::*;` pulls in the shared types, std imports, and the sibling
// modules' items that `engine` re-exports via `pub(crate) use`.
use super::*;
use std::sync::Mutex;

pub(crate) fn profile_access_advice(error: &std::io::Error) -> &'static str {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        "macOS may be blocking access; check Privacy & Security > Files & Folders > Grinch, \
         enable access to this browser, then Reload Config."
    } else {
        "Open the browser once, check the profile-data path and file access, then Reload Config."
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogFormat {
    #[default]
    Json,
    Text,
}

/// Outcome of parsing the user config's `options` block. Only fields
/// Grinch acts on appear here. Known inert options are still accepted at
/// parse time and discarded (see `parse_options_block`).
#[derive(Default, Debug, Clone, Copy)]
pub struct OptionsConfig {
    /// Whether the menu-bar status item should be skipped at app launch.
    /// Read once by AppDelegate during `setup_menu_bar`; reloads won't
    /// hide or re-show the icon mid-session (consistent with most macOS
    /// background apps that surface this kind of toggle).
    pub hide_icon: bool,
    /// Whether Sparkle checks for updates automatically. `None` (the key is
    /// absent) leaves the choice to the menu bar toggle; `Some` overrides it
    /// on every config load.
    pub check_for_updates: Option<bool>,
    /// Whether to add resolve events to the app's diagnostic log.
    /// Config-load and runtime-JavaScript errors are recorded regardless;
    /// this flag preserves Finicky's opt-in request-logging semantics.
    pub log_requests: bool,
    /// Output format for diagnostic events; defaults to JSON.
    pub(crate) log_format: LogFormat,
    /// Whether an invalid format should leave the current format unchanged.
    pub(crate) invalid_log_format: bool,
    /// Rotate the diagnostic log when it grows past this many bytes.
    /// `None` (the default) disables size-based rotation. Rotation
    /// renames the current file to `<path>.<iso-timestamp>` (with a numeric
    /// suffix if that name already exists) and starts
    /// a fresh empty file, so older entries are preserved on disk for
    /// post-mortem until the user prunes them.
    pub log_rotate_bytes: Option<u64>,
    /// Rotate after this many seconds since opening or the most recent rotation.
    /// `None` disables time-based rotation. Combine with
    /// `log_rotate_bytes` to get "rotate on either trigger".
    pub log_rotate_seconds: Option<u64>,
}

/// App-owned diagnostic log shared across config reloads and engines.
/// Error events are always recorded; `Engine` decides whether to add resolve
/// events from `options.logRequests`. The file is opened lazily on the first
/// event or when the menu action explicitly opens it.
pub(crate) struct DiagnosticLog {
    // Only the writer is shared with background launch completions; UI state stays local.
    writer: Arc<Mutex<LogWriter>>,
    profile_error: RefCell<Option<String>>,
    config_warnings: RefCell<Vec<String>>,
    profile_error_handler: RefCell<Option<Box<dyn Fn()>>>,
}

impl Default for DiagnosticLog {
    fn default() -> Self {
        let log = Self::new(log_file_path());
        log.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .new_launch = true;
        log
    }
}

impl DiagnosticLog {
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            writer: Arc::new(Mutex::new(LogWriter::new(path, None, None))),
            profile_error: RefCell::new(None),
            config_warnings: RefCell::new(Vec::new()),
            profile_error_handler: RefCell::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn at_path(path: std::path::PathBuf) -> Self {
        Self::new(path)
    }

    pub(crate) fn configure_rotation(&self, bytes: Option<u64>, seconds: Option<u64>) {
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .configure_rotation(bytes, seconds);
    }

    pub(crate) fn configure_format(&self, format: LogFormat) {
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .configure_format(format);
    }

    pub(crate) fn ensure_file(&self) -> std::io::Result<std::path::PathBuf> {
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ensure_file()
    }

    /// Return the first launch-suppressing profile failure since the last config load.
    pub(crate) fn profile_error(&self) -> Option<String> {
        self.profile_error.borrow().clone()
    }

    pub(crate) fn config_warning(&self) -> Option<String> {
        self.config_warnings.borrow().first().cloned()
    }

    pub(crate) fn clear_config_warnings(&self) {
        self.config_warnings.borrow_mut().clear();
    }

    pub(crate) fn record_config_warning(&self, message: &str) {
        let mut warnings = self.config_warnings.borrow_mut();
        if warnings.len() >= 32 || warnings.iter().any(|warning| warning == message) {
            return;
        }
        warnings.push(message.to_string());
        drop(warnings);
        eprintln!("grinch: {message}");
        self.write_event(serde_json::json!({"event": "config_warning", "ts": now_unix_f64(), "message": message}));
        if let Some(handler) = self.profile_error_handler.borrow().as_ref() {
            handler();
        }
    }

    /// Notify the app when profile warning state changes, including during resolution.
    /// The handler may run under a profile-cache lock; it must not re-enter resolution.
    pub(crate) fn set_profile_error_handler(&self, handler: impl Fn() + 'static) {
        *self.profile_error_handler.borrow_mut() = Some(Box::new(handler));
    }

    pub(crate) fn clear_profile_error(&self) {
        let previous = self.profile_error.borrow_mut().take();
        if previous.is_some()
            && let Some(handler) = self.profile_error_handler.borrow().as_ref()
        {
            handler();
        }
    }

    pub(crate) fn record_config_error(&self, path: Option<&std::path::Path>, message: &str) {
        let path = path.map(|value| value.display().to_string());
        self.write_event(serde_json::json!({
            "event": "config_error",
            "ts": now_unix_f64(),
            "path": path,
            "message": message,
        }));
    }

    pub(crate) fn record_runtime_js_error(&self, path: &str, message: &str) {
        self.write_event(serde_json::json!({
            "event": "runtime_js_error",
            "ts": now_unix_f64(),
            "path": path,
            "message": message,
        }));
    }

    pub(crate) fn record_profile_error(
        &self,
        browser: &str,
        path: Option<&std::path::Path>,
        message: &str,
    ) {
        let first_error = self.profile_error.borrow().is_none();
        if first_error {
            *self.profile_error.borrow_mut() = Some(format!("{browser}: {message}"));
        }
        self.record_profile_warning(browser, path, message);
        if first_error && let Some(handler) = self.profile_error_handler.borrow().as_ref() {
            handler();
        }
    }

    /// Log a recoverable profile issue without changing the launch-suppression status.
    /// Keeps the existing `profile_error` event schema for diagnostic consumers.
    pub(crate) fn record_profile_warning(
        &self,
        browser: &str,
        path: Option<&std::path::Path>,
        message: &str,
    ) {
        eprintln!("grinch: {browser}: {message}");
        self.write_event(serde_json::json!({
            "event": "profile_error",
            "ts": now_unix_f64(),
            "browser": browser,
            "path": path.map(|p| p.display().to_string()),
            "message": message,
        }));
    }

    pub(crate) fn write_event(&self, event: serde_json::Value) {
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(&event);
    }

    /// Retain the same rotating writer for callbacks that can outlive an engine
    /// and run on arbitrary queues. No UI state crosses the thread boundary.
    pub(crate) fn event_sink(&self) -> impl Fn(serde_json::Value) + Send + Sync + use<> {
        let writer = Arc::clone(&self.writer);
        move |event| {
            writer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .write(&event);
        }
    }
}

/// Build the shared failure schema for preflight drops and native completions.
pub(crate) fn launch_error_event(
    url: &str,
    browser: &str,
    strategy: &str,
    message: &str,
) -> serde_json::Value {
    serde_json::json!({
        "event": "launch_error",
        "ts": now_unix_f64(),
        "url": url,
        "browser": browser,
        "strategy": strategy,
        "message": message,
    })
}

/// Append-only writer behind [`DiagnosticLog`]. After a write failure it
/// stops trying so one broken destination cannot spam stderr per resolve.
///
/// Rotation: when either `rotate_bytes` or `rotate_seconds` is set and the
/// corresponding threshold is exceeded, the current file is renamed to
/// `<path>.<iso-timestamp>` (with a numeric suffix for collisions), and a
/// fresh file is opened on the next write.
/// `bytes_written` is tracked in-process (initialised from the existing
/// file's size on open) so rotation decisions don't stat() per write.
pub(crate) struct LogWriter {
    base_path: std::path::PathBuf,
    path: std::path::PathBuf,
    format: LogFormat,
    pending_format_switch: bool,
    new_launch: bool,
    pub(crate) file: Option<std::fs::File>,
    failed: bool,
    rotate_bytes: Option<u64>,
    rotate_seconds: Option<u64>,
    pub(crate) bytes_written: u64,
    pub(crate) opened_at_unix: u64,
}

impl LogWriter {
    pub(crate) fn new(
        path: std::path::PathBuf,
        rotate_bytes: Option<u64>,
        rotate_seconds: Option<u64>,
    ) -> Self {
        Self {
            base_path: path.clone(),
            path,
            format: LogFormat::Json,
            pending_format_switch: false,
            new_launch: false,
            file: None,
            failed: false,
            rotate_bytes,
            rotate_seconds,
            bytes_written: 0,
            opened_at_unix: 0,
        }
    }

    pub(crate) fn write(&mut self, event: &serde_json::Value) {
        use std::io::Write;
        if self.failed {
            return;
        }
        // One buffer, one write(2): `writeln!` on an unbuffered File issues a
        // second syscall for the newline, which doubled the cost per event.
        let seconds = event["ts"].as_f64().unwrap_or_else(now_unix_f64) as libc::time_t;
        let date = local_timestamp(seconds, c"%Y-%m-%dT%H:%M:%S%z");
        let line = match self.format {
            LogFormat::Json => {
                let event = event.to_string();
                let separator = if event.len() > 2 { "," } else { "" };
                format!("{{\"date\":\"{date}\"{separator}{}\n", &event[1..])
            }
            LogFormat::Text => format_text_event(event, &date),
        };
        let about_to_write = line.len() as u64;
        if self.should_rotate(about_to_write, now_unix()) {
            self.rotate();
        }
        if let Err(e) = self.ensure_open() {
            eprintln!(
                "grinch: couldn't open log file {}: {e} — disabling \
                 diagnostic file logging for this session",
                self.path.display()
            );
            self.failed = true;
            return;
        }
        if let Some(f) = self.file.as_mut() {
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!(
                    "grinch: write to {} failed: {e} — disabling \
                     diagnostic file logging for this session",
                    self.path.display()
                );
                self.failed = true;
                self.file = None;
            } else {
                self.bytes_written += about_to_write;
            }
        }
    }

    /// Create the log file if it has not been opened yet and return its path.
    /// Used by the menu action so an explicit request can open an otherwise
    /// lazy log before the first URL arrives.
    pub(crate) fn ensure_file(&mut self) -> std::io::Result<std::path::PathBuf> {
        self.ensure_open()
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", self.path.display())))?;
        self.failed = false;
        Ok(self.path.clone())
    }

    fn configure_rotation(&mut self, bytes: Option<u64>, seconds: Option<u64>) {
        self.rotate_bytes = bytes;
        self.rotate_seconds = seconds;
    }

    fn configure_format(&mut self, format: LogFormat) {
        if self.format == format {
            return;
        }
        if self.file.is_none() && (self.new_launch || !self.path.exists()) {
            self.format = format;
            return;
        }
        self.file = None;
        self.format = format;
        self.pending_format_switch = true;
        if let Err(error) = self.ensure_open() {
            eprintln!("grinch: couldn't create a new diagnostic log: {error}");
            self.failed = true;
        } else {
            self.failed = false;
        }
    }

    fn open_next_format_file(&mut self) -> std::io::Result<(std::fs::File, u64)> {
        if let Some(parent) = self.base_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        for number in 1_u64.. {
            let path = self.base_path.with_extension(format!("{number}.log"));
            match std::fs::OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(&path)
            {
                Ok(file) => {
                    self.path = path;
                    self.pending_format_switch = false;
                    return Ok((file, 0));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }

    fn ensure_open(&mut self) -> std::io::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        let (file, size) = if self.new_launch {
            self.open_launch_file()?
        } else if self.pending_format_switch {
            self.open_next_format_file()?
        } else {
            Self::open(&self.path)?
        };
        self.file = Some(file);
        self.bytes_written = size;
        self.opened_at_unix = now_unix();
        Ok(())
    }

    fn open_launch_file(&mut self) -> std::io::Result<(std::fs::File, u64)> {
        if let Some(parent) = self.base_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        for number in 0_u64.. {
            let path = if number == 0 {
                self.base_path.clone()
            } else {
                self.base_path
                    .with_extension(format!("launch-{number}.log"))
            };
            match std::fs::OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(&path)
            {
                Ok(file) => {
                    self.base_path = path.clone();
                    self.path = path;
                    self.new_launch = false;
                    return Ok((file, 0));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }

    /// True when writing `extra_bytes` more would push the file past
    /// `rotate_bytes`, OR `now` is past `rotate_seconds` since the file
    /// was opened. Pure function so it's testable without a real fs.
    pub(crate) fn should_rotate(&self, extra_bytes: u64, now: u64) -> bool {
        if self.file.is_none() {
            return false;
        }
        if let Some(cap) = self.rotate_bytes
            && self.bytes_written.saturating_add(extra_bytes) > cap
        {
            return true;
        }
        if let Some(secs) = self.rotate_seconds
            && now.saturating_sub(self.opened_at_unix) >= secs
        {
            return true;
        }
        false
    }

    fn rotate(&mut self) {
        // Drop the file handle so the rename can complete on platforms
        // that hold it locked (not macOS, but cheap to do everywhere).
        self.file = None;
        if let Err(e) = rename_rotated_log(&self.path, &iso_timestamp_for_filename()) {
            // Rename can fail under very-unusual conditions (the source
            // disappeared because someone deleted it externally, or
            // permissions changed). Report it and carry on — the next
            // write will lazily re-open the path; in the worst case we
            // keep appending to a file that has grown past the cap,
            // which is still better than dropping log lines.
            eprintln!(
                "grinch: log rotation rename {} failed: {e}",
                self.path.display()
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

fn rename_rotated_log(path: &std::path::Path, stamp: &str) -> std::io::Result<()> {
    use std::ffi::CString;

    let source = CString::new(path.as_os_str().as_encoded_bytes())?;
    // ponytail: scan same-second collisions; retain a sequence if rapid rotation becomes common.
    let mut suffix = 0_u64;
    loop {
        let extension = if suffix == 0 {
            format!("log.{stamp}")
        } else {
            format!("log.{stamp}.{suffix}")
        };
        let destination = path.with_extension(extension);
        let destination = CString::new(destination.as_os_str().as_encoded_bytes())?;
        // RENAME_EXCL refuses existing destinations atomically, including across restarts.
        if unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) }
            == 0
        {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        suffix += 1;
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_unix_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn format_text_event(event: &serde_json::Value, date: &str) -> String {
    use std::fmt::Write;
    let kind = event["event"].as_str().unwrap_or("unknown");
    let mut text = format!("{date}  {kind}\n");
    if let Some(fields) = event.as_object() {
        for (key, value) in fields {
            if key == "event" || key == "ts" {
                continue;
            }
            if let Some(fields) = value.as_object() {
                let _ = writeln!(text, "  {key}:");
                for (name, value) in fields {
                    write_text_field(&mut text, "    ", name, value);
                }
            } else {
                write_text_field(&mut text, "  ", key, value);
            }
        }
    }
    text.push('\n');
    text
}

fn write_text_field(text: &mut String, indent: &str, name: &str, value: &serde_json::Value) {
    use std::fmt::Write;
    let value = text_value(value);
    if value.is_empty() {
        let _ = writeln!(text, "{indent}{name}:");
    } else {
        let _ = writeln!(text, "{indent}{name}: {value}");
    }
}

fn text_value(value: &serde_json::Value) -> String {
    match value.as_str() {
        Some(s) => {
            let mut result = String::new();
            for character in s.chars() {
                if character.is_control() || character == '\\' {
                    result.extend(character.escape_default());
                } else {
                    result.push(character);
                }
            }
            result
        }
        None => value.to_string(),
    }
}

/// One `resolve` JSONL event. Schema:
///
/// - `event`: always `resolve`
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
pub(crate) fn format_resolve_event(
    input_url: &str,
    opener: &Opener,
    modifiers: ModifierFlags,
    res: &Resolution<'_>,
    matched: Option<(usize, &str)>,
) -> serde_json::Value {
    let final_url = res.url.as_ref();
    let strategy = LaunchPlan::from_spec(&res.browser, final_url).strategy();
    let matched_json = matched.map(|(idx, name)| serde_json::json!({"index": idx, "name": name}));
    serde_json::json!({
        "event": "resolve",
        "ts": now_unix_f64(),
        "url": input_url,
        "final": final_url,
        "rewritten": final_url != input_url,
        "browser": res.browser.bundle_id,
        "args": res.browser.args,
        "strategy": strategy,
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
    })
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
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    local_timestamp(secs, c"%Y-%m-%dT%H-%M-%S")
}

fn local_timestamp(secs: libc::time_t, format: &std::ffi::CStr) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    let mut buf = [0i8; 64];
    let n = unsafe { libc::strftime(buf.as_mut_ptr(), buf.len(), format.as_ptr(), &tm) };
    if n == 0 {
        return secs.to_string();
    }
    unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// Keys accepted inside the config's `options` block.
pub(crate) const OPTION_KEYS: &[&str] = &[
    "urlShorteners",
    "logRequests",
    "logFormat",
    "logRotateBytes",
    "logRotateDays",
    "checkForUpdates",
    "keepRunning",
    "hideIcon",
];

/// Parse Finicky v4's `options` block. Its five keys are accepted
/// without error so a copied-over Finicky config doesn't break:
///
/// | Key | Grinch behaviour |
/// |---|---|
/// | `urlShorteners` | silently ignored — Finicky's hard-coded list isn't user-configurable there either; Grinch expects external expansion (see `examples/expand-shortener.sh`) |
/// | `logRequests`   | **honoured** — adds resolve events to the app-wide diagnostic log |
/// | `checkForUpdates` | **honoured** — turns Sparkle's automatic update checks on or off; absent leaves the menu bar toggle in charge |
/// | `keepRunning`   | silently ignored — Grinch is always resident |
/// | `hideIcon`      | **honoured** — propagated through `OptionsConfig` to AppDelegate, which skips menu-bar status item creation when set |
///
/// Unknown keys log a one-line warning so users can spot typos.
pub(crate) fn parse_options_block(opts: &JSValue) -> OptionsConfig {
    let mut out = OptionsConfig::default();
    for (k, v) in iter_object(opts) {
        match k.as_str() {
            "hideIcon" => {
                out.hide_icon = unsafe { v.toBool() };
            }
            "checkForUpdates" => {
                out.check_for_updates = Some(unsafe { v.toBool() });
            }
            "logRequests" => {
                out.log_requests = unsafe { v.toBool() };
            }
            "logFormat" if is_undef_or_null(&v) => {}
            "logFormat" => match js_to_string(&v).as_deref() {
                Some("json") => out.log_format = LogFormat::Json,
                Some("text") => out.log_format = LogFormat::Text,
                _ => out.invalid_log_format = true,
            },
            "logRotateBytes" | "logRotateDays" if is_undef_or_null(&v) => {}
            "logRotateBytes" => {
                // JS numbers are doubles; coerce to u64 with bounds-check
                // so a negative/NaN/infinity value disables rotation
                // rather than silently producing a giant cap.
                let n = unsafe { v.toDouble() };
                if n.is_finite() && n >= 1.0 && n < u64::MAX as f64 && n.fract() == 0.0 {
                    out.log_rotate_bytes = Some(n as u64);
                } else {
                    eprintln!(
                        "grinch: invalid options.logRotateBytes — use a positive whole number"
                    );
                }
            }
            "logRotateDays" => {
                let n = unsafe { v.toDouble() };
                let seconds = n * 86_400.0;
                if seconds.is_finite() && seconds >= 1.0 && seconds < u64::MAX as f64 {
                    out.log_rotate_seconds = Some(seconds as u64);
                } else {
                    eprintln!("grinch: invalid options.logRotateDays — use at least one second");
                }
            }
            other if !OPTION_KEYS.contains(&other) => {
                eprintln!(
                    "grinch: unknown options.{other} — accepted keys are {}",
                    OPTION_KEYS.join(", ")
                );
            }
            // Known but inert keys: accept silently.
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn simultaneous_launches_keep_formats_in_separate_files() {
        let first = crate::engine::integration_tests::isolated_diagnostics();
        let path = first.writer.lock().unwrap().path.clone();
        first.writer.lock().unwrap().new_launch = true;
        let second = super::DiagnosticLog::new(path);
        second.writer.lock().unwrap().new_launch = true;
        first.configure_format(super::LogFormat::Text);
        first.record_config_error(None, "text event");
        second.record_config_error(None, "json event");
        let text_path = first.ensure_file().unwrap();
        let json_path = second.ensure_file().unwrap();
        assert_ne!(text_path, json_path);
        let text = std::fs::read_to_string(text_path).unwrap();
        let json = std::fs::read_to_string(json_path).unwrap();
        assert!(text.contains("text event") && !text.contains("json event"));
        assert!(json.contains("json event") && !json.contains("text event"));
        assert!(serde_json::from_str::<serde_json::Value>(json.trim()).is_ok());
    }
    use crate::engine::logging::rename_rotated_log;

    #[test]
    fn profile_access_advice_distinguishes_denial_from_missing_files() {
        for code in [libc::EACCES, libc::EPERM] {
            let message = super::profile_access_advice(&std::io::Error::from_raw_os_error(code));
            assert!(message.contains("Files & Folders"));
        }
        let missing =
            super::profile_access_advice(&std::io::Error::from_raw_os_error(libc::ENOENT));
        assert!(!missing.contains("Files & Folders"));
    }

    #[test]
    fn format_switch_retries_after_directory_failure() {
        let diagnostics = crate::engine::integration_tests::isolated_diagnostics();
        diagnostics.record_config_error(None, "before");
        let old = diagnostics.ensure_file().unwrap();
        let directory = old.parent().unwrap();
        let moved = directory.with_extension("moved");
        std::fs::rename(directory, &moved).unwrap();
        std::fs::write(directory, "not a directory").unwrap();
        diagnostics.configure_format(super::LogFormat::Text);
        let failed = diagnostics.ensure_file().is_err();
        std::fs::remove_file(directory).unwrap();
        std::fs::rename(&moved, directory).unwrap();
        assert!(failed);
        let current = diagnostics.ensure_file().unwrap();
        assert_ne!(old, current);
        diagnostics.record_config_error(None, "after");
        let body = std::fs::read_to_string(current).unwrap();
        assert!(body.contains("  config_error\n"), "{body}");
        assert!(body.contains("message: after"), "{body}");
        let body = std::fs::read_to_string(old).unwrap();
        assert!(body.contains("\"before\""));
        assert!(!body.contains("after"));
    }

    #[test]
    fn background_events_share_rotation_and_survive_the_log_owner() {
        let diagnostics = crate::engine::integration_tests::isolated_diagnostics();
        let path = diagnostics.ensure_file().unwrap();
        diagnostics.configure_rotation(Some(1), None);
        let record = diagnostics.event_sink();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = std::sync::Arc::clone(&barrier);
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            for id in 0..10 {
                record(serde_json::json!({"id": id, "ts": 0.0}));
            }
        });
        barrier.wait();
        for id in 10..20 {
            diagnostics.write_event(serde_json::json!({"id": id, "ts": 0.0}));
        }
        worker.join().unwrap();
        let record = diagnostics.event_sink();
        drop(diagnostics);
        record(serde_json::json!({"id": 20, "ts": 0.0}));
        let expected = std::process::Command::new("/bin/date")
            .args(["-r", "0", "+%Y-%m-%dT%H:%M:%S%z"])
            .output()
            .unwrap();
        assert!(expected.status.success());
        let expected = String::from_utf8(expected.stdout).unwrap();
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(path.parent().unwrap()).unwrap() {
            for line in std::fs::read_to_string(entry.unwrap().path())
                .unwrap()
                .lines()
            {
                assert!(
                    line.starts_with("{\"date\":\""),
                    "date must be first: {line}"
                );
                let row: serde_json::Value = serde_json::from_str(line).unwrap();
                let date = row["date"].as_str().unwrap();
                assert_eq!(date, expected.trim());
                assert_eq!(row["ts"], 0.0);
                ids.push(row["id"].as_u64().unwrap());
            }
        }
        ids.sort_unstable();
        assert_eq!(ids, (0..21).collect::<Vec<_>>());
    }

    #[test]
    fn rotation_preserves_existing_logs_and_reports_errors() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("grinch-log-collisions-{nanos}"));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("active.log");
        let stamp = "2026-09-30T12-00-00";
        let previous = path.with_extension(format!("log.{stamp}"));
        let link = path.with_extension(format!("log.{stamp}.1"));
        std::fs::write(&previous, "previous session\n").unwrap();
        std::os::unix::fs::symlink(&previous, &link).unwrap();

        for number in 2..22 {
            let line = format!("record-{number}\n");
            std::fs::write(&path, &line).unwrap();
            rename_rotated_log(&path, stamp).unwrap();
            let rotated = path.with_extension(format!("log.{stamp}.{number}"));
            assert_eq!(std::fs::read_to_string(rotated).unwrap(), line);
            assert!(!path.exists());
        }
        assert_eq!(
            std::fs::read_to_string(&previous).unwrap(),
            "previous session\n"
        );
        assert_eq!(std::fs::read_link(&link).unwrap(), previous);
        assert_eq!(
            rename_rotated_log(&path, stamp).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            rename_rotated_log(std::path::Path::new("bad\0path"), stamp)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
