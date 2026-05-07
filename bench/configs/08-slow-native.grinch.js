// Slow path: 4 fn matchers reading ctx.opener. None match — the resolve
// crosses the JS bridge 4 times (once per matcher) plus pays for the
// initial URL polyfill + ctx build.
//
// URL: https://github.com/jamtur01/grinch
// Iterations: 100000
module.exports = {
  default: "com.google.Chrome",
  rules: [
    { match: (url, ctx) => ctx.opener.bundleId === "a.example", open: "com.google.Chrome" },
    { match: (url, ctx) => ctx.opener.bundleId === "b.example", open: "com.google.Chrome" },
    { match: (url, ctx) => ctx.opener.bundleId === "c.example", open: "com.google.Chrome" },
    { match: (url, ctx) => ctx.opener.bundleId === "d.example", open: "com.google.Chrome" },
  ],
};
