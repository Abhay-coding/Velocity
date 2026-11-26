param(
    [ValidateRange(1000, 1000000)]
    [int]$Entries = 200000,
    [ValidateRange(1, 100)]
    [int]$Trials = 10,
    [string]$ExistingRoot,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $repo
try {
    if (!$SkipBuild) {
        cargo build --locked --release -p velocity-ui --features desktop-acceptance
        if ($LASTEXITCODE) { throw "Acceptance build exited with code $LASTEXITCODE" }
    }

    $target = (Resolve-Path -LiteralPath target).Path
    $fixture = Join-Path $target ('velocity-typing-' + [guid]::NewGuid().ToString('N'))
    $root = if ($ExistingRoot) { $ExistingRoot } else { Join-Path $fixture 'corpus' }
    $oldTrials = $env:VELOCITY_TYPING_ACCEPTANCE
    $oldBackend = $env:SLINT_BACKEND
    $oldLog = $env:RUST_LOG
    $previousBenchmarkRoot = $env:VELOCITY_TYPING_BENCHMARK_ROOT
    try {
        $null = [IO.Directory]::CreateDirectory($fixture)
        if ($ExistingRoot) {
            $env:VELOCITY_TYPING_BENCHMARK_ROOT = $ExistingRoot
        } else {
            $null = [IO.Directory]::CreateDirectory($root)
            for ($index = 1; $index -lt $Entries; $index++) {
                if (($index - 1) % 1000 -eq 0) {
                    $group = Join-Path $root ('group-' + $index)
                    $null = [IO.Directory]::CreateDirectory($group)
                }
                [IO.File]::Create((Join-Path $group "report-$index.txt")).Dispose()
            }
            [IO.File]::Create((Join-Path $root 'report-needle.txt')).Dispose()
        }
        Copy-Item -LiteralPath target/release/velocity-ui.exe -Destination $fixture
        $config = @('[search]', ('roots = [' + (ConvertTo-Json -InputObject $root -Compress) + ']'),
            '[excludes]', 'patterns = ["**/System Volume Information/**", "**/$RECYCLE.BIN/**"]')
        [IO.File]::WriteAllLines((Join-Path $fixture 'config.toml'), $config, [Text.UTF8Encoding]::new($false))

        $env:VELOCITY_TYPING_ACCEPTANCE = $Trials.ToString()
        $env:SLINT_BACKEND = 'winit-femtovg'
        $env:RUST_LOG = 'velocity=debug,info'
        "corpus_files=$Entries renderer=winit-femtovg"
        & (Join-Path $fixture 'velocity-ui.exe')
        if ($LASTEXITCODE) { throw "Desktop acceptance exited with code $LASTEXITCODE" }
    } finally {
        $env:VELOCITY_TYPING_ACCEPTANCE = $oldTrials
        $env:SLINT_BACKEND = $oldBackend
        $env:RUST_LOG = $oldLog
        $env:VELOCITY_TYPING_BENCHMARK_ROOT = $previousBenchmarkRoot
        if (Test-Path -LiteralPath $fixture) {
            $resolved = (Resolve-Path -LiteralPath $fixture).Path
            if ($resolved -ine $fixture -or [IO.Path]::GetDirectoryName($resolved) -ine $target) {
                throw 'Unexpected acceptance fixture path'
            }
            Remove-Item -LiteralPath $resolved -Recurse -Force
        }
    }
} finally {
    Pop-Location
}
