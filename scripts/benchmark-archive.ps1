param(
    [ValidateRange(1000, 1000000)][int[]]$Counts = @(100000, 500000, 1000000),
    [ValidateRange(64, 4096)][int]$BodyBytes = 256,
    [ValidateRange(1, 30)][int]$QuerySamples = 5,
    [ValidateSet('Stream', 'Legacy')][string]$ExportMode = 'Stream',
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
    & $Cargo build --release --locked --offline -p wecom-archive-server --example archive-benchmark
    if ($LASTEXITCODE -ne 0) { throw 'Offline benchmark build failed.' }
    $benchmarkBinary = Join-Path $benchmarkRoot 'target/release/examples/archive-benchmark.exe'
    $reportRoot = Join-Path $benchmarkRoot ('target/benchmarks/report-' + [guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $reportRoot
    foreach ($count in $Counts) {
        Write-Host "Benchmark: $count synthetic messages"
        $startInfo = New-Object Diagnostics.ProcessStartInfo
        $startInfo.FileName = $benchmarkBinary
        $startInfo.Arguments = "$count $BodyBytes $QuerySamples $($ExportMode.ToLowerInvariant())"
        $startInfo.WorkingDirectory = $benchmarkRoot
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $false
        $process = New-Object Diagnostics.Process
        $process.StartInfo = $startInfo
        try {
            $null = $process.Start()
            $stdout = $process.StandardOutput.ReadToEndAsync()
            $peakWorkingSet = 0L
            while (-not $process.WaitForExit(250)) {
                $process.Refresh()
                $peakWorkingSet = [Math]::Max($peakWorkingSet, $process.PeakWorkingSet64)
            }
            $process.Refresh()
            $peakWorkingSet = [Math]::Max($peakWorkingSet, $process.PeakWorkingSet64)
            if ($process.ExitCode -ne 0) { throw 'Synthetic benchmark failed.' }
            $report = $stdout.GetAwaiter().GetResult() | ConvertFrom-Json
            $report | Add-Member -NotePropertyName peak_working_set_bytes -NotePropertyValue $peakWorkingSet
            $report | Add-Member -NotePropertyName memory_measurement -NotePropertyValue 'OS process peak working set; includes fixture and exports'
            $target = Join-Path $reportRoot "$count.json"
            $partial = "$target.partial"
            [IO.File]::WriteAllText($partial, ($report | ConvertTo-Json -Depth 12), (New-Object Text.UTF8Encoding($false)))
            [IO.File]::Move($partial, $target)
            Write-Host "Completed: $count messages; peak working set $([Math]::Round($peakWorkingSet / 1MB)) MiB"
        }
        finally { $process.Dispose() }
    }
    Write-Host ('Reports: target/benchmarks/' + [IO.Path]::GetFileName($reportRoot))
}
finally { Pop-Location }
