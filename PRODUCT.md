# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

Design work covers only the static project site in `docs/` (GitHub Pages,
`https://jamtur01.github.io/grinch/`). The app itself is a native macOS menu-bar
app; its menu and the `hideIcon` actions window stay standard AppKit and are out
of scope for design work.

## Users

Technical Mac users who keep several browsers or browser profiles and are
comfortable writing a small JavaScript config. Their job: make links clicked in
Slack, Mail, and other apps open in the right browser, profile, or app without
thinking about it.

Secondary audience: Finicky users moving their existing v4 config to Grinch.

## Product Purpose

Grinch chooses which browser or app opens each link on a Mac. The user sets it
as the default browser and writes rules; every link from any app then follows
those rules. Success means a link opens in the intended place every time, and
when it doesn't, the user can see why.

The site's job is to get a visitor from "what is this" to an installed app with
a working first config, and to serve as the reference afterward.

## Positioning

- Native Rust binary evaluating a JavaScript config through JavaScriptCore;
  declarative rules compile to native matchers, so routing costs nanoseconds
  (measured, reproducible via `bench/run.sh`).
- Most Finicky v4 configs work unchanged.
- Sign-in popups from apps using `ASWebAuthenticationSession` route through the
  same rules as ordinary links.
- Routes to browser profiles, private and new windows in supported Chrome and
  Firefox browsers, and to non-browser apps (Zoom, Slack).

## Operating Context

- Install: universal DMG from GitHub Releases, dragged to `/Applications`;
  community Homebrew cask (`bric3/tap`, maintained outside the project); or
  `make run` from source.
- Config: `~/.config/grinch.js`, edited in the user's own editor, reloaded from
  the menu bar. Failed reloads keep the last good config.
- Setup ritual: System Settings → Desktop & Dock → Default web browser → Grinch.
- Debugging: `Grinch --test <url>`, `--list-rules`, and a JSON or text
  diagnostic log recording each resolve's matched rule, opener, and modifiers.
- `grinch:` URL scheme for Shortcuts, AppleScript, and shell scripts.
- Automatic updates via Sparkle in release builds.

## Capabilities and Constraints

- macOS 13 or later; universal binary (Apple Silicon and Intel).
- Config is JavaScript the app executes; users must only load configs they trust.
- Site is plain static HTML/CSS in `docs/` (seven pages sharing
  `docs/style.css`), no build step and no JavaScript. It is the user
  documentation; `README.md` is a short overview that links to it. Site,
  README, and examples stay aligned with released behavior and change
  together.
- Public docs describe current released behavior only. Release history lives in
  release notes.
- Only the latest published release is supported for issues and security fixes.

## Brand Commitments

- Name: Grinch.
- Icon: green rounded-square app icon with eyes (`brand/grinch-icon.svg`,
  `brand/grinch-icon-1024.png`); menu-bar template image of the eyes
  (`brand/grinchTemplate.svg`); social preview (`brand/social-preview.png`).
- Voice: plain and factual. Describe current behavior. No marketing
  intensifiers. Every performance claim links to the reproducible benchmarks.
- Always credit Finicky and Finch as inspirations, and state Finicky v4
  compatibility plainly ("most configs work"), with a link to the differences.
- Never promise backports or support for anything but the latest release.

## Evidence on Hand

- Benchmarks: `bench/README.md`, `bench/configs/`, `bench/run.sh`; README
  **Performance** tables (release builds, Apple Silicon, median of ten runs,
  routing only, excluding browser launch).
- Example config: `examples/grinch.example.js`.
- Brand assets in `brand/` and copies in `docs/`.
- No testimonials, user counts, press, or third-party reviews exist. Do not
  invent them.

## Product Principles

1. Get to a working first rule fast; the rest is reference.
2. Show the mechanism, not adjectives: real config snippets, real commands,
   measured numbers with their source.
3. When a link goes the wrong way, the user can find out why.
4. Respect what users bring: Finicky configs, existing browsers and profiles,
   their own editor.
5. Say what Grinch cannot do as clearly as what it can.
