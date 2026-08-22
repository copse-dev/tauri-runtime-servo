# Servo patches

Patches this runtime needs on top of the Servo revision pinned in
`Cargo.toml`, each one a candidate upstream PR. This directory is the record;
the *applied* form lives on a fork branch that the pin points at.

## Workflow

1. Fork servo and branch from the pinned rev:
   `git clone https://github.com/servo/servo && git checkout -b tauri-runtime-patches f4dde27`
2. `git am servo-patches/*.patch`
3. Push the branch to the fork and point `Cargo.toml`'s `servo` dependency at
   it: `servo = { version = "0.4.0", git = "https://github.com/<fork>/servo.git", rev = "<tip>" }`
4. Submit each patch upstream; when one merges, advance the pin past it and
   delete the file here.

Consumers can also try a patch without a runtime release by checking servo
out locally, applying the series, and adding to their workspace root:

```toml
[patch."https://github.com/servo/servo.git"]
servo = { path = "../servo/components/servo" }
```

## Landed upstream since the pin

- **CSS Grid now defaults to on** (servo#45621, `76f48dc9e3a`). The
  runtime's `preferences.layout_grid_enabled = true` and the
  `runtime-0001-enable-css-grid` patch in the Copse repo both become
  no-ops when the pin advances past it; `layout_grid_enabled` has also
  been dropped from `EXPERIMENTAL_PREFS`.
- **No native SVG layout work has landed or is in flight.** Upstream
  `main` has no `components/layout/svg/`, and the four commits touching
  `components/script/dom/svg/` since the pin are refactors (interface
  subfoldering, `FontContext` plumbing, autofocus, borrow hazards) — not
  the geometry-interface work Phase 0 would duplicate.

## Known engine issues (no patch yet)

- **Variable fonts render at the default fvar instance — resolved by a
  pref, no patch needed.** A variable TTF (`font-weight: 100 900` via
  `@font-face`) ignored `font-weight` entirely: every weight rendered as
  the font's *default* fvar instance, which for fonts whose default is
  the wght minimum (e.g. Pliant, default wght=100) means hairline-Thin
  text with the Thin master's spacing. Root cause: Servo ships the
  complete variations pipeline — CSS `font-weight`/`font-width`/
  `font-optical-sizing`/`font-variation-settings` composed into fvar
  coordinates per CSS Fonts 4, applied to HarfBuzz shaping, FreeType
  metrics, and WebRender rasterization — but the whole path sits behind
  `layout.variable_fonts.enabled`, which is off by default. The runtime
  now flips that pref; weights and spacing then match Chromium on the
  same page (validated with a shaping-trace comparison against offline
  HarfBuzz and a four-family probe). The initially-reported "spurious
  gap in kerning pairs" ("Servo" → "Ser vo") turned out to be a
  characteristic of the test typeface, not an engine bug: Chromium
  renders the identical gap (the font's `r` carries a wide advance and
  has no r→v kern pair).

## Current series

- **0001 — Honor embedder-registered secure schemes in secure-context checks.**
  `ProtocolHandler::is_secure()` was honored by the fetch layer but not by
  script's `GlobalScope::is_secure_context()`, because a non-special scheme's
  opaque origin can never be "potentially trustworthy" — so `[SecureContext]`
  APIs (`crypto.randomUUID`, `crypto.subtle`, …) were absent on
  custom-protocol pages like `tauri://localhost`. Registers secure schemes
  from the merged `ProtocolRegistry` into `servo_url` at startup and consults
  them in `ServoUrl::is_potentially_trustworthy`. The related CSP gap
  (`'self'` never matches an opaque origin) is addressed by 0008 +
  csp-0001.
  *Validated end-to-end on Linux*: with this patch and the randomUUID
  polyfill disabled, the Copse renderer's id-minting boot path (previously
  dead without the polyfill) works on the native API under
  `tauri://localhost`.

