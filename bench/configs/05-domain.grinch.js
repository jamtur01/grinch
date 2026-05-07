// domain() helper match. Same matcher kind as bare-hostname but goes through
// the explicit Matcher::Domain variant; URL hits a subdomain.
//
// URL: https://api.github.com/users/jamtur01
// Iterations: 200000
module.exports = {
  default: "org.mozilla.firefox",
  rules: [
    { match: domain("github.com"), open: "com.google.Chrome" },
  ],
};
