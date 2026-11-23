$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run the disposable NTFS test from an elevated Windows PowerShell.'
}

function New-TestVolume([string]$Image, [string]$Label, [string]$Stage) {
    if (Test-Path -LiteralPath $Image) { throw 'The test image must be new.' }
    $createScript = Join-Path $Stage "$Label.diskpart.txt"
    'create vdisk file="' + $Image + '" maximum=128 type=expandable' |
        Set-Content -LiteralPath $createScript -Encoding Oem
    $created = & diskpart.exe /s $createScript
    if ($LASTEXITCODE -or !(Test-Path -LiteralPath $Image -PathType Leaf)) {
        throw "Could not create the test VHD. $($created -join ' ')"
    }

    $mounted = Mount-DiskImage -ImagePath $Image -NoDriveLetter -PassThru
    $disk = @($mounted | Get-Disk)
    $byDevice = Get-DiskImage -DevicePath $mounted.DevicePath
    if ($disk.Count -ne 1 -or $byDevice.ImagePath -ine $Image -or $disk[0].IsBoot -or $disk[0].IsSystem -or
        $disk[0].PartitionStyle -ne 'RAW' -or $disk[0].Size -ne 128MB) {
        throw 'The mounted disk did not match the new disposable image.'
    }

    $partition = Initialize-Disk -InputObject $disk[0] -PartitionStyle MBR -PassThru |
        New-Partition -UseMaximumSize -AssignDriveLetter
    if ($partition.DiskNumber -ne $disk[0].Number -or $partition.DriveLetter -notmatch '^[A-Z]$') {
        throw 'The new partition did not match the disposable image.'
    }
    $null = Format-Volume -Partition $partition -FileSystem NTFS -NewFileSystemLabel $Label -Confirm:$false
    $drive = "$($partition.DriveLetter):"
    & fsutil.exe usn createjournal m=4194304 a=1048576 $drive | Out-Null
    if ($LASTEXITCODE) { throw 'Could not create the test volume change journal.' }
    "$drive\"
}

function New-TestCorpus([string]$Root, [string]$Trustee) {
    $fixture = Join-Path $Root 'fixture'
    foreach ($directory in 'alpha', 'beta', 'batch', 'restricted', 'excluded') {
        $null = [IO.Directory]::CreateDirectory((Join-Path $fixture $directory))
    }
    $primary = Join-Path $fixture 'alpha/budget-shared.txt'
    [IO.File]::WriteAllText($primary, 'Synthetic NTFS hard link')
    $null = New-Item -ItemType HardLink -Path (Join-Path $fixture 'beta/budget-alias.txt') -Target $primary
    $null = New-Item -ItemType Junction -Path (Join-Path $fixture 'junction') -Target (Join-Path $fixture 'alpha')
    [IO.File]::WriteAllText((Join-Path $fixture 'restricted/budget-private.txt'), 'Restricted synthetic entry')
    [IO.File]::WriteAllText((Join-Path $fixture 'excluded/budget-excluded.txt'), 'Excluded synthetic entry')
    foreach ($index in 1..80) {
        [IO.File]::Create((Join-Path $fixture ('batch/synthetic-file-for-multi-batch-enumeration-{0:D3}.txt' -f $index))).Dispose()
    }
    & icacls.exe (Join-Path $fixture 'restricted') /deny "${Trustee}:(RD)" | Out-Null
    if ($LASTEXITCODE) { throw 'Could not deny enumeration of the restricted test directory.' }
}

$repo = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $repo
try {
    $null = New-Item -ItemType Directory -Path target -Force
    $target = (Resolve-Path -LiteralPath target).Path
    $stage = Join-Path $target ('velocity-ntfs-' + [Guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $stage
    $images = @((Join-Path $stage 'scope-a.vhd'), (Join-Path $stage 'scope-b.vhd'))
    $previousA = $env:VELOCITY_NTFS_VOLUME_A
    $previousB = $env:VELOCITY_NTFS_VOLUME_B
    try {
        $roots = @((New-TestVolume $images[0] 'VelocityM6A' $stage), (New-TestVolume $images[1] 'VelocityM6B' $stage))
        foreach ($root in $roots) { New-TestCorpus $root ('*' + $identity.User.Value) }
        $env:VELOCITY_NTFS_VOLUME_A = $roots[0]
        $env:VELOCITY_NTFS_VOLUME_B = $roots[1]
        cargo test --locked -p velocity-native --test ntfs_snapshot -- --ignored --nocapture
        if ($LASTEXITCODE) { throw "NTFS acceptance exited with code $LASTEXITCODE." }
    } finally {
        $env:VELOCITY_NTFS_VOLUME_A = $previousA
        $env:VELOCITY_NTFS_VOLUME_B = $previousB
        foreach ($image in $images) {
            if (Test-Path -LiteralPath $image) {
                $mounted = Get-DiskImage -ImagePath $image -ErrorAction SilentlyContinue
                if ($mounted.Attached) { Dismount-DiskImage -ImagePath $image | Out-Null }
            }
        }
        $resolved = (Resolve-Path -LiteralPath $stage).Path
        if ($resolved -ine $stage -or [IO.Path]::GetDirectoryName($resolved) -ine $target) {
            throw 'Unexpected test image directory.'
        }
        Remove-Item -LiteralPath $resolved -Force -Recurse
    }
} finally {
    Pop-Location
}
