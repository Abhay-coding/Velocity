param([switch]$Journal, [switch]$Desktop)

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

function Test-DesktopJournal([string]$Root, [string]$Stage) {
    cargo build --locked -p velocity-ui --features desktop-acceptance
    if ($LASTEXITCODE) { throw "Desktop acceptance build exited with code $LASTEXITCODE." }
    $fixture = Join-Path $Root 'desktop-fixture'
    $before = Join-Path $fixture 'before'
    $null = [IO.Directory]::CreateDirectory($before)
    [IO.File]::WriteAllText((Join-Path $before 'desktop-journal-old.txt'), 'Synthetic desktop fixture')
    Copy-Item -LiteralPath target/debug/velocity-ui.exe -Destination $Stage
    @('[search]', "roots = ['$Root']", 'ntfs_snapshots = true') |
        Set-Content -LiteralPath (Join-Path $Stage 'config.toml') -Encoding Ascii
    $previousFixture = $env:VELOCITY_NTFS_DESKTOP_FIXTURE
    $previousRenderer = $env:SLINT_BACKEND
    try {
        $env:VELOCITY_NTFS_DESKTOP_FIXTURE = $fixture
        $env:SLINT_BACKEND = 'winit-femtovg'
        $launch = [Diagnostics.ProcessStartInfo]::new((Join-Path $Stage 'velocity-ui.exe'))
        $launch.WorkingDirectory = $Stage
        $launch.UseShellExecute = $false
        $launch.RedirectStandardOutput = $true
        $launch.RedirectStandardError = $true
        $process = [Diagnostics.Process]::Start($launch)
        try {
            $output = $process.StandardOutput.ReadToEndAsync()
            $errors = $process.StandardError.ReadToEndAsync()
            if (!$process.WaitForExit(90000)) {
                $process.Kill()
                $process.WaitForExit()
                throw 'Desktop acceptance timed out.'
            }
            $output.GetAwaiter().GetResult()
            $errors.GetAwaiter().GetResult()
            if ($process.ExitCode -ne 0) { throw "Desktop acceptance exited with code $($process.ExitCode)." }
        } finally {
            $process.Dispose()
        }
    } finally {
        $env:VELOCITY_NTFS_DESKTOP_FIXTURE = $previousFixture
        $env:SLINT_BACKEND = $previousRenderer
    }
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
    $previousTrustee = $env:VELOCITY_NTFS_TRUSTEE
    try {
        $roots = @((New-TestVolume $images[0] 'VelocityM6A' $stage), (New-TestVolume $images[1] 'VelocityM6B' $stage))
        foreach ($root in $roots) { New-TestCorpus $root ('*' + $identity.User.Value) }
        $env:VELOCITY_NTFS_VOLUME_A = $roots[0]
        $env:VELOCITY_NTFS_VOLUME_B = $roots[1]
        cargo test --locked -p velocity-native --test ntfs_snapshot scoped_ntfs_snapshots_match_independent_walks -- --ignored --nocapture
        if ($LASTEXITCODE) { throw "NTFS acceptance exited with code $LASTEXITCODE." }
        if ($Journal) {
            $env:VELOCITY_NTFS_TRUSTEE = '*' + $identity.User.Value
            cargo test --locked -p velocity-native --test ntfs_snapshot tails_ntfs_journal_and_matches_a_fresh_walk -- --ignored --nocapture
            if ($LASTEXITCODE) { throw "NTFS journal acceptance exited with code $LASTEXITCODE." }
            cargo test --locked -p velocity-native --lib catches_changes_during_projection_and_replays_idempotently -- --ignored --nocapture
            if ($LASTEXITCODE) { throw "NTFS catch-up acceptance exited with code $LASTEXITCODE." }
        }
        if ($Desktop) { Test-DesktopJournal $roots[1] $stage }
    } finally {
        $env:VELOCITY_NTFS_VOLUME_A = $previousA
        $env:VELOCITY_NTFS_VOLUME_B = $previousB
        $env:VELOCITY_NTFS_TRUSTEE = $previousTrustee
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
