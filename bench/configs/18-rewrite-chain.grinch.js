// Mutable URL callbacks followed by a matcher; all callbacks need only URL.
// URL: https://example.com/path?utm_source=mail#section
// Iterations: 100000
module.exports = {
  default: "com.google.Chrome",
  rewrite: [
    { match: "example.com", url: (url) => { url.searchParams.delete("utm_source"); return url; } },
    { match: "example.com", url: (url) => { url.hash = ""; return url; } },
  ],
  rules: [{ match: (url) => url.hostname === "example.com", open: "com.apple.Safari" }],
};
