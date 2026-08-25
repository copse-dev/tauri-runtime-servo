#!/usr/bin/env bash
#
# Prepares a patched-servo build: adds the README's `[patch.crates-io]` block
# to this workspace's manifest, checks out whatever that block overrides by
# path, and applies servo-patches/ to each. It runs no cargo itself — the
# build stays a visible step of the job that calls this.
#
# check-patch-series.sh already proves the series applies. What it cannot
# prove is that the tree it produces still compiles, or that this crate's
# feature-gated code still matches what the series provides: `patched-servo`
# sets `layout_svg_native_enabled`, a pref that exists only once patch 0009
# has landed, so a series that applies but has stopped carrying that pref
# fails in the build this prepares and nowhere else.
#
# Nothing is pinned here, for the same reason nothing is pinned there. The
# versions in Cargo.lock decide the revisions — every published crate records
# the commit it was cut from — and the override block is lifted out of
# README.md rather than restated, so what CI builds is the block the "Using a
# patched Servo" section hands to a consumer. Break that recipe and this is
# where it shows.
#
# Which repositories get checked out is read out of that block too, rather
# than listed here. A crate can be overridden by path (needs a patched
# checkout) or by git rev (a fork already carries the fixes, and cargo fetches
# it), and content-security-policy has been both. Following the block means
# this keeps working across that move instead of silently building the wrong
# thing; an override by path this script has no recipe for is a hard error.
#
# The checkouts are siblings of the repository because that is what the
# README's paths mean: `../servo/components/servo` resolves against the
# workspace root the overrides are added to. Cloning them somewhere else
# would mean rewriting the paths, and the documented ones would go untested.

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
patches="$repo_root/servo-patches"
checkouts=$(cd "$repo_root/.." && pwd)

fail() { printf '\n%s\n' "$*" >&2; exit 1; }

# How to build each checkout the override block can ask for: where it comes
# from, which locked crate version fixes its revision, which patches belong to
# it, and how they apply. The upstreams are deliberately explicit rather than
# read out of crate metadata — if one ever moves, a human should notice
# instead of CI quietly following it somewhere else.
#
# `am` is format-patch output. `apply` is for plain diffs behind a prose
# preamble, which `git am` rejects — the stylo files are written that way.
recipe() {
  case $1 in
    servo)
      echo "https://github.com/servo/servo servo 0*.patch am" ;;
    stylo)
      echo "https://github.com/servo/stylo stylo stylo-*.patch apply" ;;
    rust-content-security-policy)
      echo "https://github.com/rust-ammonia/rust-content-security-policy content-security-policy csp-*.patch am" ;;
    *)
      return 1 ;;
  esac
}

