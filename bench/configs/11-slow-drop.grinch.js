// Slow path: URL is dropped via a fn rewrite returning null. Exercises
// the suppress-via-rewriter path including the helper-call to normalise
// the JS return value.
//
// URL: https://tracking.example.com/pixel
// Iterations: 100000
module.exports = {
  default: "com.google.Chrome",
  rewrite: [
    { match: (url) => url.hostname === "tracking.example.com", url: () => null },
  ],
  rules: [],
};
