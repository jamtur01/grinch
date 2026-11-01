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
  hairline-strong: "#6e7681"
  ink: "#e6edf3"
  ink-muted: "#8b949e"
typography:
  display:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "2.75rem"
    fontWeight: 700
    lineHeight: 1.12
    letterSpacing: "-0.035em"
  display-mobile:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "2.1rem"
    fontWeight: 700
    lineHeight: 1.12
    letterSpacing: "-0.035em"
  page-title:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "2.25rem"
    fontWeight: 700
    lineHeight: 1.15
    letterSpacing: "-0.03em"
  page-title-mobile:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.9rem"
    fontWeight: 700
    lineHeight: 1.15
    letterSpacing: "-0.03em"
  headline:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.6rem"
    fontWeight: 700
    lineHeight: 1.25
    letterSpacing: "-0.02em"
  headline-mobile:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.4rem"
    fontWeight: 700
    lineHeight: 1.25
    letterSpacing: "-0.02em"
  title:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.1rem"
    fontWeight: 600
    lineHeight: 1.35
  lead:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.15rem"
    fontWeight: 400
    lineHeight: 1.6
  lead-page:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.1rem"
    fontWeight: 400
    lineHeight: 1.6
  lead-mobile:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.05rem"
    fontWeight: 400
    lineHeight: 1.6
  brand:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1.05rem"
    fontWeight: 700
    letterSpacing: "-0.01em"
  body:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "1rem"
    fontWeight: 400
    lineHeight: 1.6
  body-table:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "0.95rem"
    fontWeight: 400
    lineHeight: 1.6
  nav:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "0.92rem"
    fontWeight: 400
    lineHeight: 1.6
  small:
    fontFamily: "-apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "0.9rem"
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
  mono-heading:
    fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, Consolas, monospace"
    fontSize: "0.92em"
  mono-hero:
    fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, Consolas, monospace"
    fontSize: "0.82rem"
    lineHeight: 1.5
  mono-small:
    fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, Consolas, monospace"
    fontSize: "0.8rem"
    lineHeight: 1.5
rounded:
  focus: "3px"
  sm: "4px"
  md: "6px"
spacing:
  2xs: "0.25rem"
  xs: "0.5rem"
  sm: "0.75rem"
  md: "1rem"
  ml: "1.25rem"
  lg: "1.5rem"
  gutter: "2.5rem"
  xl: "3rem"
  section: "3.5rem"
  page: "4rem"
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
  button-header:
    backgroundColor: "{colors.grinch-green}"
    textColor: "{colors.canvas}"
    typography: "{typography.small}"
    rounded: "{rounded.md}"
    padding: "0.35rem 0.9rem"
  nav-link:
    textColor: "{colors.ink-muted}"
    typography: "{typography.nav}"
    padding: "1rem 0 0.9rem"
  nav-link-current:
    textColor: "{colors.ink}"
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
  code-caption:
    backgroundColor: "{colors.canvas}"
    textColor: "{colors.ink-muted}"
    typography: "{typography.mono-small}"
    padding: "0.35rem 1.25rem"
  table-header:
    textColor: "{colors.ink-muted}"
    typography: "{typography.label}"
    padding: "0.5rem 0.75rem"
  note:
    textColor: "{colors.ink-muted}"
    typography: "{typography.small}"
    padding: "0.25rem 0 0.25rem 1rem"
  disclosure:
    rounded: "{rounded.md}"
    padding: "0 1.1rem"
---

# Design System: Grinch

## Overview

**Creative North Star: "The Terminal Margin"**

The site reads like a terminal session open beside a config file. Real
config blocks, captured `Grinch --test` transcripts, and benchmark tables are
the content; prose sits between them as margin notes. One accent, hairlines,
and a single tonal step do the structural work. The only image is the app
icon, at 28px in the top bar.

