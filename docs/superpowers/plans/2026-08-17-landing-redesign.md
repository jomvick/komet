# Landing redesign — comet ASCII + ambiance — implementation plan

Date: 2026-08-17
Design: docs/superpowers/specs/2026-08-17-landing-redesign-design.md (approved)

Implementation order (approved design decision): **comet ASCII generator + ambiance
layers first, then CSS coherence pass, then orphaned-asset deletion**. Each step
commits independently with a targeted `git add` (see "Git discipline" below).

## File structure map

- `apps/landing/public/index.html` — single-file landing page. `<style>` l.1–688;
  nav l.692–708; hero l.710–737 (grid l.711, `<pre class="hero-ascii">` l.731–776);
  shot l.780–786; panel-sect l.788–917; tweets l.919–1187; closing l.1190–1200;
  footer l.1202–1216; script l.1218–1565.
- `apps/landing/public/assets/` — served images. `comet-bg.png` will be added here.
- `dist/macos/background.png` — 1536×1024 comet wallpaper, **untracked**, the single
  source for both the ASCII map and the ambiance texture (design "Verified facts").
- `dist/komet_logo.svg`, `dist/macos/dmg-background*.png`, `dist/macos/icon-1024.png`
  — **untracked or tracked, but never to be staged by this plan**.

## Git discipline (critical)

The working tree carries many unrelated unstaged modifications (pre-existing
Zeron→Komet rebrand renames, committed in `5957556`; worktree edits on top).
Therefore:
- Every commit must `git add` **only** the specific files a step touches.
  Never `git add -A`, `git add .`, or `git commit -am`.
- Step 9's `git rm` stages exactly the two statue paths; verify with
  `git status --short` before committing that no `dist/*` path appears.
- `apps/ios/Komet/App/ZeronApp.swift` is intentionally out of scope (user decision).

## TDD / verification (no test harness exists)

This is a static, hand-written HTML file — no unit tests. Verification is:
- **Scripted checks**: `python3` (3.14.6 + Pillow 12.3.0 verified available) and
  `grep` assertions listed per step. ImageMagick (`convert`) also available at
  `/usr/bin` as a fallback.
- **Visual validation**: the executor cannot see the rendered page; the human
  partner confirms each visual result in the browser (or a `python3 -m http.server`
  preview) before the commit for that step is finalized.

## Steps

### Step 1 — copy comet asset into landing assets

`apps/landing/public/assets/comet-bg.png` ← copy of `dist/macos/background.png`.

- `cp dist/macos/background.png apps/landing/public/assets/comet-bg.png`
- Verify: `python3 -c "from PIL import Image; im=Image.open('apps/landing/public/assets/comet-bg.png'); print(im.size)"` → `(1536, 1024)`.
- Commit (targeted): `git add apps/landing/public/assets/comet-bg.png` + commit
  `feat(landing): add comet wallpaper asset`.
- Human check: file exists in assets; (optional) open the PNG to confirm it's the
  comet visual.

### Step 2 — comet ASCII: generator + splice into hero `<pre>`

Replace the statue art inside `<pre class="hero-ascii">` (l.731–776) with ASCII
generated from `comet-bg.png`.

- Write one-off script at `/tmp/opencode/comet-ascii.py` (keep out of the repo):
  - Load `apps/landing/public/assets/comet-bg.png`, convert to grayscale (Rec.709
    luma), **center-crop** to aspect ≈ 1.148 (= 110 cols × 0.48 cell aspect ÷ 46
    rows; mono cell ≈ 4.56×9.5px at 7.6px/1.25 line-height), downscale to
    **110 cols × 46 rows** (LANCZOS).
  - Map luma→glyph with the design ramp `+ = - . : @ * # %` (sparse→dense,
    brightest cells densest).
  - Splice the generated 46 lines into `index.html`, replacing the inner text of
    `<pre class="hero-ascii" aria-hidden="true">…</pre>` (regex anchored on the
    exact opening tag; assert exactly one match; preserve the opening-tag line so
    the first art line shares it, matching current format).
  - Write the art to `/tmp/opencode/hero-ascii.txt` too, for review.
- Verify (script): each of the 46 rows is 110 chars; alphabet ⊆
  `{+ = - . : @ * # %}`; a bright comet core present (some `#`/`%` density in the
  center rows), background sparse.
- Verify (file): `grep -c '^..*$' ... ` assert 46 pre rows; `git diff` shows only
  the `<pre>` inner text changed.
- Commit (targeted): `git add apps/landing/public/index.html` + commit
  `feat(landing): comet hero ascii`.
- Human check: hero reads as a comet; adjust crop offsets/luma curve in the script
  and re-splice (iterating is fine) until the partner approves.

### Step 3 — hero ambiance layer

Behind the hero grid (`header.hero`, l.710) add the comet wallpaper as a subtle,
radial-masked texture.

- In `<style>` (near `.hero`, l.141): add `.hero::before` (or a `.comet-glow`
  class on a new `<div>` child) — `position:absolute; inset:0; z-index:0;`
  `background: url('/assets/comet-bg.png') center 65% / 62% auto no-repeat;`
  `-webkit-mask-image`/`mask-image`: radial-gradient fade (mirror the hero-ascii
  fade, centered ~55% 45%); `opacity` low (≈0.14); `pointer-events:none;`
  `@media (prefers-reduced-motion: reduce){ .hero::before{ display:none; } }`.
- Ensure `.hero-grid` gains `position:relative; z-index:1;` so text sits above the
  glow.
- Verify: `grep -n "comet-bg" apps/landing/public/index.html`; `grep -n "hero::before"` present.
- Commit (targeted): `git add apps/landing/public/index.html` + commit
  `feat(landing): hero comet ambiance`.
