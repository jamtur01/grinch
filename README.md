<p align="center">
  <img src="brand/grinch-icon.svg" width="160" height="160" alt="Grinch logo">
</p>

# Grinch

Grinch chooses which browser or app opens each link on your Mac. Set it as
your default browser, then give it a few rules.

For example, you can open GitHub links in Chrome, send everything else to
Safari, and use a separate browser profile for work. Links from Slack, Mail,
and other apps follow the same rules automatically.

Grinch is inspired by [Finicky](https://github.com/johnste/finicky) and
[Finch](https://github.com/expelledboy/finch), two other macOS apps for routing
links. If you already use Finicky, most v4 configs work with Grinch; see
[Moving from Finicky](https://jamtur01.github.io/grinch/finicky.html).

**Documentation:** <https://jamtur01.github.io/grinch/>

## Install

Requires macOS 13 or later. Releases are universal binaries (Apple silicon and
Intel), signed with a Developer ID and notarized.

Download the latest `Grinch-vX.Y.Z.dmg` from
[Releases](https://github.com/jamtur01/grinch/releases/latest), open it, and
drag `Grinch.app` onto the `Applications` shortcut in the DMG window. Install
to `/Applications` directly: running `Grinch.app` from `~/Downloads` or
anywhere else triggers Gatekeeper translocation, which makes the same app
appear several times in the default-browser picker.

[Get started](https://jamtur01.github.io/grinch/start.html) also covers
installing from the terminal, with the community
[Homebrew cask](https://github.com/bric3/homebrew-tap/blob/main/Casks/grinch.rb),
and how to uninstall.

### From source

Requires Rust stable via rustup. Put `~/.cargo/bin` first on `PATH`; see
[the contributor prerequisites](CONTRIBUTING.md#prerequisites) to verify the tools.

```sh
git clone https://github.com/jamtur01/grinch
cd grinch
make run
```

The default source build doesn't include automatic updates. Pass `UNIVERSAL=1`
to `make build` to produce a fat binary locally (needs both rustup targets:
`rustup target add aarch64-apple-darwin x86_64-apple-darwin`).

## Your first config

1. Create `~/.config/grinch.js`, creating `~/.config` first if needed:

   ```js
   module.exports = {
     default: "com.apple.Safari",
     rules: [
       { match: "github.com", open: "com.google.Chrome" },
     ],
   };
   ```

   This sends `github.com` and its subdomains to Chrome, and everything else
   to Safari. Only use configs you trust: Grinch evaluates them as JavaScript.
2. Launch Grinch from Applications. Its eyes appear in the menu bar.
3. In **System Settings → Desktop & Dock → Default web browser**, select
   **Grinch**.
4. Check a decision without opening a browser:

   ```console
   $ /Applications/Grinch.app/Contents/MacOS/Grinch --test "https://github.com/"
   URL:     https://github.com/
   Final:   https://github.com/
   Browser: com.google.Chrome
   ```

After changing the config, choose **Reload Config** from the Grinch menu. If a
reload fails, Grinch keeps the last config that loaded.

## Documentation

- [Get started](https://jamtur01.github.io/grinch/start.html): install, first
  rule, default browser, updates, and uninstalling.
- [Configuration](https://jamtur01.github.io/grinch/config.html): rules, match
  types, browser targets and profiles, URL rewrites, the `ctx` object, and
  options.
- [Troubleshooting](https://jamtur01.github.io/grinch/troubleshooting.html):
  `--test`, `--list-rules`, the menu bar, the diagnostic log, and every
  command-line flag.
- [Integrations](https://jamtur01.github.io/grinch/integrations.html): sign-in
  popups, the `grinch:` URL scheme, browser add-ons, and URL shorteners.
- [Moving from Finicky](https://jamtur01.github.io/grinch/finicky.html): what
  carries over and what to change.
- [Performance](https://jamtur01.github.io/grinch/performance.html): routing
  benchmarks and memory measurements, reproducible with `bench/run.sh`.

[`examples/grinch.example.js`](examples/grinch.example.js) is a feature
reference config.

## Support and security

Only the [latest published release](https://github.com/jamtur01/grinch/releases/latest)
is supported for issues and security fixes. Upgrade and reproduce the problem
before filing a bug; older releases do not receive backported fixes.

For bug reports, feature requests, and development, see
[CONTRIBUTING.md](CONTRIBUTING.md). Report vulnerabilities privately using
[SECURITY.md](SECURITY.md), not a public issue.

## License

MIT — see [LICENSE](LICENSE).
