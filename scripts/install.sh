#!/usr/bin/env bash
# Install Edifier control for macOS.
#
# One-liner (after the repository is published):
#
#   curl -fsSL https://raw.githubusercontent.com/<owner>/<repo>/main/scripts/install.sh | sh
#
# From a checkout, without arguments, it builds the workspace in release mode and installs the three
# binaries into ~/.local/bin. Environment overrides:
#
#   EDIFIER_REPO=owner/repo   repository to download releases from (required when this script is
#                             streamed, optional in a checkout where the git remote is used)
#   EDIFIER_VERSION=v0.1.0    release tag to install (default: latest)
#   EDIFIER_PREFIX=~/.local   install prefix (binaries go to <prefix>/bin)
#   EDIFIER_FROM_SOURCE=1     build from the checkout instead of downloading a release
#   EDIFIER_NO_LAUNCH_AGENT=1 skip the "start the menu bar app at login" step
#
# Flags: --prefix DIR | --version TAG | --from-source | --launch-agent | --no-launch-agent | --uninstall
set -euo pipefail

PREFIX="${EDIFIER_PREFIX:-$HOME/.local}"
VERSION="${EDIFIER_VERSION:-latest}"
REPO="${EDIFIER_REPO:-}"
FROM_SOURCE="${EDIFIER_FROM_SOURCE:-0}"
LAUNCH_AGENT="prompt"
UNINSTALL=0

BINARIES=(edifierctl edifier-mcp edifier-menubar)

usage() {
  cat <<'TEXT'
Install Edifier control for macOS.

  scripts/install.sh [--prefix DIR] [--version TAG] [--from-source]
                     [--launch-agent|--no-launch-agent] [--uninstall]

Environment: EDIFIER_REPO=owner/repo, EDIFIER_VERSION=vX.Y.Z, EDIFIER_PREFIX=~/.local,
             EDIFIER_FROM_SOURCE=1, EDIFIER_NO_LAUNCH_AGENT=1

In a checkout it builds the workspace with cargo; when streamed with curl it downloads the release
asset for this architecture and verifies its sha256 when one is published.
TEXT
}

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
    --version) VERSION="${2:?--version needs a tag}"; shift 2 ;;
    --from-source) FROM_SOURCE=1; shift ;;
    --launch-agent) LAUNCH_AGENT="yes"; shift ;;
    --no-launch-agent) LAUNCH_AGENT="no"; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument $1 (try --help)" ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || die "this project only supports macOS (CoreBluetooth + AppKit)"
case "$(uname -m)" in
  arm64) ARCH=aarch64 ;;
  x86_64) ARCH=x86_64 ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac

if [ "$UNINSTALL" -eq 1 ]; then
  say "Removing $PREFIX/bin/{${BINARIES[*]// /,}}"
  for binary in "${BINARIES[@]}"; do rm -f "$PREFIX/bin/$binary"; done
  if [ -f "$HOME/Library/LaunchAgents/sh.edifier.menubar.plist" ]; then
    launchctl bootout "gui/$UID" "$HOME/Library/LaunchAgents/sh.edifier.menubar.plist" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/sh.edifier.menubar.plist"
    say "Removed the menu bar login item."
  fi
  say "Done. The headset keeps whatever settings it has."
  exit 0
fi

# Where did we come from? A checkout has Cargo.toml next to this script's parent directory.
SCRIPT_DIR=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi
CHECKOUT=""
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/../Cargo.toml" ]; then
  CHECKOUT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi
if [ -z "$REPO" ] && [ -n "$CHECKOUT" ] && have git; then
  REPO="$(git -C "$CHECKOUT" remote get-url origin 2>/dev/null | sed -E 's#.*[:/]([^/]+/[^/]+?)(\.git)?$#\1#' || true)"
fi
if [ "$FROM_SOURCE" = "0" ] && [ -z "$CHECKOUT" ] && [ -z "$REPO" ]; then
  die "no repository to download from: set EDIFIER_REPO=owner/repo (see --help) or clone the repository and run scripts/install.sh"
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

