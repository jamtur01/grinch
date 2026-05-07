// Default fallback with a global strip rewrite that actually removes a
// param. Exercises strip_params() rebuild + a Cow::Owned transition.
//
// URL: https://example.com/?utm_source=a&utm_medium=b&q=ok
// Iterations: 200000
module.exports = {
  default: "com.google.Chrome",
  rewrite: [
    strip("utm_source", "utm_medium", "utm_campaign", "fbclid", "gclid"),
    strip("utm_*"),
  ],
  rules: [],
};
