# Shared by the repository's offline build and desktop acceptance entry points.
function Resolve-WorkflowTool([string]$Name, [string]$ExplicitPath = '') {
    if ($ExplicitPath) {
        $command = Get-Command $ExplicitPath -CommandType Application -ErrorAction SilentlyContinue
        if ($command) { return $command.Source }
        throw "Local $Name is unavailable. No tools were installed."
    }
    $command = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    if ($Name -eq 'cargo') {
        $cached = Join-Path $env:USERPROFILE '.cargo/bin/cargo.exe'
        if (Test-Path -LiteralPath $cached) { return $cached }
    }
    throw "Local $Name is unavailable. No tools were installed."
}

function Invoke-WorkflowCheck([string]$Label, [string]$Executable, [string[]]$Arguments) {
    Write-Host $Label
    & $Executable @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Label failed (exit $LASTEXITCODE)." }
}

function Invoke-WorkflowCargoBuild([string]$Cargo, [string]$RepoRoot, [string[]]$BuildArguments, [string]$ExpectedExecutable) {
    $emitted = $false
    # Check Cargo's actual artifact, so a custom build target cannot silently select an old EXE.
    & $Cargo build @BuildArguments --target-dir (Join-Path $RepoRoot 'target') --message-format=json | ForEach-Object {
        $message = $_ | ConvertFrom-Json
        if ($message.reason -eq 'compiler-artifact' -and $message.executable -and
            [IO.Path]::GetFullPath($message.executable).Equals([IO.Path]::GetFullPath($ExpectedExecutable), [StringComparison]::OrdinalIgnoreCase)) {
            $emitted = $true
        }
        if ($message.reason -eq 'compiler-message' -and $message.message.rendered) { Write-Host $message.message.rendered }
    }
    if ($LASTEXITCODE -ne 0) { throw "Offline Cargo build failed (exit $LASTEXITCODE)." }
    if (-not $emitted -or -not (Test-Path -LiteralPath $ExpectedExecutable -PathType Leaf)) {
        throw 'Cargo did not emit the required executable in target/release. Check the configured build target.'
    }
}

function Get-WorkflowExeInfo([string]$Path, [string]$Version) {
    $file = Get-Item -LiteralPath $Path
    $stream = [IO.File]::OpenRead($file.FullName)
    try {
        $reader = [IO.BinaryReader]::new($stream)
        if ($reader.ReadUInt16() -ne 0x5a4d) { throw 'Invalid EXE header.' }
        $stream.Position = 0x3c
        $offset = $reader.ReadInt32()
        if ($offset -lt 64 -or $offset + 26 -gt $stream.Length) { throw 'Invalid PE offset.' }
        $stream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) { throw 'Windows x64 EXE required.' }
        $stream.Position = $offset + 24
        if ($reader.ReadUInt16() -ne 0x20b) { throw 'PE32+ EXE required.' }
    } finally { $stream.Dispose() }
    if ($file.VersionInfo.FileVersion -ne $Version -or $file.VersionInfo.ProductVersion -ne $Version) { throw 'EXE version does not match source version.' }
    return [pscustomobject]@{ version=$Version; architecture='AMD64 PE32+'; size=$file.Length; sha256=(Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash }
}

function Assert-WorkflowChildPath([string]$Root, [string]$Path) {
    $parent = [IO.Path]::GetFullPath($Root).TrimEnd('\','/') + [IO.Path]::DirectorySeparatorChar
    $child = [IO.Path]::GetFullPath($Path)
    if (-not $child.StartsWith($parent, [StringComparison]::OrdinalIgnoreCase)) { throw 'Workflow path leaves its owned directory.' }
}

function Assert-WorkflowDirectory([string]$Root, [string]$Path) {
    Assert-WorkflowChildPath $Root $Path
    $directory = [IO.Path]::GetFullPath($Path)
    $rootPath = [IO.Path]::GetFullPath($Root)
    while ($directory.Length -gt $rootPath.Length) {
        if (Test-Path -LiteralPath $directory) {
            $item = Get-Item -LiteralPath $directory -Force
            if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                throw 'Workflow directory must be a local directory without links.'
            }
        }
        $directory = [IO.Path]::GetDirectoryName($directory)
    }
}

function Assert-WorkflowPackageName([string]$Name) {
    if ($Name -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,95}$' -or $Name.EndsWith('.') -or
        $Name -match '^(CON|PRN|AUX|NUL|COM[0-9]|LPT[0-9])(?:\.|$)') {
        throw 'PackageName must be a simple, non-reserved directory name (1-96 characters).'
    }
}

function Assert-WorkflowSingleExe([string]$Directory) {
    $entries = @(Get-ChildItem -LiteralPath $Directory -Force)
    if ($entries.Count -ne 1 -or $entries[0].Name -cne 'WeComArchive.exe' -or $entries[0].PSIsContainer -or
        ($entries[0].Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Delivery directory must contain only WeComArchive.exe.'
    }
}
