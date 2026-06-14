#!/usr/bin/env bash
# vmette release cutter — encodes the manual release playbook so a release is
# one reviewed command instead of a dozen error-prone steps.
#
# Pipeline:
#   preflight  →  version bump + CHANGELOG  →  gates  →  commit + tag
#              →  [confirm]  →  crates.io publish  →  push (fires release CI)
#
# Everything up to and including the commit + tag is LOCAL and reversible. The
# crates.io publish and the tag push are IRREVERSIBLE and outward-facing, so
# they sit behind an explicit confirmation (or --yes).
#
# Usage:
#   scripts/release.sh X.Y.Z            cut the release (prompts before publish)
#   scripts/release.sh X.Y.Z --dry-run  run every check + print the plan; change nothing
#   scripts/release.sh X.Y.Z --yes      skip the confirmation prompt (CI / unattended)
#
# Env equivalents: VERSION=X.Y.Z, DRY_RUN=1, RELEASE_YES=1.
#
# Mirrors `make publish` (same 7 libs, same dep order). The 3 binary crates
# (vmette-cli/-daemon/-mcp) are `publish = false` and ship via the GitHub
# release tarball that the v* tag push builds.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$HERE"

# The 7 published library crates, in dependency order (proto first, aggregator
# last). Keep in sync with the `publish` target in the Makefile.
LIBS=(
    vmette-proto
    vmette-assets
    vmette
    vmette-provider-oci
    vmette-provider-squashfs
    vmette-provider-tar
    vmette-providers
)

# ---- args ----------------------------------------------------------------
NEW="${VERSION:-}"
DRY_RUN="${DRY_RUN:-}"
YES="${RELEASE_YES:-}"
for a in "$@"; do
    case "$a" in
        --dry-run) DRY_RUN=1 ;;
        --yes|-y)  YES=1 ;;
        -h|--help) sed -n '2,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*)        echo "✗ unknown flag: $a" >&2; exit 2 ;;
        *)         NEW="$a" ;;
    esac
