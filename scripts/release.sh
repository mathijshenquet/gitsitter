#!/bin/sh
# Release helper: validate, tag, and push a release from Cargo.toml version.
#
# Usage: ./scripts/release.sh [--dry-run]
set -eu

DRY_RUN=false
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=true ;;
    *) echo "Unknown arg: $arg"; exit 1 ;;
  esac
done

bold() { printf '\033[1m%s\033[0m' "$1"; }
info() { printf '  %s\n' "$@"; }
warn() { printf '  \033[33mwarn:\033[0m %s\n' "$@"; }
error() { printf '  \033[31merror:\033[0m %s\n' "$@"; exit 1; }

# 1. Extract version from Cargo.toml
VERSION=$(cargo pkgid | sed 's/.*@//')
TAG="v${VERSION}"

printf '\n'
info "$(bold "Version"): ${VERSION}"
info "$(bold "Tag"):     ${TAG}"
printf '\n'

# 2. Check working directory is clean
if [ -n "$(git status --porcelain)" ]; then
  error "Working directory is not clean. Commit or stash your changes first."
fi

# 3. Check we're on main
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" != "main" ]; then
  warn "You are on branch '${BRANCH}', not 'main'."
  printf '  Continue anyway? [y/N] '
  read -r answer < /dev/tty || answer="n"
  case "$answer" in
    [yY]*) ;;
    *) echo "  Aborted."; exit 1 ;;
  esac
fi

# 4. Check tag doesn't already exist
if git rev-parse "$TAG" >/dev/null 2>&1; then
  error "Tag ${TAG} already exists. Bump the version in Cargo.toml first."
fi

# 5. Run tests
info "Running tests..."
cargo test --quiet 2>&1
info "Tests passed."
printf '\n'

# 6. Create and push tag
if [ "$DRY_RUN" = true ]; then
  info "[dry-run] Would create tag ${TAG} and push to origin"
else
  info "Creating tag ${TAG}..."
  git tag -a "$TAG" -m "Release ${TAG}"

  info "Pushing tag to origin..."
  git push origin "$TAG"

  printf '\n'
  info "Done! Tag ${TAG} pushed."
  info "GitHub Actions will build and create a draft release."
  info "Go to https://github.com/mathijshenquet/gitsitter/releases to review and publish."
fi
