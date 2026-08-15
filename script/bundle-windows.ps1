[CmdletBinding()]
Param(
    [Parameter()][Alias('i')][switch]$Install,
    [Parameter()][Alias('h')][switch]$Help,
    [Parameter()][Alias('a')][string]$Architecture,
    [Parameter()][string]$Name
)

. "$PSScriptRoot/lib/workspace.ps1"

# https://stackoverflow.com/questions/57949031/powershell-script-stops-if-program-fails-like-bash-set-o-errexit
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$buildSuccess = $false
$canCodeSign = $false
$setupPath = $null

$OSArchitecture = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    "X64" { "x86_64" }
    "Arm64" { "aarch64" }
    default { throw "Unsupported architecture" }
}

$Architecture = if ($Architecture) {
    $Architecture
} else {
    $OSArchitecture
}

$CargoOutDir = "./target/$Architecture-pc-windows-msvc/release"

function Get-VSArch {
    param(
        [string]$Arch
    )

    switch ($Arch) {
        "x86_64" { "amd64" }
        "aarch64" { "arm64" }
    }
}

$vswherePath = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$visualStudioPath = & $vswherePath -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ([string]::IsNullOrWhiteSpace($visualStudioPath)) {
    throw "Visual Studio C++ Build Tools were not found"
}

Push-Location
& (Join-Path $visualStudioPath "Common7\Tools\Launch-VsDevShell.ps1") -Arch (Get-VSArch -Arch $Architecture) -HostArch (Get-VSArch -Arch $OSArchitecture)
Pop-Location

$target = "$Architecture-pc-windows-msvc"

if ($Help) {
    Write-Output "Usage: test.ps1 [-Install] [-Help]"
    Write-Output "Build the installer for Windows.\n"
    Write-Output "Options:"
    Write-Output "  -Architecture, -a Which architecture to build (x86_64 or aarch64)"
    Write-Output "  -Install, -i      Run the installer after building."
    Write-Output "  -Help, -h         Show this help message."
    exit 0
}

Push-Location -Path crates/zed
$channel = Get-Content "RELEASE_CHANNEL"
if ($channel -notin @('dev', 'nightly', 'stable')) {
    throw "Unsupported ZOrca release channel: $channel"
}
$env:ZED_RELEASE_CHANNEL = $channel
$env:RELEASE_CHANNEL = $channel
Pop-Location

if ([string]::IsNullOrWhiteSpace($env:RELEASE_VERSION)) {
    $metadata = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
    $env:RELEASE_VERSION = ($metadata.packages | Where-Object name -eq 'zed').version
}

function CheckEnvironmentVariables {
    if(-not $env:CI) {
        return
    }

    $requiredVars = @('ZED_WORKSPACE', 'RELEASE_VERSION', 'ZED_RELEASE_CHANNEL')

    foreach ($var in $requiredVars) {
        if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($var))) {
            Write-Error "$var is not set"
            exit 1
        }
    }

    # On PRs from forks the signing secrets are not populated,
    # so skip code signing instead of failing, like bundle-mac does.
    $signingVars = @(
        'AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET',
        'ACCOUNT_NAME', 'CERT_PROFILE_NAME', 'ENDPOINT',
        'FILE_DIGEST', 'TIMESTAMP_DIGEST', 'TIMESTAMP_SERVER'
    )

    $missingVars = @($signingVars | Where-Object { [string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($_)) })
    if ($missingVars.Count -eq 0) {
        $script:canCodeSign = $true
    } else {
        Write-Host "====== WARNING ======"
        Write-Host "One or more of the following variables are missing: $($missingVars -join ', ')"
        Write-Host "This bundle will not be code signed"
        Write-Host "====== WARNING ======"
    }
}

function PrepareForBundle {
    if (Test-Path "$innoDir") {
        Remove-Item -Path "$innoDir" -Recurse -Force
    }
    New-Item -Path "$innoDir" -ItemType Directory -Force
    Copy-Item -Path "$env:ZED_WORKSPACE\crates\zed\resources\windows\*" -Destination "$innoDir" -Recurse -Force
    New-Item -Path "$innoDir\bin" -ItemType Directory -Force
    New-Item -Path "$innoDir\tools" -ItemType Directory -Force

    rustup target add $target
}

function GenerateLicenses {
    . $PSScriptRoot/generate-licenses.ps1
}

