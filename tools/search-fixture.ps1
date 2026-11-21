param(
    [ValidateSet('Create', 'Verify', 'SelfTest', 'Everything')]
    [string]$Action = 'SelfTest',
    [string]$Directory,
    [string]$Actual,
    [string]$EverythingCli = 'es.exe'
)

$ErrorActionPreference = 'Stop'

function New-Fixture($Path) {
    if (Test-Path -LiteralPath $Path) { throw "Fixture directory already exists: $Path" }
    $null = New-Item -ItemType Directory -Path $Path
    $root = (Resolve-Path -LiteralPath $Path).Path
    $unicode = [string][char]0x4E88 + [char]0x7B97
    $longPath = 'long/' + ('a' * 110) + '/' + ('b' * 110) + '/' + ('c' * 80) + '/budget-long.txt'
    $entries = @(
        @{ path = 'alpha/Budget Draft.pdf'; type = 'file' }
        @{ path = 'alpha/Budget Draft.txt'; type = 'file' }
        @{ path = 'beta/Budget Draft.pdf'; type = 'file' }
        @{ path = "beta/$unicode-budget.pdf"; type = 'file' }
        @{ path = "gamma/$unicode-budget.pdf"; type = 'file' }
        @{ path = 'beta/Annual BUDGET.xlsx'; type = 'file' }
        @{ path = 'beta/budget archive'; type = 'directory' }
        @{ path = 'beta/budget-link-a.txt'; type = 'file' }
        @{ path = 'beta/budget-link-b.txt'; type = 'file' }
        @{ path = 'beta/rare-7f3a.txt'; type = 'file' }
        @{ path = 'empty'; type = 'directory' }
        @{ path = $longPath; type = 'file' }
    )
    foreach ($entry in $entries) {
        $absolute = Join-Path $root $entry.path
        if ($entry.type -eq 'directory') {
            $null = [IO.Directory]::CreateDirectory($absolute)
        } else {
            $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($absolute))
            if ($entry.path -eq 'beta/budget-link-b.txt') {
                $null = New-Item -ItemType HardLink -Path $absolute -Target (Join-Path $root 'beta/budget-link-a.txt')
            } else {
                [IO.File]::WriteAllText($absolute, $entry.path)
            }
        }
    }
    $null = [IO.Directory]::CreateDirectory((Join-Path $root 'excluded'))
    [IO.File]::WriteAllText((Join-Path $root 'excluded/budget-hidden.pdf'), 'excluded')
    $null = New-Item -ItemType Junction -Path (Join-Path $root 'junction') -Target (Join-Path $root 'beta')

    $directories = @('alpha', 'beta', 'gamma', 'long', ('long/' + ('a' * 110)),
        ('long/' + ('a' * 110) + '/' + ('b' * 110)), ($longPath -replace '/[^/]+$'))
    $manifest = [ordered]@{
        root = $root
        excluded = @('excluded', 'junction')
        entries = @($entries) + @($directories | ForEach-Object { @{ path = $_; type = 'directory' } })
        queries = @(
            @{ query = 'BuDgEt'; paths = @('alpha/Budget Draft.pdf', 'alpha/Budget Draft.txt', 'beta/Budget Draft.pdf', "beta/$unicode-budget.pdf", "gamma/$unicode-budget.pdf", 'beta/Annual BUDGET.xlsx', 'beta/budget archive', 'beta/budget-link-a.txt', 'beta/budget-link-b.txt', $longPath) }
            @{ query = 'rare-7f3a'; paths = @('beta/rare-7f3a.txt') }
            @{ query = 'no-match-67e2'; paths = @() }
            @{ query = '"budget draft"'; paths = @('alpha/Budget Draft.pdf', 'alpha/Budget Draft.txt', 'beta/Budget Draft.pdf') }
            @{ query = '"beta\budget draft.pdf"'; paths = @('beta/Budget Draft.pdf') }
            @{ query = 'ext:pdf budget'; paths = @('alpha/Budget Draft.pdf', 'beta/Budget Draft.pdf', "beta/$unicode-budget.pdf", "gamma/$unicode-budget.pdf") }
        )
    }
    $manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath "$root.manifest.json" -Encoding utf8
    $manifest
}

