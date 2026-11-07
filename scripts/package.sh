#!/usr/bin/env bash
# Build a release BINARY tarball of system-builder for GitHub releases.
#
# Usage:
#   scripts/package.sh [--allow-dirty] [--upload]
#
# Runs the gates (fmt, clippy with warnings denied, tests), builds
# optimized release binaries (gb-server, gb-client) from a pristine
# HEAD tree, checks both report the Cargo.toml version, and writes
#   dist/system-builder-<version>-<platform>.tar.gz (+ .sha256)
# e.g. system-builder-0.0.1-linux-x86_64.tar.gz. The tarball also
# carries README.md, config.example.toml, and the systemd unit so the
# server can be installed by hand from that one archive.
#
# The release version is the plain Cargo.toml workspace version (e.g.
# 0.0.1). Gentoo installs track git via a 9999 live ebuild on the
# overlay, so no source tarball is built here.
#
# With --upload, the script additionally creates tag v<version> (unless
# it exists), pushes it to the github.com remote, and creates (or
# reuses) the GitHub release with the tarball attached, via the `gh`
# CLI. Without it, tagging, pushing, and uploading are left to you;
# the script prints the exact commands at the end.
set -euo pipefail

ALLOW_DIRTY=0
UPLOAD=0
for arg in "$@"; do
    case "$arg" in
        --allow-dirty) ALLOW_DIRTY=1 ;;
        --upload) UPLOAD=1 ;;
        *) echo "package: unknown flag $arg" >&2; exit 1 ;;
    esac
done

cd "$(dirname "$0")/.."

VERSION="$(awk '/^\[workspace\.package\]/{p=1} p && /^version *= *"/{gsub(/"/, "", $3); print $3; exit}' Cargo.toml)"
if [[ -z "$VERSION" ]]; then
    echo "package: cannot parse version from Cargo.toml" >&2
    exit 1
fi

# Fail fast on upload prerequisites before the long build.
if [[ "$UPLOAD" -eq 1 ]]; then
    if ! command -v gh >/dev/null 2>&1; then
        echo "package: --upload needs the gh CLI (not found)" >&2
        exit 1
    fi
    if ! gh auth status >/dev/null 2>&1; then
        echo "package: gh is not authenticated (run: gh auth login)" >&2
        exit 1
    fi
    GH_REMOTE="$(git remote -v | awk '/github\.com[:\/]/{print $1; exit}')"
    if [[ -z "$GH_REMOTE" ]]; then
        echo "package: no github.com remote found" >&2
        exit 1
    fi
    GH_REPO="$(awk -F'"' '/^repository *= *".*github\.com\//{url=$2; sub(/\.git$/,"",url); sub(/.*github\.com\//,"",url); print url; exit}' Cargo.toml)"
    if [[ -z "$GH_REPO" ]]; then
        echo "package: cannot parse GitHub repo from Cargo.toml" >&2
        exit 1
    fi
fi

# Canonical triple -> short platform name for asset filenames.
# `unknown` is just the conventional vendor slot for Linux triples;
# the short form drops it. Unmapped triples pass through unchanged.
short_target() {
    case "$1" in
        x86_64-unknown-linux-gnu) echo linux-x86_64 ;;
        aarch64-unknown-linux-gnu) echo linux-aarch64 ;;
        x86_64-apple-darwin) echo macos-x86_64 ;;
        aarch64-apple-darwin) echo macos-aarch64 ;;
        x86_64-pc-windows-msvc) echo windows-x86_64 ;;
        *) echo "$1" ;;
    esac
}

if [[ "$ALLOW_DIRTY" -eq 0 ]] && { ! git diff --quiet || ! git diff --cached --quiet || [[ -n "$(git ls-files --others --exclude-standard)" ]]; }; then
    echo "package: working tree is dirty; commit first or pass --allow-dirty" >&2
    exit 1
