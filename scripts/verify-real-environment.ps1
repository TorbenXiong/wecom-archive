param(
    [switch]$Authorized,
    [string]$Cargo = '',
    [ValidateNotNullOrEmpty()][ValidateSet('source','probe','groups','media')][string[]]$Cases = @('source','probe','groups','media'),
    [ValidateRange(30,3600)][int]$TestTimeoutSeconds = 900
)

$ErrorActionPreference = 'Stop'
if (-not $Authorized) { throw 'Explicit authorization required: pass -Authorized to inspect local WXWork sources.' }
$Cases = @($Cases | ForEach-Object { $_.ToLowerInvariant() } | Select-Object -Unique)
. (Join-Path $PSScriptRoot 'workflow.ps1')
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$Cargo = Resolve-WorkflowTool 'cargo' $Cargo
$runRoot = Join-Path $repoRoot ('target/real-acceptance/' + [guid]::NewGuid().ToString('N'))
Assert-WorkflowDirectory $repoRoot $runRoot
$null = New-Item -ItemType Directory -Path $runRoot
$previousEnvironment = @{}
foreach ($name in @('PATH','TEMP','TMP','CARGO_NET_OFFLINE','RUSTUP_AUTO_INSTALL','WECOM_ARCHIVE_SERVER_DATA','WECOM_ARCHIVE_PROBE_EXE','WECOM_ARCHIVE_SERVER_LOG','WECOM_ARCHIVE_COLLECTOR_LOG')) {
    $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$caseResults = [Collections.Generic.List[bool]]::new()
$failed = $false

function Invoke-AuthorizedTest([string]$Binary, [string]$Test) {
    Write-Host ("Running: $Test")
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Binary
    $info.Arguments = "--ignored --exact $Test --test-threads=1"
    $info.WorkingDirectory = $repoRoot
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    try {
        $null = $process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TestTimeoutSeconds * 1000)) {
            $process.Kill()
            $process.WaitForExit()
            Write-Host ("$Test : timeout")
            return $false
        }
        $output = $stdout.GetAwaiter().GetResult()
        $null = $stderr.GetAwaiter().GetResult()
        $pass = $process.ExitCode -eq 0 -and $output.Contains("test $Test ... ok") -and $output.Contains('1 passed; 0 failed')
        Write-Host ("$Test : " + $(if ($pass) { 'pass' } else { 'fail' }) + " (" + [int]$timer.Elapsed.TotalSeconds + 's)')
        return $pass
    } finally { $process.Dispose() }
}

Push-Location $repoRoot
try {
    if (-not (Get-Process -Name WXWork -ErrorAction SilentlyContinue)) { throw 'WXWORK_NOT_RUNNING' }
    foreach ($directory in @('temp','data','bin')) { $null = New-Item -ItemType Directory -Path (Join-Path $runRoot $directory) }
    $env:PATH = (Split-Path -Parent $Cargo) + ';' + $env:PATH
    $env:TEMP = Join-Path $runRoot 'temp'
    $env:TMP = $env:TEMP
    $env:CARGO_NET_OFFLINE = 'true'
    $env:RUSTUP_AUTO_INSTALL = '0'
    $env:WECOM_ARCHIVE_SERVER_LOG = 'off'
    $env:WECOM_ARCHIVE_COLLECTOR_LOG = 'off'
    $env:WECOM_ARCHIVE_SERVER_DATA = Join-Path $runRoot 'data'
    $version = (Get-Content package.json -Raw -Encoding utf8 | ConvertFrom-Json).version
    $exe = Join-Path $repoRoot 'target/release/WeComArchive.exe'
    Invoke-WorkflowCargoBuild $Cargo $repoRoot @('--release','--locked','--offline','--bin','WeComArchive') $exe
    $null = Get-WorkflowExeInfo $exe $version
    $env:WECOM_ARCHIVE_PROBE_EXE = Join-Path $runRoot 'bin/WeComArchive.exe'
    Copy-Item -LiteralPath $exe -Destination $env:WECOM_ARCHIVE_PROBE_EXE
    $binaries = @{}
    & $Cargo test --no-run --lib --locked --offline --target-dir (Join-Path $repoRoot 'target') -p source-windows -p wecom-archive-desktop --message-format=json | ForEach-Object {
        $message = $_ | ConvertFrom-Json
        if ($message.reason -eq 'compiler-artifact' -and $message.profile.test -and $message.executable) {
            $binaries[$message.target.name] = $message.executable
        }
    }
    if ($LASTEXITCODE -ne 0 -or -not $binaries.source_windows -or -not $binaries.wecom_archive_desktop) { throw 'REAL_TEST_BUILD_FAILED' }
    if ($Cases -contains 'source') {
        $caseResults.Add((Invoke-AuthorizedTest $binaries.source_windows 'tests::authorized_real_source_discovers_nonempty_media_roots_without_reading_content'))
    }
    $desktopCases = [ordered]@{
        probe='authorized_real_probe_finds_a_key_without_exposing_it'
        groups='authorized_real_collection_includes_group_metadata_without_exposing_content'
        media='authorized_real_collection_includes_media_without_exposing_content'
    }
    foreach ($key in $desktopCases.Keys) {
        if ($Cases -contains $key) { $caseResults.Add((Invoke-AuthorizedTest $binaries.wecom_archive_desktop ('tests::' + $desktopCases[$key]))) }
    }
} catch {
    $failed = $true
    Write-Host 'REAL_ENVIRONMENT_ACCEPTANCE_FAILED'
} finally {
    # Remove only this run's runtime copies. Source paths never enter the cleanup list.
    try {
        $probePath = Join-Path $runRoot 'bin/WeComArchive.exe'
        foreach ($probe in Get-CimInstance Win32_Process -Filter "Name = 'WeComArchive.exe'" | Where-Object { $_.ExecutablePath -eq $probePath }) { Stop-Process -Id $probe.ProcessId -ErrorAction Stop }
        Assert-WorkflowDirectory $repoRoot $runRoot
        Assert-WorkflowChildPath (Join-Path $repoRoot 'target/real-acceptance') $runRoot
        Remove-Item -LiteralPath $runRoot -Recurse -Force
    } catch { $failed = $true }
    foreach ($name in $previousEnvironment.Keys) { [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], 'Process') }
    Pop-Location
    $passed = @($caseResults.ToArray() | Where-Object { $_ }).Count
    if ($caseResults.Count -ne $Cases.Count -or $passed -ne $Cases.Count) { $failed = $true }
    Write-Host ("Real environment acceptance: $passed/$($Cases.Count) passed")
}
if ($failed) { exit 1 }
