# Builds Arca for Windows and registers the context menu.
#
# Requirements:
#   - Rust with the x86_64-pc-windows-msvc target
#   - Visual Studio Build Tools (the MSVC linker)
#   - Windows SDK, for makeappx.exe and signtool.exe
#
# Usage:  .\build.ps1            builds and registers in development mode
#         .\build.ps1 -Remove    unregisters

param([switch]$Remove)

$ErrorActionPreference = "Stop"
$Root    = Split-Path -Parent $PSScriptRoot
$Dest    = Join-Path $PSScriptRoot "salida"
$Package = "Arca.Archivador"

# The modern Windows 11 menu comes out of the MSIX package. The classic one,
# behind "Show more options" and the only one Windows 10 has, is IContextMenu
# and cannot be declared in the manifest: it goes through the registry. Written
# under HKCU so that no administrator rights are needed.
#
# This CLSID belongs to the classic handler and has to match CLSID_ARCA_CLASSIC
# in src/lib.rs. It differs from the IExplorerCommand one on purpose: they are
# two objects with two different interfaces.
$ClassicClsid = "{B528A7F3-C889-4C98-B052-5D7F7F778E14}"
$MenuTypes    = @("*", "Directory")

# The .NET API instead of New-Item, because one of the keys is named "*" and the
# PowerShell registry provider would take it for a wildcard.
function Register-ClassicMenu($dll) {
    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\CLSID\$ClassicClsid\InprocServer32")
    $k.SetValue("", $dll)
    $k.SetValue("ThreadingModel", "Apartment")
    $k.Close()
    foreach ($t in $MenuTypes) {
        $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\$t\shellex\ContextMenuHandlers\Arca")
        $k.SetValue("", $ClassicClsid)
        $k.Close()
    }
}

function Unregister-ClassicMenu {
    foreach ($t in $MenuTypes) {
        [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree("Software\Classes\$t\shellex\ContextMenuHandlers\Arca", $false)
    }
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree("Software\Classes\CLSID\$ClassicClsid", $false)
}

if ($Remove) {
    Get-AppxPackage $Package | Remove-AppxPackage
    Unregister-ClassicMenu
    Write-Host "Unregistered, both the modern and the classic menu. Restart Explorer."
    exit 0
}

# --- 1. Binaries ------------------------------------------------------------
# cargo writes its progress to stderr even when everything is fine, and with
# ErrorActionPreference = Stop that blows the script up as soon as anyone
# captures its output. The exit code is the only thing that means a real failure.
function Invoke-Tool($program, $arguments) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & $program @arguments
    $code = $LASTEXITCODE
    $ErrorActionPreference = $previous
    if ($code -ne 0) {
        throw "$program $($arguments -join ' ') failed with code $code"
    }
}

Write-Host "==> Building arca.exe" -ForegroundColor Cyan
Push-Location $Root
Invoke-Tool "cargo" @("build","--release","--target","x86_64-pc-windows-msvc")
Pop-Location

Write-Host "==> Building arca_shell.dll" -ForegroundColor Cyan
Push-Location (Join-Path $PSScriptRoot "arca-shell")
Invoke-Tool "cargo" @("build","--release","--target","x86_64-pc-windows-msvc")
Pop-Location

# --- 2. Package folder ------------------------------------------------------
New-Item -ItemType Directory -Force -Path $Dest, "$Dest\Assets" | Out-Null
Copy-Item "$Root\target\x86_64-pc-windows-msvc\release\arca.exe" $Dest -Force
Copy-Item "$PSScriptRoot\arca-shell\target\x86_64-pc-windows-msvc\release\arca_shell.dll" $Dest -Force
Copy-Item "$PSScriptRoot\AppxManifest.xml" $Dest -Force

# Real package icons now. These used to be 1x1 PNGs generated here just so the
# manifest would validate; they come from the brand, in windows\assets.
Copy-Item "$PSScriptRoot\assets\*.png" "$Dest\Assets" -Force

# --- 3. Register in development mode ----------------------------------------
# Add-AppxPackage -Register needs no signature, but it does need Developer Mode
# turned on in Settings. Distributing for real means packaging with makeappx and
# signing with signtool.
Write-Host "==> Registering the package (modern menu)" -ForegroundColor Cyan
Add-AppxPackage -Register "$Dest\AppxManifest.xml" -ExternalLocation $Dest

Write-Host "==> Registering the classic menu" -ForegroundColor Cyan
Register-ClassicMenu "$Dest\arca_shell.dll"

Write-Host ""
Write-Host "Done. Restart Explorer so it loads the extension:" -ForegroundColor Green
Write-Host "    Stop-Process -Name explorer -Force"
Write-Host ""
Write-Host "Then right click a .zip: 'Arca' should appear with three options."
