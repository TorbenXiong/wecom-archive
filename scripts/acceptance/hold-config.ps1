param([Parameter(Mandatory=$true)][string]$RunRoot)
$ErrorActionPreference = 'Stop'
try {
    $repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
    . (Join-Path $PSScriptRoot '../workflow.ps1')
    $allowed = [IO.Path]::GetFullPath((Join-Path $repoRoot 'target/acceptance')) + [IO.Path]::DirectorySeparatorChar
    $resolved = (Get-Item -LiteralPath $RunRoot).FullName
    Assert-WorkflowDirectory $repoRoot $resolved
    if (-not $resolved.StartsWith($allowed, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.File]::ReadAllText((Join-Path $resolved '.synthetic-run')) -ne 'desktop-acceptance.v1') {
        throw 'Unsafe test directory'
    }
    $config = Join-Path $resolved 'main-data/enterprise-config.json'
    Assert-WorkflowDirectory $repoRoot (Join-Path $resolved 'main-data')
    if ((Get-Item -LiteralPath $config).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Unexpected test link' }
    $file = [IO.File]::Open($config, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        [Console]::Out.WriteLine('ready')
        [Console]::Out.Flush()
        $null = [Console]::In.ReadLine()
    } finally { $file.Dispose() }
} catch {
    [Console]::Error.WriteLine('SYNTHETIC_CONFIG_LOCK_FAILED')
    exit 1
}