fi
if [[ "$ALLOW_DIRTY" -eq 1 ]]; then
    echo "package: warning: archiving HEAD; uncommitted changes are excluded" >&2
fi

echo "package: system-builder $VERSION binary tarball"
echo "package: gates (fmt, clippy, tests)..."
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --quiet

echo "package: archiving HEAD..."
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/system-builder-$VERSION"
git archive HEAD | tar -x -C "$STAGE/system-builder-$VERSION"

# Optimized binaries from the staged tree, so they always match the
# archived sources exactly (--locked forbids lockfile drift).
PLATFORM="$(short_target "$(rustc -vV | awk '/^host:/{print $2}')")"
echo "package: building release binaries ($PLATFORM)..."
(
    cd "$STAGE/system-builder-$VERSION"
    cargo build --release --locked
)
BINNAME="system-builder-$VERSION-$PLATFORM"
mkdir -p "$STAGE/$BINNAME"
cp "$STAGE/system-builder-$VERSION/target/release/gb-server" \
    "$STAGE/system-builder-$VERSION/target/release/gb-client" \
    "$STAGE/system-builder-$VERSION/README.md" \
    "$STAGE/system-builder-$VERSION/config.example.toml" \
    "$STAGE/$BINNAME/"
cp "$STAGE/system-builder-$VERSION/packaging/system-builder.service" \
    "$STAGE/$BINNAME/system-builder.service"
if [[ -f "$STAGE/system-builder-$VERSION/LICENSE" ]]; then
    cp "$STAGE/system-builder-$VERSION/LICENSE" "$STAGE/$BINNAME/"
fi

# Stable tarball: sorted entries, root ownership, HEAD commit mtime.
EPOCH="$(git log -1 --format=%ct HEAD)"
mkdir -p dist
BINTARBALL="dist/$BINNAME.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$EPOCH" \
    -czf "$BINTARBALL" -C "$STAGE" "$BINNAME"
sha256sum "$BINTARBALL" | awk '{print $1}' > "$BINTARBALL.sha256"

# Both binaries report the Cargo.toml workspace version.
check_binaries() {
    local dir="$1" want="$2"
    local got bin
    for bin in gb-server gb-client; do
        got="$("$dir/$bin" --version 2>/dev/null || true)"
        if [[ "$got" != "$bin $want" ]]; then
            echo "package: binary verify failed: got '$got', want '$bin $want'" >&2
            return 1
        fi
    done
}
check_binaries "$STAGE/$BINNAME" "$VERSION"

TAG="v$VERSION"
if [[ "$UPLOAD" -eq 1 ]]; then
    if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
        echo "package: tag $TAG already exists, reusing"
    else
        echo "package: tagging $TAG"
        git tag "$TAG"
    fi
    echo "package: pushing $TAG to $GH_REMOTE"
    git push "$GH_REMOTE" "$TAG"
    if gh release view "$TAG" -R "$GH_REPO" >/dev/null 2>&1; then
        echo "package: release $TAG exists, uploading assets"
        gh release upload "$TAG" -R "$GH_REPO" --clobber \
            "$BINTARBALL" "$BINTARBALL.sha256"
    else
        echo "package: creating release $TAG"
        gh release create "$TAG" -R "$GH_REPO" --title "$TAG" --generate-notes \
            "$BINTARBALL" "$BINTARBALL.sha256"
    fi
fi

echo "package: ok: $BINTARBALL ($(du -h "$BINTARBALL" | cut -f1))"
echo "package: sha256: $(cat "$BINTARBALL.sha256")"
if [[ "$UPLOAD" -eq 0 ]]; then
    if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
        echo "package: note: tag $TAG already exists; bump Cargo.toml for a new release" >&2
    else
        echo "package: next (or rerun with --upload):"
        echo "  git tag $TAG"
        echo "  git push github $TAG   # and/or: git push origin $TAG"
        echo "  gh release create $TAG --generate-notes $BINTARBALL $BINTARBALL.sha256"
    fi
fi
