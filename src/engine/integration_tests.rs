//! End-to-end tests that build a real `Engine` from a JS config string,
//! then exercise `resolve()` with synthetic openers and modifiers. The
//! fixture (`build_engine`) creates a fresh `JSContext` per test so
//! parallel test execution doesn't share JS-side state.
//!
//! These tests cover the parse + resolve pipeline (matchers, rewriters,
//! targets, browser specs, ctx semantics, URL polyfill, fn-arity skip)
//! that the pure-Rust unit tests in `mod tests` above can't reach
//! without a JSC fixture.
use super::*;
use crate::helpers::{JS_PRELUDE, wrap_user_config};
use crate::loader::LoadedConfig;
use crate::workspace::Opener;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::NSString;
use objc2_javascript_core::JSContext;
use std::path::PathBuf;
use std::rc::Rc;

/// Build an `Engine` from a JS config source. Each call gets its own
/// `JSContext` (and its own JavaScriptCore VM) so two parallel tests
/// can't see each other's globals. Panics on any JSC error — caller's
/// job to keep the synthetic config valid.
pub(crate) fn build_engine(user_src: &str) -> Engine {
    try_build_engine(user_src).expect("engine init failed")
}

fn build_engine_with_diagnostics(user_src: &str, diagnostics: Rc<DiagnosticLog>) -> Engine {
    try_build_engine_with_diagnostics(user_src, diagnostics).expect("engine init failed")
}

/// Variant that returns the Result so tests can assert on
/// EngineError variants (e.g. PreludeBroken when a hostile config
/// trashes a prelude global).
fn try_build_engine(user_src: &str) -> Result<Engine, EngineError> {
    try_build_engine_with_diagnostics(user_src, Rc::new(isolated_diagnostics()))
}

pub(crate) fn isolated_diagnostics() -> DiagnosticLog {
    TEST_LOGS.with(|logs| DiagnosticLog::at_path(logs.0.join("diagnostic.log")))
}

thread_local! {
    static TEST_LOGS: TestLogs = TestLogs(unique_tmp("engine-test-logs"));
}

/// Each test thread owns its logs and removes them when it exits.
struct TestLogs(PathBuf);

impl Drop for TestLogs {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
        }
    }
}

fn try_build_engine_with_diagnostics(
    user_src: &str,
    diagnostics: Rc<DiagnosticLog>,
) -> Result<Engine, EngineError> {
    let ctx: Retained<JSContext> = unsafe { JSContext::new() };

    let prelude_ns = NSString::from_str(JS_PRELUDE);
    unsafe { ctx.evaluateScript(Some(&prelude_ns)) }.expect("prelude evaluation returned null");
    let checks = crate::loader::KeyCheckers::capture(&ctx);

    // Match the loader's ordering: install bridges between prelude eval
    // and user-config eval so top-level `console.log` / `finicky.*`
    // calls in the user source land on real Rust hooks.
    super::install_console_callbacks(&ctx);
    super::install_finicky_callbacks(&ctx);

    let wrapped = wrap_user_config(user_src);
    let wrapped_ns = NSString::from_str(&wrapped);
    unsafe { ctx.evaluateScript(Some(&wrapped_ns)) }.expect("user config evaluation returned null");

    let module_key = NSString::from_str("__grinchModule");
    let module_ref: &AnyObject = &module_key;
    let module = unsafe { ctx.objectForKeyedSubscript(Some(module_ref)) }
        .expect("__grinchModule missing from global");
    let exports_key = NSString::from_str("exports");
    let exports_ref: &AnyObject = &exports_key;
    let exports = unsafe { module.objectForKeyedSubscript(Some(exports_ref)) }
        .expect("__grinchModule.exports missing");

    Engine::new(LoadedConfig {
        exports,
        ctx,
        path: PathBuf::from("test-config.js"),
        diagnostics,
        checks,
    })
}

/// Synthetic opener for tests. `pid = 0` short-circuits any AX/IPC
/// lookups (see `frontmost_window_title`) so tests stay hermetic.
fn opener(bundle_id: &str, name: &str) -> Opener {
    Opener {
        bundle_id: bundle_id.to_string(),
        name: name.to_string(),
        path: String::new(),
        pid: 0,
    }
}

/// Resolve and return `(browser_bundle_id, final_url)` so tests can
/// assert on plain strings.
fn resolve(engine: &Engine, url: &str) -> (String, String) {
    let res = engine.resolve(url, &Opener::default(), ModifierFlags::default());
    (res.browser.bundle_id.clone(), res.url.into_owned())
}

fn resolve_with(
    engine: &Engine,
    url: &str,
    opener: &Opener,
    modifiers: ModifierFlags,
) -> (String, String) {
    let res = engine.resolve(url, opener, modifiers);
    (res.browser.bundle_id.clone(), res.url.into_owned())
}

// ---------- Engine end-to-end ----------

#[test]
fn negative_array_like_lengths_do_not_abort_config_loading() {
    for args in ["{ length: -1 }", "new Array(2147483647)"] {
        let diagnostics = Rc::new(isolated_diagnostics());
        let engine = build_engine_with_diagnostics(
            &format!(
                "module.exports = {{ default: {{ name: 'com.apple.Safari', args: {args} }} }};"
            ),
            Rc::clone(&diagnostics),
        );
        assert!(resolve(&engine, "https://example.com").0.is_empty());
        assert!(
            diagnostics
                .config_warning()
                .unwrap()
                .contains("launch suppressed")
        );
    }
    for source in [
        "module.exports = { default: 'com.apple.Safari', rules: { length: -1 } };",
        "module.exports = { default: 'com.apple.Safari', rewrite: { length: -1 } };",
        "module.exports = { default: 'com.apple.Safari', rules: new Array(2147483647) };",
        "module.exports = { default: 'com.apple.Safari', rules: [{ match: new Array(2147483647), open: null }] };",
        "module.exports = { default: 'com.apple.Safari', rules: [{ match: { __type: 'domain', hosts: { length: -1 } }, open: null }] };",
        "module.exports = { default: 'com.apple.Safari', rewrite: [{ __type: 'strip', params: new Array(2147483647) }] };",
    ] {
        assert!(matches!(
            try_build_engine(source),
            Err(EngineError::InvalidArray { .. })
        ));
    }
}

#[test]
fn url_hostname_excludes_query_userinfo_and_normalizes_case() {
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
        { match: url => url.hostname === "gitlab.com", open: "com.google.Chrome" }
    ] };"#,
    );
    assert_eq!(
        resolve(&engine, "https://evil.com?@gitlab.com/").0,
        "com.apple.Safari"
    );
    assert_eq!(
        resolve(&engine, r"https://evil.com\@gitlab.com/").0,
        "com.apple.Safari"
    );
    assert_eq!(
        resolve(&engine, r"myapp://evil.com\@gitlab.com/").0,
        "com.google.Chrome"
    );
    assert_eq!(
        resolve(&engine, "https://user:pass@GitLab.COM/path").0,
        "com.google.Chrome"
    );
    let protocol = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
        { match: url => url.protocol === "https:", open: "com.google.Chrome" }
    ] };"#,
    );
    assert_eq!(
        resolve(&protocol, "HTTPS://example.com/").0,
        "com.google.Chrome"
    );
    assert_eq!(
        resolve(&engine, "https://user@evil.com@GitLab.COM/path").0,
        "com.google.Chrome"
    );
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
        { match: domain("gitlab.com"), open: "com.google.Chrome" }
    ] };"#,
    );
    assert_eq!(
        resolve(&engine, r"https://evil.com\@gitlab.com/").0,
        "com.apple.Safari"
    );
}

#[test]
fn deprecated_url_opener_never_uses_a_previous_link() {
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
        { match: domain("first.test"), open: (url, ctx) => "com.apple.Safari" },
        { match: url => url.opener?.bundleId === "com.first", open: "com.google.Chrome" }
    ] };"#,
    );
    let first = opener("com.first", "First");
    let second = opener("com.second", "Second");
    assert_eq!(
        resolve_with(
            &engine,
            "https://first.test/",
            &first,
            ModifierFlags::default()
        )
        .0,
        "com.apple.Safari"
    );
    assert_eq!(
        resolve_with(
            &engine,
            "https://second.test/",
            &second,
            ModifierFlags::default()
        )
        .0,
        "com.apple.Safari"
    );

    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
        { match: (url, ctx) => url.opener?.bundleId === ctx.opener?.bundleId, open: "com.google.Chrome" }
    ] };"#,
    );
    assert_eq!(
        resolve_with(
            &engine,
            "https://example.com/",
            &second,
            ModifierFlags::default()
        )
        .0,
        "com.google.Chrome"
    );

    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari",
        rewrite: [{ match: () => true, url: (url, ctx) => "https://rewritten.test/" }],
        rules: [{ match: (url, ctx) => url.opener?.bundleId === ctx.opener?.bundleId,
                  open: "com.google.Chrome" }]
    };"#,
    );
    assert_eq!(
        resolve_with(
            &engine,
            "https://example.com/",
            &second,
            ModifierFlags::default()
        )
        .0,
        "com.google.Chrome"
    );
}

#[test]
fn hostname_and_port_patterns_match_urls_with_that_port() {
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
        { match: "example.com:8080/*", open: "com.google.Chrome" },
        { match: "localhost:3000", open: "org.mozilla.firefox" }
    ] };"#,
    );
    assert_eq!(
        resolve(&engine, "https://example.com:8080/path").0,
        "com.google.Chrome"
    );
    assert_eq!(
        resolve(&engine, "https://example.com:8081/path").0,
        "com.apple.Safari"
    );
    assert_eq!(
        resolve(&engine, "http://localhost:3000").0,
        "org.mozilla.firefox"
    );
    assert_eq!(
        resolve(&engine, "http://localhost:3000/path").0,
        "org.mozilla.firefox"
    );
    assert_eq!(
        resolve(&engine, "HTTP://localhost:3000/path").0,
        "org.mozilla.firefox"
    );
    assert_eq!(
        resolve(&engine, "http://localhost:30001/").0,
        "com.apple.Safari"
    );
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
        { match: "LOCALHOST:3000/*", open: "com.google.Chrome" }
    ] };"#,
    );
    assert_eq!(
        resolve(&engine, "http://localhost:3000/path").0,
        "com.google.Chrome"
    );
}

#[test]
fn wildcard_normalizes_hostname_but_not_path() {
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
        { match: "github.com/Path/*", open: "com.google.Chrome" }
    ] };"#,
    );
    assert_eq!(
        resolve(&engine, "https://GitHub.COM/Path/item").0,
        "com.google.Chrome"
    );
    assert_eq!(
        resolve(&engine, "HTTPS://github.com/Path/item").0,
        "com.google.Chrome"
    );
    assert_eq!(
        resolve(&engine, "HTTPS://github.com/path/item").0,
        "com.apple.Safari"
    );
    assert_eq!(
        resolve(&engine, "https://GitHub.COM/path/item").0,
        "com.apple.Safari"
    );
    assert_eq!(
        resolve(&engine, "https://evil.example/?@github.com/Path/item").0,
        "com.apple.Safari"
    );
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
        { match: "Example.NET/Path/*", open: "com.google.Chrome" }
    ] };"#,
    );
    assert_eq!(
        resolve(&engine, "https://example.net/Path/item").0,
        "com.google.Chrome"
    );
    assert_eq!(
        resolve(&engine, "https://EXAMPLE.NET/path/item").0,
        "com.apple.Safari"
    );
    for pattern in ["https://Example.NET/Path/*", "*.Example.NET/Path/*"] {
        let source = format!(
            r#"module.exports = {{ default: "com.apple.Safari", rules: [
            {{ match: "{pattern}", open: "com.google.Chrome" }}
        ] }};"#
        );
        let engine = build_engine(&source);
        assert_eq!(
            resolve(&engine, "https://sub.example.net/Path/item").0,
            if pattern.starts_with("*.") {
                "com.google.Chrome"
            } else {
                "com.apple.Safari"
            }
        );
        if pattern.starts_with("https:") {
            assert_eq!(
                resolve(&engine, "https://example.net/Path/item").0,
                "com.google.Chrome"
            );
            assert_eq!(
                resolve(&engine, "HTTPS://example.net/Path/item").0,
                "com.google.Chrome"
            );
        }
    }
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
        { match: "mailto:*GitHub.COM*", open: "com.google.Chrome" }
    ] };"#,
    );
    assert_eq!(
        resolve(&engine, "mailto:test@example.com?body=https://GitHub.COM/").0,
        "com.google.Chrome"
    );
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari",
        rewrite: [{ match: "first.example/*", url: () => "https://SECOND.example/Path" }],
        rules: [
          { match: "first.example/*", open: "com.apple.Safari" },
          { match: "second.example/Path", open: "com.google.Chrome" }
        ] };"#,
    );
    assert_eq!(
        resolve(&engine, "https://FIRST.example/start").0,
        "com.google.Chrome"
    );
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
          { match: "first.example/*", url: () => "https://SECOND.example/Path", open: () => null },
          { match: "second.example/Path", open: "com.google.Chrome" }
        ] };"#,
    );
    assert_eq!(
        resolve(&engine, "https://FIRST.example/start").0,
        "com.google.Chrome"
    );
}

#[test]
fn wildcard_scheme_case_matrix() {
    assert_wildcard_cases(&[
        ("github.com/Path/*", "HTTPS://GITHUB.COM/Path/x", true),
        ("github.com/Path/*", "HtTpS://github.com/Path/x", true),
        ("github.com/Path/*", "HTTP://github.com/path/x", false),
        ("github.com/Path/*", "FTP://github.com/Path/x", true),
        ("github.com/Path/*", "MAILTO:github.com/Path/x", true),
        ("github.com/Path/*", "FILE://github.com/Path/x", true),
        ("github.com/Path/*", "TEL:github.com/Path/x", true),
        ("github.com/Path/*", "SMS:github.com/Path/x", true),
        ("github.com/Path/*", "DATA:github.com/Path/x", true),
        ("github.com/Path/*", "WS://github.com/Path/x", false),
        ("github.com/Path/*", "git+https://github.com/Path/x", false),
        (
            "https://github.com/Path/*",
            "HTTPS://GITHUB.COM/Path/x",
            true,
        ),
        (
            "HTTPS://github.com/Path/*",
            "https://github.com/Path/x",
            true,
        ),
        (
            "http://github.com/Path/*",
            "HTTPS://github.com/Path/x",
            false,
        ),
        (
            "web+app://github.com/Path/*",
            "WEB+APP://GITHUB.COM/Path/x",
            true,
        ),
        (
            "git+https://github.com/Path/*",
            "GIT+HTTPS://github.com/Path/x",
            true,
        ),
        ("chrome-extension://abc/*", "CHROME-EXTENSION://abc/x", true),
        ("mailto:*@example.com", "MAILTO:User@example.com", true),
        ("MA*TO:*@example.com", "MAILTO:User@example.com", true),
        ("MA*TO:*@example.com", "MOLTO:User@example.com", false),
        ("T*L:123", "TEL:123", true),
        ("loc*lhost:3000/*", "HTTP://localhost:3000/x", true),
        ("fo*:123", "HTTP://foo:123", true),
        ("*://github.com/Path/*", "HTTPS://GITHUB.COM/Path/x", true),
        ("*://github.com/Path/*", "WS://GITHUB.COM/Path/x", true),
        ("*://github.com/Path/*", "WSS://github.com/Path/x", true),
        ("*://[2001:DB8::1]/*", "HTTPS://[2001:db8::1]/x", true),
        (
            "HT*PS://github.com/Path/*",
            "https://github.com/Path/x",
            true,
        ),
        (
            "HT*PS://github.com/Path/*",
            "HTTPS://github.com/Path/x",
            true,
        ),
        (
            "HT*PS://github.com/Path/*",
            "FTP://github.com/Path/x",
            false,
        ),
        (
            "ht*ps://GitHub.COM/Path/*",
            "HTTPS://github.com/Path/x",
            true,
        ),
    ]);
}