done

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
die()  { printf '\033[31m✗ %s\033[0m\n' "$*" >&2; exit 1; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m•\033[0m %s\n' "$*"; }

FAILED=0
# check "DESCRIPTION" CMD [ARGS...] — runs CMD itself (so `set -e` can't abort
# before the verdict is recorded). A real run aborts on failure (showing the
# command's captured output); --dry-run records it and keeps going so the whole
# plan is visible.
check() {
    local desc="$1"; shift
    local out rc
    out="$("$@" 2>&1)" && rc=0 || rc=$?
    if [[ $rc -eq 0 ]]; then
        ok "$desc"
    elif [[ -n "$DRY_RUN" ]]; then
        warn "WOULD BLOCK: $desc"
        FAILED=1
    else
        [[ -n "$out" ]] && printf '%s\n' "$out" >&2
        die "$desc"
    fi
}

[[ -n "$NEW" ]] || die "usage: scripts/release.sh X.Y.Z [--dry-run] [--yes]"
[[ "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "not a semver version: '$NEW'"

CUR="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
DATE="$(date +%F)"
TAG="v$NEW"

# Predicates for the preflight checks (so `check` runs a single command).
is_macos()       { [[ "$(uname -s)" == "Darwin" ]]; }
on_main()        { [[ "$(git branch --show-current)" == "main" ]]; }
tree_clean()     { git diff --quiet && git diff --cached --quiet; }
in_sync()        { [[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main 2>/dev/null || echo none)" ]]; }
version_newer()  { [[ "$NEW" != "$CUR" && "$(printf '%s\n%s\n' "$CUR" "$NEW" | sort -V | tail -1)" == "$NEW" ]]; }
tag_free()       { ! git rev-parse "$TAG" >/dev/null 2>&1 && [[ -z "$(git ls-remote --tags origin "$TAG" 2>/dev/null)" ]]; }
unreleased_body() { awk '/^## \[Unreleased\]/{f=1;next} f&&/^## \[/{exit} f' CHANGELOG.md | grep -vqE '^[[:space:]]*$'; }
have_creds()     { [[ -f "$HOME/.cargo/credentials.toml" || -n "${CARGO_REGISTRY_TOKEN:-}" ]]; }

bold "vmette release — $CUR → $NEW  ($TAG, $DATE)${DRY_RUN:+   [DRY RUN]}"
echo

# ---- preflight -----------------------------------------------------------
bold "preflight"
git fetch -q origin main 2>/dev/null || true
check "on macOS (publish runs a verify build)"       is_macos
check "on branch main"                               on_main
check "working tree clean"                           tree_clean
check "HEAD in sync with origin/main"                in_sync
check "$NEW is newer than current $CUR"              version_newer
check "tag $TAG is free (local + origin)"            tag_free
check "CHANGELOG [Unreleased] has content"           unreleased_body
check "crates.io credentials present"                have_creds
echo

# ---- gates ---------------------------------------------------------------
bold "gates"
check "cargo fmt --all --check"        cargo fmt --all --check
check "cargo clippy (-D warnings)"     cargo clippy --workspace --all-targets -- -D warnings
check "cargo test --workspace"         cargo test --workspace
echo

# ---- plan ----------------------------------------------------------------
bold "plan"
echo "  bump Cargo.toml: workspace version + ${#LIBS[@]} internal dep pins, $CUR → $NEW"
echo "  CHANGELOG: promote [Unreleased] → [$NEW] — $DATE"
echo "  commit 'release: $TAG' (Cargo.toml, Cargo.lock, CHANGELOG.md) + tag $TAG"
echo "  publish to crates.io (dep order): ${LIBS[*]}"
echo "  push origin main + $TAG → fires release.yml (tarball/GitHub Release)"
echo

if [[ -n "$DRY_RUN" ]]; then
    [[ "$FAILED" -eq 0 ]] || die "dry run: one or more checks WOULD BLOCK a real release (see above)"
    bold "dry run complete — all checks pass; nothing changed."
    exit 0
fi

# ---- prepare (local, reversible) -----------------------------------------
bold "prepare"
# Bump the workspace version (standalone line) and the internal dep pins (lines
# carrying path = "crates/…", so an external dep version equal to $CUR is never
# touched).
sed -i '' "s/^version = \"$CUR\"/version = \"$NEW\"/" Cargo.toml
sed -i '' "/path = \"crates\//s/version = \"$CUR\"/version = \"$NEW\"/" Cargo.toml
if grep -qE "^version = \"$CUR\"" Cargo.toml || grep -q "path = \"crates/.*version = \"$CUR\"" Cargo.toml; then
    die "version bump incomplete — stray $CUR remains in Cargo.toml (inspect/revert manually)"
fi
ok "bumped Cargo.toml to $NEW"

cargo update -w >/dev/null 2>&1
ok "refreshed Cargo.lock"

# Promote [Unreleased] → [NEW] — DATE (insert the dated heading right after the
# Unreleased heading; the existing entries fall under the new version).
awk -v ver="$NEW" -v d="$DATE" '
    !done && /^## \[Unreleased\]/ { print; print ""; print "## [" ver "] — " d; done=1; next }
    { print }
' CHANGELOG.md > CHANGELOG.md.tmp && mv CHANGELOG.md.tmp CHANGELOG.md
ok "promoted CHANGELOG [Unreleased] → [$NEW]"

cargo check --workspace >/dev/null 2>&1 || die "cargo check failed after bump (changes left in tree for inspection)"
ok "cargo check passes after bump"

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -q -m "release: $TAG" \
    -m "Lockstep bump of the 7 published library crates to $NEW; CHANGELOG [Unreleased] promoted to [$NEW]." \
    -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
git tag "$TAG"
ok "committed 'release: $TAG' and tagged $TAG"
echo

# ---- confirm the irreversible steps --------------------------------------
if [[ -z "$YES" ]]; then
    bold "About to PUBLISH to crates.io and PUSH $TAG — both irreversible."
    echo "  (decline to stop here; the commit + tag are local and undoable with:"
    echo "     git reset --hard HEAD~1 && git tag -d $TAG)"
    ans=""
    read -r -p "  Proceed? [y/N] " ans < /dev/tty || true
    [[ "$ans" == "y" || "$ans" == "Y" ]] || die "aborted before publish (commit + tag kept locally for review)"
fi

# ---- publish (IRREVERSIBLE) ----------------------------------------------
bold "publish to crates.io"
publish_one() {
    local c="$1" out
    if out="$(cargo publish -p "$c" 2>&1)"; then
        ok "published $c $NEW"
    elif grep -qiE "already (exists|uploaded)|already being published" <<<"$out"; then
        warn "$c $NEW already on crates.io — skipping"
    else
        printf '%s\n' "$out" >&2
        die "publishing $c failed (any crates published above are live; re-run to resume)"
    fi
}
for c in "${LIBS[@]}"; do publish_one "$c"; done
echo

# ---- push (fires release CI) ---------------------------------------------
bold "push"
git push -q origin main; ok "pushed main"
git push -q origin "$TAG"; ok "pushed $TAG → release.yml"
echo

bold "✓ released $TAG"
echo "  crates.io:      https://crates.io/crates/vmette/$NEW"
echo "  github release: https://github.com/chamuka-inc/vmette/releases/tag/$TAG (built by CI)"
echo "  watch CI:       gh run list --limit 3"
