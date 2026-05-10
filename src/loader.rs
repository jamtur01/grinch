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

pub fn load_config() -> Option<LoadedConfig> {
    let (path, source) = match read_first_existing(&config_paths()) {
        Some(found) => found,
        None => {
            eprintln!(
                "grinch: no config at any of: ~/.grinch.js, ~/.config/grinch.js, \
                 ~/.config/grinch/grinch.js — create one"
            );
            return None;
        }
    };
    let path_str = path.display().to_string();

    let ctx: Retained<JSContext> = unsafe { JSContext::new() };

    // Exception handler: log the message + line, mark error so we abort the load.
    // The path is captured so users see which config file the error came from
    // (matters now that we accept two locations).
    let last_error = Rc::new(RefCell::new(false));
    {
        let last_error = last_error.clone();
        let path_for_handler = path_str.clone();
        let handler = RcBlock::new(move |_ctx_ptr: *mut JSContext, ex_ptr: *mut JSValue| {
            *last_error.borrow_mut() = true;
            if ex_ptr.is_null() {
                eprintln!("grinch: js error in {path_for_handler}: unknown");
                return;
            }
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
                eprintln!("grinch: js error in {path_for_handler}: {msg} (line {line})");
            }
        });
        unsafe { ctx.setExceptionHandler(Some(&handler)) };
    }

    if eval(&ctx, JS_PRELUDE).is_none() || *last_error.borrow() {
        return None;
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
            return None;
        }
    };
    let wrapped = wrap_user_config(&preprocessed);
    if eval(&ctx, &wrapped).is_none() || *last_error.borrow() {
        return None;
    }

    // Pull __grinchModule.exports off the global object.
    let module_key = NSString::from_str("__grinchModule");
    let module_ref: &AnyObject = &module_key;
    let module = unsafe { ctx.objectForKeyedSubscript(Some(module_ref)) }?;
    let exports_key = NSString::from_str("exports");
    let exports_ref: &AnyObject = &exports_key;
    let exports = unsafe { module.objectForKeyedSubscript(Some(exports_ref)) }?;
    if unsafe { exports.isUndefined() } || unsafe { exports.isNull() } {
        eprintln!("grinch: config did not export anything (use module.exports = {{...}})");
        return None;
    }

    Some(LoadedConfig { exports, ctx })
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

fn read_first_existing(paths: &[PathBuf]) -> Option<(PathBuf, String)> {
    for path in paths {
        if let Ok(source) = std::fs::read_to_string(path) {
            return Some((path.clone(), source));
        }
    }
    None
}