Green comes from the app icon and marks what a reader acts on or copies: the
download buttons, the `$` prompt, step numbers, the current-page underline,
inline code. Everything else is a cool near-black canvas with light ink and a
muted grey for secondary text. Density is that of reference documentation:
1.6 line height, wide gaps between sections, and a page-top index so a
reader lands on the one answer they came for.

The site is dark-only today (`color-scheme: dark`). A light theme is
undecided, not ruled out. The neutrals happen to sit close to GitHub's dark
palette; that is incidental, not a commitment to follow it.

**Key Characteristics:**
- A 760px reading column inside a 1120px frame for the header, hero, and footer.
- One accent (Grinch Green) taken from the app icon.
- Flat surfaces separated by 1px hairlines; only the hero download button lifts.
- Sans for prose, mono for everything a user types, copies, or sees in a terminal.
- Code is captioned with its file path or "Terminal" and shows real output.

## Colors

A cool near-black palette with one green accent and one blue for links.

### Primary
- **Grinch Green** (`grinch-green`): primary button fill, the transcript `$`
  prompt, step numbers, the current-page nav underline, the disclosure marker,
  and inline code text. It marks things to act on or copy.
- **Grinch Green, Bright** (`grinch-green-bright`): primary button hover and
  the focus-visible outline.

### Secondary
- **Link Blue** (`link-blue`): links. In page content and the footer, links
  carry a permanent underline at 40% opacity that goes solid on hover; in the
  header they are unadorned. Doc-index page names use it at weight 600.

### Neutral
- **Canvas** (`canvas`): page background, text on green, and the code-caption
  strip.
- **Surface** (`surface`): one step up. Code blocks, inline code chips, the
  secondary button, and table-row hover.
- **Hairline** (`hairline`): every divider and outline: header and footer
  rules, h2 rules, code and caption outlines, table rows, proof rows, the
  index rules, definition-list rules, disclosures, and the note rule.
- **Hairline, Strong** (`hairline-strong`): the secondary button's border, so
  it reads as a button against Surface.
- **Ink** (`ink`): body text, headings, the typed command in transcripts, and
  bold values in transcript output.
- **Ink Muted** (`ink-muted`): leads, meta lines, nav links at rest, the
  "On this page" label, table headers, notes, footer, transcript output, config
  comments, code captions, and stacked-table field labels.

Text selection uses Grinch Green at 35% under Ink.

### Named Rules
**The One Green Rule.** Grinch Green is the only saturated hue besides link
blue. It marks actions, prompts, numbering, location, and copyable code, and
never fills a large area. The hero's radial wash (7%) and the selection tint
(35%) are its only translucent uses.

## Typography

**Display Font:** system sans (`-apple-system`, BlinkMacSystemFont, Segoe UI, system-ui)
**Body Font:** the same system sans
**Label/Mono Font:** system mono (`ui-monospace`, SF Mono, Menlo, Consolas)

**Character:** The native Mac UI face for prose and headings, as shipped.
Mono carries every path, flag, command, config key, and transcript.

### Hierarchy
Base size is 16px. Sizes below are as used, with the under-600px step where
one exists.
- **Display** (700, 2.75rem → 2.1rem, 1.12, -0.035em, balanced): the home
  hero's promise sentence, in Ink. Used once.
- **Page Title** (700, 2.25rem → 1.9rem, 1.15, -0.03em): the h1 of each
  inner page.
- **Headline** (700, 1.6rem → 1.4rem, 1.25, -0.02em): section headings, with a
  hairline rule beneath.
- **Title** (600, 1.1rem, 1.35, Ink): subsection headings and proof-row
  headings.
- **Lead** (400, 1.15rem → 1.05rem, Ink Muted): the hero explanation, capped
  at 34rem. Inner pages use 1.1rem.
- **Brand** (700, 1.05rem, -0.01em): the "Grinch" wordmark beside the icon in
  the top bar.
- **Body** (400, 1rem, 1.6): prose. The 760px column gives lines of up to
  about 90 characters.
