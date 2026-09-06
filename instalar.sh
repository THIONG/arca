#!/bin/bash
# Instala Arca para el usuario actual en Linux y macOS.
#
# Sin sudo por defecto: el binario va a ~/.local/bin, que es lo que dice la
# especificacion de directorios de freedesktop y lo que macOS respeta igual.
# Con --sistema se instala en /usr/local/bin, y eso si pide privilegios.
#
# Aqui no hay extension del gestor de archivos. En Windows existe porque el
# menu contextual es una DLL COM; el equivalente en GNOME o en Finder es otra
# implementacion distinta y todavia no esta escrita.
#
# Uso:  ./instalar.sh              instala en ~/.local/bin
#       ./instalar.sh --sistema    instala en /usr/local/bin
#       ./instalar.sh --quitar     desinstala

set -euo pipefail

RAIZ=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PREFIJO="$HOME/.local"
QUITAR=0

for arg in "$@"; do
  case "$arg" in
    --sistema) PREFIJO="/usr/local" ;;
    --quitar)  QUITAR=1 ;;
    --ayuda|-h) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "opcion desconocida: $arg" >&2; exit 1 ;;
  esac
done

BIN="$PREFIJO/bin/arca"
COMPLETADO_BASH="$PREFIJO/share/bash-completion/completions/arca"
COMPLETADO_FISH="$PREFIJO/share/fish/vendor_completions.d/arca.fish"
COMPLETADO_ZSH="$PREFIJO/share/zsh/site-functions/_arca"
MANUAL="$PREFIJO/share/man/man1/arca.1"

# Escribir en /usr/local casi siempre necesita permisos; en ~/.local nunca.
SUDO=""
if [ "$PREFIJO" = "/usr/local" ] && [ ! -w "/usr/local/bin" ]; then
  SUDO="sudo"
fi

if [ "$QUITAR" -eq 1 ]; then
  echo "==> Desinstalando Arca de $PREFIJO"
  for f in "$BIN" "$COMPLETADO_BASH" "$COMPLETADO_FISH" "$COMPLETADO_ZSH" "$MANUAL"; do
    if [ -e "$f" ]; then
      $SUDO rm -f "$f"
      echo "    borrado $f"
    fi
  done
  echo
  echo "Listo. Si anadiste $PREFIJO/bin al PATH a mano, quitalo tu."
  exit 0
fi

if ! command -v cargo > /dev/null; then
  echo "no encuentro cargo; instala Rust desde https://rustup.rs" >&2
  exit 1
fi

echo "==> Compilando"
cargo build --release --manifest-path "$RAIZ/Cargo.toml"

ORIGEN="$RAIZ/target/release/arca"
if [ ! -x "$ORIGEN" ]; then
  echo "no encuentro el binario en $ORIGEN" >&2
  exit 1
fi

echo "==> Instalando en $PREFIJO/bin"
$SUDO install -d "$PREFIJO/bin"
$SUDO install -m 755 "$ORIGEN" "$BIN"
echo "    $BIN"

# clap sabe generar los completados, pero el CLI todavia no expone el
# subcomando que los emite. Cuando lo haga, se rellenan aqui.

echo
"$BIN" --version
echo

case ":$PATH:" in
  *":$PREFIJO/bin:"*)
    echo "Listo. $PREFIJO/bin ya esta en tu PATH."
    ;;
  *)
    echo "Listo, pero $PREFIJO/bin NO esta en tu PATH. Anadelo a tu shell:"
    echo
    echo "    echo 'export PATH=\"$PREFIJO/bin:\$PATH\"' >> ~/.bashrc"
    echo
    echo "y abre una terminal nueva."
    ;;
esac