# Kept in step with check-patch-series.sh rather than shared with it: the two
# scripts have to agree on which revision a locked version implies, and a
# divergence would mean the series is checked against one tree and built
# against another.
locked_version() {
  local crate=$1 found count
  found=$(awk -v want="name = \"$crate\"" '
    $0 == want { getline; if ($1 == "version") { gsub(/"/, "", $3); print $3 } }
  ' "$repo_root/Cargo.lock")
  count=$(printf '%s' "$found" | grep -c . || true)
  [ "$count" -eq 1 ] || fail "Cargo.lock: expected exactly one '$crate' entry, found $count"
  printf '%s' "$found"
}

crate_revision() {
  local crate=$1 version=$2 rev
  rev=$(curl -fsSL "https://static.crates.io/crates/$crate/$crate-$version.crate" \
    | tar xzO "$crate-$version/.cargo_vcs_info.json" \
    | jq -re '.git.sha1') \
    || fail "$crate $version: no .cargo_vcs_info.json — cannot tell which revision it was cut from"
  printf '%s' "$rev"
}

# Shallow: the patches only ever touch the one tree, and nothing in the build
# reads history, so it is dead weight. Fetching a bare sha needs it to be
# reachable from a ref, which is true of anything crates.io was published from.
checkout() {
  local name=$1 url=$2 rev=$3 dest="$checkouts/$1"
  git init -q "$dest"
  # Keep git from detaching background maintenance into a checkout the job is
  # about to hammer with a build.
  git -C "$dest" config gc.auto 0
  git -C "$dest" config maintenance.auto false
  git -C "$dest" remote add origin "$url"
  git -C "$dest" fetch -q --depth 1 origin "$rev" \
    || fail "$name: cannot fetch $rev from $url — was it force-pushed away?"
  git -C "$dest" -c advice.detachedHead=false checkout -q FETCH_HEAD
  git -C "$dest" config user.name "patched servo build"
  git -C "$dest" config user.email "ci@invalid"
}

count_files() {
  local n=0 f
  for f in "$patches"/$1; do [ -e "$f" ] && n=$((n + 1)); done
  printf '%s' "$n"
}

# --- the override block -----------------------------------------------------
#
# Lifted out of the README instead of restated here. The paths in it are
# relative to the workspace root, which is exactly where it is appended, so
# the block goes in verbatim.

overrides=$(awk '
  /^### 3\./             { section = 1; next }
  section && /^```toml$/ { block = 1; next }
  block && /^```/        { exit }
  block                  { print }
' "$repo_root/README.md")

[ -n "$overrides" ] \
  || fail "README.md: found no toml block under a '### 3.' heading.
The override block that section publishes is what this builds; if it moved,
point this script at wherever it went."
grep -q '^\[patch\.crates-io\]' <<< "$overrides" \
  || fail "README.md: the block under '### 3.' does not open with [patch.crates-io]:

$overrides"
grep -q '^servo = ' <<< "$overrides" \
  || fail "README.md: the block under '### 3.' overrides no servo:

$overrides"

# Every `path = "../<dir>/..."` in the block is a checkout this has to build.
# Anything overridden by git rev is cargo's problem, not ours.
needed=$(grep -o 'path[[:space:]]*=[[:space:]]*"\.\./[^/"]*' <<< "$overrides" \
  | sed 's|.*\.\./||' | sort -u)
[ -n "$needed" ] \
  || fail "README.md: the block under '### 3.' overrides nothing by path, so
there is no patched tree to build:

$overrides"

# Everything that can be known without touching the network, before the
# first multi-gigabyte clone.
for name in $needed; do
  recipe "$name" > /dev/null 2>&1 \
    || fail "README.md's override block wants ../$name, which this script has
no recipe for. Add it to recipe() — upstream, the locked crate that fixes its
revision, its patch glob, and whether the patches are format-patch output."
  if [ -e "$checkouts/$name" ]; then
    fail "$checkouts/$name already exists — refusing to clobber it.
Remove it, or run this where the repository has no such sibling."
  fi
  read -r _ _ glob _ <<< "$(recipe "$name")"
  # A path override exists to put patched code in the graph. Overriding a
  # pristine checkout builds something indistinguishable from stock while
  # reporting success, which is the one outcome this job must not have.
  [ "$(count_files "$glob")" -gt 0 ] \
    || fail "README.md overrides $name by path, but no patches match $glob"
done

# --- check out and patch ----------------------------------------------------
#
# The same commands the README's step 2 gives, for the same reason: testing
# anything else would leave the documented recipe unverified. Failures are
# reported tersely — check-patch-series.sh is the job that exists to explain
# them, and it runs on every pull request rather than only on this one's
# triggers.

echo "Resolving the revisions behind the versions in Cargo.lock"
echo

for name in $needed; do
  read -r url crate glob mode <<< "$(recipe "$name")"
  version=$(locked_version "$crate")
  rev=$(crate_revision "$crate" "$version")
  printf '  %-28s %-10s %s\n' "$name" "$version" "$rev"

  checkout "$name" "$url" "$rev"

  expected=$(count_files "$glob")  # non-zero, checked above

  case $mode in
    am)
      before=$(git -C "$checkouts/$name" rev-parse HEAD)
      git -C "$checkouts/$name" -c advice.mergeConflict=false am "$patches"/$glob || {
        git -C "$checkouts/$name" am --abort > /dev/null 2>&1 || true
        fail "$name: the series no longer applies to $rev"
      }
      applied=$(git -C "$checkouts/$name" rev-list --count "$before"..HEAD)
      [ "$applied" -eq "$expected" ] \
        || fail "$name: expected $expected commits, got $applied"
      ;;
    apply)
      applied=0
      for p in "$patches"/$glob; do
        git -C "$checkouts/$name" apply --3way "$p" \
          || fail "$name: $(basename "$p") no longer applies to $rev"
        applied=$((applied + 1))
      done
      ;;
  esac
  echo "  $name: $applied/$expected applied"
  echo
done

# --- wire up the overrides --------------------------------------------------

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
