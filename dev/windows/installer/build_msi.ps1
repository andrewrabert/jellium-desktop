# WiX is a .NET global tool, installed on first use into the current user's
# tool store. v5 is pinned deliberately: v6+ gates every invocation behind the
# Open Source Maintenance Fee EULA, which is not something a build script
# should accept on the user's behalf.

[CmdletBinding()]
param(
    [ValidateSet('x64', 'arm64')]
    [string]$Arch,

    # perMachine installs to Program Files and prompts for elevation.
    # perUser installs to %LOCALAPPDATA%\Programs and never prompts.
    [ValidateSet('perMachine', 'perUser')]
    [string]$Scope = 'perMachine',

    [string]$StageDir = 'build/installer/stage',
    [string]$DistDir = 'dist',

    # Compile the app before staging, instead of reusing an existing build/.
    [switch]$Rebuild,

    # Take -StageDir exactly as-is; skip `cargo xtask install` entirely.
    [switch]$SkipStage,

    [switch]$Validate,

    # Signs the executable before it is packed, then the .msi itself.
    # Without a thumbprint both steps are skipped.
    [string]$SignThumbprint,
    [string]$TimestampUrl = 'http://timestamp.digicert.com',

    [string]$WixVersion = '5.0.2'
)

$ErrorActionPreference = 'Stop'
$RepoRoot = (Get-Item $PSScriptRoot).Parent.Parent.Parent.FullName
$WorkDir = Join-Path $RepoRoot 'build\installer'

function Resolve-RepoPath([string]$Path) {
    if ([System.IO.Path]::IsPathRooted($Path)) { return $Path }
    return [System.IO.Path]::GetFullPath((Join-Path $RepoRoot $Path))
}

$StageDir = Resolve-RepoPath $StageDir
$DistDir = Resolve-RepoPath $DistDir

if (-not $Arch) {
    $Arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
}

Write-Host "=== Jellium Desktop installer ===" -ForegroundColor Cyan
Write-Host "Architecture: $Arch"
Write-Host "Scope:        $Scope"
Write-Host ""

function Initialize-Wix {
    $ToolPath = Join-Path $env:USERPROFILE '.dotnet\tools'
    if (Test-Path $ToolPath -PathType Container) {
        if (($env:PATH -split ';') -notcontains $ToolPath) { $env:PATH = "$ToolPath;$env:PATH" }
    }

    $Installed = $null
    if (Get-Command wix -ErrorAction SilentlyContinue) {
        $Installed = (& wix --version) -replace '\+.*', ''
    }
    if ($Installed -ne $WixVersion) {
        if (-not (Get-Command dotnet -ErrorAction SilentlyContinue)) {
            throw "The .NET SDK is required to install the WiX Toolset. Install it with: winget install Microsoft.DotNet.SDK.8"
        }
        if ($Installed) {
            Write-Host "Replacing WiX $Installed with $WixVersion..." -ForegroundColor Yellow
            & dotnet tool uninstall --global wix | Out-Null
        } else {
            Write-Host "Installing WiX Toolset $WixVersion..." -ForegroundColor Yellow
        }
        & dotnet tool install --global wix --version $WixVersion
        if ($LASTEXITCODE -ne 0) { throw "dotnet tool install wix failed" }
        if (Test-Path $ToolPath -PathType Container) {
            if (($env:PATH -split ';') -notcontains $ToolPath) { $env:PATH = "$ToolPath;$env:PATH" }
        }
    }

    # Extensions are cached per-user and versioned in lockstep with the tool.
    $Extensions = @('WixToolset.UI.wixext', 'WixToolset.Util.wixext')
    $Cached = @(& wix extension list --global) -join "`n"
    foreach ($Ext in $Extensions) {
        if ($Cached -notmatch [regex]::Escape("$Ext $WixVersion")) {
            Write-Host "Adding WiX extension $Ext/$WixVersion..." -ForegroundColor Yellow
            & wix extension add --global "$Ext/$WixVersion"
            if ($LASTEXITCODE -ne 0) { throw "wix extension add $Ext failed" }
        }
    }
}

