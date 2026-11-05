param(
    [ValidateRange(1000, 1000000)][int[]]$Counts = @(100000),
    [ValidateRange(1, 30)][int]$QuerySamples = 5,
    [string]$Cargo = 'cargo'
)

$ErrorActionPreference = 'Stop'
$benchmarkRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Push-Location $benchmarkRoot
try {
    if (-not (Get-Command $Cargo -ErrorAction SilentlyContinue)) {
        $cachedCargo = Join-Path $env:USERPROFILE '.cargo/bin/cargo.exe'
        if (Test-Path -LiteralPath $cachedCargo) { $Cargo = $cachedCargo }
        else { throw 'Local Cargo is unavailable. No tools were downloaded.' }
    }
    & $Cargo build --release --locked --offline -p wecom-archive-server --example search-api-benchmark
    if ($LASTEXITCODE -ne 0) { throw 'Offline search API benchmark build failed.' }
    $reportRoot = Join-Path $benchmarkRoot ('target/benchmarks/search-report-' + [guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $reportRoot
    foreach ($count in $Counts) {
        $workRoot = Join-Path $benchmarkRoot ('target/benchmarks/search-api-' + [guid]::NewGuid().ToString('N'))
        $null = New-Item -ItemType Directory -Path $workRoot
        $portProbe = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
        try { $portProbe.Start(); $port = $portProbe.LocalEndpoint.Port }
        finally { $portProbe.Stop() }
        $startInfo = New-Object Diagnostics.ProcessStartInfo
        $startInfo.FileName = Join-Path $benchmarkRoot 'target/release/examples/search-api-benchmark.exe'
        $startInfo.Arguments = "$count $QuerySamples"
        $startInfo.WorkingDirectory = $benchmarkRoot
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $false
        # These settings belong only to the synthetic child process.
        $startInfo.EnvironmentVariables['WECOM_ARCHIVE_SERVER_DATA'] = $workRoot
        $startInfo.EnvironmentVariables['WECOM_ARCHIVE_SERVER_LISTEN'] = "127.0.0.1:$port"
        $startInfo.EnvironmentVariables['WECOM_ARCHIVE_SERVER_LOG'] = 'off'
        $process = New-Object Diagnostics.Process
        $process.StartInfo = $startInfo
        try {
            Write-Host "Search API benchmark: $count synthetic messages"
            $null = $process.Start()
            $stdout = $process.StandardOutput.ReadToEndAsync()
            $peakWorkingSet = 0L
            while (-not $process.WaitForExit(250)) {
                $process.Refresh()
                $peakWorkingSet = [Math]::Max($peakWorkingSet, $process.PeakWorkingSet64)
            }
            $process.Refresh()
            $peakWorkingSet = [Math]::Max($peakWorkingSet, $process.PeakWorkingSet64)
            if ($process.ExitCode -ne 0) { throw 'Synthetic search API benchmark failed.' }
            $report = $stdout.GetAwaiter().GetResult() | ConvertFrom-Json
            $report | Add-Member -NotePropertyName peak_working_set_bytes -NotePropertyValue $peakWorkingSet
            $report | Add-Member -NotePropertyName memory_measurement -NotePropertyValue 'Process peak working set; includes fixture ingest, startup and all query scenarios'
            $target = Join-Path $reportRoot "$count.json"
            $partial = "$target.partial"
            [IO.File]::WriteAllText($partial, ($report | ConvertTo-Json -Depth 12), (New-Object Text.UTF8Encoding($false)))
            [IO.File]::Move($partial, $target)
            Write-Host "Completed: $count messages"
        }
        finally { $process.Dispose() }
    }
    Write-Host ('Reports: target/benchmarks/' + [IO.Path]::GetFileName($reportRoot))
}
finally { Pop-Location }
