// Loads the user's grinch config, evaluates it in a JSContext with helpers +
// URL polyfill pre-injected, and returns the module.exports JSValue plus the
// context that owns it (must be kept alive — JSValues retain their context).
//
// Four config locations are checked, in order. First file found wins:
//   1. ~/.grinch.js                                    (legacy/dotfile)
//   2. ~/.config/grinch.js                             (flat XDG)
//   3. ~/.config/grinch/grinch.js                      (XDG subdir, Finicky-style)
//   4. /Library/Application Support/Grinch/grinch.js   (system-wide / MDM)
// The XDG subdir form is for users who keep one folder per tool under
// ~/.config. The system-wide path is last so user configs always win;
// it's there so MDM-managed machines can ship a baseline config without
// per-user provisioning.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSString, NSURL};
use objc2_javascript_core::{JSContext, JSValue};

use crate::engine::{DiagnosticLog, js_exception_detail};
use crate::helpers::{JS_PRELUDE, preprocess_es_module_syntax, wrap_user_config};

pub(crate) const CONFIG_PATH_HINT: &str = "~/.grinch.js, ~/.config/grinch.js, \
    ~/.config/grinch/grinch.js, or /Library/Application Support/Grinch/grinch.js (system-wide)";

pub struct LoadedConfig {
    pub exports: Retained<JSValue>,
    // Context owns all JSValues; must outlive the engine.
    pub ctx: Retained<JSContext>,
    pub path: PathBuf,
    pub diagnostics: Rc<DiagnosticLog>,
    pub(crate) checks: KeyCheckers,
}

/// Config-field checks retained before user code can replace their globals.
pub(crate) struct KeyCheckers {
    pub(crate) rule: Option<Retained<JSValue>>,
    pub(crate) browser: Option<Retained<JSValue>>,
}

impl KeyCheckers {
    pub(crate) fn capture(ctx: &JSContext) -> Self {
        fn get(ctx: &JSContext, name: &str) -> Option<Retained<JSValue>> {
            let name = NSString::from_str(name);
            unsafe { ctx.objectForKeyedSubscript(Some(&*name)) }
        }
        Self {
            rule: get(ctx, "__grinchUnknownRuleKeys"),
            browser: get(ctx, "__grinchUnknownBrowserKeys"),
        }
    }
}

/// Returns the path to the config file the loader would (or did) read,
/// regardless of whether evaluation succeeds. Used by the menu's "Open
/// Config" action so the user can fix a broken config from inside the app.
pub fn find_config_path() -> Option<PathBuf> {
    config_paths().into_iter().find(|p| match p.metadata() {
        Ok(metadata) => metadata.is_file(),
        Err(error) => !missing_path(&error),
    })
}

pub fn load_config(diagnostics: Rc<DiagnosticLog>) -> Result<LoadedConfig, String> {
    let (path, source) = match read_first_existing(&config_paths()) {
        ReadOutcome::Found { path, source } => (path, source),
        ReadOutcome::Unreadable { path, error } => {
            // Distinguish "config exists but we can't read it" (permission
            // denied, non-UTF-8 contents, mid-read I/O failure) from "no
            // config at any of the candidate paths". The previous code
            // collapsed both into the latter, leaving users staring at
            // a "no config found" message while their config sat right
            // there at the path it claimed didn't exist.
            let msg = format!("couldn't read config at {}: {error}", path.display());
            eprintln!("grinch: {msg}");
            return Err(record_config_error(&diagnostics, Some(&path), msg));
        }
        ReadOutcome::Missing => {
            let msg = format!("no config at any of: {CONFIG_PATH_HINT} — create one");
            eprintln!("grinch: {msg}");
            return Err(record_config_error(&diagnostics, None, msg));
        }
    };
    evaluate_config(path, source, diagnostics)
}

