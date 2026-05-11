// Loads the user's grinch config, evaluates it in a JSContext with helpers +
// URL polyfill pre-injected, and returns the module.exports JSValue plus the
// context that owns it (must be kept alive — JSValues retain their context).
//
// Three config locations are checked, in order. First file found wins:
//   1. ~/.grinch.js                         (legacy/dotfile)
//   2. ~/.config/grinch.js                  (flat XDG)
//   3. ~/.config/grinch/grinch.js           (XDG subdir, mirrors Finicky)
// The subdir form is for users who keep one folder per tool under ~/.config.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSString, NSURL};
use objc2_javascript_core::{JSContext, JSValue};

use crate::helpers::{preprocess_es_module_syntax, wrap_user_config, JS_PRELUDE};

pub struct LoadedConfig {
    pub exports: Retained<JSValue>,
    // Context owns all JSValues; must outlive the engine.
    pub ctx: Retained<JSContext>,
}

/// Returns the path to the config file the loader would (or did) read,
/// regardless of whether evaluation succeeds. Used by the menu's "Open
/// Config" action so the user can fix a broken config from inside the app.
pub fn find_config_path() -> Option<PathBuf> {
    config_paths().into_iter().find(|p| p.is_file())
}

pub fn load_config() -> Result<LoadedConfig, String> {
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
            return Err(msg);
        }
        ReadOutcome::Missing => {
            let msg = "no config at any of: ~/.grinch.js, ~/.config/grinch.js, \
                       ~/.config/grinch/grinch.js — create one"
                .to_string();
            eprintln!("grinch: {msg}");
            return Err(msg);
        }
    };
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
            let detail = if ex_ptr.is_null() {
                "unknown".to_string()
            } else {
                unsafe {
                    let ex = &*ex_ptr;
                    let msg = ex
                        .toString()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let line_key = NSString::from_str("line");
                    let line_ref: &AnyObject = &line_key;
                    let line = ex
                        .objectForKeyedSubscript(Some(line_ref))
                        .and_then(|v| v.toString())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    format!("{msg} (line {line})")
                }
            };
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
        return Err(take_error("prelude eval failed"));
    }
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
            return Err(msg);
        }
    };
    let wrapped = wrap_user_config(&preprocessed);
    if eval(&ctx, &wrapped).is_none() || last_error.borrow().is_some() {
        return Err(take_error("config eval failed"));
    }

    // Pull __grinchModule.exports off the global object.
    let module_key = NSString::from_str("__grinchModule");
    let module_ref: &AnyObject = &module_key;
    let module = unsafe { ctx.objectForKeyedSubscript(Some(module_ref)) }
        .ok_or_else(|| "__grinchModule missing from global".to_string())?;
    let exports_key = NSString::from_str("exports");
    let exports_ref: &AnyObject = &exports_key;
    let exports = unsafe { module.objectForKeyedSubscript(Some(exports_ref)) }
        .ok_or_else(|| "__grinchModule.exports missing".to_string())?;
    if unsafe { exports.isUndefined() } || unsafe { exports.isNull() } {
        let msg = "config did not export anything (use module.exports = {...})".to_string();
        eprintln!("grinch: {msg}");
        return Err(msg);
    }

    // Swap the loud load-time exception handler for a quiet resolve-time
    // one. The load-time handler logs every JS exception with file/line —
    // useful for catching syntax errors and broken-helper exports, but
    // catastrophic during resolve(): a single malformed URL like
    // `https://x/?key=%ZZ` makes user fn matchers throw on every click,
    // and the loud handler then floods stderr with one
    // `grinch: js error in <path>: URI malformed` per click, forever.
    //
    // The replacement no-ops by default; user fn matchers that throw
    // still produce a None result inside the engine and silently fail
    // to match (same as before). Set GRINCH_DEBUG=1 to re-enable
    // per-exception logging when chasing a bad rule.
    install_resolve_exception_handler(&ctx, path_str.clone());

    Ok(LoadedConfig { exports, ctx })
}

fn install_resolve_exception_handler(ctx: &JSContext, path: String) {
    let debug = std::env::var("GRINCH_DEBUG").is_ok();
    let handler = RcBlock::new(move |_ctx_ptr: *mut JSContext, ex_ptr: *mut JSValue| {
        if !debug {
            return;
        }
        if ex_ptr.is_null() {
            eprintln!("grinch: js error during resolve in {path}: unknown");
            return;
        }
        unsafe {
            let ex = &*ex_ptr;
            let msg = ex
                .toString()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            eprintln!("grinch: js error during resolve in {path}: {msg}");
        }
    });
    unsafe { ctx.setExceptionHandler(Some(&handler)) };
}

fn eval(ctx: &JSContext, script: &str) -> Option<Retained<JSValue>> {
    let s = NSString::from_str(script);
    let url = NSURL::fileURLWithPath(&NSString::from_str("grinch-config.js"));
    unsafe { ctx.evaluateScript_withSourceURL(Some(&s), Some(&url)) }
}

fn config_paths() -> Vec<PathBuf> {
    let Ok(home) = std::env::var("HOME") else {
        return vec![];
    };
    let home = PathBuf::from(home);
    vec![
        home.join(".grinch.js"),
        home.join(".config/grinch.js"),
        home.join(".config/grinch/grinch.js"),
    ]
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
    let mut first_unreadable: Option<(PathBuf, std::io::Error)> = None;
    for path in paths {
        if !path.is_file() {
            continue;
        }
        match std::fs::read_to_string(path) {
            Ok(source) => {
                return ReadOutcome::Found {
                    path: path.clone(),
                    source,
                };
            }
            Err(error) => {
                if first_unreadable.is_none() {
                    first_unreadable = Some((path.clone(), error));
                }
            }
        }
    }
    if let Some((path, error)) = first_unreadable {
        ReadOutcome::Unreadable { path, error }
    } else {
        ReadOutcome::Missing
    }
}