#[test]
fn wildcard_authority_case_matrix() {
    assert_wildcard_cases(&[
        ("*GitHub.com/*", "HTTPS://github.com/Path/x", true),
        ("*GitHub.com/*", "https://other.example/GitHub.com/x", true),
        ("*GitHub.com/*", "https://other.example/github.com/x", false),
        ("*GitHub.com/*", "https://github.com@evil.example/x", false),
        ("*GitHub.com*", "https://github.com/x", true),
        ("*GitHub.com:*", "https://github.com:8080/x", true),
        ("*GitHub.com:8080/*", "https://github.com:8080/x", true),
        ("*GitHub.com*", "https://github.com@evil.example/x", false),
        ("*.GitHub.com/*", "https://sub.github.com/x", true),
        (
            "*.GitHub.com/*",
            "https://other.example/x.GitHub.com/x",
            true,
        ),
        (
            "*.GitHub.com/*",
            "https://other.example/x.github.com/x",
            false,
        ),
        ("*GitHub.com?x=*", "https://github.com?x=value", true),
        ("*GitHub.com#Fragment", "https://github.com#Fragment", true),
        ("*GitHub.com#Fragment", "https://github.com#fragment", false),
        ("localhost:3000/*", "HTTP://LOCALHOST:3000/x", true),
        ("localhost:3000", "HTTP://LOCALHOST:3000/x", true),
        ("LOCALHOST:3000/*", "HTTP://localhost:3000/x", true),
        ("LOCALHOST:3000", "HTTP://localhost:3000/x", true),
    ]);
}

fn assert_wildcard_cases(cases: &[(&str, &str, bool)]) {
    for &(pattern, url, matches) in cases {
        let source = format!(
            "module.exports = {{ default: 'com.apple.Safari', rules: \
             [{{ match: {}, open: 'com.google.Chrome' }}] }};",
            serde_json::to_string(pattern).unwrap()
        );
        let engine = build_engine(&source);
        let actual = resolve(&engine, url).0;
        let expected = if matches {
            "com.google.Chrome"
        } else {
            "com.apple.Safari"
        };
        assert_eq!(actual, expected, "pattern {pattern:?}, URL {url:?}");
    }
}

#[test]
fn url_protocol_normalizes_schemes_for_functions() {
    for (url, scheme) in [
        ("HTTPS://example.com/x", "https:"),
        ("HtTp://example.com/x", "http:"),
        ("FTP://example.com/x", "ftp:"),
        ("WS://example.com/x", "ws:"),
        ("WSS://example.com/x", "wss:"),
        ("FILE://example.com/x", "file:"),
        ("MAILTO:user@example.com", "mailto:"),
        ("GIT+HTTPS://example.com/x", "git+https:"),
        ("WEB+APP://Open/Thing", "web+app:"),
    ] {
        let source = format!(
            "module.exports = {{ default: 'com.apple.Safari', rules: [{{ \
             match: url => url.protocol === {}, open: 'com.google.Chrome' }}] }};",
            serde_json::to_string(scheme).unwrap()
        );
        let engine = build_engine(&source);
        assert_eq!(resolve(&engine, url).0, "com.google.Chrome", "URL {url:?}");
    }
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [{
            match: url => {
                url.protocol = "HTTPS";
                if (url.protocol !== "https:" || !Object.keys(url).includes("protocol") ||
                    Object.getOwnPropertySymbols(url).length !== 0 ||
                    Object.assign({}, url).protocol !== "https:") return false;
                url.protocol = "1invalid";
                if (url.protocol !== "https:") return false;
                url.protocol = "HTTP:ignored";
                if (url.protocol !== "http:") return false;
                url.href = "HtTpS://example.com/x";
                return url.protocol === "https:";
            }, open: "com.google.Chrome"
        }] };"#,
    );
    assert_eq!(
        resolve(&engine, "http://example.com/").0,
        "com.google.Chrome"
    );
}

#[test]
fn default_engine_fixture_isolates_runtime_error_logs() {
    let home = unique_tmp("fixture-log-home");
    with_home(&home, || {
        let engine = build_engine(
            r#"module.exports = { default: 'com.apple.Safari', rules: [
                { match: () => { throw new Error('isolated boom'); }, open: null }
            ] };"#,
        );
        let path = engine.diagnostics.ensure_file().unwrap();
        assert!(
            !path.starts_with(&home),
            "fixture must not select the user log path"
        );
        assert!(path.starts_with(std::env::temp_dir()));
        assert_eq!(
            resolve(&engine, "https://example.com/").0,
            "com.apple.Safari"
        );
        let events = read_log_events(&path);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "runtime_js_error");
        assert!(
            events[0]["message"]
                .as_str()
                .unwrap()
                .contains("isolated boom")
        );
    });
    assert!(!home.join("Library/Logs/Grinch").exists());
}

#[test]
fn wrapper_rewrites_ignore_embedded_urls_in_opaque_inputs() {
    for helper in ["safelinks()", "teams_launcher()"] {
        let engine = build_engine(&format!(
            "module.exports = {{default: 'com.apple.Safari', rewrite: [{helper}]}};"
        ));
        for url in [
            "mailto:user@example.com?body=https://safelinks.protection.outlook.com/?url=x",
            "mailto:user@example.com?body=https://teams.microsoft.com/dl/launcher/launcher.html",
            "data:text/plain,https://safelinks.protection.outlook.com/?url=x",
            "?body=https://teams.microsoft.com/dl/launcher/launcher.html",
        ] {
            assert_eq!(resolve(&engine, url).1, url, "{helper}: {url}");
            assert_eq!(quick_host(url), None);
        }
    }
}

#[test]
fn default_browser_fires_when_no_rules() {
    let e = build_engine(r#"module.exports = { default: "com.apple.Safari" };"#);
    let (browser, url) = resolve(&e, "https://example.com/");
    assert_eq!(browser, "com.apple.Safari");
    assert_eq!(url, "https://example.com/");
}

#[test]
fn options_block_with_all_known_keys_is_accepted() {
    // Finicky-config compat: the entire options block should be
    // accepted without erroring even though Grinch implements none
    // of these today. Verify by checking that the engine builds
    // (build_engine would panic if Engine::new returned Err) and
    // that resolve still works.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                options: {
                    urlShorteners: ["bit.ly", "t.co"],
                    logRequests: false, // tested for real in
                                        // options_log_requests_writes_jsonl_per_resolve;
                                        // false here to avoid creating a log
                                        // file at whatever HOME the parallel
                                        // test runner happens to have set
                    checkForUpdates: false,
                    keepRunning: true,
                    hideIcon: false,
                },
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
}

#[test]
fn options_hideicon_parses_to_engine_accessor() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                options: { hideIcon: true },
            };"#,
    );
    assert!(e.hide_icon());
}

#[test]
fn options_check_for_updates_is_unset_unless_configured() {
    let e = build_engine(r#"module.exports = { default: "com.apple.Safari" };"#);
    assert_eq!(e.check_for_updates(), None);

    for (value, expected) in [("true", Some(true)), ("false", Some(false))] {
        let e = build_engine(&format!(
            r#"module.exports = {{
                    default: "com.apple.Safari",
                    options: {{ checkForUpdates: {value} }},
                }};"#
        ));
        assert_eq!(e.check_for_updates(), expected);
    }
}

#[test]
fn options_hideicon_default_is_false() {
    let e = build_engine(r#"module.exports = { default: "com.apple.Safari" };"#);
    assert!(!e.hide_icon());

    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                options: { hideIcon: false },
            };"#,
    );
    assert!(!e.hide_icon());
}

#[test]
fn diagnostic_log_is_available_without_request_logging() {
    let tmp = unique_tmp("open-log");
    let _ = std::fs::remove_dir_all(&tmp);

    with_home(&tmp, || {
        let diagnostics = Rc::new(DiagnosticLog::default());
        let engine = build_engine_with_diagnostics(
            r#"module.exports = { default: "com.apple.Safari" };"#,
            Rc::clone(&diagnostics),
        );
        assert!(!engine.options.log_requests);
        let path = diagnostics
            .ensure_file()
            .expect("diagnostic log should be creatable");
        assert!(path.exists(), "diagnostic log was not created at {path:?}");
        assert_eq!(
            path.parent(),
            Some(tmp.join("Library/Logs/Grinch").as_path())
        );

        let _ = resolve(&engine, "https://example.com/");
        let body = std::fs::read_to_string(path).expect("diagnostic log should be readable");
        assert!(body.is_empty(), "resolve events must remain opt-in");
    });

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn log_format_switches_files_and_preserves_background_events() {
    let diagnostics = Rc::new(isolated_diagnostics());
    let json = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.apple.Safari", options: { logRequests: true } };"#,
        Rc::clone(&diagnostics),
    );
    let background = diagnostics.event_sink();
    let _ = resolve(&json, "https://example.com/before");
    let json_path = diagnostics.ensure_file().unwrap();
    let text = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.apple.Safari", options: { logRequests: true, logFormat: "text" } };"#,
        Rc::clone(&diagnostics),
    );
    let _ = resolve(&text, "https://example.com/after");
    background(serde_json::json!({
        "event": "launch_error", "ts": 1.0, "message": "launch failed\ntry again\u{1b}\t"
    }));
    diagnostics.record_config_error(None, "config failed");
    diagnostics.record_runtime_js_error("config.js", "script failed");
    diagnostics.record_profile_warning("browser", None, "profile failed");
    let text_path = diagnostics.ensure_file().unwrap();
    let _ = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.apple.Safari", options: { logFormat: "text" } };"#,
        Rc::clone(&diagnostics),
    );
    assert_eq!(diagnostics.ensure_file().unwrap(), text_path);
    assert_ne!(json_path, text_path);
    let initial = std::fs::read_to_string(&json_path).unwrap();
    let row: serde_json::Value = serde_json::from_str(initial.trim()).unwrap();
    assert_eq!(row["url"], "https://example.com/before");
    let body = std::fs::read_to_string(&text_path).unwrap();
    assert!(body.contains("  resolve\n"));
    assert!(body.contains("url: https://example.com/after"));
    assert!(body.contains("  launch_error\n"));
    assert!(body.contains("message: launch failed\\ntry again"));
    assert!(body.contains("\\u{1b}\\t"), "{body:?}");
    assert!(!body.contains('\u{1b}'));
    assert!(body.contains("    bundleId:\n"));
    assert!(body.contains("  config_error\n"));
    assert!(body.contains("  runtime_js_error\n"));
    assert!(body.contains("  profile_error\n"));
    let _ = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.apple.Safari" };"#,
        Rc::clone(&diagnostics),
    );
    let next = diagnostics.ensure_file().unwrap();
    assert_ne!(next, text_path);
    assert_ne!(next, json_path);
    background(serde_json::json!({ "event": "config_error", "ts": 2.0 }));
    let row: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(next).unwrap().trim()).unwrap();
    assert_eq!(row["event"], "config_error");
    assert_eq!(std::fs::read_to_string(json_path).unwrap(), initial);
    assert_eq!(std::fs::read_to_string(text_path).unwrap(), body);

    let text = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.apple.Safari", options: {
            logRequests: true, logFormat: "text", logRotateBytes: 1
        } };"#,
        Rc::clone(&diagnostics),
    );
    let path = diagnostics.ensure_file().unwrap();
    let _ = resolve(&text, "https://example.com/rotated");
    let _ = resolve(&text, "https://example.com/current");
    let active = std::fs::read_to_string(&path).unwrap();
    assert!(active.contains("url: https://example.com/current"));
    let rotated = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            std::fs::read_to_string(entry.path())
                .is_ok_and(|body| body.contains("url: https://example.com/rotated"))
        });
    assert!(rotated, "the previous text event should survive rotation");
}

#[test]
fn rotation_settings_preserve_fractional_days_and_reject_fractional_bytes() {
    let engine = build_engine(
        r#"module.exports = { default: "com.apple.Safari",
        options: { logRotateDays: 0.5, logRotateBytes: 0.5 } };"#,
    );
    assert_eq!(engine.options.log_rotate_seconds, Some(43_200));
    assert_eq!(engine.options.log_rotate_bytes, None);
}

#[test]
fn config_warnings_include_options_and_prioritize_suppressed_rules() {
    let diagnostics = Rc::new(isolated_diagnostics());
    let engine = build_engine_with_diagnostics(
        r#"
        const rule = { match: "example.com", open: "com.apple.Safari" };
        for (let i = 0; i < 32; i++) rule["typo" + i] = true;
        rule.profile = "Work";
        module.exports = { default: "com.apple.Safari", hideIcon: true,
            options: { logFormat: "text", logRequest: true, logRotateBytes: 0.5 }, rules: [rule] };
    "#,
        Rc::clone(&diagnostics),
    );
    assert!(
        diagnostics
            .config_warning()
            .unwrap()
            .contains("this rule is suppressed")
    );
    let body = std::fs::read_to_string(diagnostics.ensure_file().unwrap()).unwrap();
    for expected in [
        "options.logRequest",
        "options.logRotateBytes",
        "top-level `hideIcon`",
        "this rule is suppressed",
    ] {
        assert!(body.contains(expected), "missing {expected}: {body}");
    }
    assert!(body.contains("  config_warning\n"));
    assert!(resolve(&engine, "https://example.com").0.is_empty());
}

#[test]
fn dead_rules_and_unsupported_options_produce_config_warnings() {
    for (source, expected) in [
        (
            r#"module.exports={default:"com.apple.Safari",rules:[{match:/(?<=a)b/,open:"com.apple.Safari"}]}"#,
            "failed to compile",
        ),
        (
            r#"module.exports={default:"com.apple.Safari",rules:[{open:"com.apple.Safari"}]}"#,
            "no match",
        ),
        (
            r#"module.exports={default:"com.apple.Safari",rules:[{match:"example.com"}]}"#,
            "neither `open` nor `url`",
        ),
        (
            r#"module.exports={default:"com.apple.Safari",rewrite:[{url:"https://example.com/"}]}"#,
            "rewrite[0] has no match",
        ),
        (
            r#"module.exports={default:"com.apple.Safari",rewrite:[{match:"example.com"}]}"#,
            "rewrite[0] has no `url`",
        ),
        (
            r#"module.exports={default:{name:"com.apple.Safari",incognito:true}}"#,
            "incognito",
        ),
    ] {
        let diagnostics = Rc::new(isolated_diagnostics());
        let engine = build_engine_with_diagnostics(source, Rc::clone(&diagnostics));
        assert!(diagnostics.config_warning().unwrap().contains(expected));
        if expected == "no match" {
            assert!(engine.rule_listing()[0].contains("(no match)"));
        }
    }
}

#[test]
fn top_level_warning_scan_does_not_evaluate_properties() {
    let diagnostics = Rc::new(isolated_diagnostics());
    let engine = build_engine_with_diagnostics(
        r#"
        let reads = 0;
        const config = { get default() {
            return ++reads === 1 ? "com.apple.Safari" : "com.google.Chrome";
        } };
        Object.defineProperty(config, "typo", { enumerable: true, get() { throw Error("read"); } });
        module.exports = config;
    "#,
        Rc::clone(&diagnostics),
    );
    assert_eq!(
        resolve(&engine, "https://example.com").0,
        "com.apple.Safari"
    );
    assert!(diagnostics.config_warning().unwrap().contains("typo"));
}

#[test]
fn format_change_includes_profile_errors_from_config_load() {
    let home = unique_tmp("format-profile-error");
    with_home(&home, || {
        let diagnostics = Rc::new(isolated_diagnostics());
        diagnostics.record_config_error(None, "previous error");
        let old = diagnostics.ensure_file().unwrap();
        let _ = build_engine_with_diagnostics(
            r#"module.exports = { default: "Google Chrome:NoSuchProfileXYZ", options: {
            logFormat: "text"
        } };"#,
            Rc::clone(&diagnostics),
        );
        let current = diagnostics.ensure_file().unwrap();
        assert_ne!(old, current);
        assert!(diagnostics.profile_error().is_some());
        let body = std::fs::read_to_string(current).unwrap();
        assert!(body.contains("  profile_error\n"), "{body}");
        assert!(body.contains("NoSuchProfileXYZ"), "{body}");
        let body = std::fs::read_to_string(old).unwrap();
        assert!(!body.contains("profile_error"), "{body}");
        let result = try_build_engine_with_diagnostics(
            r#"module.exports = { options: { logFormat: "json" } };"#,
            Rc::clone(&diagnostics),
        );
        assert!(result.is_err());
        diagnostics.record_config_error(None, "missing default");
        let current = diagnostics.ensure_file().unwrap();
        let body = std::fs::read_to_string(current).unwrap();
        let row: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(row["message"], "missing default");
    });
}

