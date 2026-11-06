param([string]$Cargo = '', [string]$Node = '')
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'workflow.ps1')
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$oldPath = $env:PATH
$oldOffline = $env:CARGO_NET_OFFLINE
$oldAutoInstall = $env:RUSTUP_AUTO_INSTALL
Push-Location $repoRoot
try {
    $Cargo = Resolve-WorkflowTool 'cargo' $Cargo
    $Node = Resolve-WorkflowTool 'node' $Node
    $env:PATH = (Split-Path -Parent $Cargo) + ';' + $env:PATH
    $env:CARGO_NET_OFFLINE = 'true'
    $env:RUSTUP_AUTO_INSTALL = '0'
    $version = (Get-Content -LiteralPath 'package.json' -Raw -Encoding utf8 | ConvertFrom-Json).version
    $null = Get-WorkflowExeInfo (Join-Path $repoRoot 'target/release/WeComArchive.exe') $version
    Write-Host 'Build synthetic acceptance generator (offline)'
    Invoke-WorkflowCargoBuild $Cargo $repoRoot @('--release','--locked','--offline','-p','wecom-archive-server','--example','acceptance-fixtures') (Join-Path $repoRoot 'target/release/examples/acceptance-fixtures.exe')
    Invoke-WorkflowCheck 'Isolated desktop acceptance' $Node @('scripts/acceptance/desktop.mjs')
} finally {
    Pop-Location
    $env:PATH = $oldPath
    $env:CARGO_NET_OFFLINE = $oldOffline
    $env:RUSTUP_AUTO_INSTALL = $oldAutoInstall
}