function Compare-Entries($Expected, $Observed) {
    $counts = [Collections.Generic.Dictionary[string, int]]::new([StringComparer]::Ordinal)
    foreach ($entry in $Expected) { $counts["$($entry.type) $($entry.path)"] = 1 }
    foreach ($entry in $Observed) {
        $key = "$($entry.type) $($entry.path)"
        $counts[$key] = [int]$counts[$key] - 1
    }
    foreach ($key in ($counts.Keys | Sort-Object)) {
        if ($counts[$key] -gt 0) { "Missing $($counts[$key]) x $key" }
        if ($counts[$key] -lt 0) { "Extra $(-$counts[$key]) x $key" }
    }
}

if ($Action -eq 'SelfTest') {
    $Directory = Join-Path ([IO.Path]::GetTempPath()) ('velocity-fixture-' + [guid]::NewGuid())
    try {
        $manifest = New-Fixture $Directory
        $observed = @($manifest.entries | Select-Object -Skip 1) + @($manifest.entries[1])
        $differences = @(Compare-Entries $manifest.entries $observed)
        if ($differences.Count -ne 2 -or $differences -cnotcontains 'Missing 1 x file alpha/Budget Draft.pdf' -or
            $differences -cnotcontains 'Extra 1 x file alpha/Budget Draft.txt') {
            throw "Unexpected comparison result: $differences"
        }
        $differences
        'Fixture and comparison self-test passed.'
    } finally {
        if (Test-Path -LiteralPath $Directory) {
            $fixturePath = [IO.Path]::GetFullPath($Directory)
            $tempPath = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
            if (![string]::Equals([IO.Path]::GetDirectoryName($fixturePath),
                [IO.Path]::TrimEndingDirectorySeparator($tempPath), [StringComparison]::OrdinalIgnoreCase)) {
                throw "Unexpected fixture cleanup path: $fixturePath"
            }
            $junction = Join-Path $fixturePath 'junction'
            if (Test-Path -LiteralPath $junction) { Remove-Item -LiteralPath $junction -Force }
            [IO.Directory]::Delete($fixturePath, $true)
        }
        if (Test-Path -LiteralPath "$Directory.manifest.json") {
            Remove-Item -LiteralPath "$Directory.manifest.json"
        }
    }
} elseif ($Action -eq 'Create') {
    if (!$Directory) { throw 'Supply a new fixture directory with -Directory.' }
    $manifest = New-Fixture $Directory
    "$($manifest.root).manifest.json"
} else {
    if (!$Directory) { throw 'Supply the fixture directory with -Directory.' }
    $root = (Resolve-Path -LiteralPath $Directory).Path
    $manifest = Get-Content -LiteralPath "$root.manifest.json" -Raw | ConvertFrom-Json
    if ($Action -eq 'Verify') {
        if (!$Actual) { throw 'Supply a JSON array of relative path and type objects with -Actual.' }
        $observed = Get-Content -LiteralPath $Actual -Raw | ConvertFrom-Json
        $differences = @(Compare-Entries $manifest.entries $observed)
        if ($differences.Count) { $differences; exit 1 }
        'All fixture entries matched.'
    } else {
        foreach ($case in $manifest.queries) {
            $scope = '"' + $root + '\" !"' + $root + '\excluded\" !"' + $root + '\junction\" '
            $paths = & $EverythingCli -utf8 -- ($scope + $case.query)
            if ($LASTEXITCODE -ne 0) { throw "Everything CLI exited with code $LASTEXITCODE." }
            $observed = @($paths | ForEach-Object {
                @{ path = $_.Substring($root.Length + 1).Replace('\', '/'); type = $(if ([IO.Directory]::Exists($_)) { 'directory' } else { 'file' }) }
            })
            $case.query
            $expected = @($manifest.entries | Where-Object { $case.paths -ccontains $_.path })
            $differences = @(Compare-Entries $expected $observed)
            if ($differences.Count) { $differences; $mismatch = $true } else { 'Matched.' }
        }
        if ($mismatch) { exit 1 }
    }
}