#[test]
fn invalid_log_format_preserves_routing_and_current_format() {
    let diagnostics = Rc::new(isolated_diagnostics());
    let engine = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.apple.Safari", options: { logFormat: "Text", logRequests: true } };"#,
        Rc::clone(&diagnostics),
    );
    assert_eq!(
        resolve(&engine, "https://example.com/").0,
        "com.apple.Safari"
    );
    let body = std::fs::read_to_string(diagnostics.ensure_file().unwrap()).unwrap();
    let mut events = body.lines();
    let row: serde_json::Value = serde_json::from_str(events.next().unwrap()).unwrap();
    assert_eq!(row["event"], "config_error");
    assert!(row["message"].as_str().unwrap().contains("logFormat"));
    let row: serde_json::Value = serde_json::from_str(events.next().unwrap()).unwrap();
    assert_eq!(row["event"], "resolve");
    let _ = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.apple.Safari", options: { logFormat: "text" } };"#,
        Rc::clone(&diagnostics),
    );
    let path = diagnostics.ensure_file().unwrap();
    let engine = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.apple.Safari", options: { logFormat: "Text" } };"#,
        Rc::clone(&diagnostics),
    );
    assert_eq!(
        resolve(&engine, "https://example.com/").0,
        "com.apple.Safari"
    );
    assert_eq!(diagnostics.ensure_file().unwrap(), path);
    let body = std::fs::read_to_string(path).unwrap();
    assert!(body.contains("  config_error\n"));
    assert!(body.contains("logFormat"));
}

#[test]
fn null_and_undefined_log_format_use_default() {
    for value in ["null", "undefined"] {
        let diagnostics = Rc::new(isolated_diagnostics());
        let engine = build_engine_with_diagnostics(
            &format!(
                r#"module.exports = {{ default: "com.apple.Safari", options: {{
                logFormat: {value}, logRequests: true
            }} }};"#
            ),
            Rc::clone(&diagnostics),
        );
        assert_eq!(
            resolve(&engine, "https://example.com/").0,
            "com.apple.Safari"
        );
        let body = std::fs::read_to_string(diagnostics.ensure_file().unwrap()).unwrap();
        for line in body.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_ne!(row["event"], "config_error");
        }
    }
}

#[test]
fn ensure_diagnostic_log_reports_filesystem_errors() {
    let tmp = unique_tmp("open-log-error");
    std::fs::write(&tmp, b"not a directory").expect("fixture file should be writable");

    with_home(&tmp, || {
        let diagnostics = DiagnosticLog::default();
        let error = diagnostics
            .ensure_file()
            .expect_err("a file cannot contain the log directory");
        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
        assert!(
            error
                .to_string()
                .contains(&tmp.to_string_lossy().into_owned())
        );
    });

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rule_listing_describes_each_rule_with_index_and_target() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                browsers: { work: { name: "com.google.Chrome", profile: "Profile 1" } },
                rules: [
                    { match: "github.com", open: "com.google.Chrome", name: "code-hosts" },
                    { match: "slack:*", open: "com.tinyspeck.slackmacgap" },
                    { match: (url) => url.searchParams.has("incognito"), open: null },
                ],
            };"#,
    );
    let lines = e.rule_listing();
    assert_eq!(lines.len(), 3, "expected three rules: {lines:?}");
    // user-supplied name takes precedence, target is the bundle id
    assert_eq!(lines[0], "0: [code-hosts] github.com → com.google.Chrome");
    // no name → auto-derived label from the string pattern
    assert_eq!(lines[1], "1: slack:* → com.tinyspeck.slackmacgap");
    // fn matcher → first line of f.toString(); open:null → "(suppress)"
    assert!(
        lines[2].starts_with("2: fn:") && lines[2].ends_with("→ (suppress)"),
        "fn rule line had unexpected shape: {}",
        lines[2]
    );
}

#[test]
fn matched_rule_in_log_uses_user_name_when_present() {
    let tmp = unique_tmp("log-name");
    let _ = std::fs::remove_dir_all(&tmp);
    with_home(&tmp, || {
        let e = build_engine_with_diagnostics(
            r#"module.exports = {
                    default: "com.apple.Safari",
                    options: { logRequests: true },
                    rules: [{ match: "github.com", open: "com.google.Chrome", name: "code-hosts" }],
                };"#,
            Rc::new(DiagnosticLog::default()),
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    });
    let log_dir = tmp.join("Library/Logs/Grinch");
    let entries: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    let body = std::fs::read_to_string(entries[0].path()).unwrap();
    let row: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
    assert_eq!(row["matchedRule"]["index"], 0);
    assert_eq!(row["matchedRule"]["name"], "code-hosts");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// HOME is process-global. Tests of default log paths and profile paths
/// serialise their overrides. Ordinary engine fixtures use explicit log paths.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with HOME pointed at `home` for its duration, holding the
/// shared HOME_LOCK. The Engine's log writer is lazy and opens the
/// file on first write, so HOME must still point at the test tmpdir
/// when resolves happen — hence holding the lock around the whole
/// engine-and-resolves block, not just the construction call.
fn with_home<R>(home: &std::path::Path, f: impl FnOnce() -> R) -> R {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", home);
    }
    let out = f();
    unsafe {
        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }
    out
}

/// Build a guaranteed-unique tmp-dir path. Per-test pid+name+counter
/// to avoid cross-test pollution if a previous run left junk behind
/// or another parallel test happens to compose the same path.
fn unique_tmp(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("grinch-{}-{}-{}", name, std::process::id(), n))
}

fn read_log_events(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .expect("diagnostic log should be readable")
        .lines()
        .map(|line| serde_json::from_str(line).expect("diagnostic event should be JSON"))
        .collect()
}

#[test]
fn profile_warning_survives_log_failure_and_clears_on_reload() {
    let tmp = unique_tmp("profile-warning");
    std::fs::write(&tmp, "not a directory").unwrap();
    let diagnostics = Rc::new(DiagnosticLog::at_path(tmp.join("diagnostic.log")));
    let states = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&states);
    let weak = Rc::downgrade(&diagnostics);
    diagnostics.set_profile_error_handler(move || {
        observed
            .borrow_mut()
            .push(weak.upgrade().unwrap().profile_error());
    });

    diagnostics.record_profile_warning("org.mozilla.firefox", None, "Cannot validate Personal");
    assert!(diagnostics.profile_error().is_none());
    assert!(states.borrow().is_empty());
    diagnostics.record_profile_error("com.google.Chrome", None, "Cannot resolve Work");
    diagnostics.record_profile_warning("org.mozilla.firefox", None, "Cannot validate Personal");
    diagnostics.record_profile_error("com.google.Chrome", None, "Cannot resolve Other");
    assert_eq!(
        *states.borrow(),
        [Some("com.google.Chrome: Cannot resolve Work".into())]
    );

    let engine = build_engine_with_diagnostics(
        "module.exports = { default: null };",
        Rc::clone(&diagnostics),
    );
    assert!(resolve(&engine, "https://x/").0.is_empty());
    assert_eq!(
        *states.borrow(),
        [Some("com.google.Chrome: Cannot resolve Work".into()), None]
    );
    assert!(
        diagnostics.profile_error().is_none(),
        "intentional suppression is not an error"
    );
    diagnostics.record_profile_warning("org.mozilla.firefox", None, "Cannot validate Personal");
    assert_eq!(states.borrow().len(), 2);
    diagnostics.record_profile_error("com.google.Chrome", None, "Cannot resolve Work");
    assert_eq!(states.borrow().len(), 3);
    std::fs::remove_file(tmp).unwrap();
}

#[test]
fn runtime_js_error_is_recorded_without_request_logging() {
    let tmp = unique_tmp("runtime-error");
    let path = tmp.join("diagnostic.log");
    let diagnostics = Rc::new(DiagnosticLog::at_path(path.clone()));
    let engine = build_engine_with_diagnostics(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => { throw new Error("direct boom"); },
                    open: "com.google.Chrome",
                }],
            };"#,
        diagnostics,
    );

    assert!(!engine.options.log_requests);
    assert_eq!(
        resolve(&engine, "https://example.com/").0,
        "com.apple.Safari"
    );
    let events = read_log_events(&path);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "runtime_js_error");
    assert_eq!(events[0]["path"], "test-config.js");
    assert!(
        events[0]["message"]
            .as_str()
            .unwrap()
            .contains("direct boom")
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn throwing_matcher_is_recorded_and_later_rule_runs() {
    let tmp = unique_tmp("throwing-matcher");
    let path = tmp.join("diagnostic.log");
    let diagnostics = Rc::new(DiagnosticLog::at_path(path.clone()));
    let engine = build_engine_with_diagnostics(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    {
                        match: () => { throw new Error("later-rule boom"); },
                        open: "com.brave.Browser",
                    },
                    { match: () => true, open: "com.google.Chrome" },
                ],
            };"#,
        diagnostics,
    );

    assert_eq!(
        resolve(&engine, "https://example.com/").0,
        "com.google.Chrome"
    );
    let events = read_log_events(&path);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "runtime_js_error");
    assert!(
        events[0]["message"]
            .as_str()
            .unwrap()
            .contains("later-rule boom")
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn options_log_requests_writes_jsonl_per_resolve() {
    let tmp = unique_tmp("log-on");
    let _ = std::fs::remove_dir_all(&tmp);

    with_home(&tmp, || {
        let e = build_engine_with_diagnostics(
            r#"module.exports = {
                    default: "com.apple.Safari",
                    options: { logRequests: true },
                    rules: [{
                        match: "github.com",
                        open: { name: "com.google.Chrome", profile: "Profile 1" },
                    }],
                };"#,
            Rc::new(DiagnosticLog::default()),
        );
        assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
        assert_eq!(resolve(&e, "https://example.com/").0, "com.apple.Safari");
    });

    let log_dir = tmp.join("Library/Logs/Grinch");
    let entries: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap_or_else(|e| panic!("expected log dir at {}: {e}", log_dir.display()))
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one log file");
    let body = std::fs::read_to_string(entries[0].path()).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2, "expected two log lines, got: {body}");
    let row0: serde_json::Value = serde_json::from_str(lines[0]).expect("line 0 is JSON");
    let row1: serde_json::Value = serde_json::from_str(lines[1]).expect("line 1 is JSON");
    // Rule-hit row: matchedRule object with index + auto-derived name,
    // opener nested, modifiers nested with all four booleans,
    // rewritten = false.
    assert_eq!(row0["event"], "resolve");
    assert_eq!(row0["url"], "https://github.com/");
    assert_eq!(row0["final"], "https://github.com/");
    assert_eq!(row0["rewritten"], false);
    assert_eq!(row0["browser"], "com.google.Chrome");
    assert_eq!(row0["matchedRule"]["index"], 0);
    assert_eq!(row0["matchedRule"]["name"], "github.com");
    assert_eq!(
        row0["strategy"], "launch_new_instance",
        "a profile route must log the new-instance launch strategy"
    );
    assert!(
        row0["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a.as_str() == Some("--profile-directory=Profile 1")),
        "profile launch should log the profile-directory arg"
    );
    assert!(row0["opener"].is_object(), "opener should be an object");
    assert!(row0["opener"]["bundleId"].is_string());
    assert!(row0["opener"]["name"].is_string());
    assert!(row0["opener"]["pid"].is_number());
    assert_eq!(row0["modifiers"]["shift"], false);
    assert_eq!(row0["modifiers"]["option"], false);
    assert_eq!(row0["modifiers"]["command"], false);
    assert_eq!(row0["modifiers"]["control"], false);
    // Default-fallback row: matchedRule = null, browser = default.
    assert_eq!(row1["url"], "https://example.com/");
    assert_eq!(row1["browser"], "com.apple.Safari");
    assert_eq!(
        row1["strategy"], "open_urls",
        "a plain (no-args) route must log the open_urls strategy"
    );
    assert!(
        row1["matchedRule"].is_null(),
        "matchedRule should be null when default fired"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn cached_js_string_stops_inserting_past_soft_cap() {
    // Build a context, hand it a cache that's already at the cap,
    // and verify the next insert is a no-op. The lookup must still
    // succeed (returns a fresh JSValue) — only growth is capped.
    let ctx: Retained<JSContext> = unsafe { JSContext::new() };
    let cache = RefCell::new(std::collections::HashMap::new());
    // Pre-fill to the cap with synthetic entries.
    for i in 0..STRING_CACHE_SOFT_CAP {
        let key = format!("preload_{i}");
        let v = js_string(&ctx, &key).expect("js_string ok");
        cache.borrow_mut().insert(key, v);
    }
    assert_eq!(cache.borrow().len(), STRING_CACHE_SOFT_CAP);
    // New miss → still returns a JSValue but doesn't grow the map.
    let v = cached_js_string(&ctx, &cache, "post_cap").expect("returns value");
    assert!(unsafe { v.isString() });
    assert_eq!(
        cache.borrow().len(),
        STRING_CACHE_SOFT_CAP,
        "cache must not grow past the soft cap"
    );
    // Existing key still hits → no allocation.
    let hit = cached_js_string(&ctx, &cache, "preload_0").expect("returns value");
    assert!(unsafe { hit.isString() });
}

#[test]
fn log_writer_should_rotate_unit() {
    // Pure-function rotation predicate — verifies bytes-based and
    // time-based thresholds independently, without touching the fs.
    let mut w = LogWriter::new(
        std::path::PathBuf::from("/tmp/never-opened.log"),
        Some(1024),
        Some(7 * 86_400),
    );
    // No file open yet → never rotates (rotation rebinds bytes_written
    // when the new file opens; nothing to rotate before that).
    assert!(!w.should_rotate(2048, 1_000_000));
    // Pretend a file is open with some bytes already written.
    w.file = Some(
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(std::env::temp_dir().join("grinch-log-rotate-unit.tmp"))
            .unwrap(),
    );
    w.bytes_written = 1000;
    w.opened_at_unix = 1_000_000;
    // Under both thresholds: no rotation.
    assert!(!w.should_rotate(20, 1_000_000));
    // Adding 25 bytes would push past 1024.
    assert!(w.should_rotate(25, 1_000_000));
    // Time threshold (7 days = 604_800s): exactly at the threshold rotates.
    w.bytes_written = 0;
    assert!(w.should_rotate(1, 1_000_000 + 604_800));
    // Just under: no rotation.
    assert!(!w.should_rotate(1, 1_000_000 + 604_799));
}

#[test]
fn log_rotates_on_size_threshold() {
    // End-to-end: configure a 200-byte cap and write enough lines to
    // trigger a rotation. After the test we expect (a) a rotated
    // file with the .log.<timestamp> suffix containing the early
    // lines, and (b) a fresh active file with the later ones.
    let tmp = unique_tmp("log-rotate");
    let _ = std::fs::remove_dir_all(&tmp);
    with_home(&tmp, || {
        let e = build_engine_with_diagnostics(
            r#"module.exports = {
                    default: "com.apple.Safari",
                    options: { logRequests: true, logRotateBytes: 200 },
                };"#,
            Rc::new(DiagnosticLog::default()),
        );
        // Each log line is ~250 bytes; the first write opens the
        // file (size 0, 0 + ~250 > 200 — wait, the should_rotate
        // check skips when file is None, so the FIRST line lands
        // un-rotated). The SECOND write sees bytes_written=~250 +
        // ~250 = ~500 > 200 → rotates before writing the second
        // line. Drive enough resolves to trigger at least one
        // rotation regardless of exact line length.
        for _ in 0..5 {
            let _ = resolve(&e, "https://example.com/");
        }
    });
    let log_dir = tmp.join("Library/Logs/Grinch");
    let entries: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .collect();
    assert!(
        entries.len() >= 2,
        "expected at least one rotated log file + the active one, got: {entries:?}"
    );
    let has_rotated = entries
        .iter()
        .any(|p| p.to_string_lossy().contains(".log."));
    assert!(
        has_rotated,
        "expected a .log.<timestamp> rotated file in: {entries:?}"
    );
    let mut lines = 0;
    for path in entries {
        lines += std::fs::read_to_string(path).unwrap().lines().count();
    }
    assert_eq!(lines, 5, "rotation must preserve every resolve event");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn options_log_requests_off_writes_nothing() {
    let tmp = unique_tmp("log-off");
    let _ = std::fs::remove_dir_all(&tmp);

    with_home(&tmp, || {
        let e = build_engine_with_diagnostics(
            r#"module.exports = { default: "com.apple.Safari" };"#,
            Rc::new(DiagnosticLog::default()),
        );
        let _ = resolve(&e, "https://x/");
    });

    let log_dir = tmp.join("Library/Logs/Grinch");
    if log_dir.exists() {
        // Debug: dump what's there so we can see what actually got
        // written if this fails again.
        let listing: Vec<_> = std::fs::read_dir(&log_dir)
            .map(|d| {
                d.filter_map(|r| r.ok())
                    .map(|e| e.path().display().to_string())
                    .collect()
            })
            .unwrap_or_default();
        panic!(
            "log dir was created with logRequests off (path: {}, contents: {:?})",
            log_dir.display(),
            listing,
        );
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn nested_configuration_warnings_show_and_misplaced_profiles_suppress() {
    let diagnostics = Rc::new(isolated_diagnostics());
    let engine = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.apple.Safari", rules: [
            { match: "example.com", browser: "com.google.Chrome", profile: "Work" },
        ] };"#,
        Rc::clone(&diagnostics),
    );
    let result = engine.resolve(
        "https://example.com/",
        &Opener::default(),
        ModifierFlags::default(),
    );
    assert!(result.browser.bundle_id.is_empty());
    assert!(
        diagnostics
            .config_warning()
            .unwrap()
            .contains("rules[0].profile")
    );
    assert_eq!(
        resolve(&engine, "https://other.example/").0,
        "com.apple.Safari"
    );

    let engine = build_engine_with_diagnostics(
        r#"module.exports = { default: { name: "com.google.Chrome", incognit0: true } };"#,
        Rc::clone(&diagnostics),
    );
    assert_eq!(
        resolve(&engine, "https://example.com/").0,
        "com.google.Chrome"
    );
    assert!(diagnostics.config_warning().unwrap().contains("incognit0"));
    let body = std::fs::read_to_string(diagnostics.ensure_file().unwrap()).unwrap();
    assert!(body.contains("config_warning"));
    assert!(body.contains("rules[0].profile"));
}

#[test]
fn nested_warnings_ignore_inherited_keys_and_do_not_read_values() {
    let diagnostics = Rc::new(isolated_diagnostics());
    let engine = build_engine_with_diagnostics(
        r#"var rule = Object.create({ profile: "inherited" });
           rule.match = "example.com"; rule.open = "com.apple.Safari";
           Object.defineProperty(rule, "hidden", { value: 1 });
           Object.defineProperty(rule, "typo", { enumerable: true, get() { throw Error("read"); } });
           module.exports = { default: null, rules: [new Proxy(rule, {})] };"#,
        Rc::clone(&diagnostics),
    );
    assert_eq!(
        resolve(&engine, "https://example.com/").0,
        "com.apple.Safari"
    );
    assert!(diagnostics.config_warning().unwrap().contains("typo"));
    for replacement in [
        "undefined",
        "3",
        "({})",
        "(() => { throw Error(\"bad\"); })",
    ] {
        let source = format!(
            r#"__grinchUnknownRuleKeys = {replacement};
            __grinchUnknownBrowserKeys = {replacement};
            module.exports = {{ default: () => ({{ name: "com.apple.Safari", wrong: true }}),
                rules: [{{match: "example.com", open: "com.apple.Safari", profile: "Work"}}] }};"#
        );
        let engine = build_engine_with_diagnostics(&source, Rc::clone(&diagnostics));
        assert!(resolve(&engine, "https://example.com/").0.is_empty());
        assert!(diagnostics.config_warning().unwrap().contains("profile"));
        assert_eq!(
            resolve(&engine, "https://other.test/").0,
            "com.apple.Safari"
        );
        let events = std::fs::read_to_string(diagnostics.ensure_file().unwrap()).unwrap();
        assert!(events.contains("wrong"));
    }
    let engine = build_engine_with_diagnostics(
        r#"Object.defineProperty(Array.prototype, "0", {
               set(v) {}, get() { return undefined; }, configurable: true
           });
           module.exports = { default: "com.apple.Safari", rules: [
               {match: "example.com", open: "com.google.Chrome", profile: "Work"}
           ] };"#,
        Rc::clone(&diagnostics),
    );
    assert!(resolve(&engine, "https://example.com/").0.is_empty());
    assert!(diagnostics.config_warning().unwrap().contains("profile"));
}

