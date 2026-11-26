param([switch]$DefaultHotkey, [switch]$PhysicalHotkey)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run the desktop controls acceptance without elevation.'
}

$repo = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $repo
try {
    cargo build --locked -p velocity-ui --features desktop-acceptance
    if ($LASTEXITCODE) { throw "Desktop acceptance build exited with code $LASTEXITCODE." }
    $target = (Resolve-Path -LiteralPath target).Path
    $stage = Join-Path $target ('velocity-controls-' + [Guid]::NewGuid().ToString('N'))
    $healthy = Join-Path $stage 'healthy'
    $reservoir = Join-Path $stage 'reservoir'
    $process = $null
    try {
        foreach ($directory in 'alpha', 'beta', 'report-folder') {
            $null = [IO.Directory]::CreateDirectory((Join-Path $healthy $directory))
        }
        $unicodeName = 'report-' + [char]0x540c + [char]0x540d + '.txt'
        [IO.File]::WriteAllText((Join-Path $healthy "alpha/$unicodeName"), 'Alpha desktop test file')
        [IO.File]::WriteAllText((Join-Path $healthy "beta/$unicodeName"), 'Beta desktop test file')
        [IO.File]::WriteAllText((Join-Path $healthy 'report-needle.txt'), 'Rapid typing test file')
        for ($index = 1; $index -le 16; $index++) {
            [IO.File]::WriteAllText((Join-Path $healthy ('report-{0:d2}.txt' -f $index)), 'Desktop test file')
        }
        for ($index = 0; $index -lt 25000; $index++) {
            if (($index % 500) -eq 0) {
                $group = Join-Path $reservoir "group-$index"
                $null = [IO.Directory]::CreateDirectory($group)
            }
            [IO.File]::Create((Join-Path $group "shutdown-$index.txt")).Dispose()
        }
        Copy-Item -LiteralPath target/debug/velocity-ui.exe -Destination $stage
        $roots = ConvertTo-Json -InputObject @($healthy, (Join-Path $stage 'missing')) -Compress
        $config = @('[search]', "roots = $roots")
        if (!$DefaultHotkey) { $config += @('[hotkey]', 'key = "Ctrl+Shift+F9"') }
        [IO.File]::WriteAllLines((Join-Path $stage 'config.toml'), [string[]]$config, [Text.UTF8Encoding]::new($false))

        $launch = [Diagnostics.ProcessStartInfo]::new((Join-Path $stage 'velocity-ui.exe'))
        $launch.WorkingDirectory = $stage
        $launch.UseShellExecute = $false
        $launch.RedirectStandardOutput = $true
        $launch.RedirectStandardError = $true
        $launch.EnvironmentVariables['VELOCITY_CONTROLS_ACCEPTANCE'] = $stage
        if ($PhysicalHotkey) { $launch.EnvironmentVariables['VELOCITY_CONTROLS_PHYSICAL_HOTKEY'] = '1' }
        $launch.EnvironmentVariables['SLINT_BACKEND'] = 'winit-femtovg'
        $launch.EnvironmentVariables['RUST_LOG'] = 'velocity_ui::hotkey=debug,warn'
        $process = [Diagnostics.Process]::Start($launch)
        $out = $process.StandardOutput.ReadToEndAsync()
        $err = $process.StandardError.ReadToEndAsync()
        if (!$process.WaitForExit(90000)) { throw 'Desktop controls acceptance timed out.' }
        if ($process.ExitCode -ne 0) { throw "Desktop controls acceptance exited with code $($process.ExitCode)." }
    } finally {
        if ($process) {
            if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() }
            $out.GetAwaiter().GetResult()
            $err.GetAwaiter().GetResult()
            $process.Dispose()
        }
        if (Test-Path -LiteralPath $stage) {
            $resolved = (Resolve-Path -LiteralPath $stage).Path
            if ($resolved -ine $stage -or [IO.Path]::GetDirectoryName($resolved) -ine $target) {
                throw 'Unexpected desktop test fixture path.'
            }
            Remove-Item -LiteralPath $resolved -Recurse -Force
        }
    }
} finally {
    Pop-Location
}
