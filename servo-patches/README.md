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

## Current series

- **0001 — Honor embedder-registered secure schemes in secure-context checks.**
  `ProtocolHandler::is_secure()` was honored by the fetch layer but not by
  script's `GlobalScope::is_secure_context()`, because a non-special scheme's
  opaque origin can never be "potentially trustworthy" — so `[SecureContext]`
  APIs (`crypto.randomUUID`, `crypto.subtle`, …) were absent on
  custom-protocol pages like `tauri://localhost`. Registers secure schemes
  from the merged `ProtocolRegistry` into `servo_url` at startup and consults
  them in `ServoUrl::is_potentially_trustworthy`. The related CSP gap
  (`'self'` never matches an opaque origin) is *not* addressed here.
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
  applied. Known issue: under synthetic X11 input (xdotool), shift-wrapped
  characters double-insert ("T" becomes "TT"); needs a real-keyboard repro
  to attribute (patch logic vs key-event delivery) before fixing.

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
  Note this fixes only the inherited-`color` input; paints set via CSS
  *classes* on SVG descendants (`fill`/`stroke`/`stroke-width` in a
  stylesheet) are still lost by serialization. The general fix is a
  candidate 0005, now investigated and feasible — all open questions
  resolved in its favor:
  - Stylo *does* style SVG descendants (their presentational hints are
    synthesized from the cascade via `element.rs`'s call into
    `LayoutDom<SVGElement>::synthesize_presentational_hints`, and Servo's
    style system has real `fill`/`stroke`/`stroke-width`/`d` longhands),
    so computed values exist for every element in the subtree.
  - The rasterizer honors CSS: usvg 0.47 parses `<style>` elements with
    simplecss, including attribute selectors and `!important`
    (`usvg/src/parser/svgtree/parse.rs`).
  - Design: at serialization (`SVGSVGElement::serialize_and_cache_subtree`)
    stamp each cloned element with a structural `data-servo-style-id`
    (style-independent, so the cached serialization survives restyles);
    at layout (0003's `replaced.rs` site) walk the subtree's computed
    styles each pass, generate a `<style>` block of
    `[data-servo-style-id="n"] { fill: …!important; … }` rules, and
    inject it into the decoded XML exactly like 0003 injects `color` —
    the rewritten URL is the cache key, so restyles (e.g. theme flips)
    re-rasterize for free with no new invalidation plumbing and no
    image-cache protocol changes. (usvg's `Options::style_sheet`
    injection point was considered and rejected: it would require keying
    the image cache on the stylesheet anyway.)
  - Known limitation: `<use>`-expanded clones keep the referenced
    element's id, so they flatten with the referenced element's
    at-definition styles.
  Estimated at roughly 200 lines across script and layout, reusing 0003's
  helpers.

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
