// Auto-split from the former monolithic engine.rs. Child of `engine`, so
// `use super::*;` pulls in the shared types, std imports, and the sibling
// modules' items that `engine` re-exports via `pub(crate) use`.
use super::*;

/// Install a block as `__grinchFetchWindowTitle` on the JSContext. The block
/// reads CURRENT_OPENER_PID (set by resolve()) and calls into the AX API.
/// Lazy: the JS getter on opener.windowTitle only invokes this when user code
/// reads it, so configs that don't touch windowTitle pay nothing.
/// Install five `__grinchConsole*` blocks that the prelude wires up to
/// `console.log/warn/error/info/debug`. Each block takes a single
/// already-formatted string (the prelude joins varargs JS-side) and prints
/// it to stderr with a `grinch [level]:` prefix.
///
/// Called from the loader after the prelude evaluates but before user
/// config evaluates, so top-level `console.log()` calls in the user file
/// land on the wired blocks rather than the prelude's `typeof` no-op
/// fallback. Without this ordering, configs that call `console.log("…")`
/// at module scope got silent drops — debugging a non-firing rule was
/// painful.
/// Manual Obj-C block encoding for `void (^)(NSString *)`. JSC will only
/// auto-bridge a block to a JS function if it carries `_Block_signature`
/// metadata — objc2's `RcBlock::new` uses `NoBlockEncoding`, which omits
/// it. With this encoding string in place (`v16@?0@8` on 64-bit), JSC
/// reads the signature and exposes the block as a callable JS function;
/// without it, the block stays an opaque `NSBlock` and JS-side calls
/// throw "is not a function".
struct OneStringArgEncoding;

unsafe impl block2::ManualBlockEncoding for OneStringArgEncoding {
    type Arguments = (*mut NSString,);
    type Return = ();
    const ENCODING_CSTR: &'static std::ffi::CStr = if cfg!(target_pointer_width = "64") {
        c"v16@?0@8"
    } else {
        c"v8@?0@4"
    };
}

pub(crate) fn install_console_callbacks(ctx: &JSContext) {
    fn install(ctx: &JSContext, key: &str, level: &'static str) {
        let block =
            RcBlock::with_encoding::<_, _, _, OneStringArgEncoding>(move |msg: *mut NSString| {
                if msg.is_null() {
                    return;
                }
                // SAFETY: JSC owns the NSString; we just borrow it for one call.
                let s = unsafe { (*msg).to_string() };
                eprintln!("grinch [{level}]: {s}");
            });
        let block_ref: &block2::Block<_> = &block;
        let block_obj: &AnyObject = unsafe { &*(block_ref as *const _ as *const AnyObject) };
        let key_ns = NSString::from_str(key);
        let key_ref: &objc2_foundation::NSObject = &key_ns;
        unsafe {
            ctx.setObject_forKeyedSubscript(Some(block_obj), Some(key_ref));
        }
        drop(block);
    }
    install(ctx, "__grinchConsoleLog", "log");
    install(ctx, "__grinchConsoleWarn", "warn");
    install(ctx, "__grinchConsoleError", "error");
    install(ctx, "__grinchConsoleInfo", "info");
    install(ctx, "__grinchConsoleDebug", "debug");
}

/// Manual encoding for `NSString * (^)(void)` — block returning id, no
/// args. Same JSC reason as the console encoding: without a signature,
/// JSC sees an opaque NSBlock and JS-side `typeof` returns "object",
/// silently dropping the call. The previous implementation looked
/// correct but was effectively dead code; opener.windowTitle just
/// returned "" because the JS-side fallback (`typeof === "function"`)
/// failed.
struct ZeroArgIdReturnEncoding;

unsafe impl block2::ManualBlockEncoding for ZeroArgIdReturnEncoding {
    type Arguments = ();
    type Return = *mut NSString;
    const ENCODING_CSTR: &'static std::ffi::CStr = if cfg!(target_pointer_width = "64") {
        c"@8@?0"
    } else {
        c"@4@?0"
    };
}

