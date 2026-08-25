#!/usr/bin/env bash
#
# Prepares a patched-servo build: adds the README's `[patch.crates-io]` block
# to this workspace's manifest. It runs no cargo itself — the build stays a
# visible step of the job that calls this.
#
# There is nothing to check out and nothing to apply. The patched engine lives
# as `tauri-runtime-patches` branches on this organisation's forks, and every
# override names one by `rev`, so cargo fetches them as ordinary git
# dependencies. This script used to clone servo and stylo and `git am` a
# series onto each; all of that went away with the patch files.
#
# What remains is worth keeping honest: the block is lifted out of README.md
# rather than restated, so what CI builds is the block the "Using a patched
# Servo" section hands to a consumer. Break that recipe and this is where it
# shows.
#
# check-engine-pins.sh already proves each pin resolves to a version the
# lockfile accepts. What it cannot prove is that the tree those pins produce
# still compiles, or that this crate's feature-gated code still matches what
# the branches provide: `patched-servo` sets `layout_svg_native_enabled`, a
# pref that exists only on the patched tree, so a branch that resolves but has
# stopped carrying that pref fails in the build this prepares and nowhere else.

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

fail() { printf '\n%s\n' "$*" >&2; exit 1; }

overrides=$(awk '
  /^### 1\./             { section = 1; next }
  section && /^```toml$/ { block = 1; next }
  block && /^```/        { exit }
  block                  { print }
' "$repo_root/README.md")

[ -n "$overrides" ] \
  || fail "README.md: found no toml block under a '### 1.' heading.
The override block that section publishes is what this builds; if it moved,
point this script at wherever it went."
grep -q '^\[patch\.crates-io\]' <<< "$overrides" \
  || fail "README.md: the block under '### 1.' does not open with [patch.crates-io]:

$overrides"
grep -q '^servo = ' <<< "$overrides" \
  || fail "README.md: the block under '### 1.' overrides no servo:

$overrides"

# A path override would need a checkout this job does not make, and would
# resolve to whatever happens to sit at that path on the runner — which is
# nothing. Fail loudly rather than build something indistinguishable from
# stock while reporting success.
if grep -q 'path[[:space:]]*=' <<< "$overrides"; then
  fail "README.md: the block under '### 1.' overrides something by path:

$(grep 'path[[:space:]]*=' <<< "$overrides")

Every published override has to name a fork by rev — a path resolves only on
the machine that has the checkout."
fi

{
  printf '\n# Appended by .github/scripts/prepare-patched-servo.sh, copied from\n'
  printf '# README.md "Using a patched Servo". Not part of the committed manifest:\n'
  printf '# this crate depends on stock libservo so that it stays publishable.\n'
  printf '%s\n' "$overrides"
} >> "$repo_root/Cargo.toml"

echo "Added to Cargo.toml:"
printf '%s\n' "$overrides" | sed 's/^/  /'
echo
echo "Ready. Build with --features patched-servo."
