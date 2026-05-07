// Default fallback, no query. The URL doesn't match any rule, so resolve()
// walks the rule list and falls through to the default browser. Exercises
// the matcher dispatch loop without any rewrite work.
//
// URL: https://example.com/no/match
// Iterations: 200000
module.exports = {
  default: "com.google.Chrome",
  rules: [
    { match: "github.com", open: "com.google.Chrome" },
    { match: "zoom.us/j/*", open: "us.zoom.xos" },
    { match: domain("paymentology.atlassian.net"), open: "org.mozilla.firefox" },
    { match: /\.figma\.com/, open: "com.google.Chrome" },
  ],
};
