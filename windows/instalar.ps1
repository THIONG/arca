# Instala Arca para el usuario actual: binario, extension del menu contextual
# y entrada en «Aplicaciones instaladas».
#
# No es lo mismo que construir.ps1. Ese compila y registra desde la carpeta de
# trabajo, para desarrollar. Este copia a una ubicacion estable fuera del repo,
# de modo que un `cargo clean` o mover el proyecto no rompa nada.
#
# Todo va en HKCU y en LOCALAPPDATA: no hace falta administrador.
#
# Uso:  .\instalar.ps1              instala
#       .\instalar.ps1 -Quitar      desinstala
#       .\instalar.ps1 -Destino D:\Arca

param(
    [switch]$Quitar,
    [string]$Destino = "$env:LOCALAPPDATA\Programs\Arca"
)

$ErrorActionPreference = "Stop"

$Paquete      = "Arca.Archivador"
$ClsidClasico = "{B528A7F3-C889-4C98-B052-5D7F7F778E14}"
$TiposMenu    = @("*", "Directory")
$ClaveDesinst = "Software\Microsoft\Windows\CurrentVersion\Uninstall\Arca"

# Extensiones que Arca sabe abrir. Cada una necesita su propio ProgID: Windows
# asocia la extension al ProgID, y el ProgID a un comando.
$Formatos = @{
    ".zip"    = @{ ProgId = "Arca.zip";    Texto = "Archivo ZIP" }
    ".tar"    = @{ ProgId = "Arca.tar";    Texto = "Archivo TAR" }
    ".gz"     = @{ ProgId = "Arca.targz";  Texto = "Archivo TAR comprimido" }
    ".tgz"    = @{ ProgId = "Arca.tgz";    Texto = "Archivo TAR comprimido" }
}

