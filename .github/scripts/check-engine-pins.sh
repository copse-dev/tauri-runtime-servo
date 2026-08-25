#!/usr/bin/env bash
#
# Checks that the `[patch.crates-io]` overrides the README publishes still
# resolve — the contract its "Using a patched Servo" section promises
# consumers.
#
# The patched engine lives as `tauri-runtime-patches` branches on this
# organisation's forks, so there is nothing here to apply and nothing to
# compile. What can rot instead is a pin, in two ways, and cargo reports both
# at build time rather than in the manifest:
#
#   * the rev is gone      — the branch was rebased or force-pushed away.
#   * the rev no longer    — cargo accepts a [patch] entry only if the
#     satisfies the lock     replacement's own version satisfies the
#                            requirement it replaces. Publish a new release,
#                            let Cargo.lock move, and a fork left behind stops
#                            resolving.
#
# Both are checked against the versions in Cargo.lock, so nothing is pinned by
# hand here: when the `servo` requirement moves, this check moves with it, and
# a Dependabot bump that outruns the forks arrives as a red pull request. That
# is the intended signal — rebase the branches, move the revs, and it goes
# green.
#
# Read over the GitHub API rather than by cloning. A shallow clone of servo is
# well over a gigabyte, which is not a per-pull-request cost; eleven API reads
# take seconds. The cost is that this only understands github.com URLs, which
# it says plainly rather than skipping.

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

fail() { printf '\n%s\n' "$*" >&2; exit 1; }

# Where each overridden crate's manifest lives inside its fork. Deliberately
# explicit: these were the `path = "../stylo/<dir>"` values the README used to
# publish, and if a crate ever moves inside its repository a human should
# notice here rather than have CI quietly check the wrong manifest.
crate_dir() {
  case $1 in
    servo)                   echo "components/servo" ;;
    content-security-policy) echo "." ;;
    selectors)               echo "selectors" ;;
    servo_arc)               echo "servo_arc" ;;
    stylo)                   echo "style" ;;
    stylo_atoms)             echo "stylo_atoms" ;;
    stylo_dom)               echo "stylo_dom" ;;
    stylo_malloc_size_of)    echo "malloc_size_of" ;;
    stylo_static_prefs)      echo "stylo_static_prefs" ;;
    stylo_traits)            echo "style_traits" ;;
    *)                       return 1 ;;
  esac
}

