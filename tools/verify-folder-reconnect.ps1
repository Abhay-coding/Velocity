$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run the disposable SMB test from an elevated Windows PowerShell.'
}
if ((Get-Service -Name LanmanServer).Status -ne 'Running') {
    throw 'This test needs the Windows Server service to support local SMB shares.'
}

function Wait-TestMarker([string]$Path, [Diagnostics.Process]$Process) {
    $timeout = [Diagnostics.Stopwatch]::StartNew()
    while (!(Test-Path -LiteralPath $Path) -and !$Process.HasExited -and $timeout.Elapsed.TotalSeconds -lt 45) {
        Start-Sleep -Milliseconds 100
    }
    if (!(Test-Path -LiteralPath $Path)) { throw "Test did not reach $([IO.Path]::GetFileName($Path))." }
}

$repo = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $repo
try {
    $null = New-Item -ItemType Directory -Path target -Force
    $target = (Resolve-Path -LiteralPath target).Path
    $name = 'velocity-m8-' + [Guid]::NewGuid().ToString('N')
    $stage = Join-Path $target $name
    $backing = Join-Path $stage 'network'
    $null = New-Item -ItemType Directory -Path $stage
    $null = New-Item -ItemType Directory -Path $backing
    $null = New-Item -ItemType Directory -Path (Join-Path $stage 'local')
    [IO.File]::WriteAllText((Join-Path $backing 'before-disconnect.txt'), 'Synthetic SMB fixture')
    [IO.File]::WriteAllText((Join-Path $stage 'local/still-searchable.txt'), 'Synthetic local fixture')
    $share = @{
        Name = $name
        Path = $backing
        ReadAccess = $identity.Name
        CachingMode = 'None'
        Temporary = $true
    }
    $process = $null
    try {
        $null = New-SmbShare @share
        $launch = [Diagnostics.ProcessStartInfo]::new((Get-Command cargo.exe).Source)
        $launch.WorkingDirectory = $repo
        $launch.Arguments = 'test --locked -p velocity-native --test search network_root_reconnects_and_reconciles_downtime -- --ignored --nocapture'
        $launch.UseShellExecute = $false
        $launch.RedirectStandardOutput = $true
        $launch.RedirectStandardError = $true
        $launch.EnvironmentVariables['VELOCITY_M8_STAGE'] = $stage
        $launch.EnvironmentVariables['VELOCITY_M8_SHARE'] = "\\localhost\$name\"
        $process = [Diagnostics.Process]::Start($launch)
        $out = $process.StandardOutput.ReadToEndAsync()
        $err = $process.StandardError.ReadToEndAsync()
        Wait-TestMarker (Join-Path $stage 'offline-request') $process
        if ((Get-SmbShare -Name $name).Path -ine $backing) { throw 'Unexpected test share.' }
        Remove-SmbShare -Name $name -Force -Confirm:$false
        Wait-TestMarker (Join-Path $stage 'offline-observed') $process
        $null = New-SmbShare @share
        if (!$process.WaitForExit(45000)) { throw 'Folder reconnect test timed out.' }
        if ($process.ExitCode -ne 0) { throw "Folder reconnect test exited with code $($process.ExitCode)." }
    } finally {
        if ($process) {
            if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() }
            $out.GetAwaiter().GetResult()
            $err.GetAwaiter().GetResult()
            $process.Dispose()
        }
        $existing = Get-SmbShare -Name $name -ErrorAction SilentlyContinue
        if ($existing) {
            if ($existing.Path -ine $backing) { throw 'Unexpected test share during cleanup.' }
            Remove-SmbShare -Name $name -Force -Confirm:$false
        }
        $resolved = (Resolve-Path -LiteralPath $stage).Path
        if ($resolved -ine $stage -or [IO.Path]::GetDirectoryName($resolved) -ine $target) {
            throw 'Unexpected test fixture directory.'
        }
        Remove-Item -LiteralPath $resolved -Force -Recurse
    }
} finally {
    Pop-Location
}