function BuildZorcaAndItsFriends {
    Write-Output "Building ZOrca for channel: $channel"
    cargo build --release --package zed --package cli --package auto_update_helper --package ade_session_daemon --target $target
    Copy-Item -Path ".\$CargoOutDir\zorca.exe" -Destination "$innoDir\ZOrca.exe" -Force
    Copy-Item -Path ".\$CargoOutDir\ade-daemon.exe" -Destination "$innoDir\ade-daemon.exe" -Force
    Copy-Item -Path ".\$CargoOutDir\cli.exe" -Destination "$innoDir\cli.exe" -Force
    Copy-Item -Path ".\$CargoOutDir\auto_update_helper.exe" -Destination "$innoDir\auto_update_helper.exe" -Force
}

function BuildRemoteServer {
    Write-Output "Building remote_server for $target"
    cargo build --release --package remote_server --target $target

    # Create zipped remote server binary
    $remoteServerSrc = (Resolve-Path ".\$CargoOutDir\remote_server.exe").Path

    if ($canCodeSign) {
        Write-Output "Code signing remote_server.exe"
        & "$innoDir\sign.ps1" $remoteServerSrc
    }

    $remoteServerDst = "$env:ZED_WORKSPACE\target\zorca-remote-server-windows-$Architecture.zip"
    Write-Output "Compressing remote_server to $remoteServerDst"
    Compress-Archive -Path $remoteServerSrc -DestinationPath $remoteServerDst -Force

    Write-Output "Remote server compressed successfully"
}

function SignZorcaAndItsFriends {
    if (-not $canCodeSign) {
        return
    }

    $files = "$innoDir\ZOrca.exe,$innoDir\ade-daemon.exe,$innoDir\cli.exe,$innoDir\auto_update_helper.exe"
    & "$innoDir\sign.ps1" $files
}

function DownloadAMDGpuServices {
    # If you update the AGS SDK version, please also update the version in `crates/gpui/src/platform/windows/directx_renderer.rs`
    $url = "https://codeload.github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/zip/refs/tags/v6.3.0"
    $zipPath = ".\AGS_SDK_v6.3.0.zip"
    # Download the AGS SDK zip file
    Invoke-WebRequest -Uri $url -OutFile $zipPath
    # Extract the AGS SDK zip file
    Expand-Archive -Path $zipPath -DestinationPath "." -Force
}

function DownloadConpty {
    $url = "https://github.com/microsoft/terminal/releases/download/v1.23.13503.0/Microsoft.Windows.Console.ConPTY.1.23.251216003.nupkg"
    $zipPath = ".\Microsoft.Windows.Console.ConPTY.1.23.251216003.nupkg"
    Invoke-WebRequest -Uri $url -OutFile $zipPath
    Expand-Archive -Path $zipPath -DestinationPath ".\conpty" -Force
}

function CollectFiles {
    Move-Item -Path "$innoDir\cli.exe" -Destination "$innoDir\bin\zorca.exe" -Force
    Move-Item -Path "$innoDir\zed.sh" -Destination "$innoDir\bin\zorca" -Force
    Move-Item -Path "$innoDir\auto_update_helper.exe" -Destination "$innoDir\tools\auto_update_helper.exe" -Force
    if($Architecture -eq "aarch64") {
        New-Item -Type Directory -Path "$innoDir\arm64" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\runtimes\win-arm64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
    }
    else {
        New-Item -Type Directory -Path "$innoDir\x64" -Force
        New-Item -Type Directory -Path "$innoDir\arm64" -Force
        Move-Item -Path ".\AGS_SDK-6.3.0\ags_lib\lib\amd_ags_x64.dll" -Destination "$innoDir\amd_ags_x64.dll" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\x64\OpenConsole.exe" -Destination "$innoDir\x64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\runtimes\win-x64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
    }
}

