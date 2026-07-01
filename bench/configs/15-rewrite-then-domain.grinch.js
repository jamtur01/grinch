// Adversarial host-recompute case: a global strip rewrite fires on every
// resolve (Cow::Owned transition), and the rules use domain() matchers, so
// `needs_host` is true and the resolve loop recomputes quick_host() after
// the rewrite mutates the URL. Isolates the post-rewrite host-recompute
// cost the perf review flagged (engine resolve loop). The matching domain
// rule is last so the full rule scan runs against the recomputed host.
//
// URL: https://shop.example/?utm_source=a&utm_medium=b&q=ok
// Iterations: 200000
module.exports = {
  default: "org.mozilla.firefox",
  rewrite: [strip("utm_*", "fbclid", "gclid")],
  rules: [
    { match: domain("a.example"), open: "com.google.Chrome" },
    { match: domain("b.example"), open: "com.google.Chrome" },
    { match: domain("c.example"), open: "com.google.Chrome" },
    { match: domain("shop.example"), open: "com.apple.Safari" },
  ],
};