/// Manual encoding for `NSString * (^)(NSString *)` — used for
/// `finicky.isAppRunning`'s underlying bridge (returns "1"/"0" so the
/// JS wrapper can coerce to boolean cheaply, no JSON parse needed).
struct OneStringArgIdReturnEncoding;

unsafe impl block2::ManualBlockEncoding for OneStringArgIdReturnEncoding {
    type Arguments = (*mut NSString,);
    type Return = *mut NSString;
    const ENCODING_CSTR: &'static std::ffi::CStr = if cfg!(target_pointer_width = "64") {
        c"@16@?0@8"
    } else {
        c"@8@?0@4"
    };
}

pub(crate) fn install_window_title_callback(ctx: &JSContext) {
    // Block return follows ARC's id-returning convention: autoreleased, not
    // +1 retained. JSC's Obj-C bridge calls objc_retainAutoreleasedReturnValue
    // on the result; pairing an autorelease here means the retain counts
    // balance. Returning Retained::into_raw (a +1 pointer) leaks the NSString
    // every time user code reads opener.windowTitle.
    let block = RcBlock::with_encoding::<_, _, _, ZeroArgIdReturnEncoding>(|| -> *mut NSString {
        let pid = CURRENT_OPENER_PID.load(Ordering::Relaxed);
        let title = frontmost_window_title(pid);
        Retained::autorelease_return(NSString::from_str(&title))
    });
    // SAFETY: A block is an Objective-C object (NSBlock). `&Block<F>` is
    // ABI-compatible with a block pointer, which is itself a valid `id`.
    // JSC accepts blocks as JS-callable functions via the standard objc bridge.
    let block_ref: &block2::Block<_> = &block;
    let block_obj: &AnyObject = unsafe { &*(block_ref as *const _ as *const AnyObject) };
    let key_ns = NSString::from_str("__grinchFetchWindowTitle");
    // JSContext::setObject_forKeyedSubscript takes the key as &NSObject
    // (NSCopying-typed historically), unlike the JSValue variant which takes
    // &AnyObject. NSString -> NSObject deref-coerces in argument position.
    let key_ref: &objc2_foundation::NSObject = &key_ns;
    unsafe {
        ctx.setObject_forKeyedSubscript(Some(block_obj), Some(key_ref));
    }
    // setObject_forKeyedSubscript copies the block into JSC's value table;
    // dropping our RcBlock here is safe — JSC keeps it alive for the lifetime
    // of the JSContext.
    drop(block);
}

