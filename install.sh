#!/bin/sh
# Installer for wiki-mcp (Linux and macOS).
#
#   curl -fsSL https://codeload.github.com/filipov-dev/wiki-mcp/tar.gz/refs/heads/install | tar -xzO | sh
#   ... | tar -xzO | sh -s -- --version v0.1.0 --prefix "$HOME/.local"
#
# Downloads the release archive for this OS and CPU, verifies it against the
# release SHA256SUMS, and installs the `wiki-mcp` binary into
# ~/.local/bin (or <prefix>/bin, or --bin-dir). No sudo. Running it again
# updates the binary in place.
#
# The list of releases (archive links and checksums) is filled in by the
# release pipeline in the copy published on the `install` branch. A copy
# without the list (for example, from the main branch) downloads that branch
# to read it.
set -eu

REPO_URL="https://github.com/filipov-dev/wiki-mcp"
INSTALLER_URL="https://codeload.github.com/filipov-dev/wiki-mcp/tar.gz/refs/heads/install"
BIN_NAME="wiki-mcp"

# Release list: "<tag> <asset name> <sha256 or -> <url>" per line, newest
# release first. Filled in by the release pipeline; keep the markers.
# @@RELEASES_BEGIN@@
RELEASES='
v0.3.1 wiki-mcp-v0.3.1-macos-universal.tar.gz 70abe61dfb9e04d7f0408ceca58ae7c11374057c5457f63673c14359d707820b https://github.com/filipov-dev/wiki-mcp/releases/download/v0.3.1/wiki-mcp-v0.3.1-macos-universal.tar.gz
v0.3.1 wiki-mcp-v0.3.1-linux-x86_64.tar.gz 01597e2e17462037ac67363b74f519e7fb77991b0c9e5b881fb7585eb91a87df https://github.com/filipov-dev/wiki-mcp/releases/download/v0.3.1/wiki-mcp-v0.3.1-linux-x86_64.tar.gz
v0.3.1 wiki-mcp-v0.3.1-linux-aarch64.tar.gz 38eb97b9344147322287b147f2b4fe27d67c43d42c7e396565fc94a6b769fa6c https://github.com/filipov-dev/wiki-mcp/releases/download/v0.3.1/wiki-mcp-v0.3.1-linux-aarch64.tar.gz
v0.3.1 SHA256SUMS - https://github.com/filipov-dev/wiki-mcp/releases/download/v0.3.1/SHA256SUMS
v0.1.1 wiki-mcp-v0.1.1-macos-universal.tar.gz a734b35f96b28f0d49bcc4285aa6faec448545f2714a6f870160de17cb581269 https://github.com/filipov-dev/wiki-mcp/releases/download/v0.1.1/wiki-mcp-v0.1.1-macos-universal.tar.gz
v0.1.1 wiki-mcp-v0.1.1-linux-x86_64.tar.gz 19792abf1cd243e18421f3b5d40ba99d2e6b9b6f811a0f2652c2ec475256c5b1 https://github.com/filipov-dev/wiki-mcp/releases/download/v0.1.1/wiki-mcp-v0.1.1-linux-x86_64.tar.gz
v0.1.1 wiki-mcp-v0.1.1-linux-aarch64.tar.gz a2a3d5400cd97474063c134568c273e76da38a5ceddbae344d9b987a1b9ef1fe https://github.com/filipov-dev/wiki-mcp/releases/download/v0.1.1/wiki-mcp-v0.1.1-linux-aarch64.tar.gz
v0.1.1 SHA256SUMS - https://github.com/filipov-dev/wiki-mcp/releases/download/v0.1.1/SHA256SUMS
v0.1.0 wiki-mcp-v0.1.0-macos-universal.tar.gz 3fef9255d53ef41fb0a15c771a31e88418cb6bfb19b61e839bf341cc5ee41ca9 https://github.com/filipov-dev/wiki-mcp/releases/download/v0.1.0/wiki-mcp-v0.1.0-macos-universal.tar.gz
v0.1.0 wiki-mcp-v0.1.0-linux-x86_64.tar.gz ccf5e2433be9dbef8b2f3327e3c67cb16012b99e269b1dd5089d07b3e8e99a34 https://github.com/filipov-dev/wiki-mcp/releases/download/v0.1.0/wiki-mcp-v0.1.0-linux-x86_64.tar.gz
v0.1.0 wiki-mcp-v0.1.0-linux-aarch64.tar.gz e4da63147e46425d62213bfb303f3cf21f6f3f75df0d7474bb636372bfac46f4 https://github.com/filipov-dev/wiki-mcp/releases/download/v0.1.0/wiki-mcp-v0.1.0-linux-aarch64.tar.gz
v0.1.0 SHA256SUMS - https://github.com/filipov-dev/wiki-mcp/releases/download/v0.1.0/SHA256SUMS
'
# @@RELEASES_END@@

