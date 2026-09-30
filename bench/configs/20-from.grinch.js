// Declarative opener matching needs only the sender's bundle identifier.
// URL: https://example.com/no/match
// Iterations: 1000000
module.exports = {
  default: "com.apple.Safari",
  rules: [{ match: from("com.apple.finder"), open: "com.google.Chrome" }],
};
