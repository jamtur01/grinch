// Slow path: dynamic `open` fn that picks a browser based on ?browser=.
// One fn matcher fires (search-params has-check), then the open fn runs
// to choose the target.
//
// URL: https://example.com/?browser=work&q=1
// Iterations: 100000
module.exports = {
  default: "com.google.Chrome",
  rules: [
    {
      match: (url) => url.searchParams.has("browser"),
      open: (url) => {
        switch (url.searchParams.get("browser")) {
          case "work": return "org.mozilla.firefox";
          case "private": return { name: "Google Chrome", args: ["--incognito"] };
          default: return "com.google.Chrome";
        }
      },
    },
  ],
};
