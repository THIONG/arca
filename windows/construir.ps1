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

if ($Quitar) {
    Get-AppxPackage $Paquete | Remove-AppxPackage
    Write-Host "Desregistrado. Reinicia el Explorador para que desaparezca del menú."
    exit 0
}

# --- 1. Binarios ------------------------------------------------------------
Write-Host "==> Compilando arca.exe" -ForegroundColor Cyan
Push-Location $Raiz
cargo build --release --target x86_64-pc-windows-msvc
Pop-Location

Write-Host "==> Compilando arca_shell.dll" -ForegroundColor Cyan
Push-Location (Join-Path $PSScriptRoot "arca-shell")
cargo build --release --target x86_64-pc-windows-msvc
Pop-Location

# --- 2. Carpeta del paquete -------------------------------------------------
New-Item -ItemType Directory -Force -Path $Destino, "$Destino\Assets" | Out-Null
Copy-Item "$Raiz\target\x86_64-pc-windows-msvc\release\arca.exe" $Destino -Force
Copy-Item "$PSScriptRoot\arca-shell\target\x86_64-pc-windows-msvc\release\arca_shell.dll" $Destino -Force
Copy-Item "$PSScriptRoot\AppxManifest.xml" $Destino -Force

# Iconos de relleno: Windows rechaza el paquete si faltan.
foreach ($n in @("StoreLogo.png","Square150x150Logo.png","Square44x44Logo.png")) {
    $p = "$Destino\Assets\$n"
    if (-not (Test-Path $p)) {
        [byte[]]$png = 0x89,0x50,0x4E,0x47,0x0D,0x0A,0x1A,0x0A,0x00,0x00,0x00,0x0D,
                       0x49,0x48,0x44,0x52,0x00,0x00,0x00,0x01,0x00,0x00,0x00,0x01,
                       0x08,0x06,0x00,0x00,0x00,0x1F,0x15,0xC4,0x89,0x00,0x00,0x00,
                       0x0A,0x49,0x44,0x41,0x54,0x78,0x9C,0x63,0x00,0x01,0x00,0x00,
                       0x05,0x00,0x01,0x0D,0x0A,0x2D,0xB4,0x00,0x00,0x00,0x00,0x49,
                       0x45,0x4E,0x44,0xAE,0x42,0x60,0x82
        [System.IO.File]::WriteAllBytes($p, $png)
    }
}

# --- 3. Registrar en modo desarrollo ---------------------------------------
# Add-AppxPackage -Register no necesita firma, pero exige tener activado el
# Modo de desarrollador en Configuración. Para distribuir de verdad hay que
# empaquetar con makeappx y firmar con signtool.
Write-Host "==> Registrando el paquete" -ForegroundColor Cyan
Add-AppxPackage -Register "$Destino\AppxManifest.xml" -ExternalLocation $Destino

Write-Host ""
Write-Host "Listo. Reinicia el Explorador para que cargue la extensión:" -ForegroundColor Green
Write-Host "    Stop-Process -Name explorer -Force"
Write-Host ""
Write-Host "Luego haz clic derecho sobre un .zip: debe salir «Arca» con tres opciones."
