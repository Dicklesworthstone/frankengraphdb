#!/usr/bin/env bash
# =============================================================================
# install.sh — place the landing-lease pre-commit hook (fgdb-eesn)
# =============================================================================
# Installs scripts/git_hooks/pre-commit.sh as .git/hooks/pre-commit.
#
# WHY A COPY AND NOT core.hooksPath. core.hooksPath requires the hook be named
# exactly `pre-commit`; a tracked extensionless file lands unclaimed in
# check.sh's file-coverage closure and turns check.sh red for every pane. A copy
# into .git/hooks (untracked, and SHARED by all 29 linked worktrees, because
# hooks live in the common git dir) gets the enforcement everywhere from one
# install, with no tracked file that nothing claims.
#
# WHY IT REFUSES TO CLOBBER. An existing pre-commit hook may be another pane's or
# the owner's. This never overwrites one it did not write; it reports and exits
# non-zero so a human decides. It deletes nothing.
#
#   install.sh            report what is installed and what would change
#   install.sh --install  do it
#   install.sh --status   report only
# =============================================================================

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SRC="$ROOT/scripts/git_hooks/pre-commit.sh"
MARKER="fgdb-eesn"

GITDIR="$(git -C "$ROOT" rev-parse --git-common-dir 2>/dev/null)"
case "$GITDIR" in
  /*) ;;
  "") echo "ERROR: not a git repository: $ROOT" >&2; exit 2 ;;
  *)  GITDIR="$ROOT/$GITDIR" ;;
esac
DEST="$GITDIR/hooks/pre-commit"

mode="${1:---status}"

echo "source : $SRC"
echo "dest   : $DEST"
if [ -n "$(git -C "$ROOT" config --get core.hooksPath 2>/dev/null)" ]; then
  echo "WARNING: core.hooksPath is set to '$(git -C "$ROOT" config --get core.hooksPath)'."
  echo "         git will use THAT directory, and this install will have no effect."
fi

if [ ! -r "$SRC" ]; then
  echo "ERROR: hook source is missing or unreadable" >&2
  exit 2
fi

if [ -e "$DEST" ]; then
  if grep -q "$MARKER" "$DEST" 2>/dev/null; then
    if cmp -s "$SRC" "$DEST"; then
      echo "status : INSTALLED and current"
      exit 0
    fi
    echo "status : INSTALLED but STALE (differs from source)"
    [ "$mode" = "--install" ] || { echo "run with --install to refresh"; exit 0; }
  else
    echo "status : A DIFFERENT pre-commit hook is installed (no $MARKER marker)."
    echo "         Refusing to overwrite it — it is not ours. Nothing was changed."
    echo "         Inspect it, and merge by hand if both are wanted."
    exit 1
  fi
else
  echo "status : NOT INSTALLED"
  [ "$mode" = "--install" ] || { echo "run with --install to install"; exit 0; }
fi

mkdir -p "$GITDIR/hooks" || { echo "ERROR: cannot create $GITDIR/hooks" >&2; exit 2; }
cp "$SRC" "$DEST" || { echo "ERROR: copy failed" >&2; exit 2; }
chmod +x "$DEST" || { echo "ERROR: chmod failed" >&2; exit 2; }
echo "status : INSTALLED (shared by every linked worktree of this repository)"
