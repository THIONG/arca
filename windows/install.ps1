# Installs Arca for the current user: the binaries, the context menu extension
# and an entry under "Installed apps".
#
# Not the same as build.ps1. That one compiles and registers straight from the
# working folder, for development. This one copies to a stable location outside
# the repository, so that a `cargo clean` or moving the project breaks nothing.
#
# Everything goes under HKCU and LOCALAPPDATA: no administrator needed.
#
# Usage:  .\install.ps1              installs
#         .\install.ps1 -Remove      uninstalls
#         .\install.ps1 -Dest D:\Arca

param(
    [switch]$Remove,
    [string]$Dest = "$env:LOCALAPPDATA\Programs\Arca"
)

$ErrorActionPreference = "Stop"

$Package      = "Arca.Archivador"
$ClassicClsid = "{B528A7F3-C889-4C98-B052-5D7F7F778E14}"
$MenuTypes    = @("*", "Directory")
$UninstallKey = "Software\Microsoft\Windows\CurrentVersion\Uninstall\Arca"

# Extensions Arca can open. Each needs its own ProgID: Windows maps the
# extension to a ProgID, and the ProgID to a command.
$Formats = @{
    ".zip"    = @{ ProgId = "Arca.zip";    Text = "ZIP archive" }
    ".tar"    = @{ ProgId = "Arca.tar";    Text = "TAR archive" }
    ".gz"     = @{ ProgId = "Arca.targz";  Text = "Compressed TAR archive" }
    ".tgz"    = @{ ProgId = "Arca.tgz";    Text = "Compressed TAR archive" }
}

