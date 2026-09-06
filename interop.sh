#!/bin/bash
# F01 acceptance criterion: interoperability verified by hash.
# Resolved from the script location, not from an absolute path: the one that
# used to be here belonged to the container this was written in, and exists
# neither in CI nor in a plain clone. Override it with ARCA=... bash interop.sh
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ARCA=${ARCA:-$ROOT/target/release/arca}
if [ ! -x "$ARCA" ]; then
  echo "binary not found at $ARCA (build it with: cargo build --release)" >&2
  exit 1
fi
W=/tmp/interop; rm -rf $W; mkdir -p $W/src $W/out; cd $W
OK=0; KO=0
ok(){ printf "  \033[32mOK\033[0m   %s\n" "$1"; OK=$((OK+1)); }
ko(){ printf "  \033[31mFALLO\033[0m %s\n" "$1"; KO=$((KO+1)); }

# Corpus: text, binary, one large file and nested paths
mkdir -p src/a/b/c
head -c 3000000 /usr/share/doc/*/changelog* 2>/dev/null > src/text.txt || head -c 3000000 /dev/urandom > src/text.txt
head -c 5000000 /usr/bin/python3.11 > src/binary.bin 2>/dev/null || head -c 5000000 /dev/urandom > src/binary.bin
head -c 200 /dev/urandom > src/a/b/c/deep.dat
printf 'line\n%.0s' {1..50000} > src/repetitive.txt
REF=$(cd src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
echo "  corpus: $(du -sh src|cut -f1), reference hash ${REF:0:16}"
echo

echo "A) Arca writes -> system tools read"
for niv in store fast normal best; do
  $ARCA create out/a-$niv.zip src -l $niv >/dev/null 2>&1 || { ko "arca create -l $niv"; continue; }
  unzip -tqq out/a-$niv.zip >/dev/null 2>&1 && ok "unzip -t accepts the zip (-l $niv)" || ko "unzip -t rejects the zip (-l $niv)"
  rm -rf x; mkdir x; unzip -qq out/a-$niv.zip -d x 2>/dev/null
  H=$(cd x/src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
  [ "$H" = "$REF" ] && ok "unzip returns identical bytes (-l $niv)" || ko "unzip returns different data (-l $niv)"
done
$ARCA create out/a.tar src >/dev/null 2>&1
rm -rf x; mkdir x; tar xf out/a.tar -C x 2>/dev/null
H=$(cd x/src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
[ "$H" = "$REF" ] && ok "system tar reads Arca's .tar" || ko "system tar fails on Arca's .tar"
$ARCA create out/a.tar.gz src >/dev/null 2>&1
rm -rf x; mkdir x; tar xzf out/a.tar.gz -C x 2>/dev/null
H=$(cd x/src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
[ "$H" = "$REF" ] && ok "system tar reads Arca's .tar.gz" || ko "system tar fails on Arca's .tar.gz"
7z t out/a-normal.zip >/dev/null 2>&1 && ok "7-Zip verifies Arca's zip" || ko "7-Zip rejects Arca's zip"

echo
echo "B) System tools write -> Arca reads"
zip -qr out/z-def.zip src
zip -qr0 out/z-store.zip src
zip -q9r out/z-9.zip src
tar cf out/t.tar src
tar czf out/t.tar.gz src
for f in z-def z-store z-9; do
  rm -rf y; mkdir y
  $ARCA extract out/$f.zip -o y >/dev/null 2>&1 || { ko "arca extract $f.zip"; continue; }
  H=$(cd y/src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
  [ "$H" = "$REF" ] && ok "Arca extracts zip($f) without losing a byte" || ko "Arca mis-extracts $f.zip"
done
for f in t.tar t.tar.gz; do
  rm -rf y; mkdir y
  $ARCA extract out/$f -o y >/dev/null 2>&1 || { ko "arca extract $f"; continue; }
  H=$(cd y/src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
  [ "$H" = "$REF" ] && ok "Arca extracts tar's $f without losing a byte" || ko "Arca mis-extracts $f"
done
7z a -tzip -mx5 out/s7.zip src >/dev/null 2>&1
rm -rf y; mkdir y; $ARCA extract out/s7.zip -o y >/dev/null 2>&1
H=$(cd y/src && find . -type f|sort|xargs sha256sum|sha256sum|cut -d' ' -f1)
[ "$H" = "$REF" ] && ok "Arca reads the zip created by 7-Zip" || ko "Arca fails on 7-Zip's zip"

echo
echo "C) Corruption detection"
cp out/a-normal.zip out/corrupt.zip
printf '\xDE\xAD' | dd of=out/corrupt.zip bs=1 seek=200 conv=notrunc 2>/dev/null
$ARCA test out/corrupt.zip >/dev/null 2>&1 && ko "corrupt zip not detected" || ok "corruption detected, exits with an error"
$ARCA test out/a-normal.zip >/dev/null 2>&1 && ok "accepts the intact archive" || ko "rejects a valid archive"

echo
echo "D) Security: Zip Slip"
python3 - <<'PY'
import zipfile
z=zipfile.ZipFile('/tmp/interop/out/slip.zip','w')
z.writestr('../../../../tmp/PWNED','malicious'); z.close()
PY
rm -rf y; mkdir y
$ARCA extract out/slip.zip -o y >/dev/null 2>&1
if [ -f /tmp/PWNED ]; then ko "WROTE OUTSIDE THE DESTINATION"; rm -f /tmp/PWNED; else ok "rejects the path escaping the destination"; fi

echo
echo "-------------------------------------------"
echo "  $OK passed, $KO failed"
[ $KO -eq 0 ] || exit 1