/// Install Rust-side bridges for the `finicky.*` helpers that need access
/// to OS state. The JS-side `finicky` namespace (defined in the prelude)
/// wraps each one with a `typeof` guard and a parse-or-default fallback,
/// so configs run even on a JSContext where these aren't installed (e.g.
/// the integration-test fixture before it explicitly calls this fn).
///
/// All bridges return *strings* — JSON for the dict-shaped helpers,
/// "1"/"0" for the boolean. Returning NSDictionary directly would mean
/// constructing one Rust-side, which is more code than this is worth.
pub(crate) fn install_finicky_callbacks(ctx: &JSContext) {
    fn install_zero_arg_string(ctx: &JSContext, key: &str, body: impl Fn() -> String + 'static) {
        let block =
            RcBlock::with_encoding::<_, _, _, ZeroArgIdReturnEncoding>(move || -> *mut NSString {
                Retained::autorelease_return(NSString::from_str(&body()))
            });
        let block_ref: &block2::Block<_> = &block;
        let block_obj: &AnyObject = unsafe { &*(block_ref as *const _ as *const AnyObject) };
        let key_ns = NSString::from_str(key);
        let key_ref: &objc2_foundation::NSObject = &key_ns;
        unsafe {
            ctx.setObject_forKeyedSubscript(Some(block_obj), Some(key_ref));
        }
        drop(block);
    }

    fn install_one_arg_string(ctx: &JSContext, key: &str, body: impl Fn(&str) -> String + 'static) {
        let block = RcBlock::with_encoding::<_, _, _, OneStringArgIdReturnEncoding>(
            move |arg: *mut NSString| -> *mut NSString {
                let s = if arg.is_null() {
                    String::new()
                } else {
                    unsafe { (*arg).to_string() }
                };
                Retained::autorelease_return(NSString::from_str(&body(&s)))
            },
        );
        let block_ref: &block2::Block<_> = &block;
        let block_obj: &AnyObject = unsafe { &*(block_ref as *const _ as *const AnyObject) };
        let key_ns = NSString::from_str(key);
        let key_ref: &objc2_foundation::NSObject = &key_ns;
        unsafe {
            ctx.setObject_forKeyedSubscript(Some(block_obj), Some(key_ref));
        }
        drop(block);
    }

    install_zero_arg_string(ctx, "__grinchGetModifierKeys", || {
        let m = crate::workspace::current_modifier_flags();
        // `fn` and `function` carry the same value — Finicky uses both
        // names (with `function` as the v3-back-compat alias).
        format!(
            r#"{{"shift":{},"option":{},"command":{},"control":{},"capsLock":{},"fn":{},"function":{}}}"#,
            m.shift, m.option, m.command, m.control, m.caps_lock, m.function, m.function,
        )
    });

    install_one_arg_string(ctx, "__grinchIsAppRunning", |id| {
        // Mirrors Finicky: match against either bundle ID or localized
        // name (so `finicky.isAppRunning("Slack")` works in addition to
        // `finicky.isAppRunning("com.tinyspeck.slackmacgap")`).
        if crate::workspace::is_app_running(id) {
            "1".to_string()
        } else {
            "0".to_string()
        }
    });

    install_zero_arg_string(ctx, "__grinchGetSystemInfo", || {
        // [NSHost currentHost] gives the same two values Finicky exposes:
        //   - localizedName follows the user-set "Computer Name" (e.g.
        //     "James's MacBook Pro")
        //   - name is the canonical hostname (e.g. "jamtur01-mbp")
        // On a fresh Mac install both are the same; routing on either
        // is meaningful.
        let (localized, name) = crate::workspace::host_info();
        serde_json::json!({ "localizedName": localized, "name": name }).to_string()
    });

    install_zero_arg_string(ctx, "__grinchGetRunningBrowsers", || {
        // Intersect the running-apps snapshot with Grinch's known-browser
        // bundle ID tables (Chromium family + Firefox family + Safari).
        // The result is a JSON array of bundle IDs the user can compare
        // against — small enough to filter against a preference list in
        // JS without paying for repeated `isAppRunning` round-trips.
        //
        // Reads from the cached snapshot (`running_apps_cached`), which
        // the NSWorkspace launch/terminate observer invalidates on app
        // lifecycle events. A config calling getRunningBrowsers() on
        // every resolve would otherwise pay a 50-200-entry NSWorkspace
        // walk per click; the cache turns it into an Arc clone + hash
        // lookups.
        //
        // Result ordering follows the family-table order so configs that
        // pick "first running" get a stable answer across runs (Finicky
        // #145 was specifically "Chrome → Firefox → Safari fall-through",
        // composes via Array.prototype.find).
        let running = crate::workspace::running_apps_cached();
        let mut out: Vec<&str> = Vec::new();
        for (id, _) in crate::chromium::iter_family() {
            if running.contains(*id) {
                out.push(id);
            }
        }
        for (id, _) in crate::firefox::iter_family() {
            if running.contains(*id) {
                out.push(id);
            }
        }
        if running.contains("com.apple.Safari") {
            out.push("com.apple.Safari");
        }
        serde_json::json!(out).to_string()
    });

    install_zero_arg_string(ctx, "__grinchGetPowerInfo", || {
        // IOKit IOPSCopyPowerSourcesInfo would give real values, but the
        // call surface is heavy and most routing configs don't read this.
        // Return a sensible-shape stub; the JS wrapper logs an info note
        // the first time it's called so users know to file an issue if
        // they actually need this. `percentage: -1` matches Finicky's
        // unknown-battery sentinel (set by `info.percentage = -1` in
        // their info.m default), so configs that check
        // `if (powerInfo.percentage < 50)` get the same result on both.
        r#"{"isCharging":false,"isConnected":true,"percentage":-1}"#.to_string()
    });
}