# Tres registros distintos, y hacen falta los tres:
#   1. El ProgID, que dice con que comando se abre.
#   2. OpenWithProgids en la extension, que es lo que llena «Abrir con».
#   3. Capabilities + RegisteredApplications, que es lo que hace que Arca
#      aparezca en Configuracion > Aplicaciones predeterminadas.
function Registrar-Asociaciones($gui) {
    foreach ($ext in $Formatos.Keys) {
        $progid = $Formatos[$ext].ProgId
        $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\$progid")
        $k.SetValue("", $Formatos[$ext].Texto)
        $k.Close()
        $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\$progid\DefaultIcon")
        $k.SetValue("", "$gui,0")
        $k.Close()
        $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\$progid\shell\open\command")
        $k.SetValue("", "`"$gui`" `"%1`"")
        $k.Close()

        $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\$ext\OpenWithProgids")
        $k.SetValue($progid, [byte[]]@(), [Microsoft.Win32.RegistryValueKind]::None)
        $k.Close()
    }

    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\Applications\arca-gui.exe\shell\open\command")
    $k.SetValue("", "`"$gui`" `"%1`"")
    $k.Close()
    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\Applications\arca-gui.exe\SupportedTypes")
    foreach ($ext in $Formatos.Keys) { $k.SetValue($ext, "") }
    $k.Close()

    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Arca\Capabilities")
    $k.SetValue("ApplicationName", "Arca")
    $k.SetValue("ApplicationDescription", "Archivador rapido y seguro")
    $k.Close()
    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Arca\Capabilities\FileAssociations")
    foreach ($ext in $Formatos.Keys) { $k.SetValue($ext, $Formatos[$ext].ProgId) }
    $k.Close()
    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\RegisteredApplications")
    $k.SetValue("Arca", "Software\Arca\Capabilities")
    $k.Close()
}

function Desregistrar-Asociaciones {
    foreach ($ext in $Formatos.Keys) {
        $progid = $Formatos[$ext].ProgId
        [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree("Software\Classes\$progid", $false)
        $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Software\Classes\$ext\OpenWithProgids", $true)
        if ($k) { $k.DeleteValue($progid, $false); $k.Close() }
    }
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree("Software\Classes\Applications\arca-gui.exe", $false)
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree("Software\Arca", $false)
    $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Software\RegisteredApplications", $true)
    if ($k) { $k.DeleteValue("Arca", $false); $k.Close() }
}

# Sin esto el Explorador tarda en enterarse de que las asociaciones cambiaron.
function Refrescar-Asociaciones {
    Add-Type -Namespace Shell -Name Aviso -MemberDefinition @'
[DllImport("shell32.dll")]
public static extern void SHChangeNotify(int eventId, uint flags, IntPtr a, IntPtr b);
'@ -ErrorAction SilentlyContinue
    try { [Shell.Aviso]::SHChangeNotify(0x08000000, 0x0000, [IntPtr]::Zero, [IntPtr]::Zero) } catch {}
}

# cargo escribe su progreso en stderr, incluso cuando todo va bien. Con
# ErrorActionPreference = Stop, PowerShell lo convierte en excepcion en cuanto
# alguien captura la salida del script. Lo unico que indica un fallo de verdad
# es el codigo de salida, asi que es lo que se mira.
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

function Quitar-DelPath($carpeta) {
    $actual = [Environment]::GetEnvironmentVariable("PATH", "User")
    if (-not $actual) { return }
    $limpio = ($actual -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ne $carpeta.TrimEnd('\') }) -join ';'
    if ($limpio -ne $actual) {
        [Environment]::SetEnvironmentVariable("PATH", $limpio, "User")
        Write-Host "    quitado del PATH de usuario"
    }
}

function Anadir-AlPath($carpeta) {
    $actual = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($actual -and ($actual -split ';' | Where-Object { $_.TrimEnd('\') -eq $carpeta.TrimEnd('\') })) {
        Write-Host "    ya estaba en el PATH"
        return
    }
    $nuevo = if ($actual) { $actual.TrimEnd(';') + ";" + $carpeta } else { $carpeta }
    [Environment]::SetEnvironmentVariable("PATH", $nuevo, "User")
    Write-Host "    anadido al PATH de usuario"
}

# Se usa la API de .NET y no New-Item porque una de las claves se llama «*» y
# el proveedor de registro de PowerShell la tomaria por un comodin.
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

# --- Desinstalar ------------------------------------------------------------
# Va primero y no toca el repositorio: este script se copia dentro de la
# carpeta de instalacion y es el que invoca Windows desde «Aplicaciones
# instaladas», donde el repo puede no existir ya.
if ($Quitar) {
    Write-Host "==> Desinstalando Arca" -ForegroundColor Cyan

    $paq = Get-AppxPackage $Paquete -ErrorAction SilentlyContinue
    if ($paq) {
        $Destino = $paq.InstallLocation
        $paq | Remove-AppxPackage
        Write-Host "    menu moderno desregistrado"
    }

    Desregistrar-MenuClasico
    Write-Host "    menu clasico desregistrado"

    Desregistrar-Asociaciones
    Refrescar-Asociaciones
    Write-Host "    asociaciones de archivo borradas"

    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($ClaveDesinst, $false)
    Write-Host "    entrada de «Aplicaciones instaladas» borrada"

    $acceso = Join-Path ([Environment]::GetFolderPath("Programs")) "Arca.lnk"
    if (Test-Path $acceso) {
        Remove-Item $acceso -Force
        Write-Host "    acceso directo del menú Inicio borrado"
    }

    Quitar-DelPath $Destino

    # Solo hay que salir de la carpeta si estamos dentro: Windows la mantiene
    # bloqueada y el borrado fallaria. Fuera de ese caso no se toca el
    # directorio de quien llama al script.
    if ((Get-Location).Path.StartsWith($Destino, [StringComparison]::OrdinalIgnoreCase)) {
        Set-Location $env:TEMP
    }
    if (Test-Path $Destino) {
        Remove-Item $Destino -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path $Destino) {
            Write-Host "    NOTA: no se pudo borrar $Destino (algun fichero en uso)" -ForegroundColor Yellow
        } else {
            Write-Host "    borrado $Destino"
        }
    }

    Write-Host ""
    Write-Host "Listo. Reinicia el Explorador para que desaparezca del menu:" -ForegroundColor Green
    Write-Host "    Stop-Process -Name explorer -Force"
    exit 0
}

# --- Instalar ---------------------------------------------------------------
$Raiz = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path (Join-Path $Raiz "Cargo.toml"))) {
    throw "no encuentro el repositorio en $Raiz; para desinstalar usa -Quitar"
}

$Version = ((Get-Content (Join-Path $Raiz "Cargo.toml") | Where-Object { $_ -match '^version' } | Select-Object -First 1) -split '"')[1]
Write-Host "==> Instalando Arca $Version en $Destino" -ForegroundColor Cyan

Write-Host "==> Compilando" -ForegroundColor Cyan
Push-Location $Raiz
Ejecutar "cargo" @("build","--release","--target","x86_64-pc-windows-msvc")
Pop-Location
Push-Location (Join-Path $PSScriptRoot "arca-shell")
Ejecutar "cargo" @("build","--release","--target","x86_64-pc-windows-msvc")
Pop-Location

$Exe = "$Raiz\target\x86_64-pc-windows-msvc\release\arca.exe"
$Gui = "$Raiz\target\x86_64-pc-windows-msvc\release\arca-gui.exe"
$Dll = "$PSScriptRoot\arca-shell\target\x86_64-pc-windows-msvc\release\arca_shell.dll"

Write-Host "==> Copiando" -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $Destino, "$Destino\Assets" | Out-Null
Copy-Item $Exe, $Gui, $Dll $Destino -Force
Copy-Item "$PSScriptRoot\AppxManifest.xml" $Destino -Force
Copy-Item "$PSScriptRoot\instalar.ps1" $Destino -Force
Copy-Item "$Raiz\LICENSE", "$Raiz\README.md" $Destino -Force

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

Write-Host "==> Registrando los menus" -ForegroundColor Cyan
Get-AppxPackage $Paquete -ErrorAction SilentlyContinue | Remove-AppxPackage
Add-AppxPackage -Register "$Destino\AppxManifest.xml" -ExternalLocation $Destino
Registrar-MenuClasico "$Destino\arca_shell.dll"

Write-Host "==> Asociando los formatos comprimidos" -ForegroundColor Cyan
Registrar-Asociaciones "$Destino\arca-gui.exe"
Refrescar-Asociaciones

Write-Host "==> PATH" -ForegroundColor Cyan
Anadir-AlPath $Destino

Write-Host "==> Acceso directo en el menú Inicio" -ForegroundColor Cyan
$MenuInicio = [Environment]::GetFolderPath("Programs")
$acceso = (New-Object -ComObject WScript.Shell).CreateShortcut("$MenuInicio\Arca.lnk")
$acceso.TargetPath = "$Destino\arca-gui.exe"
$acceso.WorkingDirectory = $Destino
$acceso.IconLocation = "$Destino\arca-gui.exe,0"
$acceso.Description = "Archivador rápido y seguro"
$acceso.Save()

Write-Host "==> Entrada en «Aplicaciones instaladas»" -ForegroundColor Cyan
$tam = [math]::Round((Get-ChildItem $Destino -Recurse -File | Measure-Object -Property Length -Sum).Sum / 1KB)
$desinstalar = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$Destino\instalar.ps1`" -Quitar"
$k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($ClaveDesinst)
$k.SetValue("DisplayName", "Arca")
$k.SetValue("DisplayVersion", $Version)
$k.SetValue("Publisher", "Proyecto Arca")
$k.SetValue("DisplayIcon", "$Destino\arca.exe")
$k.SetValue("InstallLocation", $Destino)
$k.SetValue("UninstallString", $desinstalar)
$k.SetValue("QuietUninstallString", $desinstalar)
$k.SetValue("EstimatedSize", [int]$tam, [Microsoft.Win32.RegistryValueKind]::DWord)
$k.SetValue("NoModify", 1, [Microsoft.Win32.RegistryValueKind]::DWord)
$k.SetValue("NoRepair", 1, [Microsoft.Win32.RegistryValueKind]::DWord)
$k.SetValue("URLInfoAbout", "https://github.com/THIONG/arca")
$k.Close()

Write-Host ""
Write-Host "Arca $Version instalado en $Destino" -ForegroundColor Green
Write-Host "  - Abre una consola NUEVA para que el PATH tenga efecto."
Write-Host "  - Reinicia el Explorador para los menus:  Stop-Process -Name explorer -Force"
Write-Host "  - Aparece en Configuracion > Aplicaciones > Aplicaciones instaladas."
