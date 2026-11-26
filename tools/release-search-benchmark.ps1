param(
    [ValidateSet(1000, 100000, 1000000)] [int[]]$Counts = @(100000, 1000000),
    [ValidateRange(1, 100)] [int]$DesktopTrials = 20,
    [switch]$SkipDesktop,
    [switch]$FolderOnly,
    [switch]$NtfsOnly
)

$ErrorActionPreference = 'Stop'
if ($FolderOnly -and $NtfsOnly) { throw 'Choose one provider filter.' }
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (!$FolderOnly -and ![Security.Principal.WindowsPrincipal]::new($identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run the disposable-volume benchmarks from elevated Windows PowerShell.'
}

function New-BenchmarkVolume([string]$Image, [int]$SizeMiB, [string]$DiskPartScript) {
    if (Test-Path -LiteralPath $Image) { throw 'The benchmark image must be new.' }
    "create vdisk file=`"$Image`" maximum=$SizeMiB type=expandable" |
        Set-Content -LiteralPath $DiskPartScript -Encoding Oem
    $created = & diskpart.exe /s $DiskPartScript
    if ($LASTEXITCODE -or !(Test-Path -LiteralPath $Image -PathType Leaf)) {
        throw "Could not create the benchmark VHD. $($created -join ' ')"
    }
    $mounted = Mount-DiskImage -ImagePath $Image -NoDriveLetter -PassThru
    $disk = @($mounted | Get-Disk)
    $byDevice = Get-DiskImage -DevicePath $mounted.DevicePath
    if ($disk.Count -ne 1 -or $byDevice.ImagePath -ine $Image -or $disk[0].IsBoot -or $disk[0].IsSystem -or
        $disk[0].PartitionStyle -ne 'RAW' -or $disk[0].Size -ne ($SizeMiB * 1MB)) {
        throw 'The mounted disk did not match the new benchmark image.'
    }
    $partition = Initialize-Disk -InputObject $disk[0] -PartitionStyle MBR -PassThru |
        New-Partition -UseMaximumSize -AssignDriveLetter
    if ($partition.DiskNumber -ne $disk[0].Number -or $partition.DriveLetter -notmatch '^[A-Z]$') {
        throw 'The partition did not belong to the benchmark image.'
    }
    $null = Format-Volume -Partition $partition -FileSystem NTFS -NewFileSystemLabel 'VelocityBench' -Confirm:$false
    $drive = "$($partition.DriveLetter):"
    & fsutil.exe usn createjournal m=536870912 a=67108864 $drive | Out-Null
    if ($LASTEXITCODE) { throw 'Could not create the benchmark change journal.' }
    "$drive\"
}

function Invoke-Benchmark([string]$Executable, [string[]]$Arguments, [string]$Output) {
    & $Executable @Arguments | ForEach-Object {
        Add-Content -LiteralPath $Output -Value $_
        $_
    }
    if ($LASTEXITCODE) { throw "Benchmark exited with code $LASTEXITCODE." }
}

$repo = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $repo
try {
    cargo build --release --locked -p velocity-native --example search_bench
    if ($LASTEXITCODE) { throw 'Could not build the release provider benchmark.' }
    if (!$SkipDesktop) {
        cargo build --release --locked -p velocity-ui --features desktop-acceptance
        if ($LASTEXITCODE) { throw 'Could not build the release desktop benchmark.' }
    }
    $target = (Resolve-Path -LiteralPath target).Path
    $bench = Join-Path $target 'release/examples/search_bench.exe'
    $tag = Get-Date -Format 'yyyy-MM-dd-HHmmss'
    $output = Join-Path $target "m11-results-$tag"
    $null = New-Item -ItemType Directory -Path $output
    "results=$output"
    $stage = Join-Path $target ('velocity-perf-' + [Guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $stage
    $previousLog = $env:RUST_LOG
    try {
        $metadata = (Join-Path $output 'host.txt')
        "date=$(Get-Date -Format o) revision=$(git rev-parse HEAD) profile=release" | Set-Content -LiteralPath $metadata
        "provider_sha256=$((Get-FileHash -LiteralPath $bench).Hash)" | Add-Content -LiteralPath $metadata
        rustc --version | Add-Content -LiteralPath $metadata
        "windows=$([Environment]::OSVersion.Version) logical_processors=$([Environment]::ProcessorCount) processor=$env:PROCESSOR_IDENTIFIER" | Add-Content -LiteralPath $metadata
        try {
            Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber | Format-List | Out-String | Add-Content -LiteralPath $metadata
            Get-CimInstance Win32_Processor | Select-Object Name, NumberOfCores, NumberOfLogicalProcessors | Format-List | Out-String | Add-Content -LiteralPath $metadata
            Get-CimInstance Win32_ComputerSystem | Select-Object TotalPhysicalMemory | Format-List | Out-String | Add-Content -LiteralPath $metadata
            Get-Partition -DriveLetter $target.Substring(0, 1) | Get-Disk | Select-Object FriendlyName, BusType, Size | Format-List | Out-String | Add-Content -LiteralPath $metadata
        } catch {
            'Detailed Windows hardware queries were unavailable in this session.' | Add-Content -LiteralPath $metadata
        }
        if (!$SkipDesktop) { "desktop_sha256=$((Get-FileHash -LiteralPath (Join-Path $target 'release/velocity-ui.exe')).Hash)" | Add-Content -LiteralPath $metadata }

        foreach ($count in $Counts) {
            $image = Join-Path $stage "corpus-$count.vhd"
            try {
                $root = if ($FolderOnly) {
                    $directory = Join-Path $stage "corpus-$count"
                    $null = New-Item -ItemType Directory -Path $directory
                    $directory
                } else { New-BenchmarkVolume $image 8192 (Join-Path $stage "create-$count.txt") }
                $raw = Join-Path $output "$count.txt"
                $env:RUST_LOG = 'warn'
                Invoke-Benchmark $bench @($root, "$count", '--prepare') $raw
                if (!$NtfsOnly) {
                    Invoke-Benchmark $bench @($root, "$count", '--folder') $raw
                }
                if (!$FolderOnly) {
                    $env:RUST_LOG = 'warn,velocity_native::ntfs::snapshot=info'
                    Invoke-Benchmark $bench @($root, "$count", '--ntfs', (Join-Path $stage "cache-$count")) $raw
                }
                if (!$SkipDesktop) {
                    & (Join-Path $PSScriptRoot 'typing-during-rebuild.ps1') -Entries $count -Trials $DesktopTrials -ExistingRoot $root -SkipBuild |
                        Tee-Object -LiteralPath (Join-Path $output "$count-desktop.txt")
                }
            } finally {
                if (Test-Path -LiteralPath $image) {
                    if ((Get-DiskImage -ImagePath $image).Attached) { Dismount-DiskImage -ImagePath $image | Out-Null }
                    if (!(Get-DiskImage -ImagePath $image).Attached) { Remove-Item -LiteralPath $image -Force }
                }
            }
        }
    } finally {
        $env:RUST_LOG = $previousLog
        $resolved = (Resolve-Path -LiteralPath $stage).Path
        if ($resolved -ine $stage -or [IO.Path]::GetDirectoryName($resolved) -ine $target) {
            throw 'Unexpected benchmark staging directory.'
        }
        if (!(Get-ChildItem -LiteralPath $resolved -Filter '*.vhd')) {
            Remove-Item -LiteralPath $resolved -Force -Recurse
        }
    }
} finally {
    Pop-Location
}