#[test]
fn config_checks_preserve_enumeration_behavior() {
    let diagnostics = Rc::new(isolated_diagnostics());
    for source in [
        r#"__grinchUnknownRuleKeys = () => [];
           module.exports = {default: "com.apple.Safari", rules: [
             {match: "example.com", open: "com.google.Chrome", profile: "Work"}
           ]};"#,
        r#"var original = Object.keys;
           Object.keys = function(o) {
             if (o && o.match && this === Object) return [];
             return original(o);
           };
           module.exports = {default: "com.apple.Safari", rules: [
             {match: "example.com", open: "com.google.Chrome", profile: "Work"}
           ]};"#,
    ] {
        let engine = build_engine_with_diagnostics(source, Rc::clone(&diagnostics));
        assert!(resolve(&engine, "https://example.com/").0.is_empty());
        assert!(diagnostics.config_warning().unwrap().contains("profile"));
    }
    let engine = build_engine_with_diagnostics(
        r#"__grinchUnknownBrowserKeys = () => [];
           module.exports = {default: () => ({name: "com.apple.Safari", typo: true})};"#,
        Rc::clone(&diagnostics),
    );
    assert_eq!(
        resolve(&engine, "https://example.com/").0,
        "com.apple.Safari"
    );
    assert!(diagnostics.config_warning().unwrap().contains("typo"));
    let engine = build_engine_with_diagnostics(
        r#"var count = 0;
           var rule = new Proxy({match: "example.com", open: "com.google.Chrome", profile: "Work"}, {
             ownKeys(o) { if (++count === 1) throw Error("first call"); return Reflect.ownKeys(o); }
           });
           module.exports = {default: "com.apple.Safari", rules: [rule]};"#,
        Rc::clone(&diagnostics),
    );
    assert_eq!(
        resolve(&engine, "https://example.com/").0,
        "com.google.Chrome"
    );
    assert!(diagnostics.config_warning().is_none());
}

#[test]
fn config_key_check_matches_strict_object_keys_receiver() {
    let ctx: Retained<JSContext> = unsafe { JSContext::new() };
    let prelude = NSString::from_str(JS_PRELUDE);
    unsafe { ctx.evaluateScript(Some(&prelude)) }.unwrap();
    let checks = crate::loader::KeyCheckers::capture(&ctx);
    let script = NSString::from_str(
        r#"Object.keys = function(o) { "use strict"; return this === undefined ? [] : ["profile"]; };
           ({ match: "example.com", open: "com.google.Chrome", profile: "Work" });"#,
    );
    let rule = unsafe { ctx.evaluateScript(Some(&script)) }.unwrap();
    assert_eq!(
        object_keys(&rule),
        unknown_config_keys(&rule, checks.rule.as_deref())
    );
}

#[test]
fn javascript_and_rust_config_key_lists_match() {
    for (helper, expected) in [
        ("__grinchUnknownRuleKeys", RULE_KEYS),
        ("__grinchUnknownBrowserKeys", BROWSER_KEYS),
    ] {
        let source = JS_PRELUDE
            .split_once(helper)
            .unwrap()
            .1
            .split_once("})(Object.defineProperty, Reflect.apply, this);")
            .unwrap()
            .0;
        let keys: std::collections::HashSet<_> = source
            .split("k !== \"")
            .skip(1)
            .map(|part| part.split_once('"').unwrap().0)
            .collect();
        assert_eq!(keys, expected.iter().copied().collect(), "{helper}");
    }
}

#[test]
fn unknown_config_keys_do_not_error() {
    // Unknown option and top-level keys log a stderr warning but must not
    // break engine init. A root-level `hideIcon` is not honoured — it only
    // works inside `options`.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                hideIcon: true,
                notARealTopLevelKey: 1,
                options: { thisIsNotARealOption: 42 },
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
    assert!(!e.hide_icon());
}

#[test]
fn dynamic_default_browser_fn_returning_string() {
    // Finicky-style dynamic default: defaultBrowser is a fn evaluated
    // at resolve time when no rule matched.
    let e = build_engine(
        r#"module.exports = {
                default: (url) =>
                    url.hostname === "internal.corp" ? "com.apple.Safari" : "com.google.Chrome",
            };"#,
    );
    assert!(!e.needs_opener());
    assert!(!e.needs_opener_full());
    assert!(!e.needs_modifiers());
    assert!(!e.needs_host);
    assert_eq!(resolve(&e, "https://internal.corp/x").0, "com.apple.Safari");
    assert_eq!(resolve(&e, "https://github.com/x").0, "com.google.Chrome");
}

#[test]
fn dynamic_default_browser_fn_with_ctx() {
    // A default that declares ctx still requests the native metadata.
    let e = build_engine(
        r#"module.exports = {
                default: (url, ctx) =>
                    ctx.modifiers.shift ? "com.google.Chrome" : "com.apple.Safari",
            };"#,
    );
    assert!(e.needs_opener());
    assert!(e.needs_modifiers());
    assert!(e.needs_opener_full());
    assert!(e.needs_host);
    assert_eq!(
        resolve_with(
            &e,
            "https://x/",
            &Opener::default(),
            ModifierFlags::default()
        )
        .0,
        "com.apple.Safari",
    );
    let with_shift = ModifierFlags {
        shift: true,
        ..ModifierFlags::default()
    };
    assert_eq!(
        resolve_with(&e, "https://x/", &Opener::default(), with_shift).0,
        "com.google.Chrome",
    );
}

#[test]
fn dynamic_default_url_only_preserves_rule_context_needs() {
    for (matcher, needs_full) in [
        ("from('com.apple.finder')", false),
        ("(url, ctx) => ctx.modifiers.shift", true),
    ] {
        let e = build_engine(&format!(
            "module.exports = {{ default: url => 'com.apple.Safari',
                rules: [{{ match: {matcher}, open: 'com.google.Chrome' }}] }};"
        ));
        assert!(e.needs_opener());
        assert_eq!(e.needs_opener_full(), needs_full);
        assert_eq!(e.needs_modifiers(), needs_full);
        assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
        let shift = ModifierFlags {
            shift: true,
            ..ModifierFlags::default()
        };
        assert_eq!(
            resolve_with(
                &e,
                "https://x/",
                &opener("com.apple.finder", "Finder"),
                shift
            )
            .0,
            "com.google.Chrome"
        );
    }
}

#[test]
fn default_browser_null_is_explicit_suppress() {
    // Finicky-compat: `defaultBrowser: null` means "do nothing if no
    // rule matches" rather than being a config error. Mirrors how
    // a rule's `open: null` suppresses an individual URL.
    let e = build_engine(
        r#"module.exports = {
                default: null,
                rules: [{ match: "github.com", open: "com.google.Chrome" }],
            };"#,
    );
    // Match → routes normally.
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    // No match → suppressed via the about:blank sentinel.
    let (browser, url) = resolve(&e, "https://other.com/");
    assert_eq!(browser, "");
    assert_eq!(url, "about:blank");
}

#[test]
fn dynamic_default_browser_returning_null_suppresses() {
    let e = build_engine(r#"module.exports = { default: () => null };"#);
    let (browser, url) = resolve(&e, "https://x/");
    assert_eq!(browser, "");
    assert_eq!(url, "about:blank");
}

#[test]
fn export_default_es_module_syntax_works() {
    // Verify the loader's preprocess step kicks in and the user
    // can write Finicky-v4-style `export default { … }` without
    // converting to module.exports first. We stage the same way
    // the loader does — preprocess + wrap — and run through
    // build_engine's existing pipeline.
    use crate::helpers::preprocess_es_module_syntax;
    let src = preprocess_es_module_syntax(
        r#"export default {
                default: "com.apple.Safari",
                rules: [{ match: "github.com", open: "com.google.Chrome" }],
            };"#,
    )
    .unwrap();
    let e = build_engine(&src);
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://example.com/").0, "com.apple.Safari");
}

#[test]
fn defaultbrowser_alias_works() {
    // Finicky-style key name should be accepted as well.
    let e = build_engine(r#"module.exports = { defaultBrowser: "com.apple.Safari" };"#);
    assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
}

#[test]
fn handlers_alias_for_rules() {
    // Finicky's `handlers` should be accepted as a synonym for `rules`.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                handlers: [{ match: "x", open: "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}

#[test]
fn first_matching_rule_wins() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    { match: "github.com", open: "com.google.Chrome" },
                    { match: "github.com", open: "com.apple.Mail" },
                ],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
}

#[test]
fn falls_through_to_default_when_no_rule_matches() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: "github.com", open: "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://example.com/").0, "com.apple.Safari");
}

// ---------- compile_matcher per variant ----------

#[test]
fn matcher_bare_hostname_matches_subdomain() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: "github.com", open: "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    assert_eq!(
        resolve(&e, "https://api.github.com/").0,
        "com.google.Chrome"
    );
    assert_eq!(resolve(&e, "https://other.com/").0, "com.apple.Safari");
}

#[test]
fn matcher_domain_helper_handles_multiple_hosts() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: domain("github.com", "gitlab.com"),
                          open: "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://x.gitlab.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://other.com/").0, "com.apple.Safari");
}

#[test]
fn matcher_regex_against_full_url() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: /github\.com\/(paymentology|tutuka)\//,
                          open: "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(
        resolve(&e, "https://github.com/paymentology/grinch").0,
        "com.google.Chrome"
    );
    assert_eq!(resolve(&e, "https://github.com/").0, "com.apple.Safari");
}

#[test]
fn matcher_regex_default_is_case_sensitive() {
    // Regression: previously Grinch forced case_insensitive(true) on
    // every regex. Now matches Finicky / native JS RegExp.test, which
    // is case-sensitive unless the `i` flag is set.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: /github\.com/, open: "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    // Same domain, mixed case — must NOT match without /i.
    assert_eq!(resolve(&e, "https://GitHub.com/").0, "com.apple.Safari");
    let e = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
            { match: /^https:/, open: "com.google.Chrome" }
        ] };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "HTTPS://github.com/").0, "com.apple.Safari");
}

#[test]
fn matcher_regex_i_flag_makes_it_case_insensitive() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: /github\.com/i, open: "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://GitHub.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://GITHUB.COM/").0, "com.google.Chrome");
    let e = build_engine(
        r#"module.exports = { default: "com.apple.Safari", rules: [
            { match: /^https:/i, open: "com.google.Chrome" }
        ] };"#,
    );
    assert_eq!(resolve(&e, "HTTPS://github.com/").0, "com.google.Chrome");
}

#[test]
fn matcher_wildcard_with_implicit_protocol_prefix() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: "zoom.us/j/*", open: "us.zoom.xos" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://zoom.us/j/123").0, "us.zoom.xos");
    assert_eq!(resolve(&e, "zoom.us/j/123").0, "us.zoom.xos");
    assert_eq!(resolve(&e, "https://other.com/").0, "com.apple.Safari");
}

#[test]
fn matcher_from_reads_opener_bundle_id() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: from("com.tinyspeck.slackmacgap"),
                          open: "com.google.Chrome" }],
            };"#,
    );
    let slack = opener("com.tinyspeck.slackmacgap", "Slack");
    let (browser, _) = resolve_with(&e, "https://x/", &slack, ModifierFlags::default());
    assert_eq!(browser, "com.google.Chrome");

    let mail = opener("com.apple.Mail", "Mail");
    let (browser, _) = resolve_with(&e, "https://x/", &mail, ModifierFlags::default());
    assert_eq!(browser, "com.apple.Safari");
}

#[test]
fn matcher_array_is_or() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: ["github.com", "gitlab.com"],
                          open: "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://gitlab.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://other.com/").0, "com.apple.Safari");
}

#[test]
fn matcher_fn_url_only() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: (url) => url.searchParams.get("browser") === "chrome",
                          open: "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(
        resolve(&e, "https://x/?browser=chrome").0,
        "com.google.Chrome"
    );
    assert_eq!(
        resolve(&e, "https://x/?browser=other").0,
        "com.apple.Safari"
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
}

#[test]
fn matcher_fn_with_ctx_reads_opener_and_modifiers() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) =>
                        ctx.opener.bundleId === "com.outlook.X" && ctx.modifiers.shift,
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    let outlook = opener("com.outlook.X", "Outlook");
    let no_shift = ModifierFlags::default();
    let with_shift = ModifierFlags {
        shift: true,
        ..ModifierFlags::default()
    };
    assert_eq!(
        resolve_with(&e, "https://x/", &outlook, no_shift).0,
        "com.apple.Safari",
    );
    assert_eq!(
        resolve_with(&e, "https://x/", &outlook, with_shift).0,
        "com.google.Chrome",
    );
}

// ---------- compile_rewriter per variant ----------

#[test]
fn rewriter_strip_removes_named_params() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [strip("utm_source", "utm_medium", "fbclid")],
            };"#,
    );
    let (_, url) = resolve(&e, "https://x/?utm_source=a&q=1&fbclid=xyz");
    assert_eq!(url, "https://x/?q=1");
}

