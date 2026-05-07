// Bare-hostname match — the URL hits "github.com" exactly. Exercises the
// allocation-free byte-level host_matches path.
//
// URL: https://github.com/jamtur01/grinch
// Iterations: 200000
module.exports = {
  default: "org.mozilla.firefox",
  rules: [
    { match: "github.com", open: "com.google.Chrome" },
  ],
};
