#!/usr/bin/env bash
#
# Checks that servo-patches/ still applies to the revisions behind the crate
# versions this repository resolves — the contract the README's "Using a
# patched Servo" section promises consumers.
#
# No revision is pinned here. Every published crate records the commit it was
# cut from in .cargo_vcs_info.json, so the versions in Cargo.lock decide what
# gets checked: when the `servo` requirement moves, this check moves with it,
# and stylo follows because it is resolved transitively through servo.
#
# The commands mirror the README exactly — plain `git am` for the servo and
# csp series, `git apply --3way` for stylo, whose files are plain diffs behind
# a prose preamble rather than format-patch output. Testing anything else
# would leave the documented recipe unverified.
#
# Applying is checked by outcome, not just exit status: `git apply --reverse
# --check` afterwards proves each stylo patch's content is actually in the
# tree, which a silently sloppy three-way merge would not satisfy.

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
patches="$repo_root/servo-patches"

work=$(mktemp -d)
# Best-effort: a trap that fails becomes the script's exit status, and a
# temp directory that outlives the job is not worth reporting a failure over.
cleanup() { rm -rf "$work" 2> /dev/null || true; }
trap cleanup EXIT

# The upstream each group targets. Deliberately explicit rather than read out
# of the crate metadata: if one of these ever moves, a human should notice
# instead of CI quietly following it somewhere else.
servo_upstream=https://github.com/servo/servo
stylo_upstream=https://github.com/servo/stylo
csp_upstream=https://github.com/rust-ammonia/rust-content-security-policy

fail() { printf '\n%s\n' "$*" >&2; exit 1; }

# The single version Cargo.lock resolves for a crate. More than one entry
# means the graph carries duplicate copies, which the override recipe cannot
# express — worth failing on rather than guessing which to check.
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

# Shallow: the series only ever touches the one tree, so history is dead
# weight. Fetching a bare sha needs it to be reachable from a ref, which is
# true of anything crates.io was published from.
checkout() {
  local name=$1 url=$2 rev=$3
  git init -q "$work/$name"
  # Keep git from detaching background maintenance into a tree we are about
  # to delete — that races the cleanup and leaves the directory non-empty.
  git -C "$work/$name" config gc.auto 0
  git -C "$work/$name" config maintenance.auto false
  git -C "$work/$name" remote add origin "$url"
  # Returns non-zero rather than exiting: one caller wants to report the
  # failure in its own terms.
  git -C "$work/$name" fetch -q --depth 1 origin "$rev" || return 1
  git -C "$work/$name" -c advice.detachedHead=false checkout -q FETCH_HEAD
  git -C "$work/$name" config user.name "patch series check"
  git -C "$work/$name" config user.email "ci@invalid"
}

count_files() {
  local n=0 f
  for f in "$patches"/$1; do [ -e "$f" ] && n=$((n + 1)); done
  printf '%s' "$n"
}

echo "Resolving the revisions behind the versions in Cargo.lock"
echo

servo_version=$(locked_version servo)
stylo_version=$(locked_version stylo)
csp_version=$(locked_version content-security-policy)

servo_rev=$(crate_revision servo "$servo_version")
stylo_rev=$(crate_revision stylo "$stylo_version")

printf '  %-26s %-10s %s\n' servo "$servo_version" "$servo_rev"
printf '  %-26s %-10s %s\n' stylo "$stylo_version" "$stylo_rev"
echo

checkout servo "$servo_upstream" "$servo_rev" \
  || fail "servo: cannot fetch $servo_rev from $servo_upstream — was it force-pushed away?"
checkout stylo "$stylo_upstream" "$stylo_rev" \
  || fail "stylo: cannot fetch $stylo_rev from $stylo_upstream — was it force-pushed away?"

status=0

# --- servo: format-patch output, applied with git am ------------------------

apply_am_group() {
  local name=$1 glob=$2 expected before applied
  expected=$(count_files "$glob")
  [ "$expected" -gt 0 ] || fail "no patches matched $glob"

  before=$(git -C "$work/$name" rev-parse HEAD)
  # Separate streams: git block-buffers stdout when redirected but not stderr,
  # so merging them puts the error above the "Applying:" line that caused it.
  if git -C "$work/$name" -c advice.mergeConflict=false am "$patches"/$glob \
      > "$work/$name-am.out" 2> "$work/$name-am.err"; then
    applied=$(git -C "$work/$name" rev-list --count "$before"..HEAD)
    echo "$name: $applied/$expected applied"
    [ "$applied" -eq "$expected" ] \
      || { echo "  expected $expected commits, got $applied" >&2; status=1; }
  else
    echo "$name: FAILED — the series no longer applies" >&2
    sed 's/^/  /' "$work/$name-am.out" "$work/$name-am.err" >&2
    git -C "$work/$name" am --abort > /dev/null 2>&1 || true
    status=1
  fi
}

