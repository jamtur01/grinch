// Wildcard match — pattern is compiled to a regex at config load with the
// Finicky-style implicit protocol prefix.
//
// URL: https://zoom.us/j/1234567890
// Iterations: 200000
module.exports = {
  default: "com.google.Chrome",
  rules: [
    { match: "zoom.us/j/*", open: "us.zoom.xos" },
  ],
};
