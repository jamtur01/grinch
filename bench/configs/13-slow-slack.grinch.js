// Slow path: heaviest fn rewrite — Slack web URL → slack:// scheme via
// regex on pathname + searchParams construction + cross-scheme rebuild.
// Representative of the most expensive fn-driven workloads.
//
// URL: https://acme.slack.com/archives/C012345/p1700000000123456
// Iterations: 100000
module.exports = {
  default: "com.google.Chrome",
  rewrite: [
    {
      match: "*.slack.com/archives/*",
      url: (url) => {
        const m = /\/archives\/(?<channel>\w+)(?:\/(?<msg>p\d+))?/.exec(url.pathname);
        if (!m) return url;
        let search = "team=" + (url.hostname.split(".")[0]);
        search += "&id=" + m.groups.channel;
        if (m.groups.msg) {
          const t = m.groups.msg;
          search += "&message=" + t.slice(1, 11) + "." + t.slice(11);
        }
        return { protocol: "slack", host: "channel", pathname: "", search: search };
      },
    },
  ],
  rules: [],
};