function Invoke-Stage {
    # Mirrors dev/windows/build.ps1: prefer the meson install tree produced by
    # build_mpv_source.ps1, fall back to an in-tree submodule build.
    $XtaskArgs = @('xtask', 'install', '--prefix', $StageDir)
    foreach ($Candidate in @('third_party\mpv-install', 'third_party\mpv')) {
        $Dir = Join-Path $RepoRoot $Candidate
        if (Test-Path (Join-Path $Dir 'lib\mpv.lib')) {
            $XtaskArgs += "--external-mpv=$Dir"
            break
        }
    }
    if (-not $Rebuild) {
        if (-not (Test-Path (Join-Path $RepoRoot 'build\jellium-desktop.exe'))) {
            throw "build\jellium-desktop.exe not found. Run 'just build' first, or pass -Rebuild."
        }
        $XtaskArgs += '--skip-build'
    }

    # Wiping keeps a removed file from lingering in the payload, but -StageDir
    # is user-supplied, so guard it.
    $Parent = Split-Path -Parent $StageDir
    if (-not $Parent -or $StageDir -eq $RepoRoot) {
        throw "refusing to stage into $StageDir"
    }
    if (Test-Path $StageDir) { Remove-Item -Recurse -Force $StageDir }
    New-Item -ItemType Directory -Force -Path $StageDir | Out-Null

    Write-Host "Staging payload into $StageDir..." -ForegroundColor Cyan
    Push-Location $RepoRoot
    try {
        # Same MSVC + bindgen setup dev/windows/build.ps1 relies on; a no-op
        # inside a Developer Command Prompt or the CI job.
        . (Join-Path $PSScriptRoot '..\env.ps1')
        & cargo @XtaskArgs
        if ($LASTEXITCODE -ne 0) { throw "cargo xtask install failed" }
    } finally {
        Pop-Location
    }
}

function Test-Stage {
    if (-not (Test-Path (Join-Path $StageDir 'jellium-desktop.exe'))) {
        throw "$StageDir does not contain jellium-desktop.exe"
    }
    foreach ($Required in @('libcef.dll', 'locales')) {
        if (-not (Test-Path (Join-Path $StageDir $Required))) {
            throw "$StageDir is missing $Required - the CEF runtime was not staged"
        }
    }
    if (-not (Get-ChildItem $StageDir -Filter 'libmpv-2.dll' -ErrorAction SilentlyContinue)) {
        Write-Host "warning: libmpv-2.dll is not in the payload; playback will fail" -ForegroundColor Yellow
    }
}