- **0002 — script: support user input in contenteditable elements.**
  Cherry-picked from the fork branch `codex/contenteditable-user-input`
  (authored 2026-08-02); applies cleanly to the pinned rev. Routes keyboard
  input into `contenteditable` roots in
  `document_event_handler.rs` — the gap that made rich-text composers
  (e.g. Copse's chip editor) unable to accept typing under Servo.
  *Validated end-to-end on Linux*: typing into the Copse composer, pressing
  Send, and receiving a model reply all work in the Servo UI with this
  applied. The shift-wrapped double-insert ("T" became "TT") observed during
  the first validation was traced to the runtime, not this patch: tao's X11
  backend fills `KeyEvent::text` with the unshifted character while
  `logical_key` is correctly translated, so the embedder's key/IME dedup
  missed shifted characters and committed a second copy via a composition
  event. Fixed in `tauri-runtime-servo` (`inserted_key_text` now prefers the
  logical key); mixed-case typing with shifted symbols now inserts each
  character exactly once in both contenteditable and textarea.

- **0003 — layout: resolve currentColor in rasterized inline SVG.**
  Inline `<svg>` is XML-serialized to a `data:` URL and rasterized with no
  CSS context, so `currentColor` resolved to black — `currentColor` icon
  sets rendered as solid dark shapes. Injects the element's computed CSS
  `color` as a root attribute on the serialized document (honored by the
  rasterizer per SVG's `color` property) unless the markup declares one;
  the rewritten URL doubles as the cache key so rasterization is
  per-resolved-color. *Validated on Linux* with a four-variant probe
  (currentColor + CSS color now correct; explicit `color` attributes and
  explicit paints unchanged) and against Copse's titlebar icon set.
  Note this fixes only the inherited-`color` input; 0006 generalizes it
  to all selector-driven styling on the subtree.

- **0004 — script: report module evaluation errors asynchronously.**
  Cherry-picked from the fork branch `jkt/module-worker-top-level-await`
  (f7c87376be); applies cleanly to the pinned rev. `execute_module` forced
  the module evaluation promise to settle synchronously
  (`ThrowModuleErrorsSync`), which is wrong for modules using top-level
  await — their evaluation promise is still pending when `ModuleEvaluate`
  returns. Switches to `ReportModuleErrorsAsync`, matching step 8 of "run
  a module script". Ships WPT coverage for dedicated module workers using
  top-level await (static import, dynamic import, pending message,
  rejection surfacing on `Worker.onerror`). *Build-validated on Linux* as
  part of the four-patch stack; the WPT suite has not yet been run against
  it.

- **0005 — script: honor the SVG `color` presentation attribute.**
  SVG 2 defines presentation attributes for CSS properties including
  `color`, but `SVGElement`'s presentational-hint synthesis only covered
  paint and geometry properties, so `<svg color="#ff4040">` never
  influenced the computed color — breaking `currentColor` resolution for
  such markup, including in the rasterization path 0003/0006 feed.
  *Validated on Linux*: with 0006 applied, the 0003 probe's
  color-attribute cases regressed to the inherited color until this
  patch restored them.

- **0006 — layout: flatten computed styles into rasterized inline SVG.**
  The general form of 0003: selector-driven styling on the SVG subtree
  (class rules for `fill`/`stroke`/`stroke-width`/…) was lost by
  serialization, so stylesheet-styled icon sets rendered with initial
  paints. Serialization stamps each cloned element with a structural
  `data-servo-style-id` (style-independent, so the cached serialization
  survives restyles); layout walks the subtree's computed styles in the
  same preorder each pass and injects a `<style>` block of
  `[data-servo-style-id="n"] { … !important }` rules into the decoded
  document (usvg parses `<style>` via simplecss, including attribute
  selectors and `!important`), and the rewritten URL remains the cache
  key. Computed-value serializations are adapted where svgtypes disagrees
  with CSS: `currentcolor` is resolved at flatten time (svgtypes only
  parses the camelCase spelling); absolutized `url()` paint-server
  references are reduced back to their local fragment (without this,
  gradients broke even via *attributes*, since the flattened computed
  `fill` stomped them with the absolutized form); `transform` is emitted
  for descendants as the unitless `matrix()` form (functional forms with
  units are rejected). The root `<svg>` has a real box whose `opacity`
  and `transform` Servo applies natively, so those are not flattened for
  the root — flattening them too double-applied root opacity. Because SVG
  descendants generate no boxes, a subtree restyle previously surfaced
  only as repaint damage and left a stale rasterization; the damage
  traversal now escalates any damage at an `<svg>` to a box rebuild —
  which is also what makes theme flips re-rasterize.
  *Validated end-to-end on Linux* across two probes and a control:
  class `stroke: currentColor`, per-class fills, themed fill flipped
  live dark/light, `visibility: hidden` descendant, explicit-attribute
  control, class `stroke-dasharray`, gradients via attribute and via
  class `fill: url(#id)`, root `opacity: 0.5` matching an HTML
  `rgba(...)` reference exactly, descendant `opacity`, CSS
  `transform: translate(...)` on a descendant, and em-based
  `stroke-width` — all correct; the 0003 probe still passes 4/4.
  Font properties (`font-family`/`-size`/`-weight`/`-style`) are also
  flattened so `<text>` reaches the rasterizer with the author's font,
  with computed `font-size` already absolute (see 0007 for the
  complementary resolver fix). Known limitations: CSS `display: none` on
  descendants is not honored (Servo computes `display: none` for the
  whole boxless subtree, so the computed value carries no author signal;
  `visibility: hidden` works), `transform-origin` is not carried over,
  and `<use>`-expanded clones flatten with the referenced element's
  at-definition styles. With this patch the app-side
  presentation-attribute fallbacks recreated for Copse's titlebar icons
  become unnecessary (they remain harmless).

- **stylo-0001 — enable `:has()` selector parsing (STYLO repo, out of
  series).** Stylo ships complete `:has()` matching and invalidation
  (Gecko production code); Servo-mode parsing is a hardcoded `false` in
  `style/servo/selector_parser.rs`. Flipping it was validated end-to-end
  on Linux: `CSS.supports` reports the selector, live rules match, and
  dynamic invalidation restyles the parent when matching children are
  added or removed. Applies to the stylo repo at servo's pinned rev, via
  a `[patch."https://github.com/servo/stylo"]` section (the patch file
  lists the eleven crates to redirect).

- **0007 — script: fall back to generic sans-serif for SVG text font
  resolution.** The rasterizer substitutes fonts per-glyph only after a
  base font resolves for a text span; when `SvgFontResolver` returns
  `None` the whole span is dropped. Markup with no font-family reaches
  the resolver as usvg's default family ("Times New Roman"), so on
  systems without it every such `<text>` silently vanished — verified by
  tracing the resolver in the Linux container (`families=[Named("Times
  New Roman")], templates=0`). Retries with the embedder's generic
  sans-serif before giving up. *Validated on Linux* together with 0006's
  font flattening: the extended probe's class-styled `<text>` case (bold
  30px magenta "A") went from rendering nothing to correct.

- **0008 — url: give embedder-registered custom schemes tuple origins.**
  The URL standard gives every non-special-scheme URL an opaque origin,
  so a document served over a registered custom protocol
  (`tauri://localhost`) got a unique origin per load: CSP `'self'` could
  never match it (any policy blocked every same-origin subresource — the
  reason the prototype shipped tauri.html with its CSP meta stripped),
  `location.origin` was `"null"`, and localStorage was unusable. Models
  URLs on embedder-registered secure schemes (0001's registry) with a
  stable `(scheme, host, port)` tuple origin when the URL carries a host
  — the treatment Chromium/WebKit give schemes registered as "standard"
  — serialized without a port (`tauri://localhost`), and routes
  `location.origin`/`URL.origin` through it. CSP `'self'` additionally
  needs csp-0001 below, because the CSP crate compares against rust-url's
  notion of the resource URL's origin, which stays opaque. *Validated
  end-to-end on Linux* with both applied: a seven-case probe under
  `default-src 'self'` passes 7/7 (same-origin script/style/fetch load;
  inline and cross-origin scripts blocked with violation events;
  `location.origin` = `tauri://localhost`; localStorage round-trips), and
  the full Copse app boots with its real CSP enforced — including
  `connect-src` matching the loopback WebSocket — with zero violations.

- **0009 — layout: add an SVG viewport behind `layout.svg.native.enabled`.**
  First step of native SVG layout (plan and phasing:
  `docs/plans/servo-svg-layout.md` and `servo-svg-agent-brief.md` in the
  Copse repo). Inline `<svg>` is XML-serialized to a `data:` URL and
  rasterized, so nothing inside it is a layout participant — which is why
  CSS animations on SVG descendants never run, and why patches 0003, 0005
  and 0006 above exist at all: each is a workaround for style not
  surviving serialization. Adds `components/layout/svg/` with `viewBox` /
  `preserveAspectRatio` parsing and the viewport transform, and carries
  `preserveAspectRatio` through `SVGElementData` (layout could not see it
  before; the rasterizer read it out of the serialized copy).
  The one behaviour this changes today is the **intrinsic aspect ratio**:
  `SVGElementData::ratio_from_view_box` parses `viewBox` with
  `parse_integer`/`parse_unsigned_integer`, so `viewBox="0 0 24.5 12.25"`
  contributes no ratio at all and the element sizes as if it had none. The
  new parser implements SVG's `<number>` grammar and the sizing path uses
  it when the pref is on. Rasterization is untouched, so turning the pref
  on is not yet a rendering change.
  *Validated:* nine unit tests over the `viewBox` grammar and the
  meet/slice × alignment matrix (`scripts/servo-svg-unit-tests.sh` in the
  Copse repo — servo's own `cargo test` cannot be run in this stack);
  `cargo build --release -p servo-layout` clean. Not yet WPT-validated —
  that needs a standalone `./mach build`, which this stack does not have.
  Seam cost: **21 added lines** across pre-existing files, no deletions.
  Against upstream `main` (`bd220a15`, 3 weeks past the pin) the patch
  3-way-applies with a single conflict, a one-line insertion into the
  sorted `EXPERIMENTAL_PREFS` list.

- **0010 — layout: SVG geometry traversal and vello painting.** Walks the
  `<svg>` subtree, resolves each element's geometry and paint from its
  computed style, and paints with `vello_cpu` — the backend
  `components/canvas` already uses, so no rasterizer and no tessellation.
  Most geometry arrives through the cascade: Servo already maps `x`, `y`,
  `cx`, `cy`, `r`, `rx`, `ry`, `width`, `height` and `d` onto real CSS
  longhands, and stylo's `SVGPathData::normalize(true)` reduces path data
  to absolute M/L/C/A/Z. The exceptions are `<line>`/`<polyline>`/
  `<polygon>` (whose coordinates are not CSS properties in SVG 2) and the
  `transform` attribute, which Servo does not map — and could not
  usefully, since SVG's transform-list grammar is unitless and its
  `rotate()` takes an origin CSS has no equivalent for. Both are parsed
  here. *Validated:* 37 unit tests, 9 of them rendering real pixels,
  including that group `opacity` composites as a unit rather than folding
  into each child. Not implemented, each skipped rather than
  approximated: `<use>` (Servo's `SVGUseElement` is a bare stub with no
  shadow instancing), paint servers, clip, mask, marker, nested viewports.
  Seam: **1 added line** (the `vello_cpu` dependency).

- **0011 — layout: SVG hit testing against outlines, not bounding boxes.**
  Fill regions honour `fill-rule`, stroke regions are the stroke outline
  (so a `<line>`, which encloses no area, is still hittable), topmost in
  paint order wins, and a non-invertible transform collapses to no hit.
  The `pointer-events` keyword set needs `stylo-0002` below; with it, the
  full SVG set maps onto fill and stroke hit regions. *Validated:* 9
  tests, chosen as the cases where bounding-box testing gives the wrong
  answer. Seam: **0 lines**.

- **0012 — layout: SVG image registry.** Per-node `ImageKey` caching keyed
  on a content hash of the scene, so an unchanged SVG costs nothing on
  reflow and an animating one costs one upload per changed frame.

- **0013 — layout: paint native SVG, and let its descendants animate.**
  Connects the renderer to the compositor and makes CSS animations run on
  SVG content. `CrossProcessPaintApi` holds `Cell`s and is `!Sync` while
  `LayoutContext` must be `Sync`, so the upload cannot happen where the
  pixels are produced; keys come instead from `ImageCache::get_image_key`
  (a pre-filled pool, already reachable from `ImageResolver`), rendering
  is pure and runs on any layout thread, and the upload is queued and
  drained on the layout thread before the display list is sent — the same
  shape as `pending_rasterization_images`. Allocating the key up front
  means the fragment carries a real one on the first frame.
  Three things had to be fixed that were not anticipated: `svg > * {
  display: none }` in `servo.css` prunes the style traversal so a `<g>`'s
  children were never styled (moved to a stylesheet applied only when the
  pref is off); the `transform` attribute was read without consulting the
  CSS `transform` property; and `Animations::do_post_reflow_update`
  cancels animations on any node that is not "being rendered", which a
  boxless SVG descendant reports — so an SVG animation registered for one
  tick and was dropped.
  *Validated in a real browser:* rendering is within 0.13% of pixels of
  the rasterization path (antialiasing only); a 4s spinner sampled at
  +1.1s/+2.1s/+3.1s changes 0 and 0 pixels with the pref off and 2016 and
  2201 with it on; `svg/` WPT +1 test and zero regressions over 1261
  tests. Seam: **~25 added lines**.

- **0014 — layout: honor `transform-origin`, and fix `viewBox`'s intrinsic
  ratio.** The CSS `transform` property rotates about `transform-origin`
  (initial `50% 50%`, reference box the nearest viewport); ignoring it
  swung an animated `rotate()` around the viewport corner and off the
  canvas. Every headless test passed regardless — a headed run found it
  in one frame. Note the rasterization path is *worse* here, not merely
  different: it drops `transform-origin` too, so the same page renders
  blank under it.
  Also fixes `SVGElementData::ratio_from_view_box`, which parsed `viewBox`
  with `parse_integer`/`parse_unsigned_integer`, so `viewBox="0 0 24.5
  12.25"` contributed no intrinsic ratio at all — measured, a 200px-wide
  `<svg>` laid out 150px tall instead of 100px — and `viewBox="0,0,24,24"`
  failed on the comma. **This half is on the default rasterization path
  and is worth submitting upstream on its own.** The number-list parser
  moves into `layout_api` so both paths share one implementation.

- **0015 — layout: honor `pathLength` when scaling dash patterns.** Servo
  has never supported `pathLength` (a commented-out line in
  `SVGGeometryElement.webidl`), and ignoring it does not merely misplace
  dashes, it changes their count: on a 100-unit line with `pathLength="1"`
  and `stroke-dasharray: 1 1`, fifty one-unit dashes instead of one. On a
  long path those go sub-pixel and read as a solid, slightly translucent
  stroke that never appears to move — an element that looks fully drawn
  and frozen while its computed `stroke-dashoffset` advances correctly.
  This is what breaks the standard "draw a line on" idiom, and it was the
  actual cause of a frozen reasoning indicator that had been mistaken for
  an animation bug. *Validated:* the test path now renders one dash
  growing 5px → 80px then erasing, instead of a static comb.
  Seam: **0 lines**.

- **0016 — script: implement `getBBox` and `getTotalLength`.** Both were
  commented out in the WebIDL, so calling either threw `TypeError`. Cheap
  now that layout resolves SVG geometry into `kurbo` paths: a bounding box
  and a perimeter are one call each, resolved on demand from computed
  style. `getBBox` on a container unions its children's boxes through
  each child's transform, and the `<svg>` element is its own viewport.
  `getTotalLength` reports the length already renormalized by
  `pathLength`. Not done: the options argument (stroke/markers/clipped) is
  ignored, and with the pref off only direct children of the `<svg>`
  resolve. Also fixes a **latent crash** shared with 0013's
  `node_rendering_type` check — both walk DOM parents, and any walk that
  finds no `<svg>` reaches the Document, where `type_id` panics.
  *Validated:* `svg/` WPT +188 subtests, zero regressions.

- **0017 — layout: gradients and clip-path.** The two biggest remaining
  holes, and the two that mattered to real content — gradients appear in
  39 of the Copse app's own files and `clip-path` in 19. Gradients were a
  *regression* against the rasterization path, which renders them;
  `clip-path` was worse than missing, painting content unclipped.
  Paint servers are collected in one scan of the `<svg>` subtree, since
  layout has no document-wide id lookup (the same gap that stops `<use>`).
  Covers linear and radial gradients, both unit systems,
  `gradientTransform`, `spreadMethod`, focal points, gradients on strokes,
  `href`/`xlink:href` stop inheritance with a cycle guard, percent-encoded
  fragments, and `clipPath` in both unit systems on shapes and groups.
  Needs **stylo-0003**: `stop-color`/`stop-opacity` are gecko-gated, so a
  `<stop>` had no colour at all. Also maps `clip-path` as a presentation
  attribute, without which `clip-path="url(#id)"` never reached the
  cascade. `clip-rule` stays gecko-gated, so clips use the non-zero rule.
  *Validated:* gradients within **0.024%** of pixels of the rasterization
  path across six variants, clipping within **0.020%** across five, all
  edge antialiasing; `svg/` WPT **+199 subtests, zero regressions**.
  One correctness note worth carrying: an unresolvable `clip-path`
  reference means *ignore the property*, not *hide the element* — SVG 1.1
  said the opposite and it is easy to get backwards.

- **stylo-0003 — ungate `stop-color` and `stop-opacity`
  (stylo repo, out of series).** Both are `#[cfg(feature = "gecko")]`, so
  in Servo a gradient stop has no colour and every gradient comes out
  empty. Consumed by 0017.

- **stylo-0002 — ungate the SVG `pointer-events` keywords
  (stylo repo, out of series).** `visiblePainted`, `visibleFill`,
  `visibleStroke`, `visible`, `painted`, `fill`, `stroke` and `all` are
  every one of them behind `#[cfg(feature = "gecko")]`, leaving Servo with
  `auto` and `none`. Consumed by 0011. Servo's existing uses only ever
  compare against `None`, so ungating cannot change their meaning.

- **csp-0001 — match `'self'` for custom-scheme origins
  (rust-content-security-policy repo, out of series).** The `'self'`
  fast path compares the protected resource's origin against
  `url.origin()`, which rust-url reports as opaque for every non-special
  scheme — so `'self'` could never match, and the fallback branch only
  admits http(s)/ws(s) upgrades. Adds component comparison (equal
  scheme/host, ports equal or absent on both sides) when the user agent
  supplies a tuple origin, which 0008 makes Servo do. Applies to
  `rust-ammonia/rust-content-security-policy` at the published 0.8.1 rev
  (`6a523bab`), consumed via a `[patch.crates-io]` section. Special
  schemes are unaffected (same-scheme matches were already taken by the
  fast path).
