# Construye Arca para Windows y registra el menú contextual.
#
# Requisitos:
#   - Rust con el target x86_64-pc-windows-msvc
#   - Visual Studio Build Tools (el enlazador de MSVC)
#   - Windows SDK, por makeappx.exe y signtool.exe
#
# Uso:  .\construir.ps1            construye y registra en modo desarrollo
#       .\construir.ps1 -Quitar    desregistra

param([switch]$Quitar)

$ErrorActionPreference = "Stop"
$Raiz    = Split-Path -Parent $PSScriptRoot
$Destino = Join-Path $PSScriptRoot "salida"
$Paquete = "Arca.Archivador"

# El menú moderno de Windows 11 sale del paquete MSIX. El clásico —el de
# «Mostrar más opciones», y el único que hay en Windows 10— es IContextMenu y
# no se puede declarar en el manifiesto: va por registro. Se escribe en HKCU
# para no necesitar permisos de administrador.
#
# Este CLSID es el del manejador clásico y debe coincidir con
# CLSID_ARCA_CLASICO en src/lib.rs. Es distinto del de IExplorerCommand a
# propósito: son dos objetos con interfaces distintas.
$ClsidClasico = "{B528A7F3-C889-4C98-B052-5D7F7F778E14}"
$TiposMenu    = @("*", "Directory")

# Se usa la API de .NET en vez de New-Item porque una de las claves se llama
# «*» y el proveedor de registro de PowerShell la trataría como comodín.
function Registrar-MenuClasico($dll) {
    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\CLSID\$ClsidClasico\InprocServer32")
    $k.SetValue("", $dll)
    $k.SetValue("ThreadingModel", "Apartment")
    $k.Close()
    foreach ($t in $TiposMenu) {
        $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\$t\shellex\ContextMenuHandlers\Arca")
        $k.SetValue("", $ClsidClasico)
        $k.Close()
    }
}

function Desregistrar-MenuClasico {
    foreach ($t in $TiposMenu) {
        [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree("Software\Classes\$t\shellex\ContextMenuHandlers\Arca", $false)
    }
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree("Software\Classes\CLSID\$ClsidClasico", $false)
}

if ($Quitar) {
    Get-AppxPackage $Paquete | Remove-AppxPackage
    Desregistrar-MenuClasico
    Write-Host "Desregistrado, menú moderno y clásico. Reinicia el Explorador."
    exit 0
}

# --- 1. Binarios ------------------------------------------------------------
# cargo escribe su progreso en stderr aunque todo vaya bien, y con
# ErrorActionPreference = Stop eso revienta el script en cuanto alguien captura
# su salida. El codigo de salida es lo unico que indica un fallo de verdad.
function Ejecutar($programa, $argumentos) {
    $anterior = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & $programa @argumentos
    $codigo = $LASTEXITCODE
    $ErrorActionPreference = $anterior
    if ($codigo -ne 0) {
        throw "$programa $($argumentos -join ' ') fallo con codigo $codigo"
    }
}

Write-Host "==> Compilando arca.exe" -ForegroundColor Cyan
Push-Location $Raiz
Ejecutar "cargo" @("build","--release","--target","x86_64-pc-windows-msvc")
Pop-Location

Write-Host "==> Compilando arca_shell.dll" -ForegroundColor Cyan
Push-Location (Join-Path $PSScriptRoot "arca-shell")
Ejecutar "cargo" @("build","--release","--target","x86_64-pc-windows-msvc")
Pop-Location

# --- 2. Carpeta del paquete -------------------------------------------------
New-Item -ItemType Directory -Force -Path $Destino, "$Destino\Assets" | Out-Null
Copy-Item "$Raiz\target\x86_64-pc-windows-msvc\release\arca.exe" $Destino -Force
Copy-Item "$PSScriptRoot\arca-shell\target\x86_64-pc-windows-msvc\release\arca_shell.dll" $Destino -Force
Copy-Item "$PSScriptRoot\AppxManifest.xml" $Destino -Force

# Iconos del paquete, ya de verdad. Antes se generaban aquí PNG de 1×1 solo
# para que el manifiesto validara; ahora salen de la marca, en windows\assets.
Copy-Item "$PSScriptRoot\assets\*.png" "$Destino\Assets" -Force

# --- 3. Registrar en modo desarrollo ---------------------------------------
# Add-AppxPackage -Register no necesita firma, pero exige tener activado el
# Modo de desarrollador en Configuración. Para distribuir de verdad hay que
# empaquetar con makeappx y firmar con signtool.
Write-Host "==> Registrando el paquete (menú moderno)" -ForegroundColor Cyan
Add-AppxPackage -Register "$Destino\AppxManifest.xml" -ExternalLocation $Destino

Write-Host "==> Registrando el menú clásico" -ForegroundColor Cyan
Registrar-MenuClasico "$Destino\arca_shell.dll"

Write-Host ""
Write-Host "Listo. Reinicia el Explorador para que cargue la extensión:" -ForegroundColor Green
Write-Host "    Stop-Process -Name explorer -Force"
Write-Host ""
Write-Host "Luego haz clic derecho sobre un .zip: debe salir «Arca» con tres opciones."