fn evaluate_config(
    path: PathBuf,
    source: String,
    diagnostics: Rc<DiagnosticLog>,
) -> Result<LoadedConfig, String> {
    let path_str = path.display().to_string();

    let ctx: Retained<JSContext> = unsafe { JSContext::new() };

    // Exception handler: capture the first JS error so callers can surface
    // it (menu bar, log file). Also logs to stderr — invisible when stderr
    // is wired to /dev/null (LaunchServices-launched daemons), but useful
    // when grinch is run from a terminal.
    let last_error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    {
        let last_error = last_error.clone();
        let path_for_handler = path_str.clone();
        let handler = RcBlock::new(move |_ctx_ptr: *mut JSContext, ex_ptr: *mut JSValue| {
            let detail = unsafe { js_exception_detail(ex_ptr) };
            eprintln!("grinch: js error in {path_for_handler}: {detail}");
            // First error wins — chained exceptions during a single load
            // typically all stem from the first parse failure.
            let mut slot = last_error.borrow_mut();
            if slot.is_none() {
                *slot = Some(detail);
            }
        });
        unsafe { ctx.setExceptionHandler(Some(&handler)) };
    }

    let take_error = |fallback: &str| -> String {
        last_error
            .borrow_mut()
            .take()
            .unwrap_or_else(|| fallback.to_string())
    };

    if eval(&ctx, JS_PRELUDE).is_none() || last_error.borrow().is_some() {
        let msg = take_error("prelude eval failed");
        return Err(record_config_error(&diagnostics, Some(&path), msg));
    }
    let checks = KeyCheckers::capture(&ctx);
    // Console blocks must be installed BEFORE the user config evaluates so
    // top-level `console.log("…")` calls land on the wired blocks, not the
    // prelude's `typeof` no-op fallback. Same ordering applies to the
    // finicky.* bridges (getModifierKeys / isAppRunning / etc.).
    crate::engine::install_console_callbacks(&ctx);
    crate::engine::install_finicky_callbacks(&ctx);

    // Rewrite Finicky-v4-style `export default { … }` into the CommonJS
    // form JSC's evaluateScript accepts. Unsupported ESM shapes (`import`,
    // named exports) get a config-load error pointing at module.exports.
    let preprocessed = match preprocess_es_module_syntax(&source) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("grinch: js error in {path_str}: {msg}");
            return Err(record_config_error(&diagnostics, Some(&path), msg));
        }
    };
    let wrapped = wrap_user_config(&preprocessed);
    if eval(&ctx, &wrapped).is_none() || last_error.borrow().is_some() {
        let msg = take_error("config eval failed");
        return Err(record_config_error(&diagnostics, Some(&path), msg));
    }

    // Pull __grinchModule.exports off the global object.
    let module_key = NSString::from_str("__grinchModule");
    let module_ref: &AnyObject = &module_key;
    let Some(module) = (unsafe { ctx.objectForKeyedSubscript(Some(module_ref)) }) else {
        let msg = "__grinchModule missing from global".to_string();
        eprintln!("grinch: {msg}");
        return Err(record_config_error(&diagnostics, Some(&path), msg));
    };
    let exports_key = NSString::from_str("exports");
    let exports_ref: &AnyObject = &exports_key;
    let Some(exports) = (unsafe { module.objectForKeyedSubscript(Some(exports_ref)) }) else {
        let msg = "__grinchModule.exports missing".to_string();
        eprintln!("grinch: {msg}");
        return Err(record_config_error(&diagnostics, Some(&path), msg));
    };
    if unsafe { exports.isUndefined() } || unsafe { exports.isNull() } {
        let msg = "config did not export anything (use module.exports = {...})".to_string();
        eprintln!("grinch: {msg}");
        return Err(record_config_error(&diagnostics, Some(&path), msg));
    }

    // Engine compilation has recoverable JSC fallbacks of its own. Keep that
    // phase quiet, then let Engine::new install the runtime diagnostic handler
    // only after compilation succeeds.
    install_quiet_exception_handler(&ctx);

    Ok(LoadedConfig {
        exports,
        ctx,
        path,
        diagnostics,
        checks,
    })
}

fn install_quiet_exception_handler(ctx: &JSContext) {
    let handler = RcBlock::new(|_ctx_ptr: *mut JSContext, _exception: *mut JSValue| {});
    unsafe { ctx.setExceptionHandler(Some(&handler)) };
}

fn record_config_error(
    diagnostics: &DiagnosticLog,
    path: Option<&Path>,
    message: String,
) -> String {
    diagnostics.record_config_error(path, &message);
    message
}

fn eval(ctx: &JSContext, script: &str) -> Option<Retained<JSValue>> {
    let s = NSString::from_str(script);
    let url = NSURL::fileURLWithPath(&NSString::from_str("grinch-config.js"));
    unsafe { ctx.evaluateScript_withSourceURL(Some(&s), Some(&url)) }
}

/// System-wide config location. Last in the search order so user paths
/// always win, but present so MDM-managed Macs can drop a baseline config
/// here and have Grinch pick it up without per-user setup.
const SYSTEM_CONFIG_PATH: &str = "/Library/Application Support/Grinch/grinch.js";

fn config_paths() -> Vec<PathBuf> {
    config_paths_for(std::env::var("HOME").ok().as_deref())
}

