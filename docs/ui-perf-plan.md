# UI perf & polish plan — Komet-side first, fork patches last

Companion to `docs/memory-plan.md` (RSS). This one covers frame time, scroll feel, GPU
memory and visual polish. Rule of thumb (see `AGENTS.md` §"When to patch the gpui fork"):
everything below lives in `crates/ui` unless marked **[fork]**, and every **[fork]** item
needs a measurement before it is written.

## 0. Measure first (gate for everything else)

- Add `KOMET_UI_STATS=1`: per-frame log of `render()` wall time per big view
  (`shell`, `transcript`, `composer`, `history`, `terminal/panel`), element count, and the
  renderer line from `KOMET_GPU_STATS=1` (device memory, blur scratch, atlas tiles).
- Scenarios to record (dev-demo mock engine, `scripts/dev-demo.sh --slow`):
  1. idle on a 50-entry chat, 2. streaming reply (120ms ticks), 3. scroll a 500-entry chat
  top→bottom, 4. open/close 20 chats with screenshots, 5. hover the glass sidebar.
- Budgets: p95 frame < 8ms idle / < 12ms streaming at 60Hz; device memory flat after
  scenario 4; no `render()` > 4ms on the transcript.

## 1. Zero-risk wins (Komet)

- **Transcript sync clone** (`transcript.rs`): the full transcript is cloned per frame.
  Move to `Arc` per entry + a generation counter so unchanged entries share allocation.
- **Byte-budget render caches** (`tree_cache` / `RenderCache` / `HighlightStore`,
  `transcript.rs:1031,1347,1524`): today they grow with every row ever scrolled past and are
  freed only on chat switch. Bound to viewport ± K rows with an LRU.
- **Streaming re-render scope**: on a text-append op only re-parse the tail block of the
  last entry (the delta protocol already gives the op); skip `BlockTree` rebuild when the
  appended text does not close a block.
- **`shell.rs` / `composer.rs` render fan-out** (8k lines each): split into child views so a
  composer keystroke does not re-render the sidebar and history. Check with `KOMET_UI_STATS`
  which `render()` fires on each keystroke.
- **Popover/overlays** (`popover.rs`, 17 blur/fade call sites): render blur only while the
  overlay is visible and its bounds changed; cache the last `Frosted` layer otherwise.

## 2. Scroll & streaming feel (Komet)

- Virtualise the transcript list with `uniform_list`/custom measurement where rows are
  heterogeneous: keep a row-height cache keyed by (entry id, width) so re-layout on resize
  is O(visible).
- Pin-to-bottom while streaming without layout thrash: apply the scroll offset in the same
  frame as the append, not one frame later (visible as a 1-frame jitter today when paced).
- Image rows: reserve intrinsic size from metadata before decode so rows never jump.
- Terminal panel (`terminal/panel.rs`): cap scrollback (memory-plan §6) and render only the
  visible grid rows; dirty-row tracking from alacritty instead of full-grid redraw.

## 3. GPU memory (Komet, then one small fork item)

- Call `ImageSource::evict` (fork `b687328`) from every eviction site, including staged
  attachments purge and file preview close (`files/preview.rs`).
- **[fork]** Expose a drop path for `ImageSource::Image` atlas tiles — the one follow-up
  memory-plan §7 names as impossible Komet-side (tiles free only on window close). Minimal:
  one method on the asset system that releases the atlas tile for a given image id. Record
  device memory before/after on scenario 4.

## 4. Glass / blur cost (measure before anything)

- With `KOMET_GPU_STATS=1`, compare scenario 5 with blur on vs `frosted(…, 0.0)`. If the
  blur dominates frame time:
  - Komet-side first: fewer `Frosted` layers (nest one `layered()` instead of per-item
    frosted children), lower `blur_radius` on small surfaces, skip blur while scrolling fast.
  - **[fork]** only if still over budget: downsampled blur (½ or ¼ res then upscale) and
    region dirty-tracking so an unchanged backdrop is not re-blurred each frame.

## 5. Polish (Komet)

- `prefers-reduced-motion` switch (PARITY 1.12 gap): one flag in `motion.rs` read from the
  platform/settings, every curve collapses to instant when set.
- Consistent edge fades: audit `edge_faded` call sites so the band width is the same on all
  scroll surfaces (rail, history, transcript, pickers).
- Text: verify the closing-punctuation wrap fix (fork `a6bf17c`) still holds after the next
  upstream bump — add a `komet-ui` unit test that shapes a known string and asserts break
  positions, so a bad rebase is caught by `cargo test -p komet-ui`.

## 6. Fork hygiene

- Before the next gpui bump, check upstream Zed for each fork commit listed in the root
  `Cargo.toml`; drop any that landed upstream (candidates: Porter-Duff alpha `f596cde`,
  macOS 26 blur `8a8954c`).
- Consider forking `gpui-component` to point it straight at `jomvick/zed`, which removes the
  `[patch."wingleeio/zed"]` block. Only worth it when a gpui-component bump is needed anyway.

## Order

0 → 1 → 3 (Komet part) → 2 → 4 → 5 → 3/4 **[fork]** items only if the measurements from 0
still miss budget. Each phase is its own PR with the before/after numbers in the description.