# Every version Cargo.lock resolves for a crate, newline-separated.
#
# Deliberately not "the one version": the graph legitimately carries two
# `selectors`, 0.36.1 for one unrelated dependent and 0.40.0 for the stylo
# crates, and cargo replaces whichever entries the patched version satisfies
# while leaving the rest alone. So a pin is correct when it matches *a* locked
# version, and broken when it matches none — at which point nothing in the
# graph would use it and the override is silently inert.
locked_versions() {
  local crate=$1 found
  found=$(awk -v want="name = \"$crate\"" '
    $0 == want { getline; if ($1 == "version") { gsub(/"/, "", $3); print $3 } }
  ' "$repo_root/Cargo.lock")
  [ -n "$found" ] || fail "Cargo.lock: no '$crate' entry — is it still in the graph?"
  printf '%s' "$found"
}

# One file out of a repository at one revision, over the API.
fetch_file() {
  local slug=$1 path=$2 rev=$3
  gh api "repos/$slug/contents/${path#./}?ref=$rev" \
    --header 'Accept: application/vnd.github.raw' 2> /dev/null
}

# A crate's own version at a revision. `version.workspace = true` sends us to
# the workspace root, which is how both servo and stylo declare most of theirs.
crate_version_at() {
  local slug=$1 dir=$2 rev=$3 manifest version
  manifest=$(fetch_file "$slug" "${dir%/}/Cargo.toml" "$rev") || return 1
  [ -n "$manifest" ] || return 1
  version=$(awk '/^\[/ { p = ($0 == "[package]") } p && /^version/ { print; exit }' <<< "$manifest")
  if [[ $version == *workspace* ]]; then
    manifest=$(fetch_file "$slug" "Cargo.toml" "$rev") || return 1
    version=$(awk '/^\[/ { p = ($0 == "[workspace.package]") } p && /^version/ { print; exit }' <<< "$manifest")
  fi
  sed -E 's/.*"([^"]+)".*/\1/' <<< "$version"
}

# --- the override block -----------------------------------------------------
#
# Lifted out of the README rather than restated, so what is checked is the
# block a consumer is handed. Same extraction prepare-patched-servo.sh uses.

overrides=$(awk '
  /^### 1\./             { section = 1; next }
  section && /^```toml$/ { block = 1; next }
  block && /^```/        { exit }
  block                  { print }
' "$repo_root/README.md")

grep -q '^\[patch\.crates-io\]' <<< "$overrides" \
  || fail "README.md: found no [patch.crates-io] block under a '### 1.' heading.
The override block that section publishes is what this checks; if it moved,
point this script at wherever it went."
grep -q '^servo = ' <<< "$overrides" \
  || fail "README.md: the block under '### 1.' overrides no servo:

$overrides"

status=0
checked=0

while IFS= read -r line; do
  case $line in ''|'#'*|'[patch'*) continue ;; esac
  crate=${line%%=*}; crate=${crate// /}

  # A path override cannot be checked from here and has no business in the
  # published recipe: it only resolves on the machine that has the checkout.
  if grep -q 'path[[:space:]]*=' <<< "$line"; then
    echo "$crate: FAILED — the published override is a path, which resolves only locally" >&2
    status=1; continue
  fi

  url=$(sed -n 's/.*git[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' <<< "$line")
  rev=$(sed -n 's/.*rev[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' <<< "$line")
  [ -n "$url" ] || fail "README.md: cannot read a git source out of:
  $line"

  # A git override with no rev pins a branch, and these branches are rebased —
  # so what the build resolves would change under `cargo update`.
  [ -n "$rev" ] || fail "README.md: the $crate override names a git source with no rev:
  $line
A branch pin changes meaning whenever the fork is rebased. Pin the rev."

  case $url in
    https://github.com/*) slug=${url#https://github.com/}; slug=${slug%.git} ;;
    *) fail "$crate: $url is not a github.com URL, and reading it over the API
is the only thing this script knows how to do. Teach it, or clone instead." ;;
  esac

  dir=$(crate_dir "$crate") || fail "no directory known for '$crate' inside its fork.
Add it to crate_dir() — this script has to know which manifest carries its version."

  locked=$(locked_versions "$crate")
  if ! pinned=$(crate_version_at "$slug" "$dir" "$rev"); then
    echo "$crate: FAILED — cannot read ${dir%/}/Cargo.toml at ${rev:0:8} in $slug" >&2
    echo "  was the branch rebased or force-pushed away?" >&2
    status=1; continue
  fi

  checked=$((checked + 1))
  locked_list=$(tr '\n' ' ' <<< "$locked"); locked_list=${locked_list% }
  printf '  %-24s pinned %-8s at %-9s lock has %s\n' \
    "$crate" "$pinned" "${rev:0:8}" "$locked_list"
  if ! grep -qxF "$pinned" <<< "$locked"; then
    echo "    Cargo.lock resolves $locked_list, so nothing would use this pin" >&2
    status=1
  fi
done <<< "$overrides"

[ "$checked" -gt 0 ] || fail "checked nothing — the override block parsed to no entries"

echo
if [ "$status" -eq 0 ]; then
  echo "All $checked engine pins resolve to the versions Cargo.lock expects."
else
  cat >&2 <<'EOF'

An engine pin no longer resolves. If a dependency bump brought you here, the
fork branch has to move onto the new revision before that bump can land:

  1. clone the fork and check out tauri-runtime-patches
  2. rebase it onto the revision behind the new release — the revision behind
     any published version is in the crate itself:
       curl -sL https://static.crates.io/crates/servo/servo-X.Y.Z.crate \
         | tar xzO servo-X.Y.Z/.cargo_vcs_info.json
  3. push, and update the rev in README.md's "Using a patched Servo"
EOF
fi
exit "$status"
