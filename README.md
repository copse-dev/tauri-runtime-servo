# tauri-runtime-servo

An **experimental** [Tauri](https://tauri.app/) runtime backed by the
[Servo](https://servo.org/) web engine, embedded in-process via
[libservo](https://github.com/servo/servo).

Instead of the system webview used by the default `tauri-runtime-wry` runtime
(WebView2, WKWebView, WebKitGTK), this runtime renders your app with Servo —
the same engine on every platform, statically linked into your binary.

> ⚠️ **Status: experimental.** None of the exposed API of this crate is
> stable, and it may break semver compatibility in the future. The major
> version only signifies the intended Tauri version.

## Why a separate project?

This work started as a Servo backend inside wry
([tauri-apps/wry#1797](https://github.com/tauri-apps/wry/pull/1797)). The wry
maintainers' direction is to keep wry focused on system webviews and host
alternative engines as separate runtime crates at the Tauri layer instead —
the same approach taken by
[`tauri-runtime-cef`](https://github.com/tauri-apps/tauri/tree/feat/cef) and
[`tauri-runtime-verso`](https://github.com/versotile-org/tauri-runtime-verso).
A standalone repository also allows the fast-moving Servo dependency to be
updated independently of Tauri's release process.

This project is the result: the Servo backend from that PR, restructured as a
self-contained `tauri-runtime` implementation that works with **published
Tauri crates** — no patched fork of tauri or wry required.

### How it compares to tauri-runtime-verso

[`tauri-runtime-verso`](https://github.com/versotile-org/tauri-runtime-verso)
also brings Servo to Tauri, but it drives a separate `versoview` process.
`tauri-runtime-servo` embeds libservo directly in your app's process, keeps
Tao as the windowing layer (like `tauri-runtime-wry`), and needs no external
binary to bundle.

## Usage

```toml
# Cargo.toml
[build-dependencies]
tauri-build = "2"

[dependencies]
tauri = { version = "2", default-features = false, features = [
  "common-controls-v6",
] }
tauri-runtime-servo = "0.1"
```

The crate depends on stock libservo from crates.io. To build against the
[`servo-patches/`](servo-patches) series instead, see [Using a patched
Servo](#using-a-patched-servo). To track unreleased changes, depend on the
repository directly:

```toml
tauri-runtime-servo = { git = "https://github.com/copse-dev/tauri-runtime-servo" }
```

Note that the `wry` feature of `tauri` must stay disabled — this runtime
replaces it.

```rust
// src/main.rs
type ServoRuntime = tauri_runtime_servo::Servo<tauri::EventLoopMessage>;

fn main() {
  tauri::Builder::<ServoRuntime>::new()
    // Servo cannot read custom protocol request bodies, so swap in an
    // invoke system that routes IPC through Servo's postMessage bridge
    .invoke_system(tauri_runtime_servo::INVOKE_SYSTEM_SCRIPT)
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
```

See [`examples/helloworld`](examples/helloworld) for a complete app; run it
with:

```bash
cargo run -p helloworld-servo
```

## Building

Servo is compiled from source (pinned to a known-good revision in
`Cargo.toml`), so the first build is large. The first build downloads a
prebuilt SpiderMonkey archive; leave `MOZJS_FROM_SOURCE` unset unless you
explicitly want `mozjs_sys` to compile SpiderMonkey locally.

On Linux you need Servo's build dependencies, e.g. on Debian/Ubuntu:

```bash
sudo apt-get install -y libdbus-1-dev libegl1-mesa-dev libfontconfig1-dev \
  libfreetype6-dev libgtk-3-dev libharfbuzz-dev libwebkit2gtk-4.1-dev \
  libx11-dev libxkbcommon-x11-dev lld
export RUSTFLAGS="-C link-arg=-fuse-ld=lld"
```

## Using a patched Servo

This crate depends on **stock libservo from crates.io**, which is what makes
it publishable there: crates.io accepts registry dependencies only, so an
engine fork pinned by git revision cannot travel inside a release.

The [`servo-patches/`](servo-patches) series — native SVG layout,
`contenteditable`, the CSS `:has()` selector — is therefore opt-in. To build
against it, override the engine crates **in your own workspace root**.
`[patch]` is honoured only there, never from a dependency's manifest.

### 1. Check out the revisions behind the published crates

A `[patch]` entry is accepted only if the checkout's own version satisfies
the requirement it replaces, so start from the exact trees the published
crates were cut from. This crate requires `servo = "0.5"`, which is servo
`77fccacc` (2026-08-04); that tree in turn wants stylo `0.20`, which is
stylo `67faaab3`:

```bash
git clone https://github.com/servo/servo
git -C servo checkout -b tauri-runtime-patches 77fccacc1f1fdce10498d50173aafaa09d02879e

git clone https://github.com/servo/stylo
git -C stylo checkout -b tauri-runtime-patches 67faaab3ff7aa66780ec1d0f51ca47e177b812d3
```

The CSP crate needs no checkout: its override in step 3 points straight at a
fork branch that already carries its two patches.

### 2. Apply the series

The servo files are `git format-patch` output. The stylo files are plain
diffs with a prose preamble, so `git am` rejects them — apply those with
`git apply`:

```bash
git -C servo am ../tauri-runtime-servo/servo-patches/0*.patch

for p in ../tauri-runtime-servo/servo-patches/stylo-*.patch; do
  git -C stylo apply --3way "$p"
done
```

The series is authored against servo `f4dde27` and stylo `2d289c1` (the 0.19
line), but applies cleanly to the revisions above — 24/24 and 5/5 with no
conflicts, verified against servo `77fccacc` and stylo `67faaab3`. Expect
that to need rebasing once the pin moves further. The CSP crate's two
patches are not files here at all — they are commits on the fork branch the
override below names, described in
[servo-patches/README.md](servo-patches/README.md).

### 3. Add the overrides to your workspace root

Every entry goes under `[patch.crates-io]`: as of 0.5.0 servo takes its
stylo crates from the registry too, so there is no git source left to
override.

```toml
[patch.crates-io]
servo = { path = "../servo/components/servo" }
content-security-policy = { git = "https://github.com/copse-dev/rust-content-security-policy", rev = "fb5fd0f1af7f0c0dc315bf938507290b2e48cdbe" }

# All eight stylo entries are required. Overriding `stylo` alone leaves the
# others resolving from the registry, which puts a second copy of
# `stylo_traits` and friends in the graph and fails to compile.
selectors = { path = "../stylo/selectors" }
servo_arc = { path = "../stylo/servo_arc" }
stylo = { path = "../stylo/style" }
stylo_atoms = { path = "../stylo/stylo_atoms" }
stylo_dom = { path = "../stylo/stylo_dom" }
stylo_malloc_size_of = { path = "../stylo/malloc_size_of" }
stylo_static_prefs = { path = "../stylo/stylo_static_prefs" }
stylo_traits = { path = "../stylo/style_traits" }
```

`stylo_derive`, `to_shmem`, and `to_shmem_derive` need no entries — the
patched crates reach them by path.

The CSP entry is pinned by `rev` rather than by `branch = "tauri-runtime-patches"`
on purpose: that branch is rebased whenever the crate's upstream master moves,
and a branch pin would change what a CSP matcher does under you on the next
`cargo update`. The branch tip and the rev are the same commit today.

### 4. Enable the feature

```toml
[dependencies]
tauri-runtime-servo = { version = "0.1", features = ["patched-servo"] }
```

`patched-servo` sets preferences that exist only once the series is applied
(`layout_svg_native_enabled`, added by patch 0009). Without all four steps
the crate builds and runs against stock Servo.

### What CI checks

The claims above are checked rather than remembered. The *patch series
applies* job in [`ci.yml`](.github/workflows/ci.yml) re-applies the series on
every pull request, to whatever revisions the current `Cargo.lock` implies —
so a dependency bump that outruns the patches fails there.

[`patched-servo.yml`](.github/workflows/patched-servo.yml) goes the rest of
the way on Linux: it follows all four steps, lifting the override block
straight out of step 3 rather than restating it, and builds the result with
`patched-servo` enabled. That build is the only thing proving the patched
tree still compiles and still carries `layout_svg_native_enabled` — a series
that applies cleanly but has lost the pref passes every other job. It runs
when the series, the manifests, or this section change, plus weekly and on
demand; a cold build of the patched engine runs in about a quarter of an
hour.

### When the pin moves

Whenever this crate's `servo` requirement changes, the checkout revisions
above have to move with it, or the overrides stop resolving. The revision
behind any published version is recorded in the crate itself:

```bash
curl -sL https://static.crates.io/crates/servo/servo-0.5.0.crate \
  | tar xzO servo-0.5.0/.cargo_vcs_info.json
```

## Publishing

Releases go to crates.io from CI: push a `v*` tag and the
[`publish.yml`](.github/workflows/publish.yml) workflow verifies the packaged
crate builds on Windows, Linux, and macOS, then publishes it.

Publishing uses [trusted
publishing](https://crates.io/crates/rust-lang/crates-io-auth-action): the
workflow exchanges GitHub's OIDC token for a short-lived crates.io token, so
no long-lived API secret is stored in this repository. One-time setup:

1. Log in to [crates.io](https://crates.io/), open your crate's settings (or
   the publish form before the first release) and add a **trusted publishing**
   rule for `copse-dev/tauri-runtime-servo`:
   - workflow name: `publish.yml`
   - environment name: `crates`
2. Create the matching `crates` environment in the repository's GitHub
   settings (Settings → Environments → New environment).

For a new crate version:

```bash
# 1. Bump the version in Cargo.toml and commit.
# 2. Rehearse without publishing (runs verify-package only):
gh workflow run publish
# 3. Tag and push; CI does the rest.
git tag vX.Y.Z
git push origin vX.Y.Z
```

The first publish must be done by an owner of the crate name — after that,
trusted publishing works for subsequent versions.

## Platform support

| Platform    | Supported                  |
| ----------- | -------------------------- |
| Windows     | ✅                         |
| macOS       | ✅                         |
| Linux (X11) | ✅ (`x11` feature, default) |
| Linux (Wayland) | ❌ not yet             |
| Android / iOS | ❌ desktop only          |

## What works

URL and HTML navigation, custom request headers and protocols,
initialization scripts, IPC (via the postMessage bridge), navigation and
page-load handlers, per-URL cookies, browsing data clearing, zoom,
visibility, focus, background colors, HiDPI scaling, and composition into
Tao-owned windows — validated against a large real-world Tauri UI with
performance close to Electron.

## Known limitations

- Servo does not expose custom protocol **request bodies**, so the default
  Tauri invoke system must be replaced with
  [`INVOKE_SYSTEM_SCRIPT`](src/invoke-system-initialization-script.js) (see
  Usage above). The channel data fetch command still uses the custom
  protocol — its arguments travel in request headers.
- The initialization-script main-frame-only option is not exposed by Servo's
  embedding APIs.
- Printing, global cookie enumeration, and multiple Servo webviews in one
  native window are not supported yet.
- In-process devtools window controls are not supported.
- Engine gaps in Servo itself (at the pinned release) include
  `contenteditable` support and the CSS `:has()` selector. The
  [`servo-patches/`](servo-patches) series fixes these and more; it is
  entirely opt-in — see [Using a patched Servo](#using-a-patched-servo).
  Without it, the crate builds and runs against stock Servo.

## Repository layout

- [`src/`](src) — the runtime: `Runtime`/`RuntimeHandle`/dispatcher
  implementations over Tao ([`src/lib.rs`](src/lib.rs)) and the Servo
  embedder glue ([`src/servo/`](src/servo)).
- [`examples/helloworld`](examples/helloworld) — minimal Tauri app on the
  Servo runtime.

## Features

- `x11` *(default)*: X11 support on Linux.
- `dbus` *(default)*: dbus for theme support on Linux.
- `devtools`: enables devtools in release builds (see limitations above).
- `macos-private-api`: transparent windows etc. on macOS.
- `patched-servo`: sets preferences that only exist once
  [`servo-patches/`](servo-patches) is applied (native SVG layout); pair it
  with a `[patch]` override pointing at the patched servo checkout.
- `tracing`: instrument with [`tracing`](https://docs.rs/tracing).

## License

Copyright 2019-2024 Tauri Programme within The Commons Conservancy.

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE-2.0)
or [MIT license](LICENSE-MIT) at your option.

Portions of this code are derived from
[wry](https://github.com/tauri-apps/wry) and
[tauri](https://github.com/tauri-apps/tauri) (Apache-2.0 OR MIT).
