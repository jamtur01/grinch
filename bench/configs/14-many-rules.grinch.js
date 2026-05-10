// Linear-scan stress: 50 bare-hostname rules where the matching one is at
// the very end. Exercises the rules loop on every resolve to measure the
// per-rule overhead (string compare against the URL's host).
//
// Use to evaluate whether a hostname → rule index would be worth the
// added complexity. If this workload's per-op latency is close to the
// 4-bare workload (1 rule), the linear scan is fine.
//
// URL: https://r50.example/path
// Iterations: 200000
module.exports = (() => {
  var rules = [];
  for (var i = 0; i < 49; i++) {
    rules.push({ match: "r" + i + ".example", open: "com.google.Chrome" });
  }
  rules.push({ match: "r50.example", open: "com.apple.Safari" });
  return { default: "org.mozilla.firefox", rules: rules };
})();