/// Build a URL polyfill instance via `new URL(urlString)`. If the URL fails
/// to parse (e.g. exotic scheme), fall back to a plain object so user code
/// destructuring `{ href }` doesn't crash.
///
/// Returns `None` only when JSC is in an unrecoverable state (every
/// evaluateScript call fails, even on a 2-byte literal). Callers up the
/// chain (fn_args → resolve) treat None as "fn matcher doesn't match"
/// rather than panicking the daemon. Pre-fix, the bottom of this function
/// `.expect()`'d the final evaluateScript and would panic the whole
/// process on a per-resolve JSC OOM.
pub(crate) fn build_url_instance(
    url_ctor: &JSValue,
    ctx: &JSContext,
    url: &str,
) -> Option<Retained<JSValue>> {
    if let Some(url_str) = js_string(ctx, url) {
        let url_str_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(url_str) };
        let args = NSArray::from_retained_slice(&[url_str_obj]);
        if let Some(instance) = unsafe { url_ctor.constructWithArguments(Some(&args)) }
            && !unsafe { instance.isUndefined() }
            && !unsafe { instance.isNull() }
        {
            return Some(instance);
        }
    }
    // js_string failed (OOM) or `new URL(...)` returned undefined/null —
    // fall through to the stub-object path so user code can still
    // destructure { href } without crashing the resolve.
    let url_json = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".to_string());
    let stub_src = format!(
        "({{ href: {url_json}, protocol: '', hostname: '', pathname: '', search: '', hash: '' }})"
    );
    let stub_ns = NSString::from_str(&stub_src);
    if let Some(v) = unsafe { ctx.evaluateScript(Some(&stub_ns)) } {
        return Some(v);
    }
    // Last-ditch: a literal empty object. If even this fails, JSC is
    // unable to evaluate anything — propagate None so the resolve path
    // skips this fn matcher without panicking.
    unsafe { ctx.evaluateScript(Some(&NSString::from_str("({})"))) }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_ctx_object(
    ctx: &JSContext,
    helper: &JSValue,
    opener_str_cache: &RefCell<std::collections::HashMap<String, Retained<JSValue>>>,
    js_true: &Retained<JSValue>,
    js_false: &Retained<JSValue>,
    url: &str,
    opener: &Opener,
    m: ModifierFlags,
) -> Option<Retained<JSValue>> {
    // URL changes per resolve (or per rewrite); not worth caching across
    // resolves. Opener fields stabilise (same Mail / Slack / Outlook over
    // and over) → engine's opener_str_cache. Modifier flags are bools and
    // we hold a single cached Retained<JSValue> per truth value on the
    // Engine — clones here are refcount bumps, not JSC bridge crossings.
    let bool_v =
        |b: bool| -> Retained<JSValue> { if b { js_true.clone() } else { js_false.clone() } };
    // js_string / cached_js_string return None on JSC OOM. Propagate
    // via `?` to the function's Option return; the caller treats that
    // as "fn matcher won't match" and continues with the next rule.
    let url_v = js_string(ctx, url)?;
    let opener_id_v = cached_js_string(ctx, opener_str_cache, &opener.bundle_id)?;
    let opener_name_v = cached_js_string(ctx, opener_str_cache, &opener.name)?;
    let opener_path_v = cached_js_string(ctx, opener_str_cache, &opener.path)?;
    // Fixed-size array (was a heap-allocated Vec<Retained<AnyObject>>).
    // NSArray::from_retained_slice takes a `&[Retained<T>]` so the array
    // coerces cleanly; no allocation between us and JSC's NSArray copy.
    let args_objs: [Retained<AnyObject>; 10] = [
        unsafe { Retained::cast_unchecked(url_v) },
        unsafe { Retained::cast_unchecked(opener_id_v) },
        unsafe { Retained::cast_unchecked(opener_name_v) },
        unsafe { Retained::cast_unchecked(opener_path_v) },
        unsafe { Retained::cast_unchecked(bool_v(m.shift)) },
        unsafe { Retained::cast_unchecked(bool_v(m.option)) },
        unsafe { Retained::cast_unchecked(bool_v(m.command)) },
        unsafe { Retained::cast_unchecked(bool_v(m.control)) },
        unsafe { Retained::cast_unchecked(bool_v(m.caps_lock)) },
        unsafe { Retained::cast_unchecked(bool_v(m.function)) },
    ];
    let args = NSArray::from_retained_slice(&args_objs);
    let result = unsafe { helper.callWithArguments(Some(&args)) };
    if result.is_none() {
        // Helper returned null (likely the user's config replaced or broke
        // the prelude). Warn once and let the caller fall through — the fn
        // matcher/rewriter that needed this ctx will simply not match.
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "grinch: __grinchMakeCtx returned null — fn matchers won't match \
                 until the config is fixed (the prelude helper appears to have been \
                 overridden)."
            );
        }
    }
    result
}

