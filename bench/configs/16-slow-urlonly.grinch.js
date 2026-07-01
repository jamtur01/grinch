// Slow path, arity-gated: same 4-consecutive-fn-matcher shape as
// 08-slow-native, but the matchers declare a single `(url)` parameter
// instead of `(url, ctx)`. Grinch's ctx-passing contract skips the
// `__grinchMakeCtx` build (and its ~2-3µs of JSC bridge crossings)
// whenever a fn declares fewer than two formal params.
//
// Compare against 08-slow-native head-to-head: the delta is the pure
// cost of building ctx. It quantifies the biggest lever a config author
// has on slow-path latency — read the URL (first arg) rather than ctx
// when the opener/modifiers aren't needed.
//
// URL: https://github.com/jamtur01/grinch
// Iterations: 100000
module.exports = {
  default: "com.google.Chrome",
  rules: [
    { match: (url) => url.hostname === "a.example", open: "com.google.Chrome" },
    { match: (url) => url.hostname === "b.example", open: "com.google.Chrome" },
    { match: (url) => url.hostname === "c.example", open: "com.google.Chrome" },
    { match: (url) => url.hostname === "d.example", open: "com.google.Chrome" },
  ],
};
