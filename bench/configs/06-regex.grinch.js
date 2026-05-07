// Regex literal match. Compiled by the regex crate at config load.
//
// URL: https://github.com/jamtur01/grinch
// Iterations: 200000
module.exports = {
  default: "org.mozilla.firefox",
  rules: [
    { match: /github\.com\/.+/, open: "com.google.Chrome" },
  ],
};
