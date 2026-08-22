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
