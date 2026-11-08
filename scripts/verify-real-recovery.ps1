param([switch]$Authorized, [string]$Cargo = '', [string]$ResumeRun = '', [string]$Backup = '')
$ErrorActionPreference = 'Stop'
if (-not $Authorized) { throw 'Explicit authorization is required: use -Authorized only after user approval to collect real data and retain a local verified backup.' }
if ($Backup -and -not $ResumeRun) { throw 'Reusing a backup requires its authorized isolated source run.' }
. (Join-Path $PSScriptRoot 'workflow.ps1')
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$oldPath = $env:PATH
$oldOffline = $env:CARGO_NET_OFFLINE
$oldAutoInstall = $env:RUSTUP_AUTO_INSTALL
$oldAuthorization = $env:WECOM_ARCHIVE_REAL_RECOVERY
$oldSourceRun = $env:WECOM_ARCHIVE_REAL_RECOVERY_SOURCE_RUN
$oldBackup = $env:WECOM_ARCHIVE_REAL_RECOVERY_BACKUP
Push-Location $repoRoot
try {
    $Cargo = Resolve-WorkflowTool 'cargo' $Cargo
    $version = (Get-Content -LiteralPath 'package.json' -Raw -Encoding utf8 | ConvertFrom-Json).version
    $null = Get-WorkflowExeInfo (Join-Path $repoRoot 'target/release/WeComArchive.exe') $version
    $env:PATH = (Split-Path -Parent $Cargo) + ';' + $env:PATH
    $env:CARGO_NET_OFFLINE = 'true'
    $env:RUSTUP_AUTO_INSTALL = '0'
    $env:WECOM_ARCHIVE_REAL_RECOVERY = 'authorized'
    $env:WECOM_ARCHIVE_REAL_RECOVERY_SOURCE_RUN = if ($ResumeRun) { [IO.Path]::GetFullPath($ResumeRun) } else { $null }
    $env:WECOM_ARCHIVE_REAL_RECOVERY_BACKUP = if ($Backup) { [IO.Path]::GetFullPath($Backup) } else { $null }
    Invoke-WorkflowCheck 'Real drill input factory regression' $Cargo @(
        'test', '--release', '--locked', '--offline', '-p', 'wecom-archive-desktop', '--lib',
        'tests::real_recovery::batch_factory_reads_plain_database_consent_enums', '--', '--exact'
    )
    Invoke-WorkflowCheck 'Authorized isolated real backup, restore and import failure drill' $Cargo @(
        'test', '--release', '--locked', '--offline', '-p', 'wecom-archive-desktop', '--lib',
        'tests::real_recovery::authorized_real_backup_restore_and_import_failures',
        '--', '--ignored', '--exact', '--nocapture', '--test-threads=1'
    )
} finally {
    Pop-Location
    $env:PATH = $oldPath
    $env:CARGO_NET_OFFLINE = $oldOffline
    $env:RUSTUP_AUTO_INSTALL = $oldAutoInstall
    $env:WECOM_ARCHIVE_REAL_RECOVERY = $oldAuthorization
    $env:WECOM_ARCHIVE_REAL_RECOVERY_SOURCE_RUN = $oldSourceRun
    $env:WECOM_ARCHIVE_REAL_RECOVERY_BACKUP = $oldBackup
}
