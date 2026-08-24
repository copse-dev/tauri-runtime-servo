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
- Engine gaps in Servo itself (at the pinned revision) include
  `contenteditable` support and the CSS `:has()` selector. The
  [`servo-patches/`](servo-patches) series fixes these and more; it is
  entirely opt-in — apply it to local servo/stylo checkouts, add `[patch]`
  overrides in your workspace, and enable this crate's `patched-servo`
  feature (see `servo-patches/README.md`). Without all three, the crate
  builds and runs against the stock pinned Servo.

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
