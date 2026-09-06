#!/bin/bash
# Criterio de aceptacion F01: interoperabilidad verificada por hash.
ARCA=/home/claude/arca/target/release/arca
W=/tmp/interop; rm -rf $W; mkdir -p $W/src $W/out; cd $W
OK=0; KO=0
ok(){ printf "  \033[32mOK\033[0m   %s\n" "$1"; OK=$((OK+1)); }
ko(){ printf "  \033[31mFALLO\033[0m %s\n" "$1"; KO=$((KO+1)); }

# Corpus: texto, binario, un fichero grande y rutas anidadas
mkdir -p src/a/b/c
head -c 3000000 /usr/share/doc/*/changelog* 2>/dev/null > src/texto.txt || head -c 3000000 /dev/urandom > src/texto.txt
head -c 5000000 /usr/bin/python3.11 > src/binario.bin 2>/dev/null || head -c 5000000 /dev/urandom > src/binario.bin
head -c 200 /dev/urandom > src/a/b/c/hondo.dat
printf 'linea\n%.0s' {1..50000} > src/repetitivo.txt
REF=$(cd src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
echo "  corpus: $(du -sh src|cut -f1), hash de referencia ${REF:0:16}"
echo

echo "A) Arca escribe -> las herramientas del sistema leen"
for niv in store fast normal best; do
  $ARCA create out/a-$niv.zip src -n $niv >/dev/null 2>&1 || { ko "arca create -n $niv"; continue; }
  unzip -tqq out/a-$niv.zip >/dev/null 2>&1 && ok "unzip -t acepta el zip (-n $niv)" || ko "unzip -t rechaza el zip (-n $niv)"
  rm -rf x; mkdir x; unzip -qq out/a-$niv.zip -d x 2>/dev/null
  H=$(cd x/src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
  [ "$H" = "$REF" ] && ok "unzip devuelve bytes identicos (-n $niv)" || ko "unzip devuelve datos distintos (-n $niv)"
done
$ARCA create out/a.tar src >/dev/null 2>&1
rm -rf x; mkdir x; tar xf out/a.tar -C x 2>/dev/null
H=$(cd x/src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
[ "$H" = "$REF" ] && ok "tar del sistema lee el .tar de Arca" || ko "tar del sistema falla con el .tar de Arca"
$ARCA create out/a.tar.gz src >/dev/null 2>&1
rm -rf x; mkdir x; tar xzf out/a.tar.gz -C x 2>/dev/null
H=$(cd x/src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
[ "$H" = "$REF" ] && ok "tar del sistema lee el .tar.gz de Arca" || ko "tar del sistema falla con el .tar.gz de Arca"
7z t out/a-normal.zip >/dev/null 2>&1 && ok "7-Zip verifica el zip de Arca" || ko "7-Zip rechaza el zip de Arca"

echo
echo "B) Las herramientas del sistema escriben -> Arca lee"
zip -qr out/z-def.zip src
zip -qr0 out/z-store.zip src
zip -q9r out/z-9.zip src
tar cf out/t.tar src
tar czf out/t.tar.gz src
for f in z-def z-store z-9; do
  rm -rf y; mkdir y
  $ARCA extract out/$f.zip -o y >/dev/null 2>&1 || { ko "arca extract $f.zip"; continue; }
  H=$(cd y/src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
  [ "$H" = "$REF" ] && ok "Arca extrae el zip de zip($f) sin perder un byte" || ko "Arca extrae mal $f.zip"
done
for f in t.tar t.tar.gz; do
  rm -rf y; mkdir y
  $ARCA extract out/$f -o y >/dev/null 2>&1 || { ko "arca extract $f"; continue; }
  H=$(cd y/src && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
  [ "$H" = "$REF" ] && ok "Arca extrae el $f de tar sin perder un byte" || ko "Arca extrae mal $f"
done
7z a -tzip -mx5 out/s7.zip src >/dev/null 2>&1
rm -rf y; mkdir y; $ARCA extract out/s7.zip -o y >/dev/null 2>&1
H=$(cd y/src && find . -type f|sort|xargs sha256sum|sha256sum|cut -d' ' -f1)
[ "$H" = "$REF" ] && ok "Arca lee el zip creado por 7-Zip" || ko "Arca falla con el zip de 7-Zip"

echo
echo "C) Deteccion de corrupcion"
cp out/a-normal.zip out/corrupto.zip
printf '\xDE\xAD' | dd of=out/corrupto.zip bs=1 seek=200 conv=notrunc 2>/dev/null
$ARCA test out/corrupto.zip >/dev/null 2>&1 && ko "no detecta el zip corrupto" || ok "detecta la corrupcion y sale con error"
$ARCA test out/a-normal.zip >/dev/null 2>&1 && ok "acepta el archivo intacto" || ko "rechaza un archivo valido"

echo
echo "D) Seguridad: Zip Slip"
python3 - <<'PY'
import zipfile
z=zipfile.ZipFile('/tmp/interop/out/slip.zip','w')
z.writestr('../../../../tmp/PWNED','malicioso'); z.close()
PY
rm -rf y; mkdir y
$ARCA extract out/slip.zip -o y >/dev/null 2>&1
if [ -f /tmp/PWNED ]; then ko "ESCRIBIO FUERA DEL DESTINO"; rm -f /tmp/PWNED; else ok "rechaza la ruta que se escapa del destino"; fi

echo
echo "-------------------------------------------"
echo "  $OK correctas, $KO fallidas"
[ $KO -eq 0 ] || exit 1