install_from_dir() { # install_from_dir <dir with binaries>
  local source_dir="$1"
  mkdir -p "$PREFIX/bin"
  for binary in "${BINARIES[@]}"; do
    [ -x "$source_dir/$binary" ] || die "$binary is missing from $source_dir"
    install -m 0755 "$source_dir/$binary" "$PREFIX/bin/$binary"
    say "installed $PREFIX/bin/$binary"
  done
}

if [ "$FROM_SOURCE" != "0" ] || [ -n "$CHECKOUT" ]; then
  [ -n "$CHECKOUT" ] || die "--from-source needs a checkout (run this script from the repository)"
  have cargo || die "building from source needs Rust: https://rustup.rs (or rerun without --from-source)"
  say "Building from $CHECKOUT (release) …"
  ( cd "$CHECKOUT" && cargo build --release --workspace )
  install_from_dir "$CHECKOUT/target/release"
else
  have curl || die "curl is required to download the release"
  if [ "$VERSION" = "latest" ]; then
    URL="https://github.com/$REPO/releases/latest/download/edifier-macos-$ARCH.tar.gz"
    CHECKSUM_URL="https://github.com/$REPO/releases/latest/download/edifier-macos-$ARCH.tar.gz.sha256"
  else
    URL="https://github.com/$REPO/releases/download/$VERSION/edifier-macos-$ARCH.tar.gz"
    CHECKSUM_URL="https://github.com/$REPO/releases/download/$VERSION/edifier-macos-$ARCH.tar.gz.sha256"
  fi
  say "Downloading $URL"
  curl -fsSL "$URL" -o "$STAGE/edifier.tar.gz" \
    || die "cannot download the release — is $REPO public and does it have a $ARCH build?"
  if curl -fsSL "$CHECKSUM_URL" -o "$STAGE/edifier.tar.gz.sha256" 2>/dev/null; then
    ( cd "$STAGE" && shasum -a 256 -c edifier.tar.gz.sha256 >/dev/null ) \
      || die "checksum mismatch: the download is corrupted"
    say "checksum verified"
  else
    say "no checksum published for this release; skipping verification"
  fi
  tar -xzf "$STAGE/edifier.tar.gz" -C "$STAGE"
  install_from_dir "$STAGE/bin"
fi

if [ "$LAUNCH_AGENT" = "prompt" ]; then
  if [ -t 0 ]; then
    printf 'Start the menu bar app at every login? [y/N] '
    read -r answer || answer="n"
    case "$answer" in [yY]*) LAUNCH_AGENT="yes" ;; *) LAUNCH_AGENT="no" ;; esac
  else
    LAUNCH_AGENT="no"
  fi
fi

if [ "$LAUNCH_AGENT" = "yes" ]; then
  "$PREFIX/bin/edifier-menubar" --install-launch-agent >/dev/null
  launchctl bootout "gui/$UID" "$HOME/Library/LaunchAgents/sh.edifier.menubar.plist" 2>/dev/null || true
  launchctl bootstrap "gui/$UID" "$HOME/Library/LaunchAgents/sh.edifier.menubar.plist" \
    && say "menu bar app started (and will start at every login)"
fi

cat <<EOF

Installed to $PREFIX/bin.

Next steps
  1. Power the headset on and keep it in range.
  2. Allow Bluetooth for the terminal when macOS asks
     (System Settings → Privacy & Security → Bluetooth). This happens once.
  3. Try it:

       $PREFIX/bin/edifierctl doctor
       $PREFIX/bin/edifierctl anc on
       $PREFIX/bin/edifier-menubar        # the menu bar panel (battery + noise control)

  4. Use it from a coding agent (optional) — add to ~/.omp/agent/mcp.json:

       {"mcpServers": {"edifier": {"type": "stdio", "command": "$PREFIX/bin/edifier-mcp"}}}

Uninstall:  $0 --uninstall
EOF
