---
name: Grinch
description: Choose where links open on your Mac.
colors:
  grinch-green: "#46b25e"
  grinch-green-bright: "#5bc572"
  link-blue: "#79c0ff"
  canvas: "#0d1117"
  surface: "#161b22"
  hairline: "#30363d"
  ink: "#e6edf3"
  ink-muted: "#8b949e"
typography:
  display:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "3.5rem"
    fontWeight: 700
    letterSpacing: "-0.04em"
  headline:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.6rem"
    fontWeight: 700
    letterSpacing: "-0.02em"
  title:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.1rem"
    fontWeight: 600
  lead:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.2rem"
    fontWeight: 400
  body:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1rem"
    fontWeight: 400
    lineHeight: 1.6
  label:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "0.85rem"
    fontWeight: 600
    letterSpacing: "0.04em"
  mono:
    fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, Consolas, monospace"
    fontSize: "0.9em"
    lineHeight: 1.5
rounded:
  sm: "4px"
  md: "6px"
spacing:
  xs: "0.5rem"
  sm: "0.75rem"
  md: "1rem"
  lg: "1.5rem"
  xl: "3rem"
  section: "3.5rem"
components:
  button-primary:
    backgroundColor: "{colors.grinch-green}"
    textColor: "{colors.canvas}"
    rounded: "{rounded.md}"
    padding: "0.65rem 1.5rem"
  button-primary-hover:
    backgroundColor: "{colors.grinch-green-bright}"
  button-secondary:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "0.65rem 1.5rem"
  feature-card:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "0.85rem 1.1rem"
  code-inline:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.grinch-green}"
    typography: "{typography.mono}"
    rounded: "{rounded.sm}"
    padding: "1px 5px"
  code-block:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    typography: "{typography.mono}"
    rounded: "{rounded.md}"
    padding: "1rem 1.25rem"
  table-header:
    textColor: "{colors.ink-muted}"
    typography: "{typography.label}"
    padding: "0.5rem 0.75rem"
---

# Design System: Grinch

## Overview

**Creative North Star: "The Terminal Margin"**

The site reads like a well-kept README open in a dark editor. Commands, config
snippets, and benchmark tables are the content; prose sits between them as
margin notes. One column, one accent, system fonts, and thin hairlines do all
the structural work. Nothing on the page is decorative except the icon and the
faint green glow behind it in the hero.

Green is borrowed from the app icon and marks what a reader acts on or copies:
the download button, feature lead-ins, inline code. Everything else is a
cool, near-black canvas with light ink. Density is that of a reference
document: generous line height, a narrow measure, and wide gaps between
sections so a reader can scan headings and land on the snippet they need.

The page is currently dark-only. A light theme is undecided, not ruled out;
the neutral values are the current state, not a commitment to any external
palette.

**Key Characteristics:**
- Single 760px reading column, centred.
- One accent (Grinch Green) taken from the app icon.
- Flat surfaces separated by 1px hairlines; depth only on the primary action.
- System sans for prose, system mono for everything a user types or copies.
- Real snippets and measured numbers carry the page; no illustration.

## Colors

A cool near-black palette with one green accent and one blue for links.

### Primary
- **Grinch Green** (`grinch-green`): the top stop of the app icon's gradient.
  Used for the hero wordmark, the primary download button, feature-card lead-ins,
  and inline code text. It marks things to act on or copy.
- **Grinch Green, Bright** (`grinch-green-bright`): hover state of the primary
  button only.

### Secondary
- **Link Blue** (`link-blue`): inline links. Underline appears on hover only.

### Neutral
- **Canvas** (`canvas`): page background and the text colour on the green button.
- **Surface** (`surface`): one step up from canvas. Feature cards, code blocks,
  inline code chips, the secondary button, and table-row hover.
- **Hairline** (`hairline`): every border and divider: section rules, card
  outlines, code outlines, table rows, the note rule.
- **Ink** (`ink`): body text and headings.
- **Ink Muted** (`ink-muted`): tagline, h3 subheads, table headers, notes,
  footer, release meta.

### Named Rules
**The One Green Rule.** Grinch Green is the only saturated hue besides link
blue. It appears on actions, lead-ins, and copyable code, never as a fill for
large areas. The hero glow uses it at 8% opacity and is the one exception.

## Typography

**Display Font:** system sans (`-apple-system`, BlinkMacSystemFont, Segoe UI, system-ui)
**Body Font:** the same system sans
**Label/Mono Font:** system mono (`ui-monospace`, SF Mono, Menlo, Consolas)

**Character:** The native Mac UI face, so the site sounds like the platform the
app runs on. Mono carries every path, flag, command, and config key.

### Hierarchy
- **Display** (700, 3.5rem → 2.5rem under 600px, -0.04em): the hero wordmark
  only, set in Grinch Green.