#[test]
fn rewriter_strip_prefix_wildcard() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [strip("utm_*")],
            };"#,
    );
    let (_, url) = resolve(&e, "https://x/?utm_a=1&utm_b=2&keep=ok");
    assert_eq!(url, "https://x/?keep=ok");
}

#[test]
fn rewriter_literal_string_replaces_url() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: "old.example.com/*",
                            url: "https://new.example.com/" }],
            };"#,
    );
    let (_, url) = resolve(&e, "https://old.example.com/path");
    assert_eq!(url, "https://new.example.com/");
}

#[test]
fn rewriter_regex_matches_preserve_flags_and_array_alternatives() {
    let engine = build_engine(
        r#"module.exports = {
        default: 'com.apple.Safari',
        rewrite: [{match: [/old\.example/i, /legacy\.example/], url: 'https://new.example/'}]
    };"#,
    );
    for url in ["https://OLD.example/", "https://legacy.example/"] {
        assert_eq!(resolve(&engine, url).1, "https://new.example/");
    }
    assert_eq!(
        resolve(&engine, "https://other.example/").1,
        "https://other.example/"
    );
}

#[test]
fn rewriter_fn_returning_string() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: "*.medium.com/*",
                            url: (url) => "https://scribe.rip" + url.pathname }],
            };"#,
    );
    let (_, url) = resolve(&e, "https://x.medium.com/some-article");
    assert_eq!(url, "https://scribe.rip/some-article");
}

#[test]
fn rewriter_fn_returning_url_instance_via_mutation() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{
                    match: (url) => url.protocol === "http:",
                    url: (url) => { url.protocol = "https:"; return url; },
                }],
            };"#,
    );
    let (_, url) = resolve(&e, "http://example.com/path");
    assert_eq!(url, "https://example.com/path");
}

#[test]
fn rewriter_mutation_does_not_survive_a_return_to_the_original_url() {
    for args in ["url", "url, ctx"] {
        for later_args in ["url", "url, ctx"] {
            let e = build_engine(&format!(
                r#"module.exports = {{
                    default: "com.apple.Safari",
                    rewrite: [
                        {{ match: (url, ctx) => true,
                           url: ({args}) => {{ url.hostname = "changed.example"; return url; }} }},
                        {{ match: "changed.example", url: "https://original.example/path" }},
                        {{ match: (url) => true, url: ({later_args}) => url }},
                    ],
                }};"#,
            ));
            assert_eq!(
                resolve(&e, "https://original.example/path").1,
                "https://original.example/path",
                "rewriter args: {args}; later args: {later_args}"
            );
        }
    }
}

#[test]
fn rewriter_mutation_does_not_leak_after_passthrough_or_exception() {
    let path = unique_tmp("rewrite-mutation-errors");
    let diagnostics = Rc::new(DiagnosticLog::at_path(path.clone()));
    let mut results = Vec::new();
    for args in ["url", "url, ctx"] {
        for outcome in [
            "return undefined;",
            "return 'https://original.example/path';",
            "throw new Error('rewrite failed');",
        ] {
            let e = build_engine_with_diagnostics(
                &format!(
                    r#"module.exports = {{
                        default: "com.apple.Safari",
                        rewrite: [{{
                            match: (url, ctx) => true,
                            url: ({args}) => {{ url.hostname = "changed.example"; {outcome} }},
                        }}],
                        rules: [{{
                            match: (url) => url.hostname === "original.example",
                            open: (url, ctx) => url.hostname === "original.example"
                                ? "com.google.Chrome" : "com.brave.Browser",
                        }}],
                    }};"#,
                ),
                Rc::clone(&diagnostics),
            );
            results.push(resolve(&e, "https://original.example/path"));
        }
    }
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        results,
        vec![
            (
                "com.google.Chrome".to_string(),
                "https://original.example/path".to_string()
            );
            6
        ]
    );
}

#[test]
fn rewriter_fn_returning_legacy_object_concatenates_fields() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{
                    match: "*.slack.com/archives/*",
                    url: (url) => ({ protocol: "slack", host: "channel",
                                     pathname: "", search: "team=foo" }),
                }],
            };"#,
    );
    let (_, url) = resolve(&e, "https://acme.slack.com/archives/C0/p1");
    assert_eq!(url, "slack://channel?team=foo");
}

#[test]
fn rewriter_fn_returning_null_drops_url() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: (url) => url.hostname === "tracking.example.com",
                            url: () => null }],
            };"#,
    );
    let (browser, url) = resolve(&e, "https://tracking.example.com/pixel");
    assert_eq!(browser, ""); // suppress
    assert_eq!(url, "about:blank");
}

#[test]
fn rewriter_fn_returning_undefined_passes_through() {
    // Finicky v4 contract: undefined return = leave the URL alone.
    // Distinct from null (drop). Pin both behaviours together.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [
                    { match: () => true, url: () => undefined },
                ],
            };"#,
    );
    let (browser, url) = resolve(&e, "https://example.com/path?q=1");
    assert_eq!(browser, "com.apple.Safari");
    assert_eq!(url, "https://example.com/path?q=1");
}

#[test]
fn rewriter_fn_with_no_explicit_return_is_pass_through() {
    // Functions with no `return` statement implicitly return undefined,
    // which the prelude maps to "no change". Same as the explicit
    // undefined return.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [
                    { match: () => true, url: () => { /* no return */ } },
                ],
            };"#,
    );
    let (_, url) = resolve(&e, "https://x.example/path");
    assert_eq!(url, "https://x.example/path");
}

#[test]
fn rewriter_fn_returning_url_with_no_changes_is_pass_through() {
    // Returning the URL instance unchanged should yield the same href.
    // Tests both the URL-instance return path and the
    // `if s == url` shortcut in apply_rewrite.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [
                    { match: () => true, url: (url) => url },
                ],
            };"#,
    );
    let (_, url) = resolve(&e, "https://example.com/path?q=1");
    assert_eq!(url, "https://example.com/path?q=1");
}

#[test]
fn target_fn_returning_null_falls_through_to_next_fn_rule() {
    // Three consecutive fn-only rules. Rule 0's matcher fires but its
    // target fn returns null (Finicky `open: () => null` shape for
    // "rule matched but skip routing"). Rule 1's target fn returns
    // null too. Rule 2's target fn returns a real browser.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    { match: (url) => url.hostname === "github.com",
                      open: (url) => null },
                    { match: (url) => url.hostname === "github.com",
                      open: (url) => null },
                    { match: (url) => url.hostname === "github.com",
                      open: (url) => "com.google.Chrome" },
                ],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/foo").0, "com.google.Chrome");
}

#[test]
fn fn_matcher_receives_declared_arity_and_global_receiver() {
    let mut results = Vec::new();
    for matcher in [
        "function(url) { return arguments.length === 1; }",
        "function() { return arguments.length === 1; }",
        "(url, ctx = {}) => Object.keys(ctx).length === 0",
        "function(url, ctx) { return arguments.length === 2 && ctx.originalUrl === url.href; }",
        "function(url) { 'use strict'; return this === globalThis; }",
        "Object.assign(function(url) { 'use strict';
            return this === globalThis && arguments.length === 1;
        }, {call: () => false, bind: () => false})",
        "Object.assign(function(url, ctx) { 'use strict';
            return this === globalThis && arguments.length === 2 && ctx.originalUrl === url.href;
        }, {call: () => false, bind: () => false})",
    ] {
        let e = build_engine(&format!(
            r#"module.exports = {{
                default: "com.apple.Safari",
                rules: [{{match: {matcher}, open: "com.google.Chrome"}}],
            }};"#,
        ));
        results.push(resolve(&e, "https://example.com/").0);
    }
    assert_eq!(results, vec!["com.google.Chrome"; 7]);
}

#[test]
fn fn_matcher_uses_arity_captured_at_config_load() {
    let e = build_engine(
        r#"const matcher = function(url, ctx) {
                return arguments.length === 2 && ctx.originalUrl === url.href;
            };
            module.exports = {
                default: "com.apple.Safari",
                rules: [
                    {match: url => {
                        Object.defineProperty(matcher, "length", {value: 0});
                        return false;
                    }, open: null},
                    {match: matcher, open: "com.google.Chrome"},
                ],
            };"#,
    );
    assert_eq!(resolve(&e, "https://example.com/").0, "com.google.Chrome");
}

#[test]
fn invalid_regex_matcher_drops_and_warns_but_engine_still_loads() {
    // The Rust regex crate doesn't support JS lookbehind `(?<=…)`.
    // Pre-fix, compile_matcher silently dropped the matcher and the
    // rule loaded with `matchers: []`, meaning the rule never fired
    // with no diagnostic. Verify the engine still loads (we don't
    // panic the config-load on a bad regex — other rules might be
    // fine), the bad rule is inert, and a later valid rule fires
    // as a fallback.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    // Lookbehind: unsupported by `regex` crate.
                    { match: /(?<=test\.)github\.com/, open: "com.brave.Browser" },
                    // Fallback that should still match the URL.
                    { match: "github.com", open: "com.google.Chrome" },
                ],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/foo").0, "com.google.Chrome");
}

#[test]
fn rewriter_with_throwing_href_getter_doesnt_poison_next_matcher() {
    // Regression: when a fn rewriter returns an object whose .href
    // getter throws, the JSC bridge stashes the thrown value on
    // ctx.exception. The fast-path bypass correctly rejected the
    // bad object (type check), but didn't clear the exception state
    // — so the *next* JS call in the same resolve (the next matcher
    // or the helper fall-through) inherited the exception and
    // produced "unexpected fall-through to default" symptoms.
    //
    // The setup: a fn rewriter returns `{get href() { throw … }}`.
    // The fast-path read of `.href` triggers the throw. After the
    // exception is cleared, the next rule's matcher runs and routes
    // normally.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [
                    {
                        match: "trigger.example.com",
                        url: (url) => ({ get href() { throw new Error("nope"); } }),
                    },
                ],
                rules: [
                    { match: "trigger.example.com", open: "com.google.Chrome" },
                ],
            };"#,
    );
    // The rewrite returns a poisoned object; fast-path bypass sees
    // the throwing getter, rejects, leaves URL unchanged. The rule's
    // matcher then evaluates against the original URL and fires.
    assert_eq!(
        resolve(&e, "https://trigger.example.com/").0,
        "com.google.Chrome"
    );
}

#[test]
fn url_polyfill_parses_ipv6_host_literal() {
    // Regression: the URL polyfill regex's hostname class was
    // `[^:\/?#]*`, which stopped at the first `:` inside an IPv6
    // literal — `https://[::1]:8080/path` parsed with hostname=`[`
    // and the rest of the address leaked into pathname. A user fn
    // matcher reading `url.hostname` could never match an IPv6 URL
    // correctly. After the fix, the regex alternates between an
    // IPv6-bracket branch and the bare-host branch.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url) => url.hostname === "[::1]" && url.port === "8080",
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert_eq!(
        resolve(&e, "https://[::1]:8080/path").0,
        "com.google.Chrome"
    );
}

#[test]
fn url_polyfill_serialises_ipv6_round_trip() {
    // After parsing IPv6, rebuildHref must keep the brackets so
    // `url.href` round-trips. Verify via a no-op rewrite that
    // returns the polyfill instance — Grinch reads .href via the
    // fast-path bypass and resolves with that string.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{
                    match: (url) => url.hostname === "[2001:db8::1]",
                    url: (url) => url,
                }],
            };"#,
    );
    let (_, url) = resolve(&e, "https://[2001:db8::1]/api?q=1");
    assert_eq!(url, "https://[2001:db8::1]/api?q=1");
}

#[test]
fn rewriter_chain_applies_in_order() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [
                    strip("utm_source"),
                    {
                        match: (url) => url.protocol === "http:",
                        url: (url) => { url.protocol = "https:"; return url; },
                    },
                ],
            };"#,
    );
    let (_, url) = resolve(&e, "http://example.com/?utm_source=a&q=1");
    assert_eq!(url, "https://example.com/?q=1");
}

// ---------- Targets ----------

#[test]
fn target_null_suppresses() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: "tracking.com", open: null }],
            };"#,
    );
    let (browser, url) = resolve(&e, "https://tracking.com/pixel");
    assert_eq!(browser, "");
    assert_eq!(url, "about:blank");
}

#[test]
fn consecutive_fn_rules_pick_first_matching_rule() {
    // Four consecutive fn-only rules — only the second one matches.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    { match: (url) => url.hostname === "miss-a", open: "com.apple.Mail" },
                    { match: (url) => url.hostname === "github.com", open: "com.google.Chrome" },
                    { match: (url) => url.hostname === "miss-c", open: "com.brave.Browser" },
                    { match: (url) => url.hostname === "miss-d", open: "com.microsoft.edgemac" },
                ],
            };"#,
    );
    let (b, _) = resolve(&e, "https://github.com/foo");
    assert_eq!(b, "com.google.Chrome");
}

#[test]
fn consecutive_fn_rules_fall_through_to_default_when_nothing_matches() {
    // Same shape as the slow-native bench. No matcher matches.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    { match: (url, ctx) => ctx.opener && ctx.opener.bundleId === "a.example",
                      open: "com.google.Chrome" },
                    { match: (url, ctx) => ctx.opener && ctx.opener.bundleId === "b.example",
                      open: "com.google.Chrome" },
                    { match: (url, ctx) => ctx.opener && ctx.opener.bundleId === "c.example",
                      open: "com.google.Chrome" },
                    { match: (url, ctx) => ctx.opener && ctx.opener.bundleId === "d.example",
                      open: "com.google.Chrome" },
                ],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
}

#[test]
fn throwing_fn_matcher_does_not_affect_neighbouring_rules() {
    // A matcher that throws is a non-match and must not poison the
    // matchers around it.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [
                    { match: (url) => url.hostname === "miss-a", open: "com.apple.Mail" },
                    { match: (url) => { throw new Error("boom"); }, open: "com.brave.Browser" },
                    { match: (url) => url.hostname === "github.com", open: "com.google.Chrome" },
                ],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/foo").0, "com.google.Chrome");
}

#[test]
fn target_fn_returning_string() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: (url) => true, open: (url) => "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}

#[test]
fn target_fn_returning_browser_object() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: (url) => true,
                          open: (url) => ({ name: "com.google.Chrome",
                                            args: ["--incognito"] }) }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}

#[test]
fn target_browser_key_lookup_against_browsers_map() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                browsers: { work: { name: "com.google.Chrome", args: ["--guest"] } },
                rules: [{ match: "x.com", open: "work" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x.com/").0, "com.google.Chrome");
}

#[test]
fn target_browser_alias_finicky_browser_field() {
    // Finicky uses `browser:` where Grinch uses `open:` — should accept both.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: "x.com", browser: "com.google.Chrome" }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x.com/").0, "com.google.Chrome");
}

// ---------- Combined entries ----------

#[test]
fn combined_match_url_open_rewrites_then_routes() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: "itunes.apple.com/app/*",
                    url: (url) => "https://apps.apple.com" + url.pathname,
                    open: "com.apple.AppStore",
                }],
            };"#,
    );
    let (browser, url) = resolve(&e, "https://itunes.apple.com/app/123");
    assert_eq!(browser, "com.apple.AppStore");
    assert_eq!(url, "https://apps.apple.com/app/123");
}

// ---------- ctx semantics ----------

#[test]
fn ctx_modifiers_includes_caps_lock_and_function() {
    // Pin the v4 shape: ctx.modifiers exposes seven keys.
    // shift/option/command/control/capsLock/fn/function — fn and
    // function carry the same value (Finicky-style alias).
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url, ctx) => "k:" + Object.keys(ctx.modifiers).sort().join(","),
                }],
            };"#,
    );
    // Sorted: capsLock, command, control, fn, function, option, shift.
    assert_eq!(
        resolve(&e, "https://x/").0,
        "k:capsLock,command,control,fn,function,option,shift",
    );
}

#[test]
fn ctx_modifiers_caps_lock_value_propagates() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.modifiers.capsLock,
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    // No caps lock — falls through to default.
    assert_eq!(
        resolve_with(
            &e,
            "https://x/",
            &Opener::default(),
            ModifierFlags::default()
        )
        .0,
        "com.apple.Safari",
    );
    // Caps lock on — matches.
    let caps = ModifierFlags {
        caps_lock: true,
        ..ModifierFlags::default()
    };
    assert_eq!(
        resolve_with(&e, "https://x/", &Opener::default(), caps).0,
        "com.google.Chrome",
    );
}