- **Table** (0.95rem → 0.85rem): table body text.
- **Nav** (0.92rem): top-bar page links and the "On this page" index.
- **Small** (0.9rem): meta line, notes, footer, and the header Download button.
- **Label** (600, 0.85rem, 0.04em, uppercase, Ink Muted): table column headers.
- **Mono** (0.9em of its context; 1.5 in blocks): inline code and code blocks.
  Blocks drop to 0.8rem under 600px. Code in headings is 0.92em and takes the
  heading colour with no chip.
- **Mono Hero** (0.82rem): the two hero code blocks.
- **Mono Small** (0.8rem): code captions and proof-row snippets.

### Named Rules
**The Typed-Thing Rule.** Anything a reader types, copies, or sees in a
terminal or config file is set in mono. Product names and System Settings
labels are bold sans, not mono.

**The Unbroken Token Rule.** Inline code does not wrap. Only tokens marked
long (very long paths and URLs) may break, and only under 600px; inside
stacked tables all inline code may break.

## Layout

Two widths. A 1120px frame holds the top bar, the home hero, and the footer;
a 760px column holds all reading content. Both use 1.5rem side padding,
1rem under 600px.

- **Top bar:** brand, page links, and Download on one 3.5rem row with 1.5rem
  gaps; Download is pushed right.
- **Home hero:** two columns (0.9fr text, 1.1fr code) with a 3.5rem gap and
  4rem vertical padding, vertically centred. The right column stacks a config
  file block above a transcript.
- **Inner pages:** page head (3rem top padding, 2rem under 600px) with title,
  lead, and the "On this page" index, then the body column.
- **Rhythm:** content starts 2.5rem below the hero or page head; sections are
  3.5rem apart; main ends with 4rem bottom padding; first-run steps are 3rem
  apart. Paragraphs carry 1rem bottom margin.
- **Proof rows (home):** each capability is a two-column row (0.8fr text,
  1.2fr snippet), 1.25rem vertical padding, hairline between rows.
- **Doc index (home):** each row is an 11rem page name beside its description.

**Breakpoints.**
- **900px:** the hero stacks to one column (copy first, then config and
  transcript; 2.5rem gap, 3rem padding). Page links wrap onto their own row
  under the brand and Download.
- **600px:** padding tightens to 1rem; headings, leads, code, and tables step
  down; proof rows and doc-index rows stack; hero code wraps with hanging
  indents; tables marked stackable become one block per row with each cell
  prefixed by its column name.

Code blocks scroll horizontally rather than wrap, except the hero blocks under
600px.

## Elevation & Depth

Flat by default. Depth comes from the Canvas → Surface step and 1px
hairlines. Three things carry shade: the hero download button, the hero's
faint green wash, and the scroll-edge shading on code blocks that can scroll
further.

### Shadow Vocabulary
- **Primary rest** (`0 4px 16px rgba(70, 178, 94, 0.18)`): the hero download button at rest.
- **Primary hover** (`0 6px 20px rgba(70, 178, 94, 0.28)`): the same button on
  hover or focus, with a 1px lift.
- **Hero wash** (radial green at 7%, anchored top right): behind the hero only.
- **Scroll edge** (Ink at 7%, 0.6rem wide): on the left or right edge of a code
  block only while more content lies that way.

### Named Rules
**The One Lift Rule.** Only the hero download button lifts or glows. The
header Download button is the same green with no shadow and no lift; cards,
code blocks, and the secondary button stay flat in every state. Scroll-edge
shading is a scroll cue, not elevation.

## Shapes

Soft rectangles. Inline code chips are 4px; buttons, code blocks, and
disclosures are 6px; the focus ring is 3px. A captioned code block is one 6px
rectangle split in two: the caption takes the top corners, the code the
bottom. Borders are always 1px. Notes use a 2px left rule instead of a box;
the current nav item and the h2s use a rule beneath. No pills or circles.

## Components

