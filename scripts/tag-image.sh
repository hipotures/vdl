#!/usr/bin/env bash
set -euo pipefail

DRY_RUN=0
if [ "${1:-}" = "--dry-run" ]; then
    DRY_RUN=1
fi

if [ -n "$(git status --porcelain)" ]; then
    echo "Refusing to tag: working tree is dirty." >&2
    exit 1
fi

git fetch --tags origin

HEAD_SHA="$(git rev-parse HEAD)"
UPSTREAM_SHA="$(git rev-parse '@{u}')"
if [ "$HEAD_SHA" != "$UPSTREAM_SHA" ]; then
    echo "Refusing to tag: HEAD ($HEAD_SHA) is not pushed to upstream ($UPSTREAM_SHA)." >&2
    exit 1
fi

if git tag --points-at HEAD | grep -q '^vdl-[0-9]\+\.[0-9]\+\.[0-9]\+$'; then
    echo "Refusing to tag: HEAD is already tagged." >&2
    exit 1
fi

LATEST="$(git tag -l 'vdl-*' | grep -E '^vdl-[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -n 1 || true)"
if [ -z "$LATEST" ]; then
    NEXT="vdl-0.1.0"
else
    VER="${LATEST#vdl-}"
    IFS='.' read -r MAJOR MINOR PATCH <<< "$VER"
    NEXT="vdl-${MAJOR}.${MINOR}.$((PATCH + 1))"
fi

echo "Next release tag: $NEXT (HEAD=$HEAD_SHA)"
if [ "$DRY_RUN" = "1" ]; then
    echo "Dry run: no tag created."
    exit 0
fi

git tag -a "$NEXT" -m "Release $NEXT"
git push origin "$NEXT"
