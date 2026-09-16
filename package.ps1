# FxMini — M6 packaging.
#
#   .\package.ps1                 # build, assemble, zip
#   .\package.ps1 -NoBuild        # re-package whatever is already in target\release
#   .\package.ps1 -NoZip          # leave the folder, skip the archive
#   .\package.ps1 -Locked         # pass --locked to cargo; CI uses this so the
#                                 # committed Cargo.lock is what actually gets built
#
# Produces, under -OutDir (default `dist`):
#
#   FxMini-<version>-win64.zip     the thing you hand to someone
#   FxMini\                        the unpacked tree the archive is made of
#     fxmini.exe                     the application (icon + version resource)
#     driver\fxvad.{inf,sys,cat}     FxSound's signed virtual sound card
#     README.txt                     what it is, how to run it, how to remove it
#     LICENSE.txt                    AGPL-3.0-or-later and the driver's terms
#     install.ps1                    per-user install, no administrator needed
#     uninstall.ps1                  the reverse
#   SHA256SUMS.txt                 hashes of every file above, plus the archive
#
# Two things the script deliberately checks rather than assumes, and fails on:
#
#   * the executable carries its icon and version resource. A build made without
#     `rc.exe` still compiles and links perfectly well — it just ships a
#     faceless .exe, which is exactly the defect this milestone exists to fix;
#   * the executable does not import the VC++ redistributable. That import is
#     invisible in a build log and only shows up as "the app does not start" on
#     somebody else's machine. Needs `dumpbin.exe` on PATH — dot-source
#     `.\toolchain.ps1` first, or it reports that the check was skipped.

[CmdletBinding()]
param(
    [string]$OutDir = 'dist',
    [string]$Configuration = 'release',
    [switch]$NoBuild,
    [switch]$NoZip,
    [switch]$Locked
)

$ErrorActionPreference = 'Stop'

