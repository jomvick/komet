# Komet landing page redesign — design

Date: 2026-08-17
Status: Approved (brainstorming) — implementation order: comet ASCII generator +
ambiance layer first, then CSS coherence pass, then asset deletion.

## Summary

Refresh the Komet landing page (`apps/landing/public/index.html`) from its
Zeron-era visuals to a Komet look, **keeping the existing structure and every
section**. Per the agreed scope ("refresh visuel, même structure") the sections
(nav, hero, shot, panel/harness/gallery/devices, tweets marquee, closing,
footer) are untouched in layout — only the remaining Zeron artifacts are
replaced and a CSS polish/coherence pass is applied.

## Verified facts

- The hero ASCII art baked into `<pre class="hero-ascii">` (index.html l. 731-776)
  is the *statue*: `assets/statue-ascii.txt`'s first 12 lines match the pre
  content exactly, and `assets/statue.jpg` (484,854 B) is its source image.
  Both files are **orphaned** (not referenced by index.html — the art is
  inline).
- `assets/app-screenshot.jpg` (861,863 B) is the old Zeron UI screenshot,
  referenced by index.html (og:image l. 10, `<img>` l. 784) and README.md l. 5.
  Kept as-is for now; user re-shoots later (paths stay stable).
- `assets/shots/` is empty (the 3 gallery PNGs were removed in a prior session;
  user re-shoots `sessions.png`/`diff.png`/`history.png`). HTML refs at
  l. 856-864 kept.
- `assets/tweets/` holds 15 restored `.jpg` (both marquee rows). Keep.
- `assets/icons/` holds the 6 agent SVGs (claude, cursor, grok, hermes,
  openai, pi). Keep.
- `assets/geist-latin.woff2`, `geist-mono-latin.woff2`, `komet-app-icon.png`,
  `komet-favicon-v3.png`. Keep.
- `dist/macos/background.png` (1536×1024, untracked) is the DMG background /
  comet wallpaper. Single source for the new ASCII art **and** the decorative
  comet layer.
- `apps/landing/` contains only `public/` + `wrangler.jsonc` (no package.json,
  no server scripts) — a static deploy; all work is HTML/CSS/JS inline.
- Hero CSS: `.hero-grid` is `minmax(0,1.05fr) minmax(0,0.95fr)`; h1 uses
  `clamp(38px,4.8vw,58px)`; `.hero-ascii` ~7.6px mono, `display:none` on small
  screens (l. 196).
- The dither edge-railings, reach figures and hand ASCII all already render in
  Komet purple `rgba(139,92,246)` — no rebrand needed there.
- `git rm` on statue assets must avoid staging untracked `dist/*` (incl.
  `dist/komet_logo.svg` and `dist/macos/background.png`); the next commit must
  purge residual `zeron` from `.git/index`.

## Design

### 1. Hero — new comet ASCII + ambiance

- **Replace the baked statue ASCII** with a comet ASCII generated from
  `dist/macos/background.png`. Pipeline (one-off local script, output pasted
  into the `<pre>`):
  1. Load 1536×1024; center-crop to a portrait-ish slice ~matching the existing
     pre footprint (~110 cols × ~46 rows at 7.6px mono, l. 731-776).
  2. Downscale to cell grid; map luma→glyph ramp matching the current baked
     art's ramp (`+ = - . : @ * # %`, from `statue-ascii.txt` — denser glyphs
     for brighter cells), keeping the comet core bright and background sparse.
  3. Paste into `<pre class="hero-ascii">`; keep the existing mask/gradient
     CSS. The comet's look is validated by the user (model can't see images) —
     we iterate on crop/aspect until it reads as a comet.
- **Comet ambiance layer**: add `background.png` as a decorative layer behind
  the hero grid: absolutely positioned, `pointer-events:none`, low opacity,
  masked with a `radial-gradient` to fade into the page bg, `background-size:
  cover`. Static under `prefers-reduced-motion`.
- **Keep**: h1 clamp, copy, CTA buttons (`btn`/`ghost`), version badge,
  6 agent icons, grid proportions.

### 2. Shot section — ambiance only

- Keep the two dither edge-railings, keep `<img src="/assets/app-screenshot.jpg">`
  (path unchanged, re-shoot later).
- Add the same radial-masked comet glow behind the screenshot card so the
  section matches the hero's luminous treatment.

### 3. Panel section — harmonize

- Keep the hub-and-spoke harness diagram, gallery (3 figures → `shots/*.png`,
  paths kept), devices flow, reach-ascii canvases.
- Unify `--purple` glows/accents on the three panel blocks (`.p-harness`,
  `.p-gallery`, `.p-devices`) so they read as one family with the hero/shot.

### 4. Tweets marquee

- Keep both rows + 15 restored jpgs. Light hover/glow polish. Freeze already
  present under `prefers-reduced-motion`.

### 5. Closing + Footer

- Hand-ASCII canvases already purple — keep. Unify glow. Keep copy and links.

### 6. Assets

- **Add**: `dist/macos/background.png` → `apps/landing/public/assets/comet-bg.png`
  (single source for ASCII + ambiance layers).
- **Delete**: `git rm apps/landing/public/assets/statue.jpg
  apps/landing/public/assets/statue-ascii.txt` (orphaned Zeron-era).
- **Keep**: `app-screenshot.jpg` + empty `shots/` (user re-shoots), `tweets/`,
  `icons/`, fonts, app icon, favicon. og:image + README keep pointing at
  `app-screenshot.jpg` until re-shoot.

### 7. CSS coherence pass

- Buttons: consistent hover/active/focus-visible, purple shadow.
- Contrast: faint text (`--faint`) still legible over the new comet layers.
- Mobile: hero ASCII already hidden on small screens (l. 196) — keep; comet
  layer must not hurt tap targets.
- `prefers-reduced-motion`: static comet, frozen marquee/dithers/hands.

## Explicitly out of scope

- No section/layout changes (structure stays identical).
- No copy changes beyond what the CSS pass requires.
- No re-shooting of screenshots (user-owned, later).
- No opencode-agent work (separate spec).