function BuildInstaller {
    $issFilePath = "$innoDir\zed.iss"
    switch ($channel) {
        "stable" {
            $appId = "{{674DABB6-4528-493C-A91A-AE4A3534D17B}"
            $appIconName = "app-icon"
            $appName = "ZOrca"
            $appDisplayName = "ZOrca"
            $appSetupName = "ZOrca-$Architecture"
            # The mutex name here should match the mutex name in crates\zed\src\zed\windows_only_instance.rs
            $appMutex = "ZOrca-Editor-Stable-Instance-Mutex"
            $appExeName = "ZOrca"
            $regValueName = "ZOrca"
            $appUserId = "ZOrca.ZOrca"
            $appShellNameShort = "Z&Orca"
        }
        "nightly" {
            $appId = "{{E91D527F-6D94-4629-AD76-BCA2FC0947D2}"
            $appIconName = "app-icon-nightly"
            $appName = "ZOrca Nightly"
            $appDisplayName = "ZOrca Nightly"
            $appSetupName = "ZOrca-Nightly-$Architecture"
            # The mutex name here should match the mutex name in crates\zed\src\zed\windows_only_instance.rs
            $appMutex = "ZOrca-Editor-Nightly-Instance-Mutex"
            $appExeName = "ZOrca"
            $regValueName = "ZOrcaNightly"
            $appUserId = "ZOrca.ZOrca.Nightly"
            $appShellNameShort = "Z&Orca Nightly"
        }
        "dev" {
            $appId = "{{1EBDD05E-D9B8-4C72-B36F-8AF621F0E6B6}"
            $appIconName = "app-icon-dev"
            $appName = "ZOrca Dev"
            $appDisplayName = "ZOrca Dev"
            $appSetupName = "ZOrca-Dev-$Architecture"
            # The mutex name here should match the mutex name in crates\zed\src\zed\windows_only_instance.rs
            $appMutex = "ZOrca-Editor-Dev-Instance-Mutex"
            $appExeName = "ZOrca"
            $regValueName = "ZOrcaDev"
            $appUserId = "ZOrca.ZOrca.Dev"
            $appShellNameShort = "Z&Orca Dev"
        }
        default {
            Write-Error "can't bundle installer for $channel."
            exit 1
        }
    }

    # Windows runner 2022 default has iscc in PATH, https://github.com/actions/runner-images/blob/main/images/windows/Windows2022-Readme.md
    # Currently, we are using Windows 2022 runner.
    # Windows runner 2025 doesn't have iscc in PATH for now, https://github.com/actions/runner-images/issues/11228
    $innoSetupPath = "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"

    $definitions = @{
        "AppId"          = $appId
        "AppIconName"    = $appIconName
        "OutputDir"      = "$env:ZED_WORKSPACE\target"
        "AppSetupName"   = $appSetupName
        "AppName"        = $appName
        "AppDisplayName" = $appDisplayName
        "RegValueName"   = $regValueName
        "AppMutex"       = $appMutex
        "AppExeName"     = $appExeName
        "ResourcesDir"   = "$innoDir"
        "ShellNameShort" = $appShellNameShort
        "AppUserId"      = $appUserId
        "Version"        = "$env:RELEASE_VERSION"
        "SourceDir"      = "$env:ZED_WORKSPACE"
    }

    $defs = @()
    foreach ($key in $definitions.Keys) {
        $defs += "/d$key=`"$($definitions[$key])`""
    }

    $innoArgs = @($issFilePath) + $defs
    if($canCodeSign) {
        # Checked by zed.iss to decide whether to sign the installer.
        $env:ZED_SIGN_BUNDLE = "1"
        $signTool = "powershell.exe -ExecutionPolicy Bypass -File $innoDir\sign.ps1 `$f"
        $innoArgs += "/sDefaultsign=`"$signTool`""
    }

    # Execute Inno Setup
    Write-Host "🚀 Running Inno Setup: $innoSetupPath $innoArgs"
    $process = Start-Process -FilePath $innoSetupPath -ArgumentList $innoArgs -NoNewWindow -Wait -PassThru

    if ($process.ExitCode -eq 0) {
        Write-Host "✅ Inno Setup successfully compiled the installer"
        $script:setupPath = "$env:ZED_WORKSPACE/target/$appSetupName.exe"
        if ($env:GITHUB_ENV) {
            Write-Output "SETUP_PATH=target/$appSetupName.exe" >> $env:GITHUB_ENV
        }
        $script:buildSuccess = $true
    }
    else {
        Write-Host "❌ Inno Setup failed: $($process.ExitCode)"
        $script:buildSuccess = $false
    }
}

ParseZedWorkspace
$innoDir = "$env:ZED_WORKSPACE\inno\$Architecture"
CheckEnvironmentVariables
PrepareForBundle
GenerateLicenses
BuildZorcaAndItsFriends
BuildRemoteServer
SignZorcaAndItsFriends
DownloadAMDGpuServices
DownloadConpty
CollectFiles
BuildInstaller

if ($buildSuccess) {
    Write-Output "Build successful"
    if ($Install) {
        Write-Output "Installing ZOrca..."
        Start-Process -FilePath $setupPath
    }
    exit 0
}
else {
    Write-Output "Build failed"
    exit 1
}