# Three separate registrations, and all three are needed:
#   1. The ProgID, which says what command opens it.
#   2. OpenWithProgids on the extension, which is what fills "Open with".
#   3. Capabilities + RegisteredApplications, which is what makes Arca show up
#      under Settings > Default apps.
function Register-Associations($gui) {
    foreach ($ext in $Formats.Keys) {
        $progid = $Formats[$ext].ProgId
        $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Classes\$progid")
        $k.SetValue("", $Formats[$ext].Text)
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
    foreach ($ext in $Formats.Keys) { $k.SetValue($ext, "") }
    $k.Close()

    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Arca\Capabilities")
    $k.SetValue("ApplicationName", "Arca")
    $k.SetValue("ApplicationDescription", "Fast, safe archiver")
    $k.Close()
    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\Arca\Capabilities\FileAssociations")
    foreach ($ext in $Formats.Keys) { $k.SetValue($ext, $Formats[$ext].ProgId) }
    $k.Close()
    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\RegisteredApplications")
    $k.SetValue("Arca", "Software\Arca\Capabilities")
    $k.Close()
}

function Unregister-Associations {
    foreach ($ext in $Formats.Keys) {
        $progid = $Formats[$ext].ProgId
        [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree("Software\Classes\$progid", $false)
        $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Software\Classes\$ext\OpenWithProgids", $true)
        if ($k) { $k.DeleteValue($progid, $false); $k.Close() }
    }
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree("Software\Classes\Applications\arca-gui.exe", $false)
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree("Software\Arca", $false)
    $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Software\RegisteredApplications", $true)
    if ($k) { $k.DeleteValue("Arca", $false); $k.Close() }
}

# Without this Explorer takes a while to notice the associations changed.
function Update-Associations {
    Add-Type -Namespace Shell -Name Notify -MemberDefinition @'
[DllImport("shell32.dll")]
public static extern void SHChangeNotify(int eventId, uint flags, IntPtr a, IntPtr b);
'@ -ErrorAction SilentlyContinue
    try { [Shell.Notify]::SHChangeNotify(0x08000000, 0x0000, [IntPtr]::Zero, [IntPtr]::Zero) } catch {}
}

# cargo writes its progress to stderr even when everything is fine. With
# ErrorActionPreference = Stop, PowerShell turns that into an exception as soon
# as anyone captures the script's output. The exit code is the only thing that
# means a real failure, so that is what gets looked at.
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

function Remove-FromPath($folder) {
    $current = [Environment]::GetEnvironmentVariable("PATH", "User")
    if (-not $current) { return }
    $clean = ($current -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ne $folder.TrimEnd('\') }) -join ';'
    if ($clean -ne $current) {
        [Environment]::SetEnvironmentVariable("PATH", $clean, "User")
        Write-Host "    removed from the user PATH"
    }
}

function Add-ToPath($folder) {
    $current = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($current -and ($current -split ';' | Where-Object { $_.TrimEnd('\') -eq $folder.TrimEnd('\') })) {
        Write-Host "    already on the PATH"
        return
    }
    $updated = if ($current) { $current.TrimEnd(';') + ";" + $folder } else { $folder }
    [Environment]::SetEnvironmentVariable("PATH", $updated, "User")
    Write-Host "    added to the user PATH"
}

# The .NET API and not New-Item, because one of the keys is named "*" and the
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

# --- Uninstall --------------------------------------------------------------
# This comes first and touches no part of the repository: the script copies
# itself into the install folder, and that copy is what Windows invokes from
# "Installed apps", where the repo may no longer exist.
if ($Remove) {
    Write-Host "==> Uninstalling Arca" -ForegroundColor Cyan

    $pkg = Get-AppxPackage $Package -ErrorAction SilentlyContinue
    if ($pkg) {
        $Dest = $pkg.InstallLocation
        $pkg | Remove-AppxPackage
        Write-Host "    modern menu unregistered"
    }

    Unregister-ClassicMenu
    Write-Host "    classic menu unregistered"

    Unregister-Associations
    Update-Associations
    Write-Host "    file associations deleted"

    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($UninstallKey, $false)
    Write-Host "    'Installed apps' entry deleted"

    $shortcut = Join-Path ([Environment]::GetFolderPath("Programs")) "Arca.lnk"
    if (Test-Path $shortcut) {
        Remove-Item $shortcut -Force
        Write-Host "    Start menu shortcut deleted"
    }

    Remove-FromPath $Dest

    # Only step out of the folder if we are inside it: Windows keeps it locked
    # and the delete would fail. Outside that case the caller's directory is
    # left alone.
    if ((Get-Location).Path.StartsWith($Dest, [StringComparison]::OrdinalIgnoreCase)) {
        Set-Location $env:TEMP
    }
    if (Test-Path $Dest) {
        Remove-Item $Dest -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path $Dest) {
            Write-Host "    NOTE: could not delete $Dest (some file in use)" -ForegroundColor Yellow
        } else {
            Write-Host "    deleted $Dest"
        }
    }

    Write-Host ""
    Write-Host "Done. Restart Explorer so it disappears from the menu:" -ForegroundColor Green
    Write-Host "    Stop-Process -Name explorer -Force"
    exit 0
}

# --- Install ----------------------------------------------------------------
$Root = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path (Join-Path $Root "Cargo.toml"))) {
    throw "cannot find the repository at $Root; to uninstall use -Remove"
}

$Version = ((Get-Content (Join-Path $Root "Cargo.toml") | Where-Object { $_ -match '^version' } | Select-Object -First 1) -split '"')[1]
Write-Host "==> Installing Arca $Version into $Dest" -ForegroundColor Cyan

Write-Host "==> Building" -ForegroundColor Cyan
Push-Location $Root
Invoke-Tool "cargo" @("build","--release","--target","x86_64-pc-windows-msvc")
Pop-Location
Push-Location (Join-Path $PSScriptRoot "arca-shell")
Invoke-Tool "cargo" @("build","--release","--target","x86_64-pc-windows-msvc")
Pop-Location

$Exe = "$Root\target\x86_64-pc-windows-msvc\release\arca.exe"
$Gui = "$Root\target\x86_64-pc-windows-msvc\release\arca-gui.exe"
$Dll = "$PSScriptRoot\arca-shell\target\x86_64-pc-windows-msvc\release\arca_shell.dll"

Write-Host "==> Copying" -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $Dest, "$Dest\Assets" | Out-Null
Copy-Item $Exe, $Gui, $Dll $Dest -Force
Copy-Item "$PSScriptRoot\AppxManifest.xml" $Dest -Force
Copy-Item "$PSScriptRoot\install.ps1" $Dest -Force
Copy-Item "$Root\LICENSE", "$Root\README.md" $Dest -Force

# Real package icons now. These used to be 1x1 PNGs generated here just so the
# manifest would validate; they come from the brand, in windows\assets.
Copy-Item "$PSScriptRoot\assets\*.png" "$Dest\Assets" -Force

Write-Host "==> Registering the menus" -ForegroundColor Cyan
Get-AppxPackage $Package -ErrorAction SilentlyContinue | Remove-AppxPackage
Add-AppxPackage -Register "$Dest\AppxManifest.xml" -ExternalLocation $Dest
Register-ClassicMenu "$Dest\arca_shell.dll"

Write-Host "==> Associating the archive formats" -ForegroundColor Cyan
Register-Associations "$Dest\arca-gui.exe"
Update-Associations

Write-Host "==> PATH" -ForegroundColor Cyan
Add-ToPath $Dest

Write-Host "==> Start menu shortcut" -ForegroundColor Cyan
$StartMenu = [Environment]::GetFolderPath("Programs")
$shortcut = (New-Object -ComObject WScript.Shell).CreateShortcut("$StartMenu\Arca.lnk")
$shortcut.TargetPath = "$Dest\arca-gui.exe"
$shortcut.WorkingDirectory = $Dest
$shortcut.IconLocation = "$Dest\arca-gui.exe,0"
$shortcut.Description = "Fast, safe archiver"
$shortcut.Save()

Write-Host "==> 'Installed apps' entry" -ForegroundColor Cyan
$size = [math]::Round((Get-ChildItem $Dest -Recurse -File | Measure-Object -Property Length -Sum).Sum / 1KB)
$uninstall = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$Dest\install.ps1`" -Remove"
$k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($UninstallKey)
$k.SetValue("DisplayName", "Arca")
$k.SetValue("DisplayVersion", $Version)
$k.SetValue("Publisher", "Proyecto Arca")
$k.SetValue("DisplayIcon", "$Dest\arca.exe")
$k.SetValue("InstallLocation", $Dest)
$k.SetValue("UninstallString", $uninstall)
$k.SetValue("QuietUninstallString", $uninstall)
$k.SetValue("EstimatedSize", [int]$size, [Microsoft.Win32.RegistryValueKind]::DWord)
$k.SetValue("NoModify", 1, [Microsoft.Win32.RegistryValueKind]::DWord)
$k.SetValue("NoRepair", 1, [Microsoft.Win32.RegistryValueKind]::DWord)
$k.SetValue("URLInfoAbout", "https://github.com/THIONG/arca")
$k.Close()

Write-Host ""
Write-Host "Arca $Version installed into $Dest" -ForegroundColor Green
Write-Host "  - Open a NEW console so the PATH takes effect."
Write-Host "  - Restart Explorer for the menus:  Stop-Process -Name explorer -Force"
Write-Host "  - It shows up under Settings > Apps > Installed apps."