# WixUI reads the license from an RTF stream, so the plain-text LICENSE has to
# be wrapped.
function Write-LicenseRtf([string]$Source, [string]$Destination) {
    $Builder = [System.Text.StringBuilder]::new()
    [void]$Builder.Append('{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0\fswiss\fcharset0 Segoe UI;}}')
    [void]$Builder.Append("\viewkind4\uc1\pard\f0\fs18`r`n")
    foreach ($Line in [System.IO.File]::ReadAllLines($Source)) {
        foreach ($Char in $Line.ToCharArray()) {
            $Code = [int]$Char
            if ($Char -eq '\' -or $Char -eq '{' -or $Char -eq '}') {
                [void]$Builder.Append('\').Append($Char)
            } elseif ($Code -lt 128) {
                [void]$Builder.Append($Char)
            } elseif ($Code -lt 256) {
                [void]$Builder.AppendFormat("\'{0:x2}", $Code)
            } else {
                [void]$Builder.AppendFormat('\u{0}?', $Code)
            }
        }
        [void]$Builder.Append("\par`r`n")
    }
    [void]$Builder.Append('}')
    [System.IO.File]::WriteAllText($Destination, $Builder.ToString(), [System.Text.Encoding]::ASCII)
}

# WixUI composites its dialog text directly over these bitmaps, so anything it
# writes into has to stay light. MSI cannot render alpha, hence flat 24bpp.
function Write-WizardBitmaps([string]$IconPath, [string]$BannerPath, [string]$DialogPath) {
    Add-Type -AssemblyName System.Drawing

    $Accent = [System.Drawing.Color]::FromArgb(0, 164, 220)

    # System.Drawing.Icon silently ignores PNG-compressed frames and hands back
    # the largest legacy DIB (48x48 here), which looks mushy once scaled. Pick
    # the biggest frame out of the ICONDIR by hand instead.
    function Get-LogoImage([string]$Path) {
        $Bytes = [System.IO.File]::ReadAllBytes($Path)
        $Best = $null
        foreach ($Index in 0..([BitConverter]::ToUInt16($Bytes, 4) - 1)) {
            $Entry = 6 + $Index * 16
            $Width = if ($Bytes[$Entry] -eq 0) { 256 } else { [int]$Bytes[$Entry] }
            if (-not $Best -or $Width -gt $Best.Width) {
                $Best = [pscustomobject]@{
                    Width  = $Width
                    Length = [BitConverter]::ToUInt32($Bytes, $Entry + 8)
                    Offset = [BitConverter]::ToUInt32($Bytes, $Entry + 12)
                }
            }
        }
        if ($Bytes[$Best.Offset] -ne 0x89) {
            $Icon = [System.Drawing.Icon]::new($Path, $Best.Width, $Best.Width)
            try { return $Icon.ToBitmap() } finally { $Icon.Dispose() }
        }
        $Stream = [System.IO.MemoryStream]::new($Bytes, [int]$Best.Offset, [int]$Best.Length)
        try {
            $Png = [System.Drawing.Image]::FromStream($Stream)
            try { return [System.Drawing.Bitmap]::new($Png) } finally { $Png.Dispose() }
        } finally {
            $Stream.Dispose()
        }
    }

    function New-Canvas([int]$Width, [int]$Height) {
        $Bitmap = [System.Drawing.Bitmap]::new($Width, $Height, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
        $Graphics = [System.Drawing.Graphics]::FromImage($Bitmap)
        $Graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $Graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $Graphics.Clear([System.Drawing.Color]::White)
        return [pscustomobject]@{ Bitmap = $Bitmap; Graphics = $Graphics }
    }

    function Save-Canvas($Canvas, [string]$Path) {
        $Canvas.Graphics.Dispose()
        $Canvas.Bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Bmp)
        $Canvas.Bitmap.Dispose()
    }

    function Add-Fill($Canvas, [System.Drawing.Brush]$Brush, [int]$X, [int]$Y, [int]$W, [int]$H) {
        try { $Canvas.Graphics.FillRectangle($Brush, $X, $Y, $W, $H) } finally { $Brush.Dispose() }
    }

    $Logo = Get-LogoImage $IconPath
    try {
        # Banner: 493x58. WixUI draws the dialog title over the left half.
        $Banner = New-Canvas 493 58
        $Banner.Graphics.DrawImage($Logo, 431, 5, 48, 48)
        Add-Fill $Banner ([System.Drawing.SolidBrush]::new($Accent)) 0 56 493 2
        Save-Canvas $Banner $BannerPath

        # Welcome/exit background: 493x312. Body text starts around x=180, so
        # only the gutter left of it may be tinted.
        $Dialog = New-Canvas 493 312
        Add-Fill $Dialog ([System.Drawing.Drawing2D.LinearGradientBrush]::new(
                [System.Drawing.Rectangle]::new(0, 0, 164, 312),
                [System.Drawing.Color]::FromArgb(16, 24, 35),
                [System.Drawing.Color]::FromArgb(32, 52, 78),
                [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)) 0 0 164 312
        Add-Fill $Dialog ([System.Drawing.SolidBrush]::new($Accent)) 162 0 2 312
        $Dialog.Graphics.DrawImage($Logo, 34, 96, 96, 96)
        Save-Canvas $Dialog $DialogPath
    } finally {
        $Logo.Dispose()
    }
}

function Get-SignTool {
    $Cmd = Get-Command signtool -ErrorAction SilentlyContinue
    if ($Cmd) { return $Cmd.Source }
    $Roots = @("${env:ProgramFiles(x86)}\Windows Kits\10\bin", "$env:ProgramFiles\Windows Kits\10\bin")
    $HostDir = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
    $Found = Get-ChildItem -Path $Roots -Filter signtool.exe -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.DirectoryName -like "*\$HostDir" } |
        Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $Found) { throw "signtool.exe not found; install the Windows SDK signing tools" }
    return $Found.FullName
}

