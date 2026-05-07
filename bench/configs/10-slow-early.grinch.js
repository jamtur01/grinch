// Slow path config, but the URL hits an early native rule and short-
// circuits before any fn matcher fires. Measures whether the engine
// correctly skips the JS bridge cost when a cheaper rule wins early.
//
// URL: https://github.com/jamtur01/grinch
// Iterations: 200000
module.exports = {
  default: "com.google.Chrome",
  rules: [
    { match: "github.com", open: "com.google.Chrome" },
    { match: (url, ctx) => ctx.opener.bundleId === "a.example", open: "com.google.Chrome" },
    { match: (url, ctx) => ctx.opener.bundleId === "b.example", open: "com.google.Chrome" },
    { match: (url, ctx) => ctx.opener.bundleId === "c.example", open: "com.google.Chrome" },
  ],
};
