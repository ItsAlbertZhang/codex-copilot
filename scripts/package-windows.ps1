[CmdletBinding()]
param(
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string]$Target
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($env:OS -ne 'Windows_NT') {
    throw 'Run this script on Windows with Rust and the Visual C++ build tools installed.'
}

$projectRoot = Split-Path -Parent $PSScriptRoot
$previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
Push-Location -LiteralPath $projectRoot
try {
    if (-not $Target) {
        $compilerInfo = & rustc -vV
        if ($LASTEXITCODE -ne 0) { throw 'Could not determine the Rust host target.' }
        $Target = ($compilerInfo | Select-String '^host: (.+)$').Matches.Groups[1].Value
    }
    $architecture = switch ($Target) {
        'x86_64-pc-windows-msvc' { 'x64' }
        'aarch64-pc-windows-msvc' { 'arm64' }
        default { throw "Unsupported target: $Target. Use a Windows MSVC x64 or ARM64 target." }
    }

    $metadataJson = & cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed.' }
    $metadata = $metadataJson | ConvertFrom-Json
    $package = $metadata.packages | Where-Object name -EQ 'codex-copilot'

    # Isolate portable builds and force static CRT linkage, including C dependencies.
    # Encoded flags take precedence over any caller's RUSTFLAGS and are restored below.
    $env:CARGO_ENCODED_RUSTFLAGS = '-C' + [char]0x1f + 'target-feature=+crt-static'
    $buildRoot = Join-Path $projectRoot 'target/portable'
    & cargo build --locked --release --target $Target --target-dir $buildRoot
    if ($LASTEXITCODE -ne 0) {
        throw "Portable build failed. Ensure 'rustup target add $Target' and the matching Visual C++ tools are installed."
    }

    $artifactName = "codex-copilot-$($package.version)-windows-$architecture"
    $distRoot = Join-Path $projectRoot 'dist'
    $stage = Join-Path $distRoot $artifactName
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $buildRoot "$Target/release/codex-copilot.exe") -Destination $stage
    Copy-Item -LiteralPath (Join-Path $projectRoot 'README.md'), (Join-Path $projectRoot 'LICENSE') -Destination $stage

    $quickStart = @'
codex-copilot - Windows portable edition

Extract the zip to a folder you want to keep. Open PowerShell in that folder:

  .\codex-copilot.exe --help
  .\codex-copilot.exe login

Follow the login output to set COPILOT_GITHUB_TOKEN, then open a new terminal:

  .\codex-copilot.exe install
  codex --profile copilot

No Rust toolchain or Visual C++ Redistributable installation is needed to run
this executable. An installed Codex CLI and a GitHub Copilot seat are still
required. This is a command-line application; run it from a terminal.

The install command configures the Codex profile. It does not install this exe.
Keep the exe and reuse it for login, install, status, or uninstall. It writes
the profile under CODEX_HOME (normally ~/.codex), not beside the executable.
Optionally add this folder to PATH to run codex-copilot from any directory.

See README.md for all options and details about tokens and configuration.
'@
    $quickStart | Set-Content -LiteralPath (Join-Path $stage 'QUICKSTART.txt') -Encoding UTF8

    $archive = Join-Path $distRoot "$artifactName.zip"
    $files = @('codex-copilot.exe', 'README.md', 'LICENSE', 'QUICKSTART.txt') |
        ForEach-Object { Join-Path $stage $_ }
    Compress-Archive -LiteralPath $files -DestinationPath $archive -Force
    $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $artifactName.zip" | Set-Content -LiteralPath "$archive.sha256" -Encoding ASCII
    Write-Output "Portable executable: $(Join-Path $stage 'codex-copilot.exe')"
    Write-Output "Archive: $archive"
    Write-Output "SHA256: $hash"
}
finally {
    $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags
    Pop-Location
}