apply_am_group servo "0*.patch"

# --- content-security-policy: a fork pin, or patch files here ---------------
#
# These two are alternatives, and the README decides which. When the override
# names a git rev, the crate's fixes live as commits on a fork and there is
# nothing to apply — what can rot instead is the pin itself, so check that.
# When it does not, they are .patch files here like the rest of the series.
# Checking whichever the README documents keeps this honest across the move
# rather than pinning the check to one arrangement.

csp_override=$(grep -E '^content-security-policy[[:space:]]*=' \
  "$repo_root/README.md" || true)
csp_override_count=$(printf '%s' "$csp_override" | grep -c . || true)
[ "$csp_override_count" -le 1 ] \
  || fail "README.md: $csp_override_count content-security-policy override lines, expected at most one"

csp_url=$(printf '%s' "$csp_override" | sed -n 's/.*git[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p')
csp_pin=$(printf '%s' "$csp_override" | sed -n 's/.*rev[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p')

# A git override with no rev pins a branch, and that branch is rebased — so
# what a source-expression matcher does would change under `cargo update`.
[ -z "$csp_url" ] || [ -n "$csp_pin" ] \
  || fail "README.md: the content-security-policy override names a git source with no rev:
  $csp_override
A branch pin changes meaning whenever the fork is rebased. Pin the rev."

if [ -n "$csp_url" ] && [ -n "$csp_pin" ]; then
  if checkout csp "$csp_url" "$csp_pin" 2> "$work/csp-fetch.err"; then
    # Cargo accepts a [patch] entry only if the replacement's own version
    # satisfies the requirement it replaces. Publish a new crate version, let
    # the lockfile move, and a fork left behind stops resolving — silently in
    # the manifest, loudly at build time. That is the failure worth catching.
    pinned=$(awk '/^\[package\]/ { p = 1; next }
                  p && /^\[/       { exit }
                  p && /^version/  { gsub(/"/, "", $3); print $3; exit }' \
             "$work/csp/Cargo.toml")
    echo "content-security-policy: pinned to ${csp_pin:0:8} on ${csp_url##*/} (crate $pinned)"
    if [ "$pinned" != "$csp_version" ]; then
      echo "  Cargo.lock resolves $csp_version, so this pin no longer satisfies it" >&2
      status=1
    fi
  else
    echo "content-security-policy: FAILED — cannot fetch $csp_pin from $csp_url" >&2
    sed 's/^/  /' "$work/csp-fetch.err" >&2
    status=1
  fi
  # The fork branch is rebased whenever the crate's master moves — which is
  # why the override pins a rev rather than a branch — so it is deliberately
  # not checked against upstream's current tip.
else
  csp_rev=$(crate_revision content-security-policy "$csp_version")
  echo "content-security-policy: $csp_version at ${csp_rev:0:8}"
  checkout csp "$csp_upstream" "$csp_rev" \
    || fail "csp: cannot fetch $csp_rev from $csp_upstream — was it force-pushed away?"
  apply_am_group csp "csp-*.patch"
fi

# --- stylo: plain diffs, applied with git apply -----------------------------

expected=$(count_files "stylo-*.patch")
[ "$expected" -gt 0 ] || fail "no patches matched stylo-*.patch"
applied=0
for p in "$patches"/stylo-*.patch; do
  if git -C "$work/stylo" apply --3way "$p" > "$work/stylo-apply.log" 2>&1; then
    applied=$((applied + 1))
  else
    echo "stylo: FAILED on $(basename "$p")" >&2
    sed 's/^/  /' "$work/stylo-apply.log" >&2
    status=1
    break
  fi
done
echo "stylo: $applied/$expected applied"

# A three-way merge can succeed while landing something other than the patch.
# Reversing each one proves its content really is in the tree.
if [ "$applied" -eq "$expected" ]; then
  for p in "$patches"/stylo-*.patch; do
    git -C "$work/stylo" apply --reverse --check "$p" > /dev/null 2>&1 \
      || { echo "stylo: $(basename "$p") applied but is not present in the tree" >&2; status=1; }
  done
fi

echo
if [ "$status" -eq 0 ]; then
  echo "The series applies to the revisions behind the pinned releases."
else
  cat >&2 <<'EOF'

The series no longer applies, or an override no longer resolves. If a
dependency bump brought you here, the patched sources have to move onto the
new revision before that bump can land:

  .patch files  rebase onto the revision printed above and regenerate with
                git format-patch --zero-commit --no-signature --full-index --numbered

  a fork pin    rebase the fork onto the new upstream, then update the rev
                in the override

Either way, update the revisions quoted in README.md's "Using a patched Servo".
EOF
fi
exit "$status"