usage() {
  cat <<EOF
Install or update wiki-mcp.

Usage: install.sh [options]

Options:
  --version <vX.Y.Z>  Install this release instead of the latest one.
  --prefix <dir>      Install into <dir>/bin (default: \$HOME/.local).
  --bin-dir <dir>     Install into <dir> exactly (overrides --prefix).
  --force             Reinstall even if this version is already installed.
  --list              List the available releases and exit.
  -h, --help          Show this help.

With curl:
  curl -fsSL $INSTALLER_URL | tar -xzO | sh -s -- --version v0.1.0
EOF
}

say() { printf '%s\n' "$*" >&2; }
err() { say "error: $*"; exit 1; }

# Everything runs inside main(): a partially downloaded script does nothing,
# because sh reads the whole function before calling it.
main() {
  VERSION=""
  PREFIX=""
  BIN_DIR=""
  FORCE=0
  LIST=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --version)
        [ $# -ge 2 ] || err "--version needs a value, for example --version v0.1.0"
        VERSION="$2"; shift 2 ;;
      --version=*) VERSION="${1#*=}"; shift ;;
      --prefix)
        [ $# -ge 2 ] || err "--prefix needs a directory"
        PREFIX="$2"; shift 2 ;;
      --prefix=*) PREFIX="${1#*=}"; shift ;;
      --bin-dir)
        [ $# -ge 2 ] || err "--bin-dir needs a directory"
        BIN_DIR="$2"; shift 2 ;;
      --bin-dir=*) BIN_DIR="${1#*=}"; shift ;;
      --force) FORCE=1; shift ;;
      --list) LIST=1; shift ;;
      -h | --help) usage; exit 0 ;;
      *) usage >&2; err "unknown option: $1" ;;
    esac
  done

  case "$VERSION" in
    "") ;;
    v*) ;;
    *) VERSION="v$VERSION" ;;
  esac
  if [ -n "$VERSION" ] && ! printf '%s' "$VERSION" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'; then
    err "invalid version '$VERSION' (expected vX.Y.Z)"
  fi

  if [ -z "$BIN_DIR" ]; then
    if [ -n "$PREFIX" ]; then
      BIN_DIR="${PREFIX%/}/bin"
    else
      [ -n "${HOME:-}" ] || err "HOME is not set; pass --prefix or --bin-dir"
      BIN_DIR="$HOME/.local/bin"
    fi
  fi

  # --- Tools -------------------------------------------------------------------
  # Test hooks (used by the project's CI, not needed otherwise):
  # WIKI_MCP_INSTALL_TEST=1 allows file:// links in the release list,
  # WIKI_MCP_INSTALL_RELEASES=<file> replaces the release list.
  TEST_MODE="${WIKI_MCP_INSTALL_TEST:-0}"
  TEST_RELEASES="${WIKI_MCP_INSTALL_RELEASES:-}"

  if command -v curl >/dev/null 2>&1; then
    DOWNLOADER=curl
  elif command -v wget >/dev/null 2>&1; then
    DOWNLOADER=wget
  else
    err "curl or wget is required"
  fi
  # download <url> <file>: HTTPS only (redirects too, with curl).
  download() {
    case "$1" in
      https://*) ;;
      file://*)
        [ "$TEST_MODE" = 1 ] || return 1
        cp "${1#file://}" "$2"
        return
        ;;
      *) return 1 ;;
    esac
    if [ "$DOWNLOADER" = curl ]; then
      curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL --retry 3 \
        --connect-timeout 20 -o "$2" "$1"
    else
      wget -q -O "$2" "$1"
    fi
  }

  if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
  elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
  else
    err "sha256sum or shasum is required"
  fi
  command -v tar >/dev/null 2>&1 || err "tar is required"

  # --- Platform ----------------------------------------------------------------
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Linux)
      case "$arch" in
        x86_64 | amd64) PLATFORM="linux-x86_64" ;;
        aarch64 | arm64) PLATFORM="linux-aarch64" ;;
        *) err "unsupported CPU '$arch' on Linux (x86_64 and aarch64 are supported); build from source: $REPO_URL" ;;
      esac
      ;;
    Darwin)
      # One universal binary for Apple silicon and Intel.
      PLATFORM="macos-universal"
      ;;
    *) err "unsupported OS '$os' (Linux and macOS are supported); build from source: $REPO_URL" ;;
  esac

  WORK="$(mktemp -d 2>/dev/null || mktemp -d -t wiki-mcp)"
  trap 'rm -rf "$WORK"' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  # --- Release list --------------------------------------------------------------
  if [ -n "$TEST_RELEASES" ]; then
    cp "$TEST_RELEASES" "$WORK/releases"
  elif [ -n "$RELEASES" ]; then
    printf '%s\n' "$RELEASES" > "$WORK/releases"
  else
    say "Fetching the release list..."
    download "$INSTALLER_URL" "$WORK/installer.tar.gz" \
      || err "cannot download the release list from $INSTALLER_URL"
    tar -xzOf "$WORK/installer.tar.gz" > "$WORK/installer.sh" \
      || err "cannot unpack the release list"
    sed -n '/^# @@RELEASES_BEGIN@@$/,/^# @@RELEASES_END@@$/p' "$WORK/installer.sh" > "$WORK/releases"
  fi
  # Only well-formed lines: tag, asset name, checksum, URL.
  grep -E '^v[0-9][^ ]* [^ ]+ ([0-9a-f]{64}|-) [^ ]+$' "$WORK/releases" > "$WORK/releases.ok" || true
  [ -s "$WORK/releases.ok" ] || err "the release list is empty; see $REPO_URL/releases"

  if [ "$LIST" -eq 1 ]; then
    cut -d' ' -f1 "$WORK/releases.ok" | uniq
    exit 0
  fi

  if [ -z "$VERSION" ]; then
    # The list is ordered newest first.
    VERSION="$(head -n1 "$WORK/releases.ok" | cut -d' ' -f1)"
  fi
  ARCHIVE="$BIN_NAME-$VERSION-$PLATFORM.tar.gz"

  # lookup <asset name> -> "<sha256> <url>"
  lookup() {
    awk -v t="$VERSION" -v n="$1" '$1 == t && $2 == n { print $3, $4; exit }' "$WORK/releases.ok"
  }
  archive_entry="$(lookup "$ARCHIVE")"
  if [ -z "$archive_entry" ]; then
    if ! cut -d' ' -f1 "$WORK/releases.ok" | grep -qx "$VERSION"; then
      err "release $VERSION is not available for installation. Available: $(cut -d' ' -f1 "$WORK/releases.ok" | uniq | tr '\n' ' ')"
    fi
    err "release $VERSION has no archive for $PLATFORM"
  fi
  archive_sha="${archive_entry%% *}"
  archive_url="${archive_entry#* }"
  sums_entry="$(lookup SHA256SUMS)"
  [ -n "$sums_entry" ] || err "release $VERSION has no SHA256SUMS"
  sums_url="${sums_entry#* }"

  # --- Already installed? ----------------------------------------------------------
  target="$BIN_DIR/$BIN_NAME"
  if [ "$FORCE" -eq 0 ] && [ -x "$target" ]; then
    current="$("$target" --version </dev/null 2>/dev/null | awk '{ print $2 }' || true)"
    if [ "v$current" = "$VERSION" ]; then
      say "$BIN_NAME $VERSION is already installed in $BIN_DIR (use --force to reinstall)."
      exit 0
    fi
  fi

  # --- Download and verify ---------------------------------------------------------
  say "Downloading $ARCHIVE..."
  download "$archive_url" "$WORK/$ARCHIVE" || err "download failed: $archive_url"
  download "$sums_url" "$WORK/SHA256SUMS" || err "download failed: $sums_url"

  expected="$(awk -v n="$ARCHIVE" '{ f = $2; sub(/^\*/, "", f) } f == n { print $1; exit }' "$WORK/SHA256SUMS")"
  [ -n "$expected" ] || err "SHA256SUMS of $VERSION has no entry for $ARCHIVE"
  actual="$(sha256_of "$WORK/$ARCHIVE")"
  if [ "$actual" != "$expected" ]; then
    err "checksum mismatch for $ARCHIVE: got $actual, SHA256SUMS has $expected"
  fi
  if [ "$archive_sha" != "-" ] && [ "$archive_sha" != "$expected" ]; then
    err "checksum of $ARCHIVE in SHA256SUMS does not match the release list"
  fi
  say "Checksum OK (sha256 $actual)."

  # --- Install ---------------------------------------------------------------------
  mkdir "$WORK/unpacked"
  tar -xzf "$WORK/$ARCHIVE" -C "$WORK/unpacked" || err "cannot unpack $ARCHIVE"
  binary="$WORK/unpacked/$BIN_NAME-$VERSION-$PLATFORM/$BIN_NAME"
  [ -f "$binary" ] || err "$ARCHIVE does not contain $BIN_NAME"
  chmod 0755 "$binary"
  "$binary" --version </dev/null >/dev/null 2>&1 || err "the downloaded binary does not run on this system ($os $arch)"

  mkdir -p "$BIN_DIR" || err "cannot create $BIN_DIR"
  # Copy next to the target and rename: the swap is atomic, and a running
  # server keeps its old binary.
  tmp_target="$BIN_DIR/.$BIN_NAME.tmp.$$"
  cp "$binary" "$tmp_target" || err "cannot write to $BIN_DIR (pass --prefix or --bin-dir)"
  chmod 0755 "$tmp_target"
  mv -f "$tmp_target" "$target" || { rm -f "$tmp_target"; err "cannot replace $target"; }

  say "Installed $("$target" --version </dev/null) to $target"

  # --- PATH ----------------------------------------------------------------------
  case ":${PATH:-}:" in
    *":$BIN_DIR:"* | *":${BIN_DIR%/}:"*)
      found="$(command -v "$BIN_NAME" 2>/dev/null || true)"
      if [ -n "$found" ] && [ "$found" != "$target" ]; then
        say "warning: '$BIN_NAME' in PATH resolves to $found, not $target."
      fi
      ;;
    *)
      say ""
      say "warning: $BIN_DIR is not in PATH. Add it and open a new terminal:"
      case "${SHELL:-}" in
        */zsh) say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc" ;;
        */bash)
          if [ "$os" = Darwin ]; then
            say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.bash_profile"
          else
            say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.bashrc"
          fi
          ;;
        */fish) say "  fish_add_path '$BIN_DIR'" ;;
        *) say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.profile" ;;
      esac
      ;;
  esac

  say ""
  say "Next: register the server in an MCP client, for example:"
  say "  $BIN_NAME connect claude-code"
}

main "$@"
