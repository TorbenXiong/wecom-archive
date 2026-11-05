param(
    [string]$PackageName = '',
    [string]$Cargo = '',
    [string]$Node = '',
    [switch]$IncludeRealEnvironment
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'workflow.ps1')
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$version = (Get-Content -LiteralPath (Join-Path $repoRoot 'package.json') -Raw -Encoding utf8 | ConvertFrom-Json).version
if (-not $PackageName) { $PackageName = "WeComArchive-$version-windows-x64-" + [guid]::NewGuid().ToString('N').Substring(0, 8) }
Assert-WorkflowPackageName $PackageName
$artifactRoot = Join-Path $repoRoot 'artifacts'
$destination = Join-Path $artifactRoot $PackageName
Assert-WorkflowDirectory $repoRoot $destination
if (Test-Path -LiteralPath $destination) { throw 'Package directory already exists; nothing was overwritten.' }
if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitOperatingSystem) { throw 'Windows x64 is required.' }

# Fail before building if a local tool or frontend dependency is missing. Never invoke a package manager.
$Cargo = Resolve-WorkflowTool 'cargo' $Cargo
$Node = Resolve-WorkflowTool 'node' $Node
$nodeVersion = & $Node --version
if ($LASTEXITCODE -ne 0 -or ([version]($nodeVersion.TrimStart('v'))).Major -lt 24) { throw 'Installed Node.js >=24 required. Nothing was installed.' }
$nodeScripts = @('node_modules/vitest/vitest.mjs', 'node_modules/typescript/bin/tsc', 'node_modules/vite/bin/vite.js')
foreach ($file in $nodeScripts) {
    if (-not (Test-Path -LiteralPath (Join-Path $repoRoot $file) -PathType Leaf)) { throw 'Local frontend dependencies are incomplete. Nothing was installed.' }
}
$stagingRoot = Join-Path $repoRoot 'target/release-staging'
$stage = Join-Path $stagingRoot ([guid]::NewGuid().ToString('N'))
Assert-WorkflowDirectory $repoRoot $stage
$oldPath = $env:PATH
$oldOffline = $env:CARGO_NET_OFFLINE
$oldAutoInstall = $env:RUSTUP_AUTO_INSTALL
Push-Location $repoRoot
try {
    $env:PATH = (Split-Path -Parent $Cargo) + ';' + $env:PATH
    $env:CARGO_NET_OFFLINE = 'true'
    $env:RUSTUP_AUTO_INSTALL = '0'
    foreach ($file in Get-ChildItem -LiteralPath (Join-Path $repoRoot 'scripts') -Recurse -Filter '*.ps1') {
        $parseErrors = $null
        $null = [Management.Automation.Language.Parser]::ParseFile($file.FullName, [ref]$null, [ref]$parseErrors)
        if ($parseErrors.Count) { throw 'PowerShell script syntax check failed.' }
    }
    foreach ($file in Get-ChildItem -LiteralPath (Join-Path $repoRoot 'scripts') -Recurse -Filter '*.mjs') {
        & $Node --check $file.FullName
        if ($LASTEXITCODE -ne 0) { throw 'Node script syntax check failed.' }
    }
    Invoke-WorkflowCheck 'workflow-regressions' $Node @('--test', 'scripts/acceptance/runtime.node-tests.mjs')
    Invoke-WorkflowCheck 'frontend-tests' $Node @('node_modules/vitest/vitest.mjs', 'run')
    Invoke-WorkflowCheck 'frontend-types' $Node @('node_modules/typescript/bin/tsc', '-b')
    Invoke-WorkflowCheck 'build-client-assets' $Node @('node_modules/vite/bin/vite.js', 'build', '--mode', 'client')
    Invoke-WorkflowCheck 'prepare-client-assets' $Node @('scripts/prepare-client-assets.mjs')
    Invoke-WorkflowCheck 'build-server-assets' $Node @('node_modules/vite/bin/vite.js', 'build', '--mode', 'server')
    Invoke-WorkflowCheck 'rust-format' $Cargo @('fmt', '--all', '--', '--check')
    Invoke-WorkflowCheck 'rust-tests' $Cargo @('test', '--workspace', '--locked', '--offline')
    Invoke-WorkflowCheck 'rust-clippy' $Cargo @('clippy', '--workspace', '--all-targets', '--locked', '--offline', '--', '-D', 'warnings')
    $exe = Join-Path $repoRoot 'target/release/WeComArchive.exe'
    Write-Host 'windows-release'
    Invoke-WorkflowCargoBuild $Cargo $repoRoot @('--release', '--locked', '--offline', '--bin', 'WeComArchive') $exe
    $exeInfo = Get-WorkflowExeInfo $exe $version
    # Invoke the entry point in this PowerShell process so native error handling and environment restoration are preserved.
    & (Join-Path $PSScriptRoot 'verify-desktop.ps1') -Cargo $Cargo -Node $Node
    if ($IncludeRealEnvironment) {
        & (Join-Path $PSScriptRoot 'verify-real-environment.ps1') -Authorized -Cargo $Cargo
        if ($LASTEXITCODE -ne 0) { throw 'Authorized real environment acceptance failed.' }
    }
    Assert-WorkflowDirectory $repoRoot $stage
    $null = New-Item -ItemType Directory -Path $stage
    Copy-Item -LiteralPath $exe -Destination (Join-Path $stage 'WeComArchive.exe')
    Assert-WorkflowSingleExe $stage
    $stagedInfo = Get-WorkflowExeInfo (Join-Path $stage 'WeComArchive.exe') $version
    if ($stagedInfo.sha256 -ne $exeInfo.sha256) { throw 'Staged EXE hash mismatch.' }
    Assert-WorkflowDirectory $repoRoot $destination
    if (Test-Path -LiteralPath $destination) { throw 'Package directory appeared during build; nothing was overwritten.' }
    $null = New-Item -ItemType Directory -Path $artifactRoot -Force
    # Directory.Move is within this workspace and refuses an existing destination.
    Assert-WorkflowChildPath $stagingRoot $stage
    Assert-WorkflowChildPath $artifactRoot $destination
    [IO.Directory]::Move($stage, $destination)
    Assert-WorkflowSingleExe $destination
    $deliveredInfo = Get-WorkflowExeInfo (Join-Path $destination 'WeComArchive.exe') $version
    if ($deliveredInfo.sha256 -ne $exeInfo.sha256) { throw 'Packaged EXE hash mismatch.' }
    Write-Host ("Package: artifacts/$PackageName/WeComArchive.exe")
    Write-Host ("Version: $version; architecture: AMD64 PE32+; SHA-256: " + $exeInfo.sha256)
} finally {
    try {
        if (Test-Path -LiteralPath $stage) {
            Assert-WorkflowDirectory $repoRoot $stage
            Assert-WorkflowChildPath $stagingRoot $stage
            Remove-Item -LiteralPath $stage -Recurse -Force
        }
    } finally {
        Pop-Location
        $env:PATH = $oldPath
        $env:CARGO_NET_OFFLINE = $oldOffline
        $env:RUSTUP_AUTO_INSTALL = $oldAutoInstall
    }
}
