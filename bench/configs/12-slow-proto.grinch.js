// Slow path: HTTP→HTTPS upgrade via URL mutation. The fn rewriter mutates
// the URL instance and returns it; the helper normalises back to a string.
// Exercises the rewrite Cow-owned transition + URL-instance round-trip.
//
// URL: http://example.com/path
// Iterations: 100000
module.exports = {
  default: "com.google.Chrome",
  rewrite: [
    {
      match: (url) => url.protocol === "http:",
      url: (url) => { url.protocol = "https:"; return url; },
    },
  ],
  rules: [],
};
