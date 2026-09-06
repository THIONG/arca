#!/bin/bash
# Installs Arca for the current user on Linux and macOS.
#
# No sudo by default: the binaries go to ~/.local/bin, which is what the
# freedesktop directory specification says and what macOS honours just the
# same. With --system they go to /usr/local/bin, and that does need
# privileges.
#
# There is no file manager integration here yet. On Windows it exists because
# the context menu is a COM DLL; the equivalent in Dolphin, Thunar, Nemo,
# Nautilus or Finder is a different mechanism in each case.
#
# Usage:  ./instalar.sh             install into ~/.local/bin
#         ./instalar.sh --system    install into /usr/local/bin
#         ./instalar.sh --uninstall remove it

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PREFIX="$HOME/.local"
UNINSTALL=0

for arg in "$@"; do
  case "$arg" in
    --system)    PREFIX="/usr/local" ;;
    --uninstall) UNINSTALL=1 ;;
    --help|-h)   sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 1 ;;
  esac
done

BIN="$PREFIX/bin/arca"
BIN_GUI="$PREFIX/bin/arca-gui"
COMPLETION_BASH="$PREFIX/share/bash-completion/completions/arca"
COMPLETION_FISH="$PREFIX/share/fish/vendor_completions.d/arca.fish"
COMPLETION_ZSH="$PREFIX/share/zsh/site-functions/_arca"
MANPAGE="$PREFIX/share/man/man1/arca.1"

# Writing into /usr/local almost always needs permissions; ~/.local never does.
SUDO=""
if [ "$PREFIX" = "/usr/local" ] && [ ! -w "/usr/local/bin" ]; then
  SUDO="sudo"
fi

if [ "$UNINSTALL" -eq 1 ]; then
  echo "==> Removing Arca from $PREFIX"
  for f in "$BIN" "$BIN_GUI" "$COMPLETION_BASH" "$COMPLETION_FISH" "$COMPLETION_ZSH" "$MANPAGE"; do
    if [ -e "$f" ]; then
      $SUDO rm -f "$f"
      echo "    removed $f"
    fi
  done
  echo
  echo "Done. If you added $PREFIX/bin to your PATH by hand, remove it yourself."
  exit 0
fi

if ! command -v cargo > /dev/null; then
  echo "cargo not found; install Rust from https://rustup.rs" >&2
  exit 1
fi

echo "==> Building"
cargo build --release --manifest-path "$ROOT/Cargo.toml"

SRC="$ROOT/target/release/arca"
SRC_GUI="$ROOT/target/release/arca-gui"
for f in "$SRC" "$SRC_GUI"; do
  if [ ! -x "$f" ]; then
    echo "binary not found at $f" >&2
    exit 1
  fi
done

echo "==> Installing into $PREFIX/bin"
$SUDO install -d "$PREFIX/bin"
$SUDO install -m 755 "$SRC" "$BIN"
$SUDO install -m 755 "$SRC_GUI" "$BIN_GUI"
echo "    $BIN"
echo "    $BIN_GUI"

# clap can generate shell completions, but the CLI does not expose the
# subcommand that emits them yet. When it does, they get written here.

echo
"$BIN" --version
echo

case ":$PATH:" in
  *":$PREFIX/bin:"*)
    echo "Done. $PREFIX/bin is already on your PATH."
    ;;
  *)
    echo "Done, but $PREFIX/bin is NOT on your PATH. Add it to your shell:"
    echo
    echo "    echo 'export PATH=\"$PREFIX/bin:\$PATH\"' >> ~/.bashrc"
    echo
    echo "and open a new terminal."
    ;;
esac
