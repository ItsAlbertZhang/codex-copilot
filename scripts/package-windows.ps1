[CmdletBinding()]
param(
    # No ValidateSet: the switch below is the single validation point, so an
    # unsupported host detected from 'rustc -vV' also gets the friendly message.
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

    # Nothing else checks the ARM64 binary (no ARM runner), so assert the PE
    # machine field of what was just built matches the requested target.
    $executable = Join-Path $buildRoot "$Target/release/codex-copilot.exe"
    $header = [System.IO.File]::ReadAllBytes($executable)
    $machine = [System.BitConverter]::ToUInt16($header, [System.BitConverter]::ToInt32($header, 0x3C) + 4)
    $expectedMachine = if ($architecture -eq 'x64') { 0x8664 } else { 0xAA64 }
    if ($machine -ne $expectedMachine) {
        throw ('Built {0} has PE machine 0x{1:X4}, expected 0x{2:X4} for {3}.' -f $executable, $machine, $expectedMachine, $Target)
    }

    $artifactName = "codex-copilot-$($package.version)-windows-$architecture.exe"
    $distRoot = Join-Path $projectRoot 'dist'
    New-Item -ItemType Directory -Path $distRoot -Force | Out-Null
    $artifact = Join-Path $distRoot $artifactName
    Copy-Item -LiteralPath $executable -Destination $artifact -Force

    $hash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash.ToLowerInvariant()
    # Write the line with a bare LF and no BOM: 'sha256sum -c' rejects CRLF.
    [System.IO.File]::WriteAllText("$artifact.sha256", "$hash  $artifactName`n")
    Write-Output "Portable executable: $artifact"
    Write-Output "SHA256: $hash"
}
finally {
    $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags
    Pop-Location
}