- **Headline** (700, 1.6rem, -0.02em): section headings, with a hairline rule
  beneath.
- **Title** (600, 1.1rem, Ink Muted): subsection headings inside a section.
- **Lead** (400, 1.2rem → 1.05rem under 600px, Ink Muted): the hero tagline,
  capped at 560px.
- **Body** (400, 1rem, 1.6): prose. The 760px column keeps lines near 80
  characters.
- **Label** (600, 0.85rem, 0.04em, uppercase, Ink Muted): table column headers.
- **Mono** (0.9em of its context, 1.5 in blocks): inline code and code blocks;
  blocks drop to 0.8rem under 600px.

### Named Rules
**The Typed-Thing Rule.** Anything a reader types, copies, or sees in a
terminal or config file is set in mono. Product names and UI labels from
System Settings are bold sans, not mono.

## Layout

A single centred column (max 760px, 1.5rem side padding). The hero is
full-bleed with centred content and a bottom hairline; main content is
left-aligned. Sections are separated by 3.5rem; main has 3rem top and 4rem
bottom padding. Feature cards stack in a one-column grid with 0.75rem gaps.
Tables span the column; the last column is right-aligned with tabular figures
for numbers.

One breakpoint at 600px: hero padding, logo, wordmark, and tagline shrink;
code blocks and tables reduce type size and padding. Code blocks scroll
horizontally rather than wrap.

## Elevation & Depth

Flat by default. Depth comes from tonal steps (Canvas → Surface) and 1px
Hairline borders. Shadows exist only in green and only in two places: the
hero logo's drop shadow and the primary button. The hero also carries a faint
green radial wash at the top.

### Shadow Vocabulary
- **Logo glow** (`drop-shadow(0 8px 20px rgba(70, 178, 94, 0.25))`): hero icon only.
- **Primary rest** (`0 4px 16px rgba(70, 178, 94, 0.18)`): download button at rest.
- **Primary hover** (`0 6px 20px rgba(70, 178, 94, 0.28)`): download button on
  hover, paired with a 1px lift.

### Named Rules
**The One Lift Rule.** Only the primary action lifts or glows. Cards, code
blocks, and the secondary button stay flat in every state.

## Shapes

Soft rectangles. Inline code chips use a 4px radius; buttons, cards, and code
blocks use 6px. Borders are always 1px Hairline. Notes use a 2px left rule
instead of a box. No pills, circles, or clipped shapes; the app icon's large
rounded square is the only bold shape on the page.

## Components

### Buttons
Quiet and precise; only the download button asks for attention.
- **Shape:** gently rounded (6px).
- **Primary:** Grinch Green fill, Canvas text, weight 500, 0.65rem × 1.5rem
  padding, green rest shadow.
- **Hover:** Bright green fill and border, 1px upward lift, stronger green
  shadow. Transitions run 0.15s on border, background, transform, and shadow.
- **Secondary:** Surface fill, Hairline border, Ink text. Hover changes only the
  border to Grinch Green; no lift.
- Buttons sit in a centred, wrapping row with 0.75rem gaps.

### Cards / Containers
- **Corner Style:** 6px.
- **Background:** Surface on Canvas.
- **Shadow Strategy:** none (see The One Lift Rule).
- **Border:** 1px Hairline.
- **Internal Padding:** 0.85rem × 1.1rem.
- **Content:** a bold lead-in phrase in Grinch Green (weight 600) followed by
  Ink body text in the same paragraph.

### Code
- **Inline:** Surface chip, 1px Hairline, 4px radius, Grinch Green mono text.
- **Block:** Surface, 1px Hairline, 6px radius, 1rem × 1.25rem padding, Ink
  mono text, horizontal scroll on overflow.

### Tables
- Hairline under every row, no vertical rules or outer box.
- Headers use the Label style.
- Row hover tints to Surface.
- Final column right-aligned with tabular numerals.

### Notes
A muted aside: 0.9rem Ink Muted text with a 2px Hairline left rule and 1rem
left padding. Used for caveats and warnings that sit under a snippet.

## Do's and Don'ts

### Do:
- **Do** take the accent from the icon: Grinch Green for actions, lead-ins, and copyable code.
- **Do** separate surfaces with 1px Hairline borders and a single Canvas → Surface step.
- **Do** set every path, command, flag, and config key in mono.
- **Do** right-align numeric table columns with tabular figures.
- **Do** keep content in the 760px column and let code blocks scroll, not wrap.

### Don't:
- **Don't** add a second accent colour or fill large areas with green.
- **Don't** add shadows, lifts, or glows to anything but the primary action and the hero icon.
- **Don't** introduce a web font; the site uses the platform's system faces.
- **Don't** add illustration or decorative imagery beyond the app icon.
