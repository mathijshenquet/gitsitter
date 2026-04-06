#!/bin/sh
# Install script for gitsitter.
# Usage: curl -fsSL https://raw.githubusercontent.com/mathijshenquet/gitsitter/main/install.sh | sh
# Options:
#   --path <dir>       Override install directory (default: ~/.local/bin)
#   --version <ver>    Install specific version (default: latest)
#   -h, --help         Show help
set -eu

REPO="mathijshenquet/gitsitter"
BASE_URL="https://github.com/${REPO}/releases"
BIN_DIR="${HOME}/.local/bin"
VERSION=""

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

info() { printf '  %s\n' "$@"; }
warn() { printf '  \033[33mwarn:\033[0m %s\n' "$@" >&2; }
error() { printf '  \033[31merror:\033[0m %s\n' "$@" >&2; exit 1; }
bold() { printf '\033[1m%s\033[0m' "$1"; }

usage() {
  cat <<EOF
gitsitter installer

Usage: install.sh [OPTIONS]

Options:
  --path <dir>       Install directory [default: ${BIN_DIR}]
  --version <ver>    Install a specific version (e.g. v0.1.0) [default: latest]
  -h, --help         Show this help
EOF
}

need_cmd() {
  if ! command -v "$1" > /dev/null 2>&1; then
    error "need '$1' (command not found)"
  fi
}

detect_os() {
  case "$(uname -s)" in
    Linux*)  echo "unknown-linux-gnu" ;;
    Darwin*) echo "apple-darwin" ;;
    *)       error "unsupported OS: $(uname -s)" ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64)  echo "x86_64" ;;
    aarch64|arm64)  echo "aarch64" ;;
    *)              error "unsupported architecture: $(uname -m)" ;;
  esac
}

fetch_latest_version() {
  # Use GitHub API to get latest release tag
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
  elif command -v wget > /dev/null 2>&1; then
    wget -qO- "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
  else
    error "need 'curl' or 'wget'"
  fi
}

download() {
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget > /dev/null 2>&1; then
    wget -qO "$2" "$1"
  fi
}

# ---------------------------------------------------------------------------
# Parse args
# ---------------------------------------------------------------------------

while [ $# -gt 0 ]; do
  case "$1" in
    --path)    BIN_DIR="$2"; shift 2 ;;
    --path=*)  BIN_DIR="${1#*=}"; shift 1 ;;
    --version) VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift 1 ;;
    -h|--help) usage; exit 0 ;;
    *) error "unknown option: $1" ;;
  esac
done

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

need_cmd uname
need_cmd mktemp
need_cmd tar
need_cmd chmod

arch="$(detect_arch)"
os="$(detect_os)"
target="${arch}-${os}"

if [ -z "$VERSION" ]; then
  info "Fetching latest release..."
  VERSION="$(fetch_latest_version)"
  if [ -z "$VERSION" ]; then
    error "could not determine latest version"
  fi
fi

# Ensure version has v prefix for URL
case "$VERSION" in
  v*) ;;
  *)  VERSION="v${VERSION}" ;;
esac

archive="gitsitter-${target}.tar.gz"
url="${BASE_URL}/download/${VERSION}/${archive}"

printf '\n'
info "$(bold "Version"):   ${VERSION}"
info "$(bold "Target"):    ${target}"
info "$(bold "Directory"): ${BIN_DIR}"
printf '\n'

# Download and extract
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

info "Downloading ${url}..."
download "$url" "${tmpdir}/${archive}"
tar xzf "${tmpdir}/${archive}" -C "$tmpdir"

# Install binary
mkdir -p "$BIN_DIR"
mv "${tmpdir}/gitsitter" "${BIN_DIR}/gitsitter"
chmod +x "${BIN_DIR}/gitsitter"

info "Installed gitsitter to ${BIN_DIR}/gitsitter"
printf '\n'

# Run gitsitter install (shell hooks + daemon)
info "Running gitsitter install..."
"${BIN_DIR}/gitsitter" install
printf '\n'

# Check if BIN_DIR is in PATH
case ":${PATH}:" in
  *":${BIN_DIR}:"*)
    info "Done! Run 'gitsitter' to get started."
    ;;
  *)
    printf '  \033[33m%s is not in your PATH.\033[0m\n' "$BIN_DIR"
    printf '  Add it? [Y/n] '
    read -r answer < /dev/tty || answer="y"
    case "$answer" in
      [nN]*)
        printf '\n'
        info "You can invoke gitsitter as: ${BIN_DIR}/gitsitter"
        info "Or add ${BIN_DIR} to your PATH manually."
        ;;
      *)
        # Detect shell and append to rc file
        current_shell="$(basename "${SHELL:-sh}")"
        case "$current_shell" in
          zsh)  rc="${ZDOTDIR:-$HOME}/.zshrc" ;;
          bash) rc="$HOME/.bashrc" ;;
          fish) rc="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish" ;;
          *)    rc="$HOME/.profile" ;;
        esac
        case "$current_shell" in
          fish)
            echo "fish_add_path ${BIN_DIR}" >> "$rc"
            ;;
          *)
            echo "export PATH=\"${BIN_DIR}:\$PATH\"" >> "$rc"
            ;;
        esac
        # Also export for current session
        export PATH="${BIN_DIR}:${PATH}"
        info "Added ${BIN_DIR} to PATH in ${rc}"
        info "Done! Run 'gitsitter' to get started."
        ;;
    esac
    ;;
esac