#[test]
fn ctx_modifiers_function_alias_matches_fn() {
    // Finicky exposes both `fn` and `function` with the same value.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.modifiers.fn === ctx.modifiers.function,
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}

#[test]
fn ctx_url_pinned_to_input_after_global_rewrite() {
    // ctx.url stays as the original input even when global rewrites have
    // mutated the URL — by design, so handlers can branch on the click.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: (url) => true,
                            url: (url) => "https://rewritten.com/" }],
                rules: [{
                    match: (url, ctx) => ctx.url === "https://original.com/",
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    let (browser, url) = resolve(&e, "https://original.com/");
    assert_eq!(browser, "com.google.Chrome");
    assert_eq!(url, "https://rewritten.com/");
}

#[test]
fn ctx_originalurl_aliases_url() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.url === ctx.originalUrl,
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}

// ---------- UserFn arity contract ----------

#[test]
fn arity_url_only_clears_runtime_needs() {
    // A url-only matcher must NOT mark needs_opener / needs_modifiers,
    // so AppDelegate skips frontmost_opener() and current_modifier_flags()
    // entirely on real clicks.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{ match: (url) => url.hostname === "x",
                          open: "com.google.Chrome" }],
            };"#,
    );
    assert!(!e.needs_opener());
    assert!(!e.needs_modifiers());
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}

#[test]
fn arity_with_ctx_marks_runtime_needs() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.opener.bundleId === "x",
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert!(e.needs_opener());
    assert!(e.needs_modifiers());
    assert!(e.needs_opener_full());
}

#[test]
fn config_that_trashes_prelude_global_returns_error_not_panic() {
    // Hostile/buggy config that nukes a prelude global should produce
    // a clean EngineError so the previous engine survives a SIGHUP
    // reload, not a panic that tears down the process.
    let result = try_build_engine(
        r#"RegExp = null;
               module.exports = { default: "com.apple.Safari" };"#,
    );
    match result {
        Err(EngineError::PreludeBroken { global }) => assert_eq!(global, "RegExp"),
        Err(other) => panic!("wrong error variant: {other:?}"),
        Ok(_) => panic!("expected PreludeBroken, got Ok"),
    }
}

#[test]
fn config_with_circular_browsers_map_does_not_stack_overflow() {
    // Regression: iter_object used to call v.toDictionary() which
    // recursively converted every value to its NS* equivalent and
    // blew the stack on circular references. The Object.keys path
    // walks names only — circular *values* are safe; we just hand
    // the JSValue back to parse_browser_jsval, which reads specific
    // keys (name/id/profile/...) without deep traversal.
    let e = build_engine(
        r#"var x = {};
               x.self = x;
               module.exports = {
                 default: "com.apple.Safari",
                 browsers: { broken: x },
               };"#,
    );
    // Resolves without panicking; broken-browser entry is a no-op spec
    // (no `name`/`id`), so the rule below falls through to the default.
    assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
}

#[test]
fn config_that_nulls_make_ctx_helper_returns_error_not_panic() {
    // Function-declaration globals can't be `delete`d (non-configurable),
    // but can be assigned over.
    let result = try_build_engine(
        r#"globalThis.__grinchMakeCtx = null;
               module.exports = { default: "com.apple.Safari" };"#,
    );
    match result {
        Err(EngineError::PreludeBroken { global }) => assert_eq!(global, "__grinchMakeCtx"),
        Err(other) => panic!("wrong error variant: {other:?}"),
        Ok(_) => panic!("expected PreludeBroken, got Ok"),
    }
}

#[test]
fn from_matcher_needs_opener_but_not_full() {
    // `from()` matchers only read opener.bundle_id — AppDelegate can use
    // the lite `frontmost_opener_id` path that skips localizedName /
    // executableURL IPC.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: from("com.tinyspeck.slackmacgap"),
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert!(e.needs_opener());
    assert!(!e.needs_modifiers());
    assert!(!e.needs_opener_full());
}

#[test]
fn arity_zero_treated_as_url_only() {
    // `() => null` is length 0 — Grinch's contract is `length >= 2 → ctx`,
    // so length 0 is treated as url-only too.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: "x", url: () => null }],
            };"#,
    );
    assert!(!e.needs_opener());
    let (browser, url) = resolve(&e, "https://x");
    assert_eq!(browser, "");
    assert_eq!(url, "about:blank");
}

#[test]
fn arity_default_param_is_treated_as_url_only_per_contract() {
    // (url, ctx = {}) — JS's `f.length` excludes default-param slots, so
    // it reads as 1, and Grinch's contract treats it as url-only. The
    // user's default `{}` kicks in. Documented footgun; this test pins
    // the behaviour so we notice if it ever changes.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx = {}) => (ctx.opener && ctx.opener.bundleId) === "x",
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert!(!e.needs_opener());
    // Even with a "real" opener, the matcher sees ctx = {} (its default),
    // so `ctx.opener` is undefined and the rule never fires.
    let real = opener("x", "X");
    assert_eq!(
        resolve_with(&e, "https://x/", &real, ModifierFlags::default()).0,
        "com.apple.Safari",
    );
}

// ---------- URL polyfill ----------

#[test]
fn legacy_url_string_returns_href() {
    // url.urlString is the v3 alias for url.href. Shim warns and
    // returns the same value.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url) => url.urlString === url.href,
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/path").0, "com.google.Chrome");
}

#[test]
fn legacy_url_url_returns_legacy_object_shape() {
    // url.url returns a plain LegacyURLObject. Verify the shape.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url) => {
                        var u = url.url;
                        return u.protocol === "https"
                            && u.hostname === "github.com"
                            && u.pathname === "/x"
                            && u.search === "q=1"
                            && u.hash === "frag";
                    },
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert_eq!(
        resolve(&e, "https://github.com/x?q=1#frag").0,
        "com.google.Chrome",
    );
}

#[test]
fn legacy_url_opener_returns_active_opener_with_warning() {
    // Match Finicky v4: url.opener warns and returns the live opener.
    // The opener publishes onto a per-resolve global from
    // `__grinchMakeCtx`, so we need a 2-arg fn (which triggers ctx
    // build) for the value to be set. The matcher reads `url.opener`
    // and checks the bundle ID — without the warn-and-return shim
    // this would have thrown.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, _ctx) => url.opener && url.opener.bundleId === "com.x",
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    let known = opener("com.x", "X");
    let (browser, _) = resolve_with(&e, "https://x/", &known, ModifierFlags::default());
    assert_eq!(browser, "com.google.Chrome");
}

#[test]
fn legacy_url_keys_throws_with_helpful_message() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url) => {
                        try { url.keys; return false; }
                        catch (e) {
                            return e.message.indexOf("ctx.modifiers") !== -1
                                && e.message.indexOf("getModifierKeys") !== -1;
                        }
                    },
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}

#[test]
fn polyfill_url_round_trips_full_href() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: (url) => true, url: (url) => url.href }],
            };"#,
    );
    let (_, url) = resolve(&e, "https://user:pw@example.com:8443/path?q=1#frag");
    assert_eq!(url, "https://user:pw@example.com:8443/path?q=1#frag");
}

#[test]
fn polyfill_preserves_opaque_scheme_on_rewrite() {
    // Regression: rebuildHref used to unconditionally emit `scheme://...`,
    // turning `mailto:user@example.com` into `mailto://user@example.com`.
    // Verify the opaque schemes round-trip through a rewrite that
    // returns the URL object unchanged.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: () => true, url: (url) => url }],
            };"#,
    );
    assert_eq!(
        resolve(&e, "mailto:user@example.com").1,
        "mailto:user@example.com"
    );
    assert_eq!(resolve(&e, "tel:+15551234567").1, "tel:+15551234567");
    assert_eq!(resolve(&e, "javascript:void(0)").1, "javascript:void(0)");
    assert_eq!(resolve(&e, "myapp://Open/Thing").1, "myapp://Open/Thing");
    assert_eq!(
        resolve(&e, "https://EXAMPLE.COM/path").1,
        "https://example.com/path"
    );
    // Hierarchical schemes still get the `//`.
    assert_eq!(
        resolve(&e, "https://example.com/path").1,
        "https://example.com/path"
    );
    let setter = build_engine(
        r#"module.exports = { default: "com.apple.Safari",
        rewrite: [{ match: () => true, url: url => { url.host = "NewHost:99"; return url; } }] };"#,
    );
    assert_eq!(
        resolve(&setter, "myapp://Open/Thing").1,
        "myapp://NewHost:99/Thing"
    );
    assert_eq!(
        resolve(&setter, "https://Open/Thing").1,
        "https://newhost:99/Thing"
    );
}

#[test]
fn polyfill_searchparams_value_with_equals_signs() {
    // Regression: split("=") + kv[1] used to truncate values containing
    // `=` (signed tokens, base64 payloads, nested query strings). The
    // WHATWG split-on-first-= behaviour preserves the full value.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url) => "v:" + url.searchParams.get("token"),
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/?token=a=b=c&q=1").0, "v:a=b=c");
}

#[test]
fn polyfill_searchparams_immune_to_object_prototype_pollution() {
    // Regression: `_m: {}` exposed every URL's searchParams to
    // Object.prototype mutations — `Object.prototype.utm = ["x"]`
    // injected a phantom "utm" entry into every URL. Object.create(null)
    // backing object has no prototype, so for-in only enumerates own
    // keys.
    let e = build_engine(
        r#"Object.prototype.utm = ["polluted"];
               module.exports = {
                 default: "com.apple.Safari",
                 rules: [{
                   match: () => true,
                   open: (url) => "n:" + url.searchParams.size +
                                  ",has:" + (url.searchParams.has("utm") ? "yes" : "no"),
                 }],
               };"#,
    );
    // Clean URL: zero own keys, no "utm".
    assert_eq!(resolve(&e, "https://x/").0, "n:0,has:no");
    // Real ?utm=… still works.
    assert_eq!(resolve(&e, "https://x/?utm=real").0, "n:1,has:yes");
}

#[test]
fn polyfill_searchparams_set_and_delete_propagate() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{
                    match: (url) => true,
                    url: (url) => {
                        url.searchParams.delete("utm_source");
                        url.searchParams.set("added", "1");
                        return url;
                    },
                }],
            };"#,
    );
    let (_, url) = resolve(&e, "https://x/?utm_source=a&q=1");
    // searchParams iteration order is implementation-defined for `set`
    // on a brand-new key, so check the components rather than full eq.
    assert!(!url.contains("utm_source"));
    assert!(url.contains("q=1"));
    assert!(url.contains("added=1"));
}

#[test]
fn polyfill_searchparams_size_property() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url) => "n:" + url.searchParams.size,
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/?a=1&b=2&c=3").0, "n:3");
    assert_eq!(resolve(&e, "https://x/").0, "n:0");
}

#[test]
fn polyfill_searchparams_for_of_iterates_pairs() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url) => {
                        var keys = [];
                        for (var pair of url.searchParams) keys.push(pair[0]);
                        return "k:" + keys.join(",");
                    },
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/?a=1&b=2&c=3").0, "k:a,b,c");
}

#[test]
fn polyfill_searchparams_for_each_with_value_key_args() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url) => {
                        var seen = [];
                        url.searchParams.forEach(function(value, key) {
                            seen.push(key + "=" + value);
                        });
                        return "p:" + seen.join("|");
                    },
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/?a=1&b=2").0, "p:a=1|b=2");
}

#[test]
fn polyfill_searchparams_keys_values_iterators() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: (url) => {
                        var ks = []; var vs = [];
                        for (var k of url.searchParams.keys())   ks.push(k);
                        for (var v of url.searchParams.values()) vs.push(v);
                        return ks.join(",") + "/" + vs.join(",");
                    },
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/?a=1&b=2").0, "a,b/1,2");
}

#[test]
fn polyfill_hostname_setter_propagates_to_href() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rewrite: [{ match: (url) => true,
                            url: (url) => { url.hostname = "moved.com"; return url; } }],
            };"#,
    );
    let (_, url) = resolve(&e, "https://original.com/path");
    assert_eq!(url, "https://moved.com/path");
}

// ---------- Parse-side warnings ----------

#[test]
fn parse_browser_jsval_handles_args_and_openinbackground() {
    // Object form with both fields. We can't directly read BrowserSpec,
    // but we can verify it routes correctly and the engine accepted it.
    let e = build_engine(
        r#"module.exports = {
                default: { name: "com.spotify.client", openInBackground: true,
                           args: ["--no-fork"] },
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.spotify.client");
}

#[test]
fn raw_args_force_new_instances_for_static_and_dynamic_targets() {
    for (args, expected) in [
        ("[]", LaunchPlan::OpenUrls { activates: false }),
        (
            r#"["--profile-directory=Profile 2"]"#,
            LaunchPlan::LaunchApplication {
                args: vec![
                    "--profile-directory=Profile 2".to_string(),
                    "https://x/".to_string(),
                ],
                new_instance: true,
                activates: false,
            },
        ),
    ] {
        let browser =
            format!(r#"{{ id: "com.google.Chrome", args: {args}, openInBackground: true }}"#);
        for source in [
            format!("module.exports = {{ default: {browser} }};"),
            format!("module.exports = {{ default: () => ({browser}) }};"),
            format!(
                r#"module.exports = {{ default: null,
                    rules: [{{ match: "*", open: {browser} }}] }};"#
            ),
            format!(
                r#"module.exports = {{ default: null,
                    rules: [{{ match: "*", open: () => ({browser}) }}] }};"#
            ),
            format!(r#"module.exports = {{ browsers: {{ test: {browser} }}, default: "test" }};"#),
        ] {
            let engine = build_engine(&source);
            let result = engine.resolve("https://x/", &Opener::default(), ModifierFlags::default());
            assert_eq!(result.browser.bundle_id, "com.google.Chrome", "{source}");
            assert_eq!(
                LaunchPlan::from_spec(&result.browser, &result.url),
                expected,
                "{source}"
            );
        }
    }
}

