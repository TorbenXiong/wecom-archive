param([switch]$BoundedVolume, [string]$Cargo = '')
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'workflow.ps1')
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$oldPath = $env:PATH
$oldOffline = $env:CARGO_NET_OFFLINE
$oldAutoInstall = $env:RUSTUP_AUTO_INSTALL
$oldVolume = $env:WECOM_FAULT_VOLUME
$run = $null
$image = $null
$attached = $false
$safeToRemove = $false
$completed = $false

function Invoke-FaultDiskPart([string[]]$Commands) {
    $commandFile = Join-Path $run 'diskpart.txt'
    # DiskPart can return zero after ignoring UTF-16 command input. Use the
    # local ANSI code page and reject unrepresentable paths rather than replace.
    $encoding = [Text.Encoding]::GetEncoding([Text.Encoding]::Default.CodePage,
        [Text.EncoderFallback]::ExceptionFallback, [Text.DecoderFallback]::ExceptionFallback)
    [IO.File]::WriteAllLines($commandFile, $Commands, $encoding)
    # Do not persist raw diskpart output containing local paths/disk metadata.
    & "$env:SystemRoot/System32/diskpart.exe" /s $commandFile | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'BOUNDED_VHD_DISKPART_FAILED' }
}

Push-Location $repoRoot
try {
    $Cargo = Resolve-WorkflowTool 'cargo' $Cargo
    $env:PATH = (Split-Path -Parent $Cargo) + ';' + $env:PATH
    $env:CARGO_NET_OFFLINE = 'true'
    $env:RUSTUP_AUTO_INSTALL = '0'
    if ($BoundedVolume) {
        $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
        $principal = [Security.Principal.WindowsPrincipal]::new($identity)
        if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
            throw 'BOUNDED_VHD_REQUIRES_ADMINISTRATOR: run this script with -BoundedVolume in an elevated PowerShell. No volume was created.'
        }
        foreach ($command in @('Get-DiskImage','Get-Disk','Get-Volume','Initialize-Disk','New-Partition','Format-Volume','Dismount-DiskImage')) {
            if (-not (Get-Command $command -ErrorAction SilentlyContinue)) { throw 'WINDOWS_STORAGE_COMMAND_UNAVAILABLE' }
        }
    }
    Invoke-WorkflowCheck 'Import crash and SQLite capacity drills (synthetic)' $Cargo @(
        'test','--locked','--offline','-p','archive-store','fault_drill','--','--test-threads=1'
    )
    Invoke-WorkflowCheck 'Backup and restore subprocess interruption (synthetic)' $Cargo @(
        'test','--locked','--offline','-p','wecom-archive-server','fault_drill','--','--test-threads=1'
    )
    if (-not $BoundedVolume) { return }

    # Only create a new, owned 256 MiB image. Never accept an arbitrary disk,
    # device number, existing image, drive letter or user-selected mount target.
    $base = Join-Path $repoRoot 'target/acceptance'
    Assert-WorkflowDirectory $repoRoot $base
    $null = New-Item -ItemType Directory -Path $base -Force
    $run = Join-Path $base ('storage-fault-' + [Guid]::NewGuid().ToString('N'))
    Assert-WorkflowDirectory $repoRoot $run
    $null = New-Item -ItemType Directory -Path $run
    [IO.File]::WriteAllText((Join-Path $run '.storage-fault-owned'), 'storage-fault.v1')
    $image = Join-Path $run 'bounded.vhd'
    if ($image -match '["\r\n]' -or (Test-Path -LiteralPath $image)) { throw 'UNSAFE_VHD_PATH' }
    $hostDrive = [IO.DriveInfo]::new([IO.Path]::GetPathRoot($image))
    if ($hostDrive.AvailableFreeSpace -lt 2GB) { throw 'HOST_FREE_SPACE_BELOW_2_GIB' }
    $existingNumbers = @(Get-Disk | ForEach-Object { $_.Number })
    $existingLetters = @([IO.DriveInfo]::GetDrives() | ForEach-Object { $_.Name.Substring(0,1) })
    # Keep create/attach together; separate DiskPart scripts require a shutdown
    # interval. Mark possible attachment before invoking the mutating commands.
    $attached = $true
    Invoke-FaultDiskPart @("create vdisk file=`"$image`" maximum=256 type=fixed", 'attach vdisk', 'exit')
    if (-not (Test-Path -LiteralPath $image -PathType Leaf)) { throw 'DISKPART_DID_NOT_CREATE_BOUNDED_VHD' }
    $imageFile = Get-Item -LiteralPath $image
    if (($imageFile.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $imageFile.Length -lt 256MB -or $imageFile.Length -gt 257MB) {
        throw 'UNEXPECTED_VHD_SIZE_OR_LINK'
    }
    $diskImage = Get-DiskImage -ImagePath $image
    $disks = @($diskImage | Get-Disk)
    if (-not $diskImage.Attached -or $disks.Count -ne 1) { throw 'VHD_DISK_ASSOCIATION_INVALID' }
    $disk = $disks[0]
    # Get-Disk's PowerShell adapter exposes a display string for BusType.
    # Read the numeric MSFT_Disk value so the guard is locale independent.
    $busType = $disk.CimInstanceProperties['BusType'].Value
    if ($existingNumbers -contains $disk.Number -or $disk.IsSystem -or $disk.IsBoot -or
        $busType -ne 15 -or $disk.Size -ne 256MB -or [string]$disk.PartitionStyle -ne 'RAW') {
        throw 'REFUSING_NON_NEW_RAW_FILE_BACKED_VIRTUAL_DISK'
    }
    $disk = Initialize-Disk -InputObject $disk -PartitionStyle GPT -PassThru
    $partition = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter
    if (-not $partition.DriveLetter -or $existingLetters -contains [string]$partition.DriveLetter) { throw 'VHD_DRIVE_LETTER_NOT_NEW' }
    $label = 'WCAFAULT' + [Guid]::NewGuid().ToString('N').Substring(0,8)
    $volume = $partition | Format-Volume -FileSystem NTFS -NewFileSystemLabel $label -Confirm:$false
    if ($volume.FileSystemLabel -cne $label -or $volume.Size -gt 256MB) { throw 'BOUNDED_VOLUME_IDENTITY_INVALID' }
    # The product rejects junction ancestors, so use the newly assigned letter.
    # This letter belongs only to the already verified new virtual partition.
    $volumeRoot = [string]$partition.DriveLetter + ':\'
    $mounted = Get-Volume -DriveLetter $partition.DriveLetter
    if ($mounted.UniqueId -ne $volume.UniqueId -or $mounted.FileSystemLabel -cne $label) { throw 'MOUNT_VOLUME_MISMATCH' }
    Write-Host 'PASS NEW_256_MIB_FILE_BACKED_VHD_NTFS_IDENTITY'
    [IO.File]::WriteAllText((Join-Path $volumeRoot '.bounded-fault-volume'), 'bounded-volume.v1')
    $env:WECOM_FAULT_VOLUME = $volumeRoot
    Invoke-WorkflowCheck 'Bounded NTFS exhaustion during import and retry' $Cargo @(
        'test','--locked','--offline','-p','archive-store',
        'tests::fault_drill::bounded_volume_full_import_rolls_back_then_retries',
        '--','--ignored','--exact','--nocapture','--test-threads=1'
    )
    Invoke-WorkflowCheck 'Bounded NTFS exhaustion during backup/restore and retry' $Cargo @(
        'test','--locked','--offline','-p','wecom-archive-server',
        'tests::maintenance::fault_drill::bounded_volume_full_backup_and_restore_fail_without_publication_then_retry',
        '--','--ignored','--exact','--nocapture','--test-threads=1'
    )
    $completed = $true
} finally {
    $env:WECOM_FAULT_VOLUME = $oldVolume
    $env:PATH = $oldPath
    $env:CARGO_NET_OFFLINE = $oldOffline
    $env:RUSTUP_AUTO_INSTALL = $oldAutoInstall
    Pop-Location
    if ($attached -and (Test-Path -LiteralPath $image -PathType Leaf)) {
        try {
            $state = Get-DiskImage -ImagePath $image
            if ($state.Attached) {
                Dismount-DiskImage -ImagePath $image | Out-Null
            }
            if ((Get-DiskImage -ImagePath $image).Attached) { throw 'VHD_STILL_ATTACHED' }
            $safeToRemove = $true
        } catch { Write-Warning 'BOUNDED_VHD_DETACH_FAILED: owned test directory retained; no recursive cleanup attempted.' }
    } elseif ($run) { $safeToRemove = $true }
    if ($run -and $safeToRemove) {
        Assert-WorkflowDirectory $repoRoot $run
        if ((Split-Path -Parent ([IO.Path]::GetFullPath($run))) -ne ([IO.Path]::GetFullPath($base)) -or
            [IO.File]::ReadAllText((Join-Path $run '.storage-fault-owned')) -cne 'storage-fault.v1') { throw 'UNSAFE_VHD_CLEANUP' }
        Remove-Item -LiteralPath $run -Recurse -Force
    }
    if ($completed) {
        if (-not $safeToRemove -or (Test-Path -LiteralPath $run)) { throw 'BOUNDED_VHD_TESTS_PASSED_BUT_CLEANUP_PENDING' }
        Write-Host 'PASS BOUNDED_VHD_ALL_DRILLS_AND_DETACH'
    }
}