- Human check: subtle purple comet glow behind hero copy, no text contrast loss.

### Step 4 — shot ambiance layer

Same texture behind the app screenshot (`.shot`, l.200).

- Extend `.shot::before` (currently a plain radial at l.201–207) or add a sibling:
  composite the comet texture at low opacity onto the existing radial so the glow
  matches the hero. Keep `pointer-events:none;` and the current inset so the card
  stays readable.
- Verify: `grep -n "shot::before"` still present with comet reference; no change
  to `.shot img` shadow.
- Commit (targeted): `git add apps/landing/public/index.html` + commit
  `feat(landing): shot comet ambiance`.
- Human check: screenshot glow and hero glow feel like the same light source.

### Step 5 — panel glow harmonization

Align the three panel-side glows so they share one purple-comet tint
(`#8b5cf6`-based), not three ad-hoc values.

- `.p-diagram::before` (l.290–298), `.spoke-wrap::before` (l.308–314),
  `.flow-wrap::before` (l.394–400): normalize `rgba(139,92,246,…)` usage and
  radius so the three read as one system. No structural change; pure value
  normalization + comment.
- Verify: `grep -n "rgba(139,92,246"` shows the three expected occurrences;
  `grep -n "rgba(91,52,184"` shows only the intentional dim variants.
- Commit (targeted): `git add apps/landing/public/index.html` + commit
  `style(landing): harmonize panel glows`.
- Human check: harness and devices diagrams glow uniformly.

### Step 6 — tweets polish

Small coherence touch on `.tweet` (l.517–596).

- Normalize card hover: keep `rgba(139,92,246,0.05)` hover bg (l.529) but ensure
  `border-color` and the `.tweet::before` accent bar (l.530–542) use the same
  `var(--purple)`; check `.tweet .hl` uses `var(--purple-hi)` consistently.
  Cosmetic only — do not change marquee timing or card dimensions.
- Verify: `grep -n "purple-hi"` and `grep -n "purple"` reads consistently for the
  tweet rules; no width/height edits.
- Commit (targeted): `git add apps/landing/public/index.html` + commit
  `style(landing): polish tweet cards`.
- Human check: hover accent bar and highlight color feel continuous with panels.

### Step 7 — closing + footer glow

Give the closing CTA (l.603–627) and footer (l.629–641) a faint comet glow so the
page ends on the same light as the hero.

- `.closing`: add a `::before` radial (purple `rgba(139,92,246,≈0.12)`, soft,
  large radius) behind `.closing .wrap` (already `position:relative; z-index:1`,
  l.607). Respect reduced-motion (static, so just keep `animation` none — glow is
  static by nature).
- `footer`: reuse the same pattern at very low opacity.
- Verify: `grep -n "closing::before"` and `grep -n "footer::before"` (or classed
  equivalents) present; `.wrap` z-index still above.
- Commit (targeted): `git add apps/landing/public/index.html` + commit
  `style(landing): closing and footer glow`.
- Human check: no glow under the footer hand-ascii, footer text still legible.

### Step 8 — CSS coherence pass (buttons, contrast, mobile, reduced-motion)

Final sweep per the design's section on coherence.

- Buttons (`.btn`, `.btn.small`): verify/add hover, active and `:focus-visible`
  purple shadow + border tint matching the comet ramp accent; confirm `nav`/`hero`
  buttons share one style. Locate the `.btn` block (in the nav CSS area, ~l.100–140)
  before editing.
- Contrast: spot-check that text-over-glow additions kept `var(--dim)`/`var(--faint)`
  legible; bump any glow opacity that competes with copy.
- Mobile: confirm new glow layers `display:none` (or acceptable) under the
  `max-width:900px` / `max-width:640px` media blocks, and nothing overflows
  (`overflow-x:hidden` on html, l.49, is the guard).
- Reduced motion: ensure every new animated rule has a
  `@media (prefers-reduced-motion: reduce)` companion (existing pattern l.449–451,
  597–600).
- Verify: `grep -n "focus-visible"` present for `.btn`; `grep -n "prefers-reduced-motion"` count ≥ existing; no new `overflow` regressions.
- Commit (targeted): `git add apps/landing/public/index.html` + commit
  `style(landing): coherence pass (buttons, contrast, motion)`.
- Human check: full page scroll-through — nav, hero, shot, panels, tweets, closing,
  footer read as one designed system.

### Step 9 — delete orphaned statue assets

`git rm apps/landing/public/assets/statue.jpg apps/landing/public/assets/statue-ascii.txt`

- Verify (before commit): `git status --short` shows exactly the two deletions
  staged, and **no** `dist/*` path anywhere in the output.
- Verify (after): `git ls-files apps/landing/public/assets/` no longer lists the
  two files; `git grep -l statue apps/landing/` (empty) confirms no dangling refs.
- Commit: `git add` is implicit via `git rm`; commit
  `chore(landing): remove orphaned statue assets`.

### Final verify

- `git status` clean for the touched files only (unrelated worktree edits remain).
- `git log --oneline -9` shows the nine plan commits in order.
- Open `apps/landing/public/index.html` in a browser (or `python3 -m http.server`
  from `apps/landing`) and scroll the full page; confirm the comet reads as the
  hero, the glows share one light, and no console errors.

## Verification commands (repo conventions — confirm names before running)

- Python/Pillow asset check: `python3 -c "from PIL import Image; print(Image.open('apps/landing/public/assets/comet-bg.png').size)"` → `(1536, 1024)`.
- ASCII generator: `python3 /tmp/opencode/comet-ascii.py`.
- Grep asserts per step (above). No package.json test script exists for the landing.