#[test]
fn browser_spec_string_with_profile_shorthand() {
    // Finicky-style "Name:Profile" shorthand. Splits on first `:`
    // when the prefix resolves to a Chromium-family browser.
    let e = build_engine(r#"module.exports = { default: "com.google.Chrome:Profile 1" };"#);
    // Browser ID survives unchanged; profile expansion is into args
    // (not directly observable from resolve()'s public surface, but
    // we can at least verify the bundle ID is right).
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}

#[test]
fn firefox_selectable_profile_routing() {
    const CHILD: &str = "GRINCH_FIREFOX_GROUP_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::integration_tests::firefox_selectable_profile_routing",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    let home = unique_tmp("firefox-groups");
    let root = home.join("Library/Application Support/Firefox");
    let groups = root.join("Profile Groups");
    std::fs::create_dir_all(&groups).unwrap();
    for name in ["work", "one", "two"] {
        std::fs::create_dir_all(root.join("Profiles").join(name)).unwrap();
    }
    std::fs::write(
        root.join("profiles.ini"),
        "[Profile0]\nName=Legacy\nStoreID=Group1\n",
    )
    .unwrap();
    let status = std::process::Command::new("/usr/bin/sqlite3")
        .arg(groups.join("Group1.sqlite"))
        .arg("CREATE TABLE Profiles(id INTEGER, name TEXT, path TEXT); INSERT INTO Profiles VALUES (1,'Work','Profiles/work'), (2,'Legacy','Profiles/work'), (3,'Same','Profiles/one'), (4,'Same','Profiles/two'), (5,'Missing','Profiles/missing'), (6,'Café','Profiles/work');")
        .status().unwrap();
    assert!(status.success());
    with_home(&home, || {
        for (profile, expected) in [
            (
                "Work",
                vec![
                    "--profile".to_string(),
                    root.join("Profiles/work").to_string_lossy().into_owned(),
                ],
            ),
            ("Legacy", vec!["-P".to_string(), "Legacy".to_string()]),
            (
                "Café",
                vec![
                    "--profile".to_string(),
                    root.join("Profiles/work").to_string_lossy().into_owned(),
                ],
            ),
            (
                "~/Library/Application Support/Firefox/Profiles/work",
                vec![
                    "--profile".to_string(),
                    root.join("Profiles/work").to_string_lossy().into_owned(),
                ],
            ),
            (
                root.join("Profiles/work").to_str().unwrap(),
                vec![
                    "--profile".to_string(),
                    root.join("Profiles/work").to_string_lossy().into_owned(),
                ],
            ),
        ] {
            let source = format!(
                "module.exports = {{default: {{name:'org.mozilla.firefox', profile:{}}}}};",
                serde_json::to_string(profile).unwrap()
            );
            let e = build_engine(&source);
            let result = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
            assert_eq!(result.browser.args, expected, "{profile}");
        }
        for profile in ["Same", "Missing", "/missing/grinch-profile"] {
            let e = build_engine(&format!(
                "module.exports = {{default: {{name:'org.mozilla.firefox', profile:{}}}}};",
                serde_json::to_string(profile).unwrap()
            ));
            let result = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
            assert!(result.browser.bundle_id.is_empty(), "{profile}");
        }
        std::fs::write(
            root.join("profiles.ini"),
            "[Profile0]\nName=Work\n[Profile1]\nName=Work\n",
        )
        .unwrap();
        let engine = build_engine("module.exports = {default: 'org.mozilla.firefox:Work'};");
        assert!(resolve(&engine, "https://example.com/").0.is_empty());
        for ini in [
            "[Profile0]\nName=Legacy\nStoreID=Group1\n[Profile1]\nStoreID=Missing\n",
            "[Profile0]\nName=Legacy\nStoreID=../Group1\n",
        ] {
            std::fs::write(root.join("profiles.ini"), ini).unwrap();
            let engine = build_engine("module.exports = {default: 'org.mozilla.firefox:Work'};");
            let result = engine.resolve("https://x/", &Opener::default(), ModifierFlags::default());
            assert!(result.browser.bundle_id.is_empty(), "{ini}");
        }
        std::fs::write(
            root.join("profiles.ini"),
            "[Profile0]\nName=Legacy\nStoreID=Group1\n",
        )
        .unwrap();
        let status = std::process::Command::new("/usr/bin/sqlite3")
            .arg(groups.join("Group1.sqlite"))
            .arg("INSERT INTO Profiles VALUES (7, x'c3a9', 'Profiles/work');")
            .status()
            .unwrap();
        assert!(status.success());
        let diagnostics = Rc::new(isolated_diagnostics());
        let engine = build_engine_with_diagnostics(
            "module.exports = {default: 'org.mozilla.firefox:Work'};",
            Rc::clone(&diagnostics),
        );
        assert!(resolve(&engine, "https://example.com/").0.is_empty());
        assert!(diagnostics.profile_error().is_some());
        let events = std::fs::read_to_string(diagnostics.ensure_file().unwrap()).unwrap();
        assert!(events.contains("binary data in Firefox profile name or path"));
    });
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn profile_read_failures_and_reload() {
    // Run alone: the fixture changes HOME and exercises process-wide profile caches.
    const CHILD: &str = "GRINCH_PROFILE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::integration_tests::profile_read_failures_and_reload",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("1 passed"), "child ran no tests:\n{stdout}");
        return;
    }
    let tmp = unique_tmp("profile-access");
    std::fs::create_dir_all(&tmp).unwrap();
    with_home(&tmp, || check_profile_read_failures(&tmp));
    std::fs::remove_dir_all(tmp).unwrap();
}

fn check_profile_read_failures(tmp: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let chrome = tmp.join("Library/Application Support/Google/Chrome/Local State");
    let firefox = tmp.join("Library/Application Support/Firefox/profiles.ini");
    for path in [&chrome, &firefox] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    }
    std::fs::write(
        &chrome,
        r#"{"profile":{"info_cache":{"Profile 7":{"name":"Work"}}}}"#,
    )
    .unwrap();
    std::fs::write(&firefox, "[Profile0]\nName=Work\n").unwrap();
    for path in [&chrome, &firefox] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
    let log = tmp.join("profile.log");
    let diagnostics = Rc::new(DiagnosticLog::at_path(log.clone()));
    let engine = build_engine_with_diagnostics(
        "module.exports = {default: 'org.mozilla.firefox:Work'};",
        Rc::clone(&diagnostics),
    );
    assert!(resolve(&engine, "https://x/").0.is_empty());
    assert!(diagnostics.profile_error().is_some());
    check_blocked_profile_routes(&diagnostics);
    let events = read_log_events(&log);
    for path in [&chrome, &firefox] {
        assert!(events.iter().any(|event| {
            event["event"] == "profile_error"
                && event["path"] == path.to_str().unwrap()
                && event["message"]
                    .as_str()
                    .unwrap()
                    .contains("Permission denied")
                && event["message"]
                    .as_str()
                    .unwrap()
                    .contains("Files & Folders")
        }));
    }
    for path in [&chrome, &firefox] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // Reads stay cached until the same Engine::new path used by Reload Config runs.
    assert_eq!(
        crate::chromium::resolve_profile_dir("com.google.Chrome", "Work", diagnostics.as_ref()),
        None
    );
    let engine = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.google.Chrome:Work" };"#,
        Rc::clone(&diagnostics),
    );
    let result = engine.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert_eq!(result.browser.args, ["--profile-directory=Profile 7"]);
    assert!(result.browser.creates_new_instance);
    assert!(diagnostics.profile_error().is_none());
    // Firefox validation must refresh as well: an unknown name now reports known profiles.
    crate::firefox::resolve_profile_name("org.mozilla.firefox", "Missing", &diagnostics);
    assert!(read_log_events(&log).iter().any(|event| {
        event["message"]
            .as_str()
            .unwrap()
            .contains("known profiles: [\"Work\"]")
    }));
    assert!(diagnostics.profile_error().is_none());
    check_firefox_warning_does_not_mask_suppression(&diagnostics);
    check_missing_and_invalid_profile_data(&chrome, &diagnostics, &log);
    // Still in the isolated child; with_home restores HOME after this final check.
    diagnostics.clear_profile_error();
    crate::firefox::clear_profile_cache();
    unsafe { std::env::remove_var("HOME") };
    assert_eq!(
        crate::firefox::resolve_profile_name("org.mozilla.firefox", "Missing", &diagnostics),
        "Missing"
    );
    assert!(diagnostics.profile_error().is_none());
    let events = read_log_events(&log);
    let event = events.last().unwrap();
    assert_eq!(event["event"], "profile_error");
    assert_eq!(event["browser"], "org.mozilla.firefox");
    assert!(event["path"].is_null());
    assert_eq!(event["message"], "Cannot locate profiles.ini; check HOME.");
}

fn check_firefox_warning_does_not_mask_suppression(diagnostics: &Rc<DiagnosticLog>) {
    for target in [
        r#"{ name: "com.google.Chrome", profile: "Missing" }"#,
        r#"() => ({ name: "com.google.Chrome", profile: "Missing" })"#,
    ] {
        // The default is compiled before rules: Firefox warns before Chromium.
        let source = format!(
            r#"module.exports = {{ default: "org.mozilla.firefox:Missing",
                rules: [{{ match: "x", open: {target} }}] }};"#
        );
        let engine = build_engine_with_diagnostics(&source, Rc::clone(diagnostics));
        if target.starts_with("()") {
            assert!(
                diagnostics.profile_error().is_none(),
                "Firefox must not claim the status slot"
            );
        }
        let result = engine.resolve("https://x/", &Opener::default(), ModifierFlags::default());
        assert_eq!(
            LaunchPlan::from_spec(&result.browser, &result.url),
            LaunchPlan::Suppress
        );
        let error = diagnostics.profile_error().unwrap();
        assert!(
            error.starts_with("com.google.Chrome:"),
            "wrong status error: {error}"
        );
        let result = engine.resolve("https://y/", &Opener::default(), ModifierFlags::default());
        assert_eq!(result.browser.bundle_id, "org.mozilla.firefox");
        assert_eq!(result.browser.args, ["-P", "Missing"]);
        assert_eq!(diagnostics.profile_error().as_deref(), Some(error.as_str()));
    }
}

fn check_blocked_profile_routes(diagnostics: &Rc<DiagnosticLog>) {
    for target in [
        r#""com.google.Chrome:Work""#,
        r#"{ name: "com.google.Chrome", profile: "Work", incognito: true }"#,
        r#"() => ({ name: "com.google.Chrome", profile: "Work" })"#,
        r#""work""#,
    ] {
        let browsers = if target == r#""work""# {
            r#"browsers: { work: { name: "com.google.Chrome", profile: "Work" } },"#
        } else {
            ""
        };
        let source = format!(
            r#"module.exports = {{ default: "com.apple.Safari",
            {browsers} rules: [
            {{ match: "x", open: {target} }}, {{ match: "x", open: "com.apple.Safari" }}
        ] }};"#
        );
        let engine = build_engine_with_diagnostics(&source, Rc::clone(diagnostics));
        if target.starts_with("()") {
            assert!(
                diagnostics.profile_error().is_none(),
                "dynamic target has not run yet"
            );
        }
        let result = engine.resolve("https://x/", &Opener::default(), ModifierFlags::default());
        assert_eq!(
            LaunchPlan::from_spec(&result.browser, &result.url),
            LaunchPlan::Suppress
        );
        assert_eq!(result.matched_rule, Some(0));
        assert!(diagnostics.profile_error().is_some());
        let source = format!("module.exports = {{ {browsers} default: {target} }};");
        let engine = build_engine_with_diagnostics(&source, Rc::clone(diagnostics));
        if target.starts_with("()") {
            assert!(
                diagnostics.profile_error().is_none(),
                "dynamic default has not run yet"
            );
        }
        assert!(resolve(&engine, "https://x/").0.is_empty());
        assert!(diagnostics.profile_error().is_some());
    }
    for (target, expected) in [
        (
            "com.google.Chrome:Profile 7",
            vec!["--profile-directory=Profile 7"],
        ),
        (
            "com.google.Chrome:Default",
            vec!["--profile-directory=Default"],
        ),
    ] {
        let source = format!(r#"module.exports = {{ default: "{target}" }};"#);
        let engine = build_engine_with_diagnostics(&source, Rc::clone(diagnostics));
        let result = engine.resolve("https://x/", &Opener::default(), ModifierFlags::default());
        assert_eq!(result.browser.args, expected);
        assert!(result.browser.creates_new_instance);
        assert!(diagnostics.profile_error().is_none());
    }
    // Populate both failed caches after the last engine construction cleared them.
    assert_eq!(
        crate::chromium::resolve_profile_dir("com.google.Chrome", "Work", diagnostics),
        None
    );
}

fn check_missing_and_invalid_profile_data(
    chrome: &std::path::Path,
    diagnostics: &Rc<DiagnosticLog>,
    log: &std::path::Path,
) {
    std::fs::remove_file(chrome).unwrap();
    let engine = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.google.Chrome:Work" };"#,
        Rc::clone(diagnostics),
    );
    assert!(resolve(&engine, "https://x/").0.is_empty());
    assert!(
        read_log_events(log)
            .iter()
            .any(|event| event["message"].as_str().unwrap().contains("No such file"))
    );
    std::fs::write(chrome, "not json").unwrap();
    let engine = build_engine_with_diagnostics(
        r#"module.exports = { default: "com.google.Chrome:Work" };"#,
        Rc::clone(diagnostics),
    );
    assert!(resolve(&engine, "https://x/").0.is_empty());
    assert!(read_log_events(log).iter().any(|event| {
        event["message"]
            .as_str()
            .unwrap()
            .contains("expected ident")
    }));
}

#[test]
fn browser_spec_string_with_no_colon_unchanged() {
    let e = build_engine(r#"module.exports = { default: "com.google.Chrome" };"#);
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}

#[test]
fn parse_browser_jsval_firefox_profile_resolves_via_p_flag() {
    let tmp = unique_tmp("firefox-profile");
    std::fs::create_dir_all(&tmp).unwrap();
    let e = build_engine_with_diagnostics(
        r#"module.exports = {
                default: { name: "org.mozilla.firefox", profile: "Work" },
            };"#,
        Rc::new(DiagnosticLog::at_path(tmp.join("diagnostic.log"))),
    );
    let result = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert_eq!(result.browser.bundle_id, "org.mozilla.firefox");
    assert_eq!(result.browser.args, ["-P", "Work"]);
    assert!(result.browser.creates_new_instance);
    std::fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn parse_browser_jsval_firefox_profile_via_shorthand_string() {
    let tmp = unique_tmp("firefox-profile-shorthand");
    std::fs::create_dir_all(&tmp).unwrap();
    let e = build_engine_with_diagnostics(
        r#"module.exports = { default: "org.mozilla.firefox:Work" };"#,
        Rc::new(DiagnosticLog::at_path(tmp.join("diagnostic.log"))),
    );
    let result = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert_eq!(result.browser.bundle_id, "org.mozilla.firefox");
    assert_eq!(result.browser.args, ["-P", "Work"]);
    assert!(result.browser.creates_new_instance);
    std::fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn parse_browser_jsval_incognito_chromium_emits_incognito_flag() {
    // `incognito: true` should append `--incognito` for Chromium-family
    // browsers and force creates_new_instance (so Chrome's running
    // instance doesn't swallow the URL via Apple Event GURL where the
    // flag would be silently ignored).
    let e = build_engine(
        r#"module.exports = {
                default: { name: "com.google.Chrome", incognito: true },
            };"#,
    );
    let res = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert_eq!(res.browser.bundle_id, "com.google.Chrome");
    assert!(
        res.browser.args.iter().any(|a| a == "--incognito"),
        "expected --incognito in args, got {:?}",
        res.browser.args
    );
    assert!(res.browser.creates_new_instance);
}

#[test]
fn parse_browser_jsval_incognito_firefox_emits_private_window_flag() {
    // Firefox-family analog: --private-window instead of --incognito.
    let e = build_engine(
        r#"module.exports = {
                default: { name: "org.mozilla.firefox", incognito: true },
            };"#,
    );
    let res = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert!(
        res.browser.args.iter().any(|a| a == "--private-window"),
        "expected --private-window in args, got {:?}",
        res.browser.args
    );
    assert!(res.browser.creates_new_instance);
}

#[test]
fn parse_browser_jsval_incognito_false_emits_no_flag() {
    // `incognito: false` is identical to omitting the field — no flag
    // appended, no creates_new_instance forced.
    let e = build_engine(
        r#"module.exports = {
                default: { name: "com.google.Chrome", incognito: false },
            };"#,
    );
    let res = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert!(!res.browser.args.iter().any(|a| a == "--incognito"));
    assert!(!res.browser.creates_new_instance);
}

#[test]
fn parse_browser_jsval_incognito_safari_logs_and_passes_through() {
    // Safari has no CLI private-mode flag; the engine should accept
    // the config but emit no extra args (warning goes to stderr,
    // observable via the unsupported-family branch).
    let e = build_engine(
        r#"module.exports = {
                default: { name: "com.apple.Safari", incognito: true },
            };"#,
    );
    let res = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert_eq!(res.browser.bundle_id, "com.apple.Safari");
    assert!(res.browser.args.is_empty());
    // Safari incognito doesn't trigger the new-instance flag — there's
    // no reason to spawn a fresh helper if we can't pass a useful arg.
    assert!(!res.browser.creates_new_instance);
}

#[test]
fn parse_browser_jsval_open_in_new_window_chromium_emits_new_window_flag() {
    // `openInNewWindow: true` on Chromium adds `--new-window` and forces
    // creates_new_instance so the flag isn't swallowed by GURL routing.
    let e = build_engine(
        r#"module.exports = {
                default: { name: "com.google.Chrome", openInNewWindow: true },
            };"#,
    );
    let res = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert!(
        res.browser.args.iter().any(|a| a == "--new-window"),
        "expected --new-window in args, got {:?}",
        res.browser.args
    );
    assert!(res.browser.creates_new_instance);
}

#[test]
fn parse_browser_jsval_open_in_new_window_firefox_emits_new_window_flag() {
    // Modern Firefox (>= 89) accepts `--new-window` alongside legacy
    // `-new-window`; we emit the modern form for parity with Chromium.
    let e = build_engine(
        r#"module.exports = {
                default: { name: "org.mozilla.firefox", openInNewWindow: true },
            };"#,
    );
    let res = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert!(
        res.browser.args.iter().any(|a| a == "--new-window"),
        "expected --new-window in args, got {:?}",
        res.browser.args
    );
    assert!(res.browser.creates_new_instance);
}

#[test]
fn parse_browser_jsval_open_in_new_window_composes_with_incognito_and_profile() {
    // All three flags set together — args contain all three (in the
    // order profile/incognito/openInNewWindow). Single new-instance
    // launch carries all of them.
    let e = build_engine(
        r#"module.exports = {
                default: { name: "com.google.Chrome",
                           profile: "Profile 1",
                           incognito: true,
                           openInNewWindow: true },
            };"#,
    );
    let res = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert!(
        res.browser
            .args
            .iter()
            .any(|a| a.starts_with("--profile-directory="))
    );
    assert!(res.browser.args.iter().any(|a| a == "--incognito"));
    assert!(res.browser.args.iter().any(|a| a == "--new-window"));
    assert!(res.browser.creates_new_instance);
}