/// Look up a property by name. Returns None for missing/undefined fields so
/// callers can use `.or_else` chains and pattern-match on Some(value).
/// Explicit `null` (e.g. `open: null`) returns Some(null_value) — distinguishable
/// via `.isNull()`.
pub(crate) fn key(v: &JSValue, name: &str) -> Option<Retained<JSValue>> {
    if !unsafe { v.isObject() } {
        return None;
    }
    let key_ns = NSString::from_str(name);
    let key_ref: &AnyObject = &key_ns;
    let result = unsafe { v.objectForKeyedSubscript(Some(key_ref)) }?;
    if unsafe { result.isUndefined() } {
        return None;
    }
    Some(result)
}

pub(crate) fn is_undef_or_null(v: &JSValue) -> bool {
    unsafe { v.isUndefined() || v.isNull() }
}

pub(crate) fn js_to_string(v: &JSValue) -> Option<String> {
    let s = unsafe { v.toString() }?;
    Some(s.to_string())
}

pub(crate) fn js_array_len(v: &JSValue) -> usize {
    let len = key(v, "length");
    len.map(|n| unsafe { n.toInt32() } as usize).unwrap_or(0)
}

pub(crate) fn js_array_at(v: &JSValue, i: usize) -> Option<Retained<JSValue>> {
    unsafe { v.valueAtIndex(i) }
}

pub(crate) fn js_array_to_strings(v: &JSValue) -> Vec<String> {
    let count = js_array_len(v);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        if let Some(item) = js_array_at(v, i)
            && let Some(s) = js_to_string(&item)
        {
            out.push(s);
        }
    }
    out
}