function Invoke-Sign([string]$Path) {
    if (-not $SignThumbprint) { return }
    $SignTool = Get-SignTool
    Write-Host "Signing $(Split-Path -Leaf $Path)..." -ForegroundColor Cyan
    & $SignTool sign /sha1 $SignThumbprint /fd SHA256 /td SHA256 /tr $TimestampUrl /d 'Jellium Desktop' $Path
    if ($LASTEXITCODE -ne 0) { throw "signtool failed for $Path" }
}

Initialize-Wix

if ($SkipStage) {
    Write-Host "Reusing staged payload at $StageDir" -ForegroundColor Cyan
} else {
    Invoke-Stage
}
Test-Stage
Invoke-Sign (Join-Path $StageDir 'jellium-desktop.exe')

Push-Location $RepoRoot
try {
    $FullVersion = (& cargo xtask version)
    if ($LASTEXITCODE -ne 0) { throw "cargo xtask version failed" }
} finally {
    Pop-Location
}
$FullVersion = $FullVersion.Trim()

# ProductVersion must be strictly numeric, so drop any -pre / +sha suffix. The
# untruncated string still shows up in Add/Remove Programs and the file name.
if ($FullVersion -match '^(\d+)\.(\d+)\.(\d+)') {
    $ProductVersion = "$($Matches[1]).$($Matches[2]).$($Matches[3])"
} else {
    throw "cannot derive an MSI version from '$FullVersion'"
}
Write-Host "Version: $FullVersion (MSI ProductVersion $ProductVersion)"

New-Item -ItemType Directory -Force -Path $WorkDir, $DistDir | Out-Null
$LicenseRtf = Join-Path $WorkDir 'license.rtf'
$BannerBmp = Join-Path $WorkDir 'banner.bmp'
$DialogBmp = Join-Path $WorkDir 'dialog.bmp'
$IconFile = Join-Path $RepoRoot 'resources\win\jellyfin.ico'

Write-LicenseRtf (Join-Path $RepoRoot 'LICENSE') $LicenseRtf
Write-WizardBitmaps $IconFile $BannerBmp $DialogBmp

$MsiPath = Join-Path $DistDir "JelliumDesktop-$FullVersion-windows-$Arch.msi"
if (Test-Path $MsiPath) { Remove-Item -Force $MsiPath }

Write-Host "Compiling installer (this compresses the whole CEF payload)..." -ForegroundColor Cyan
& wix build `
    -arch $Arch `
    -ext WixToolset.UI.wixext `
    -ext WixToolset.Util.wixext `
    -d "StageDir=$StageDir" `
    -d "ProductVersion=$ProductVersion" `
    -d "FullVersion=$FullVersion" `
    -d "Scope=$Scope" `
    -d "IconFile=$IconFile" `
    -d "LicenseRtf=$LicenseRtf" `
    -d "BannerBmp=$BannerBmp" `
    -d "DialogBmp=$DialogBmp" `
    -intermediateFolder (Join-Path $WorkDir "obj-$Arch") `
    -pdb (Join-Path $WorkDir "jellium-desktop-$Arch.wixpdb") `
    -out $MsiPath `
    (Join-Path $PSScriptRoot 'jellium-desktop.wxs')
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }

Invoke-Sign $MsiPath

if ($Validate) {
    # ICE61 is the documented side effect of AllowSameVersionUpgrades: the
    # upgrade range's ceiling equals the product's own version, on purpose.
    $Suppressed = @('ICE61')
    if ($Scope -eq 'perUser') {
        # ICE38/64/91 all boil down to "this component lives in the user
        # profile", which is exactly what a per-user install is.
        $Suppressed += 'ICE38', 'ICE64', 'ICE91'
    }
    Write-Host "Validating (suppressing $($Suppressed -join ', '))..." -ForegroundColor Cyan
    $SuppressArgs = $Suppressed | ForEach-Object { '-sice', $_ }
    & wix msi validate $MsiPath $SuppressArgs
    if ($LASTEXITCODE -ne 0) { throw "ICE validation failed" }
}

$SizeMb = [math]::Round((Get-Item $MsiPath).Length / 1MB, 1)
Write-Host ""
Write-Host "Installer: $MsiPath ($SizeMb MB)" -ForegroundColor Green