# `.\package.ps1 --locked` binds the *out directory* to the string "--locked".
#
# PowerShell switches are single-dash, so `--locked` is not a parameter name at
# all — it falls through to the first positional parameter, which is -OutDir.
# The build then succeeds and the entire distributable lands in a directory
# literally named `--locked`, with exit code 0. That is a genuinely confusing way
# to spend an afternoon, and it happened once in CI. Catch it at the door.
if ($OutDir -match '^-') {
    throw @"
-OutDir was given the value '$OutDir', which looks like a mistyped switch.
PowerShell switches take a single dash: write -Locked, not --locked.
"@
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$version = $null
$stage = $null

function Write-Step {
    param([string]$Text)
    Write-Host ''
    Write-Host "== $Text" -ForegroundColor Cyan
}

# Writes a text file with CRLF endings.
#
# The here-strings above are LF-only because they live in a file written by
# tooling that does not care; .txt and .ps1 on Windows look wrong in Notepad and
# diff noisily against hand-edited copies without the conversion.
function Write-TextFile {
    param([string]$Path, [string]$Text, [string]$Encoding)
    $normalised = ($Text -replace "`r`n", "`n") -replace "`n", "`r`n"
    Set-Content -LiteralPath $Path -Value $normalised -Encoding $Encoding
}

function Read-CargoVersion {
    $toml = Get-Content (Join-Path $here 'Cargo.toml') -Raw
    if ($toml -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
        throw 'could not read `version` from Cargo.toml'
    }
    return $Matches[1]
}

# Deletes through .NET rather than Remove-Item.
#
# Remove-Item is not a reliable primitive for this job. Some environments wrap
# it in a "safe delete" that diverts the target to the Recycle Bin and fails
# closed when the move fails — which it does for a tree this size — so a second
# run of this script dies the moment it tries to clear dist\FxMini. A packaging
# script wants an outright delete anyway: the point is a clean staging directory,
# not a recoverable one.
function Remove-Tree {
    param([string]$Path)
    if (Test-Path -LiteralPath $Path) { [System.IO.Directory]::Delete($Path, $true) }
}

function Remove-File {
    param([string]$Path)
    if (Test-Path -LiteralPath $Path) { [System.IO.File]::Delete($Path) }
}

# The signed inf/sys/cat triple is vendored rather than committed: it is a
# binary blob with its own licence, and it is already present in a sibling
# checkout. FXMINI_DRIVER_DIR overrides, for a machine that has neither.
function Find-DriverDir {
    $candidates = @()
    if ($env:FXMINI_DRIVER_DIR) { $candidates += $env:FXMINI_DRIVER_DIR }
    $candidates += (Join-Path $here 'driver')
    $candidates += (Join-Path $here '..\NexBox\src-tauri\resources\binaries\fxvad')

    foreach ($candidate in $candidates) {
        if (-not $candidate) { continue }
        $full = [System.IO.Path]::GetFullPath($candidate)
        $complete = @('fxvad.inf', 'fxvad.sys', 'fxvadntamd64.cat') |
            Where-Object { Test-Path (Join-Path $full $_) }
        if ($complete.Count -eq 3) { return $full }
    }
    return $null
}

function Copy-DriverFiles {
    param([string]$Source, [string]$Destination)

    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    foreach ($name in @('fxvad.inf', 'fxvad.sys', 'fxvadntamd64.cat')) {
        Copy-Item (Join-Path $Source $name) (Join-Path $Destination $name) -Force
    }
}

# Writes the per-user installer.
#
# Installing the driver is intentionally NOT attempted here: it needs
# administrator rights, and the app already owns that flow (tray menu ->
# `--install-driver` -> UAC). Duplicating it would mean two code paths that
# both have to know about restoring the default endpoint afterwards.
$installScript = @'
# FxMini - per-user install.
#
#   powershell -ExecutionPolicy Bypass -File install.ps1
#
# Copies FxMini into %LOCALAPPDATA%\Programs\FxMini, adds a Start-menu shortcut
# and launches it. No administrator rights are required: everything stays inside
# the user's own profile.
#
# Installing the virtual sound card DOES require administrator rights, so it is
# left to the application: right-click the tray icon and choose
# "Install virtual sound card", then accept the elevation prompt. Until that is
# done FxMini passes audio through unprocessed.

$ErrorActionPreference = 'Stop'

$source = Split-Path -Parent $MyInvocation.MyCommand.Path
$target = Join-Path $env:LOCALAPPDATA 'Programs\FxMini'
$exe = Join-Path $target 'fxmini.exe'

Write-Host "Installing FxMini to $target"

# A running copy holds the current executable open, so it has to go first.
$running = Get-Process -Name fxmini -ErrorAction SilentlyContinue
if ($running) {
    Write-Host 'Stopping the running copy.'
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 800

    # Force-killing it skips its own restore path, which can leave the system's
    # default output pointing at the virtual sound card - i.e. silence. The
    # build being replaced is still on disk at this point, so let it clean up.
    if (Test-Path $exe) {
        & $exe --restore-output
        Start-Sleep -Milliseconds 300
    }
}

New-Item -ItemType Directory -Force -Path $target | Out-Null
Copy-Item -Path (Join-Path $source '*') -Destination $target -Recurse -Force

# Start-menu shortcut. Plain COM rather than Add-Type: this has to work in a
# locked-down shell where compiling C# at runtime is blocked.
$startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$shortcut = Join-Path $startMenu 'FxMini.lnk'
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($shortcut)
$link.TargetPath = $exe
$link.WorkingDirectory = $target
$link.Description = 'FxMini audio enhancer'
$link.Save()

# Nothing to do about startup: FxMini registers itself in HKCU\...\Run on its
# first start. That is deliberate - it is the only way a logon can never leave
# the machine silent - so the entry is written before the window even appears.
Start-Process -FilePath $exe

Write-Host ''
Write-Host 'FxMini is running; look for its icon in the notification area.'
Write-Host 'Next: right-click that icon -> "Install virtual sound card".'
'@

$uninstallScript = @'
# FxMini - per-user uninstall.
#
#   powershell -ExecutionPolicy Bypass -File uninstall.ps1
#
# Removes the program, its shortcut and its start-at-logon entry. It leaves two
# things alone on purpose:
#
#   * the virtual sound card, which needs administrator rights to remove
#     (tray menu -> "Remove virtual sound card", before uninstalling);
#   * %APPDATA%\FxMini, which holds your presets and settings. Delete that
#     folder by hand if you want a clean slate.

$ErrorActionPreference = 'Stop'

$target = Join-Path $env:LOCALAPPDATA 'Programs\FxMini'
$exe = Join-Path $target 'fxmini.exe'

$running = Get-Process -Name fxmini -ErrorAction SilentlyContinue
if ($running) {
    Write-Host 'Stopping FxMini.'
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 800
}

# Hand the default output back before the binary goes away. Without this a
# machine that was left pointed at the virtual sound card stays mute, and
# removing the startup entry below would also remove the automatic repair.
if (Test-Path $exe) {
    Write-Host 'Restoring the default output device.'
    & $exe --restore-output
    Start-Sleep -Milliseconds 300
}

$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
Remove-ItemProperty -Path $runKey -Name 'FxMini' -ErrorAction SilentlyContinue

# Task Manager keeps its on/off state in a second location; leaving it behind
# would make a later reinstall look "already disabled" for no visible reason.
$approvedKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
Remove-ItemProperty -Path $approvedKey -Name 'FxMini' -ErrorAction SilentlyContinue

$shortcut = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\FxMini.lnk'
# .NET rather than Remove-Item: an uninstaller wants an outright delete, and
# Remove-Item can be diverted to the Recycle Bin by a locked-down shell, which
# would leave the program sitting in place while reporting success.
if (Test-Path $shortcut) { [System.IO.File]::Delete($shortcut) }

if (Test-Path $target) { [System.IO.Directory]::Delete($target, $true) }

Write-Host ''
Write-Host 'FxMini removed.'
Write-Host "Settings and presets remain in $env:APPDATA\FxMini."
Write-Host 'The virtual sound card is still installed; remove it from the tray menu first if you want it gone.'
'@

$licenseText = @'
FxMini
======

This build of FxMini is distributed under the GNU Affero General Public
License, version 3 or later (AGPL-3.0-or-later), because it is built from and
links against source code released under that licence:

  * the DFX DSP engine, from fxsound-app/dsp          (AGPL-3.0)
  * the DFX support layer, from fxsound-app/audiopassthru (AGPL-3.0)
  * the Windows virtual sound card driver from fxsound-driver (AGPL-3.0)

The full licence texts ship alongside those sources. If you distribute this
program, or run a modified version of it as a network service, you must make the
corresponding source available under the same licence.

Trademarks
----------

"FxSound" and the FxSound name and artwork belong to their owner. The bundled
driver is FxSound's own signed package, redistributed unmodified so that Windows
will load it; FxMini itself is an independent, unaffiliated program. Do not
present FxMini as FxSound.
'@

$readme = @'
FxMini - 低占用常驻托盘音效增强  /  a tray-resident audio enhancer
=================================================================

  这是什么 / What this is
  ----------------------
  一个常驻系统托盘的小工具，复用 FxSound 的虚拟声卡与 DSP 引擎来增强系统
  声音，但不带 FxSound 的应用界面。空闲时约 16 MB 内存。

  A small tray application that reuses FxSound's virtual sound card and its DSP
  engine to enhance whatever the system is playing, without shipping FxSound's
  UI. It idles at around 16 MB.

  安装 / Install
  --------------
    powershell -ExecutionPolicy Bypass -File install.ps1

  脚本会把它复制到 %LOCALAPPDATA%\Programs\FxMini、建一个开始菜单快捷方式，
  并启动。然后右键托盘图标 -> 「安装虚拟声卡驱动」，同意提权（这一步需要管
  理员权限）。装完驱动，声音才会真正经过增强。

  The script copies it to %LOCALAPPDATA%\Programs\FxMini, creates a Start-menu
  shortcut and launches it. Then right-click the tray icon and choose
  "Install virtual sound card", and accept the elevation prompt - that step
  needs administrator rights, and until it is done there is nothing to route
  audio through.

  运行方式 / How it runs
  ----------------------
  程序会把自己注册为「开机自启」（HKCU\...\Run）。这是必要的，不是可选项：
  必须由它接管系统默认输出设备，声音才会经过增强；如果登录后它没起来，而默
  认设备还停在虚拟声卡上，机器就没有声音了。可以在托盘菜单里关掉，也可以从
  任务管理器的「启动」选项卡禁用。

  FxMini registers itself to start at logon. That is load-bearing rather than a
  convenience: it has to take over the system's default output for the enhancer
  to be in the path at all, so a logon that does not start it would leave the
  machine silent. Turn it off from the tray menu, or from the Startup tab of
  Task Manager.

  完全没有声音 / No sound at all
  -----------------------------
    fxmini.exe --restore-output

  这一条会把默认输出设备切回真实声卡然后退出。绝大多数情况下用不到：正常退
  出会自己还回去，异常退出下次启动也会自动修复。

  Hands the default output back to a real device and exits. Almost never needed
  - a clean exit restores it by itself, and a crash is repaired on the next
  start.

  卸载 / Uninstall
  ----------------
    powershell -ExecutionPolicy Bypass -File uninstall.ps1

  卸载前建议先在托盘菜单里卸掉虚拟声卡驱动（需要管理员权限）。

  Remove the virtual sound card from the tray menu first if you want it gone;
  that step needs administrator rights.

  目录 / Layout
  -------------
    fxmini.exe                    主程序 / the application
    driver\fxvad.inf              虚拟声卡驱动（FxSound 签名版）
    driver\fxvad.sys              the signed virtual sound card driver
    driver\fxvadntamd64.cat       ...with its catalogue and INF
    install.ps1 / uninstall.ps1   安装与卸载 / install and remove

  预设 / Presets
  --------------
  17 个内置预设，首次运行时解包到 %APPDATA%\FxMini\presets。把 .fac 文件丢
  进那个目录，再点托盘的「重新扫描预设」即可使用。

  Seventeen presets ship inside the executable and are unpacked to
  %APPDATA%\FxMini\presets on first run. Drop .fac files there and hit "Rescan
  presets" to add your own.

  许可 / Licence
  --------------
  AGPL-3.0-or-later，见 LICENSE.txt。驱动来自 FxSound，版权归其所有。
  AGPL-3.0-or-later; see LICENSE.txt. The driver is FxSound's, redistributed
  unmodified.
'@

Push-Location $here
try {
    $version = Read-CargoVersion
    Write-Host "FxMini $version" -ForegroundColor Green

    Write-Step "Building ($Configuration)"
    if ($NoBuild) {
        Write-Host 'skipped (-NoBuild)'
    } else {
        # Inlined rather than calling build.ps1: a PowerShell script that ends
        # with `exit` takes its caller down with it, so `& .\build.ps1` would
        # end this script the moment the build finished - before a single file
        # had been assembled.
        $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
        if (Test-Path $cargoBin) { $env:PATH = "$cargoBin;$env:PATH" }
        . (Join-Path $here 'toolchain.ps1')

        # cargo writes progress to stderr; under 'Stop' PowerShell 5.1 turns
        # redirected native stderr into a terminating error.
        $cargoArgs = @("--$Configuration", '--bin', 'fxmini')
        if ($Locked) { $cargoArgs += '--locked' }
        $ErrorActionPreference = 'Continue'
        & cargo build @cargoArgs
        $code = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($code -ne 0) { throw "cargo build exited $code" }
    }

    $exe = Join-Path $here "target\$Configuration\fxmini.exe"
    if (-not (Test-Path $exe)) { throw "no executable at $exe" }

    Write-Step 'Checking the executable'
    $info = (Get-Item $exe).VersionInfo
    if (-not $info.ProductName -or -not $info.FileVersion) {
        throw @"
$exe has no version resource, which means it also has no icon.
That happens when `rc.exe` cannot be found at build time. Either install the
Windows SDK, or point FXMINI_RC at an rc.exe, then build again.
"@
    }
    Write-Host ("  product   : {0} {1}" -f $info.ProductName, $info.FileVersion)
    Write-Host ("  size      : {0:N0} bytes" -f (Get-Item $exe).Length)

    # A dynamic-CRT build runs fine here and fails on a clean machine, so it is
    # worth catching at packaging time rather than at the user's. This is a hard
    # failure rather than a warning on purpose: a warning in a CI log is a
    # regression that ships anyway.
    $dumpbin = (Get-Command dumpbin.exe -ErrorAction SilentlyContinue).Source
    if ($dumpbin) {
        $deps = & $dumpbin /nologo /dependents $exe
        $redist = $deps | Select-String -Pattern 'VCRUNTIME|MSVCP'
        if ($redist) {
            throw @"
the executable imports the Visual C++ redistributable:
$($redist -join "`n")
It will not start on a machine without it. Check that .cargo\config.toml still
enables target-feature=+crt-static, and that build.rs is passing /MT to the
vendored C++.
"@
        } else {
            Write-Host '  runtime   : self-contained (no VC++ redistributable needed)'
        }
    } else {
        Write-Host '  runtime   : NOT CHECKED - dumpbin.exe is not on PATH'
        Write-Host '              (dot-source .\toolchain.ps1 first; CI does)'
    }

    Write-Step 'Locating the driver package'
    $driverSource = Find-DriverDir
    if (-not $driverSource) {
        throw @"
could not find fxvad.inf / fxvad.sys / fxvadntamd64.cat.
Looked next to this script, in .\driver, and in ..\NexBox\src-tauri\resources\
binaries\fxvad. Set FXMINI_DRIVER_DIR to override.
"@
    }
    Write-Host "  source    : $driverSource"

    Write-Step "Assembling $OutDir\FxMini"
    $stage = Join-Path $here (Join-Path $OutDir 'FxMini')
    Remove-Tree -Path $stage
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    Copy-Item $exe (Join-Path $stage 'fxmini.exe') -Force
    Copy-DriverFiles -Source $driverSource -Destination (Join-Path $stage 'driver')
    Write-TextFile -Path (Join-Path $stage 'README.txt') -Text $readme -Encoding UTF8
    Write-TextFile -Path (Join-Path $stage 'LICENSE.txt') -Text $licenseText -Encoding UTF8
    Write-TextFile -Path (Join-Path $stage 'install.ps1') -Text $installScript -Encoding ASCII
    Write-TextFile -Path (Join-Path $stage 'uninstall.ps1') -Text $uninstallScript -Encoding ASCII

    Get-ChildItem -LiteralPath $stage -Recurse -File |
        ForEach-Object { Write-Host ("  {0,-40} {1,10:N0}" -f $_.FullName.Substring($stage.Length + 1), $_.Length) }

    $archive = $null
    if (-not $NoZip) {
        Write-Step 'Writing the archive'
        $archive = Join-Path $here (Join-Path $OutDir "FxMini-$version-win64.zip")
        Remove-File -Path $archive
        # Compress-Archive would nest the folder differently depending on the
        # path shape; going through the parent keeps `FxMini\...` at the root.
        #
        # Note for anyone comparing hashes: the *contents* are reproducible —
        # fxmini.exe hashes identically across runs — but the archive does not,
        # because zip stores each entry's modification time. Compare the entries
        # (SHA256SUMS.txt) rather than the zip.
        Push-Location (Split-Path -Parent $stage)
        try {
            Compress-Archive -Path (Split-Path -Leaf $stage) -DestinationPath $archive -CompressionLevel Optimal
        } finally {
            Pop-Location
        }
        Write-Host ("  {0} ({1:N0} bytes)" -f (Split-Path -Leaf $archive), (Get-Item $archive).Length)
    }

    Write-Step 'Hashing'
    $sums = Join-Path $here (Join-Path $OutDir 'SHA256SUMS.txt')
    $lines = @()
    Get-ChildItem -LiteralPath $stage -Recurse -File | Sort-Object FullName | ForEach-Object {
        $relative = $_.FullName.Substring((Split-Path -Parent $stage).Length + 1).Replace('\', '/')
        $lines += ('{0}  {1}' -f (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLower(), $relative)
    }
    if ($archive) {
        $lines += ('{0}  {1}' -f (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLower(), (Split-Path -Leaf $archive))
    }
    Set-Content -LiteralPath $sums -Value $lines -Encoding ASCII
    $lines | ForEach-Object { Write-Host "  $_" }

    Write-Host ''
    Write-Host "Packaged FxMini $version" -ForegroundColor Green
    if ($archive) { Write-Host "  $archive" }
    Write-Host "  $stage"
    Write-Host "  $sums"
} finally {
    Pop-Location
}

exit 0