/// One-call JSValue type classification via the JSC C API
/// (`JSValueGetType`). Replaces a sequence of `isNull()` + `isUndefined()`
/// Obj-C dispatches with a single C call on the hot path; saves ~50–100 ns
/// per fn return check, which compounds when a config has multiple fn
/// matchers (each unmatched matcher's result goes through this).
#[inline]
pub(crate) fn js_value_type(ctx: &JSContext, v: &JSValue) -> JSType {
    unsafe { JSValue::r#type(ctx.JSGlobalContextRef(), v.JSValueRef()) }
}

/// Read a string property from a JSValue object and return it only when
/// the value is *actually* a non-empty string (not `undefined`, not a
/// stringified other type). Used in the fn-rewriter fast path to extract
/// `.href` from URL polyfill instances without crossing into the
/// `__grinchRewriteResult` JS helper. None on missing/wrong-type/empty.
///
/// The JSType filter is load-bearing: `objectForKeyedSubscript` on a
/// missing property returns a JSValue of type `undefined`, which would
/// otherwise `toString()` into the literal "undefined" — and routing
/// "undefined" as a URL is exactly the kind of bug an opaque fast path
/// is prone to.
pub(crate) fn read_nonempty_string_property(
    ctx: &JSContext,
    v: &JSValue,
    key: &str,
) -> Option<String> {
    let key_ns = NSString::from_str(key);
    let key_ref: &AnyObject = &key_ns;
    let prop = unsafe { v.objectForKeyedSubscript(Some(key_ref)) }?;
    // Property access can trigger a throwing getter — JSC stashes the
    // thrown value on `ctx.exception` and returns a JS-undefined here.
    // The type check below correctly rejects the undefined, but the
    // exception state would persist through any subsequent JSC call
    // in the same resolve (next matcher, next rewriter), producing
    // confusing "matcher mysteriously returned false" symptoms. Clear
    // it so downstream calls see a fresh context.
    if unsafe { ctx.exception() }.is_some() {
        unsafe { ctx.setException(None) };
        return None;
    }
    if js_value_type(ctx, &prop) != JSType::String {
        return None;
    }
    let s = unsafe { prop.toString() }?.to_string();
    if s.is_empty() { None } else { Some(s) }
}

pub(crate) fn js_string(ctx: &JSContext, s: &str) -> Option<Retained<JSValue>> {
    let ns = NSString::from_str(s);
    let any: &AnyObject = &ns;
    // `valueWithObject_inContext` returns Option in the bindings and
    // documented as infallible by Apple, but JSC can return null under
    // hard memory pressure. Returning Option lets the resolve hot path
    // suppress the click cleanly (matcher returns false, ctx build
    // returns None, engine continues) instead of panicking the process.
    unsafe { JSValue::valueWithObject_inContext(Some(any), Some(ctx)) }
}

/// Soft cap on per-cache entry counts. The interning caches in Grinch
/// are bounded in practice by the number of distinct apps that send
/// URLs (≤ a few dozen on any real machine), but a config whose dynamic
/// `open` fn or opener path varies per click could grow them without
/// bound. Stop *inserting* once the map crosses this threshold so the
/// cache size plateaus at a known limit; misses past the threshold pay
/// the lookup cost but the daemon can't be made to OOM via cache growth.
pub(crate) const STRING_CACHE_SOFT_CAP: usize = 1024;

/// Cached `js_string` keyed by the Rust `&str`. Cache hit returns a
/// refcount bump; miss allocates the JSValue and stores it. Used for
/// strings that repeat across resolves (opener fields), not per-call
/// inputs (URL).
pub(crate) fn cached_js_string(
    ctx: &JSContext,
    cache: &RefCell<std::collections::HashMap<String, Retained<JSValue>>>,
    s: &str,
) -> Option<Retained<JSValue>> {
    if let Some(v) = cache.borrow().get(s) {
        return Some(v.clone());
    }
    let v = js_string(ctx, s)?;
    // Insertion-guard: don't grow past the soft cap. Past the cap, hot
    // entries (already in the map) keep returning refcount bumps; cold
    // entries fall through and rebuild every time, which is fine — the
    // realistic ceiling on opener identities is in the dozens.
    let mut cache_mut = cache.borrow_mut();
    if cache_mut.len() < STRING_CACHE_SOFT_CAP {
        cache_mut.insert(s.to_string(), v.clone());
    }
    Some(v)
}

pub(crate) fn js_bool(ctx: &JSContext, b: bool) -> Option<Retained<JSValue>> {
    // Same OOM rationale as js_string. Engine::new propagates failure
    // here as EngineError::PreludeBroken; per-resolve callers can `?`
    // through to the build_ctx_object Option.
    unsafe { JSValue::valueWithBool_inContext(b, Some(ctx)) }
}

pub(crate) unsafe fn eval_global(ctx: &JSContext, name: &str) -> Option<Retained<JSValue>> {
    let key_ns = NSString::from_str(name);
    let key_ref: &AnyObject = &key_ns;
    unsafe { ctx.objectForKeyedSubscript(Some(key_ref)) }
}

/// Like `eval_global` but treats missing / null / undefined values as a
/// `PreludeBroken` error. Used during engine init for the constructors
/// and prelude helpers we need; the call sites would otherwise propagate
/// a null `Retained<JSValue>` into downstream `isInstanceOf` / call
/// operations and produce opaque "null is not an object" stderr per
/// click without ever failing the load.
pub(crate) fn require_global(
    ctx: &JSContext,
    name: &'static str,
) -> Result<Retained<JSValue>, EngineError> {
    let v = unsafe { eval_global(ctx, name) }.ok_or(EngineError::PreludeBroken { global: name })?;
    if unsafe { v.isNull() } || unsafe { v.isUndefined() } {
        return Err(EngineError::PreludeBroken { global: name });
    }
    Ok(v)
}

pub(crate) fn is_function(v: &JSValue, function_ctor: &JSValue) -> bool {
    let any: &AnyObject = function_ctor;
    unsafe { v.isInstanceOf(Some(any)) }
}

pub(crate) fn is_instance_of(v: &JSValue, ctor: &JSValue) -> bool {
    let any: &AnyObject = ctor;
    unsafe { v.isInstanceOf(Some(any)) }
}

pub(crate) fn is_marker(v: &JSValue, ty: &str) -> bool {
    if !unsafe { v.isObject() } {
        return false;
    }
    let Some(t) = key(v, "__type") else {
        return false;
    };
    js_to_string(&t).as_deref() == Some(ty)
}

/// Iterate the keys of a JS object as Rust strings, returning (key, value).
///
/// Uses `Object.keys(v)` rather than `v.toDictionary()`. The dictionary
/// path recursively converts every value to its NS* equivalent, which
/// stack-overflows on a circular config like `var x = {}; x.self = x;
/// module.exports = { browsers: x };`. `Object.keys` returns only the
/// own enumerable property *names* — no value walk — so circular values
/// are safe; we re-fetch each value via subscript afterwards (one JSC
/// bridge crossing per key, fine because this is engine-init only).
pub(crate) fn iter_object(v: &JSValue) -> Vec<(String, Retained<JSValue>)> {
    if !unsafe { v.isObject() } {
        return vec![];
    }
    let Some(ctx) = (unsafe { v.context() }) else {
        return vec![];
    };
    let Some(object_ctor) = (unsafe { eval_global(&ctx, "Object") }) else {
        return vec![];
    };
    let Some(keys_fn) = key(&object_ctor, "keys") else {
        return vec![];
    };
    let v_clone: Retained<AnyObject> = unsafe { Retained::cast_unchecked(v.retain()) };
    let args = NSArray::from_retained_slice(&[v_clone]);
    let Some(keys_array) = (unsafe { keys_fn.callWithArguments(Some(&args)) }) else {
        return vec![];
    };
    let Some(length_jsv) = key(&keys_array, "length") else {
        return vec![];
    };
    let length = unsafe { length_jsv.toUInt32() } as usize;
    let mut out = Vec::with_capacity(length);
    for i in 0..length {
        let Some(name_jsv) = (unsafe { keys_array.valueAtIndex(i) }) else {
            continue;
        };
        let Some(name_ns) = (unsafe { name_jsv.toString() }) else {
            continue;
        };
        let name = name_ns.to_string();
        if let Some(val) = key(v, &name) {
            out.push((name, val));
        }
    }
    out
}
