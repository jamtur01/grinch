// A URL-only default callback must not fetch opener or modifier context.
// URL: https://example.com/no/match
// Iterations: 100000
module.exports = {
  default: (url) => url.hostname === "example.com" ? "com.apple.Safari" : "com.google.Chrome",
};