#[test]
fn parse_browser_jsval_open_in_new_window_safari_logs_and_passes_through() {
    // Safari has no equivalent CLI flag; config is accepted, no flag
    // appended, no new-instance forced.
    let e = build_engine(
        r#"module.exports = {
                default: { name: "com.apple.Safari", openInNewWindow: true },
            };"#,
    );
    let res = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert_eq!(res.browser.bundle_id, "com.apple.Safari");
    assert!(res.browser.args.is_empty());
    assert!(!res.browser.creates_new_instance);
}

#[test]
fn parse_browser_jsval_incognito_composes_with_profile() {
    // Both `profile:` and `incognito: true` set — args should contain
    // both flags, and creates_new_instance stays true.
    let e = build_engine(
        r#"module.exports = {
                default: { name: "com.google.Chrome",
                           profile: "Profile 1", incognito: true },
            };"#,
    );
    let res = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert!(res.browser.args.iter().any(|a| a == "--incognito"));
    assert!(
        res.browser
            .args
            .iter()
            .any(|a| a.starts_with("--profile-directory="))
    );
    assert!(res.browser.creates_new_instance);
}

#[test]
fn unsupported_browser_profiles_are_not_opened() {
    for target in [
        r#"{ name: "com.apple.Safari", profile: "Work" }"#,
        r#""com.apple.Safari:Work""#,
        r#"() => ({ name: "com.apple.Safari", profile: "Work" })"#,
    ] {
        let diagnostics = Rc::new(isolated_diagnostics());
        let engine = build_engine_with_diagnostics(
            &format!("module.exports = {{ default: {target} }};"),
            Rc::clone(&diagnostics),
        );
        let result = engine.resolve(
            "https://example.com/",
            &Opener::default(),
            ModifierFlags::default(),
        );
        assert!(result.browser.bundle_id.is_empty(), "{target}");
        assert!(diagnostics.profile_error().is_some(), "{target}");
    }
}

#[test]
fn parse_browser_jsval_apptype_none_suppresses() {
    // appType: "none" is Finicky's explicit no-op browser. Should
    // behave identically to `open: null`.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: "tracking.com",
                    open: { name: "ignored", appType: "none" },
                }],
            };"#,
    );
    let (browser, url) = resolve(&e, "https://tracking.com/");
    assert_eq!(browser, "");
    assert_eq!(url, "about:blank");
}

#[test]
fn browser_spec_paths_keep_the_selected_bundle_copy() {
    let tmp = unique_tmp("browser-path");
    for name in ["First copy #%.app", "Second copy.app"] {
        let path = tmp.join(name);
        std::fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
        std::fs::copy("/usr/bin/true", path.join("Contents/MacOS/Browser")).unwrap();
        std::fs::write(
            path.join("Contents/Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
                <key>CFBundleIdentifier</key><string>com.google.Chrome</string>
                <key>CFBundleExecutable</key><string>Browser</string>
                <key>CFBundlePackageType</key><string>APPL</string>
                </dict></plist>"#,
        )
        .unwrap();
        let quoted = serde_json::to_string(path.to_str().unwrap()).unwrap();
        for expression in [
            quoted.clone(),
            format!("{{ name: {quoted}, appType: 'path' }}"),
            format!("{{ name: {quoted} }}"),
            format!("() => {quoted}"),
            format!("() => ({{ name: {quoted}, appType: 'path' }})"),
            format!("{{ name: {quoted}, appType: 'path', profile: 'Default', incognito: true }}"),
        ] {
            let e = build_engine(&format!("module.exports = {{ default: {expression} }};"));
            let result = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
            assert_eq!(
                result.browser.bundle_id, "com.google.Chrome",
                "{expression}"
            );
            let app_url = crate::workspace::browser_app_url(&result.browser)
                .expect("explicit app path must not require LaunchServices registration");
            assert_eq!(
                app_url.path().unwrap().to_string(),
                path.to_str().unwrap(),
                "{expression}"
            );
            if expression.contains("incognito") {
                assert!(result.browser.args.iter().any(|a| a == "--incognito"));
                assert!(
                    result
                        .browser
                        .args
                        .iter()
                        .any(|a| a == "--profile-directory=Default")
                );
            }
        }
    }
    std::fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn browser_spec_missing_path_suppresses_launch() {
    let path = unique_tmp("missing-browser").join("Missing.app");
    let quoted = serde_json::to_string(path.to_str().unwrap()).unwrap();
    let e = build_engine(&format!("module.exports = {{ default: {quoted} }};"));
    let result = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
    assert_eq!(
        LaunchPlan::from_spec(&result.browser, &result.url),
        LaunchPlan::Suppress
    );
}

#[test]
fn browser_spec_string_path_autodetects_via_nsbundle() {
    // Finicky-compat: a bare-string browser spec that looks like an
    // .app path (ends with .app + contains /) goes through NSBundle
    // directly, no `appType: "path"` required. Use Safari since it
    // ships with macOS in /Applications/Safari.app.
    let e = build_engine(r#"module.exports = { default: "/Applications/Safari.app" };"#);
    assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
}

#[test]
fn browser_spec_string_path_with_tilde_expands_home() {
    // Tilde expansion in the path. Hard to test against a real ~
    // path without polluting the home directory, so use the engine
    // fixture's HOME-override mutex to point HOME at /Applications,
    // then refer to ~/Safari.app — should resolve to the same bundle
    // ID as /Applications/Safari.app does in the test above.
    with_home(std::path::Path::new("/Applications"), || {
        for expression in [
            "'~/Safari.app'",
            "{ name: '~/Safari.app', appType: 'path' }",
            "() => '~/Safari.app'",
        ] {
            let e = build_engine(&format!("module.exports = {{ default: {expression} }};"));
            let result = e.resolve("https://x/", &Opener::default(), ModifierFlags::default());
            assert_eq!(result.browser.bundle_id, "com.apple.Safari");
            assert_eq!(
                result.browser.app_path.as_deref(),
                Some("/Applications/Safari.app")
            );
        }
    });
}

#[test]
fn parse_browser_jsval_apptype_path_resolves_to_bundle_id() {
    // appType: "path" — point at a real, always-installed system app
    // and assert we recover its bundle ID. Safari ships with macOS,
    // so /Applications/Safari.app exists in CI and on every dev box.
    let e = build_engine(
        r#"module.exports = {
                default: { name: "/Applications/Safari.app", appType: "path" },
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.apple.Safari");
}

#[test]
fn parse_browser_jsval_apptype_bundleid_skips_lookup() {
    // appType: "bundleId" trusts the value verbatim. Even an unknown ID
    // is preserved — the eventual open call is what would fail visibly.
    let e = build_engine(
        r#"module.exports = {
                default: { name: "com.totally.fake", appType: "bundleId" },
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.totally.fake");
}

#[test]
fn parse_browser_jsval_accepts_id_alias_for_bundleid() {
    let e = build_engine(
        r#"module.exports = {
                default: { id: "com.google.Chrome" },
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}

// ---------- console wiring ----------

#[test]
fn console_callbacks_are_callable_functions() {
    // typeof should be "function" for all five levels — proves the
    // manual block-encoding registration is reaching JSC's bridge.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () =>
                        typeof __grinchConsoleLog + "/" +
                        typeof __grinchConsoleWarn + "/" +
                        typeof __grinchConsoleError + "/" +
                        typeof __grinchConsoleInfo + "/" +
                        typeof __grinchConsoleDebug,
                }],
            };"#,
    );
    let (browser, _) = resolve(&e, "https://x/");
    assert_eq!(browser, "function/function/function/function/function");
}

#[test]
fn console_log_inside_fn_matcher_does_not_throw() {
    // Calling console.log from a user fn must not throw; the matcher
    // must still be able to return its value. We use the matcher's
    // return to signal success.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url) => {
                        console.log("matched", url.hostname);
                        return url.hostname === "example.com";
                    },
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://example.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://other.com/").0, "com.apple.Safari");
}

// ---------- finicky.* namespace ----------

// ---------- ctx.opener nullability ----------

#[test]
fn ctx_opener_is_null_when_opener_unknown() {
    // Default Opener (all-empty strings, pid 0) signals "no opener
    // detected" — ctx.opener should be JS null, matching Finicky v4.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.opener === null,
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    let unknown = Opener::default(); // all empty strings
    let (browser, _) = resolve_with(&e, "https://x/", &unknown, ModifierFlags::default());
    assert_eq!(browser, "com.google.Chrome");
}

#[test]
fn ctx_opener_is_object_when_opener_known() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: (url, ctx) => ctx.opener && ctx.opener.bundleId === "com.x",
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    let known = opener("com.x", "X");
    let (browser, _) = resolve_with(&e, "https://x/", &known, ModifierFlags::default());
    assert_eq!(browser, "com.google.Chrome");
}

#[test]
fn finicky_namespace_exposes_only_implemented_methods() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () =>
                        typeof finicky.matchHostnames + "/" +
                        typeof finicky.matchDomains + "/" +
                        typeof finicky.getModifierKeys + "/" +
                        typeof finicky.isAppRunning + "/" +
                        typeof finicky.getSystemInfo + "/" +
                        typeof finicky.getRunningBrowsers + "/" +
                        typeof finicky.notify + "/" +
                        typeof finicky.getBattery + "/" +
                        typeof finicky.getPowerInfo,
                }],
            };"#,
    );
    assert_eq!(
        resolve(&e, "https://x/").0,
        "function/function/function/function/function/function/undefined/undefined/undefined",
    );
}

#[test]
fn finicky_get_running_browsers_returns_array() {
    // The Rust bridge filters running apps against Grinch's
    // known-browser tables. The actual contents depend on what's
    // running in the test process — we can't pin the membership, but
    // we can verify the call returns a real array and the values are
    // known-browser bundle IDs (or empty if nothing happens to be
    // running). Shape-only assertion.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => {
                        var r = finicky.getRunningBrowsers();
                        if (!Array.isArray(r)) return "not-array:" + (typeof r);
                        // Every element must be a string, and either Safari
                        // or a bundle ID starting with a known prefix.
                        for (var i = 0; i < r.length; i++) {
                            if (typeof r[i] !== "string") return "bad-elem-type";
                            var ok = r[i] === "com.apple.Safari" ||
                                r[i].indexOf("com.google.Chrome") === 0 ||
                                r[i].indexOf("com.brave") === 0 ||
                                r[i].indexOf("com.microsoft.edgemac") === 0 ||
                                r[i].indexOf("com.vivaldi") === 0 ||
                                r[i].indexOf("org.chromium.Chromium") === 0 ||
                                r[i].indexOf("company.thebrowser") === 0 ||
                                r[i].indexOf("com.operasoftware") === 0 ||
                                r[i].indexOf("com.bookry.wavebox") === 0 ||
                                r[i].indexOf("net.imput.helium") === 0 ||
                                r[i].indexOf("ai.perplexity.comet") === 0 ||
                                r[i].indexOf("ru.yandex") === 0 ||
                                r[i].indexOf("org.mozilla") === 0 ||
                                r[i].indexOf("net.waterfox") === 0 ||
                                r[i].indexOf("io.gitlab.librewolf") === 0 ||
                                r[i].indexOf("app.zen-browser") === 0;
                            if (!ok) return "unexpected-bundle:" + r[i];
                        }
                        return "ok";
                    },
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "ok");
}

#[test]
fn finicky_get_running_browsers_supports_preference_fallback_idiom() {
    // The actual #145 use case: pick first-running from an ordered
    // preference list, fall back to Safari. Just verifies the idiom
    // type-checks and returns a bundle ID — not whether any specific
    // browser is running at test time.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => {
                        var running = finicky.getRunningBrowsers();
                        var prefs = ["com.google.Chrome",
                                     "org.mozilla.firefox",
                                     "com.apple.Safari"];
                        return prefs.find(b => running.includes(b)) ||
                               "com.apple.Safari";
                    },
                }],
            };"#,
    );
    // Whatever resolved, it must be one of the prefs.
    let (browser, _) = resolve(&e, "https://x/");
    assert!(
        browser == "com.google.Chrome"
            || browser == "org.mozilla.firefox"
            || browser == "com.apple.Safari",
        "expected one of the preference list, got {browser}"
    );
}

#[test]
fn finicky_match_hostnames_is_exact_not_subdomain() {
    // Critical semantic: matchHostnames is === on hostname, NOT
    // subdomain-matching. This is the inverse of Grinch's bare-string
    // matcher. Pin the behaviour.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: finicky.matchHostnames("github.com"),
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://api.github.com/").0, "com.apple.Safari");
}

#[test]
fn finicky_match_hostnames_accepts_array_and_regex() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: finicky.matchHostnames(["github.com", /^gitlab\./]),
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://github.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://gitlab.com/").0, "com.google.Chrome");
    assert_eq!(resolve(&e, "https://example.com/").0, "com.apple.Safari");
    // Subdomain still doesn't match the exact-hostname string.
    assert_eq!(resolve(&e, "https://api.github.com/").0, "com.apple.Safari");
}

#[test]
fn finicky_get_system_info_returns_shaped_object() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => {
                        var info = finicky.getSystemInfo();
                        return "k:" + Object.keys(info).sort().join(",");
                    },
                }],
            };"#,
    );
    // The Rust bridge fills both fields with gethostname() output;
    // we can't predict the value, just the shape.
    assert_eq!(resolve(&e, "https://x/").0, "k:localizedName,name");
}

#[test]
fn finicky_get_modifier_keys_returns_full_v4_shape() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => "k:" + Object.keys(finicky.getModifierKeys()).sort().join(","),
                }],
            };"#,
    );
    // capsLock, command, control, fn, function, option, shift — sorted.
    assert_eq!(
        resolve(&e, "https://x/").0,
        "k:capsLock,command,control,fn,function,option,shift",
    );
}

#[test]
fn finicky_is_app_running_returns_false_for_unknown_input() {
    // Pass an obviously-bogus identifier that matches no bundle ID
    // and no localized name. Verifies the bridge round-trips
    // (JS call → Rust workspace lookup → string return → JS bool
    // coerce) and that the localized-name comparison branch is
    // exercised — `is_app_running` walks every running app checking
    // BOTH `bundleIdentifier` and `localizedName` against the input
    // before returning false. (The "true" case is environment-
    // dependent — headless CI runners may not have Finder/Dock/etc.
    // running — so we don't pin a specific app here.)
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () =>
                        finicky.isAppRunning("definitely-not-installed-xyz123-fake") ? "yes" : "no",
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "no");
}

#[test]
fn finicky_is_app_running_returns_true_for_known_running_app() {
    // Round-trip the bridge against an app the workspace itself
    // confirms is running. If the workspace returns no apps at all
    // (sandboxed test env), skip — the previous test already
    // covered the false-path; this one's about the true path.
    let running = crate::workspace::running_app_bundle_ids();
    let Some(known) = running.iter().next().cloned() else {
        eprintln!("skipping: no running apps detected on this host");
        return;
    };
    // Pass the known bundle ID through the JS bridge and back.
    let src = format!(
        r#"module.exports = {{
                default: "com.apple.Safari",
                rules: [{{
                    match: () => true,
                    open: () => finicky.isAppRunning("{known}") ? "yes" : "no",
                }}],
            }};"#,
    );
    let e = build_engine(&src);
    assert_eq!(resolve(&e, "https://x/").0, "yes");
}

#[test]
fn finicky_is_app_running_returns_boolean() {
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => "t:" + typeof finicky.isAppRunning("com.apple.finder"),
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "t:boolean");
}

#[test]
fn fetch_window_title_bridge_is_a_function() {
    // Regression for the same _Block_signature issue that bit console:
    // without ManualBlockEncoding, JSC saw __grinchFetchWindowTitle as
    // an opaque NSBlock and the JS-side getter fell through to "".
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => true,
                    open: () => "t:" + typeof __grinchFetchWindowTitle,
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "t:function");
}

#[test]
fn console_log_handles_objects_and_primitives() {
    // The prelude's `__grinchFormatArgs` must not throw on mixed types
    // — number, string, object, null, undefined.
    let e = build_engine(
        r#"module.exports = {
                default: "com.apple.Safari",
                rules: [{
                    match: () => {
                        console.log("string", 42, { a: 1 }, null, undefined);
                        return true;
                    },
                    open: "com.google.Chrome",
                }],
            };"#,
    );
    assert_eq!(resolve(&e, "https://x/").0, "com.google.Chrome");
}