Quiet and precise; only the download buttons ask for attention.

### Buttons
- **Shape:** gently rounded (6px).
- **Primary:** Grinch Green fill and border, Canvas text, weight 500, green
  rest shadow.
- **Hover / Focus:** Bright green fill and border, 1px lift, stronger shadow.
  Transitions run 0.15s; reduced motion removes the transition and the lift.
- **Secondary:** Surface fill, Hairline Strong border, Ink text. Hover or
  focus changes the border to Grinch Green; no lift.
- **Header Download:** the primary colours at the small size, with no shadow
  and no lift.
- Hero buttons sit in a wrapping row with 0.75rem gaps.

### Navigation
- **Top bar:** icon plus wordmark in Ink, then page links in Ink Muted. Hover
  turns a link to Ink. The current page is Ink with a 2px Grinch Green
  underline at the bar's bottom edge. Under 900px links wrap onto a second row.
- **On this page:** a hairline-ruled band under the page lead: a muted
  "On this page" label followed by inline anchor links.
- **Next page:** a hairline above, the previous page on the left and the next
  on the right, both bold links.
- **Heading self-links:** every h2 and h3 links to itself; a muted `#` appears
  after it on hover or focus.

### Code
- **Inline:** Surface chip, 1px Hairline, 4px radius, Grinch Green mono. Inside
  links it takes the link colour.
- **Block:** Surface, 1px Hairline, 6px radius, 1rem × 1.25rem padding
  (0.75rem × 1rem under 600px), Ink mono, scroll-edge shading. Config comments
  are Ink Muted.
- **Captioned file:** a Canvas strip above the block holding the file path in
  Mono Small, Ink Muted.
- **Transcript:** captioned "Terminal". Commands are Ink with a Grinch Green
  `$ ` drawn by CSS, so the prompt is never copied; output is Ink Muted with
  the values that matter in Ink at 600. In the hero, each line is its own
  block with a hanging indent so wrapped lines align under their value column.

### Lists
- **Numbered steps:** each first-run step is an h2 prefixed by its number in
  Grinch Green mono, weight 500.
- **Option lists:** term above definition, terms bold, a hairline above the
  list and beneath each definition.
- **Doc index:** link-blue page name at 600 beside a muted description,
  hairline beneath each row; the name underlines on hover.

### Tables
- Hairline under every row, no vertical rules or outer box.
- Headers use the Label style; row headers stay regular weight.
- Row hover tints to Surface.
- Numeric columns are right-aligned with tabular figures.
- Stackable tables, under 600px, drop the header row and show each cell on its
  own line prefixed by its column name in Ink Muted.

### Disclosures
1px Hairline, 6px radius, 1.1rem side padding. The summary is bold with a
Grinch Green marker.

### Notes
Small Ink Muted text with a 2px Hairline left rule and 1rem left padding;
bold text inside a note is Ink. Used for caveats under a snippet.

### Focus
Every focusable element gets a 2px Grinch Green, Bright outline at 2px offset
with a 3px radius. A skip link appears top left on focus.

## Do's and Don'ts

### Do:
- **Do** take the accent from the icon: Grinch Green for actions, prompts, numbering, location, and copyable code.
- **Do** separate surfaces with 1px Hairline borders and a single Canvas → Surface step.
- **Do** caption config blocks with their file path and transcripts with "Terminal", and show real output.
- **Do** set every path, command, flag, and config key in mono, and keep inline code on one line unless it is marked long.
- **Do** right-align numeric table columns with tabular figures, and label stacked cells with their column name.
- **Do** keep reading content in the 760px column; use the 1120px frame only for the top bar, hero, and footer.

### Don't:
- **Don't** add a second accent colour or fill large areas with green.
- **Don't** add shadows, lifts, or glows to anything but the hero download button.
- **Don't** put a `$` into transcript text; the prompt is drawn by CSS.
- **Don't** add illustration or decorative imagery beyond the app icon.