fn config_paths_for(home: Option<&str>) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(4);
    if let Some(home) = home {
        let home = PathBuf::from(home);
        paths.push(home.join(".grinch.js"));
        paths.push(home.join(".config/grinch.js"));
        paths.push(home.join(".config/grinch/grinch.js"));
    }
    paths.push(PathBuf::from(SYSTEM_CONFIG_PATH));
    paths
}

enum ReadOutcome {
    Found {
        path: PathBuf,
        source: String,
    },
    /// A candidate path exists on disk but reading it failed (permission
    /// denied, non-UTF-8 bytes, IO error mid-read). Surfaces a specific
    /// error rather than the misleading "no config found" message.
    Unreadable {
        path: PathBuf,
        error: std::io::Error,
    },
    Missing,
}

fn read_first_existing(paths: &[PathBuf]) -> ReadOutcome {
    for path in paths {
        match path.metadata() {
            Ok(metadata) if !metadata.is_file() => continue,
            Ok(_) => {}
            Err(error) if missing_path(&error) => continue,
            Err(error) => {
                return ReadOutcome::Unreadable {
                    path: path.clone(),
                    error,
                };
            }
        }
        match std::fs::read_to_string(path) {
            Ok(source) => {
                return ReadOutcome::Found {
                    path: path.clone(),
                    source,
                };
            }
            Err(error) => {
                return ReadOutcome::Unreadable {
                    path: path.clone(),
                    error,
                };
            }
        }
    }
    ReadOutcome::Missing
}

fn missing_path(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
        || error.kind() == std::io::ErrorKind::NotADirectory
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_first_config_does_not_fall_back() {
        let root = test_path("unreadable");
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("first.js");
        let second = root.join("second.js");
        std::fs::write(&first, [0xff]).unwrap();
        std::fs::write(&second, "module.exports = {};").unwrap();
        match read_first_existing(&[first.clone(), second]) {
            ReadOutcome::Unreadable { path, .. } => assert_eq!(path, first),
            _ => panic!("expected the first config to fail"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_lookup_skips_paths_beneath_a_regular_file() {
        let root = test_path("not-directory");
        std::fs::create_dir_all(&root).unwrap();
        let parent = root.join("file");
        let fallback = root.join("fallback.js");
        std::fs::write(&parent, "file").unwrap();
        std::fs::write(&fallback, "fallback").unwrap();
        match read_first_existing(&[parent.join("config.js"), fallback.clone()]) {
            ReadOutcome::Found { path, source } => {
                assert_eq!(path, fallback);
                assert_eq!(source, "fallback");
            }
            _ => panic!("expected fallback config"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    fn test_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let value = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "grinch-loader-{}-{}-{value}",
            std::process::id(),
            name
        ))
    }

    #[test]
    fn config_syntax_error_is_recorded_in_diagnostic_log() {
        let root = test_path("syntax-error");
        let config_path = root.join("grinch.js");
        let log_path = root.join("diagnostic.log");
        let diagnostics = Rc::new(DiagnosticLog::at_path(log_path.clone()));
        let result = evaluate_config(
            config_path.clone(),
            "module.exports = { default: ; };".to_string(),
            diagnostics,
        );

        let error = result.err().expect("invalid JavaScript should fail");
        assert!(error.contains("SyntaxError"));
        let body = std::fs::read_to_string(&log_path).expect("diagnostic log should exist");
        let event: serde_json::Value =
            serde_json::from_str(body.trim()).expect("config error should be JSON");
        assert_eq!(event["event"], "config_error");
        assert_eq!(event["path"], config_path.display().to_string());
        assert!(event["message"].as_str().unwrap().contains("SyntaxError"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn config_paths_search_order() {
        let with_home = config_paths_for(Some("/Users/testuser"));
        assert_eq!(with_home.len(), 4);
        assert_eq!(with_home[0], PathBuf::from("/Users/testuser/.grinch.js"));
        assert_eq!(
            with_home[1],
            PathBuf::from("/Users/testuser/.config/grinch.js")
        );
        assert_eq!(
            with_home[2],
            PathBuf::from("/Users/testuser/.config/grinch/grinch.js")
        );
        assert_eq!(
            with_home[3],
            PathBuf::from("/Library/Application Support/Grinch/grinch.js"),
            "system path must be last so user paths win"
        );

        let without_home = config_paths_for(None);
        assert_eq!(without_home.len(), 1);
        assert_eq!(
            without_home[0],
            PathBuf::from("/Library/Application Support/Grinch/grinch.js")
        );
    }
}
