[CmdletBinding(DefaultParameterSetName = 'TwoAccounts')]
param(
    [Parameter(Mandatory, ParameterSetName = 'TwoAccounts')]
    [PSCredential]$ObserverCredential,
    [Parameter(Mandatory, ParameterSetName = 'Harness')]
    [switch]$VerifyHarness
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$ownerSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$observerSid = $ownerSid
if (!$VerifyHarness) {
    $observerName = $ObserverCredential.UserName
    if ($observerName.StartsWith('.\')) {
        $observerName = $env:COMPUTERNAME + $observerName.Substring(1)
    }
    $observerSid = ([Security.Principal.NTAccount]::new($observerName)).Translate(
        [Security.Principal.SecurityIdentifier]).Value
    if ($observerSid -eq $ownerSid) { throw 'Use an existing Windows account with a distinct SID for the observer.' }
}

function Set-FixtureAcl([string]$Path, [string[]]$Options) {
    & icacls.exe $Path @Options | Out-Null
    if ($LASTEXITCODE) { throw "Could not apply the fixture ACL to $Path (icacls exit $LASTEXITCODE)." }
}

function Set-ProbeScope([string]$Directory, [string]$Sid, [bool]$CanReadPrivate) {
    [IO.File]::WriteAllText((Join-Path $Directory 'scope-receipt.txt'), '')
    @{ expected_sid = $Sid; can_read_private = $CanReadPrivate } |
        ConvertTo-Json -Compress | Set-Content -LiteralPath (Join-Path $Directory 'scope-probe.json') -Encoding utf8
}

Push-Location -LiteralPath $repo
try {
    $messages = @(cargo test --locked -p velocity-native --test user_scope --no-run --message-format=json |
        ForEach-Object { $_ | ConvertFrom-Json })
    if ($LASTEXITCODE) { throw "Scope test build exited with code $LASTEXITCODE." }
    $binary = $messages | Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'user_scope' -and $_.executable } |
        Select-Object -Last 1 -ExpandProperty executable
    if (!$binary) { throw 'Cargo did not report the scope test executable.' }

    $target = (Resolve-Path -LiteralPath target).Path
    if ([IO.DriveInfo]::new([IO.Path]::GetPathRoot($target)).DriveFormat -ne 'NTFS') {
        throw 'Run the scope acceptance test from a workspace on a local NTFS volume.'
    }
    $fixture = Join-Path $target ('velocity-user-scope-' + [guid]::NewGuid().ToString('N'))
    $restricted = Join-Path $fixture 'shared/restricted-budget'
    $probe = Join-Path $fixture 'scope-probe.exe'
    $receipt = Join-Path $fixture 'scope-receipt.txt'
    $probeArgs = @('--ignored', '--exact', 'verify_windows_account_scope', '--nocapture')
    $trustee = "*$observerSid"
    $denyApplied = $false
    $null = New-Item -ItemType Directory -Path $fixture
    try {
        foreach ($relative in 'shared/public/budget-shared.txt', 'shared/restricted-budget/budget-private.txt',
            'shared/excluded/budget-excluded.txt', 'other/budget-independent.txt') {
            $path = Join-Path $fixture $relative
            $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($path))
            [IO.File]::WriteAllText($path, 'Disposable Velocity scope fixture')
        }
        Copy-Item -LiteralPath $binary -Destination $probe
        Set-ProbeScope $fixture $ownerSid $true
        if (!$VerifyHarness) {
            Set-FixtureAcl $fixture @('/grant', "${trustee}:(OI)(CI)(RX)")
            Set-FixtureAcl $receipt @('/grant', "${trustee}:(W)")
            Set-FixtureAcl $restricted @('/deny', "${trustee}:(OI)(CI)(RX)")
            $denyApplied = $true
        }
        & $probe @probeArgs
        if ($LASTEXITCODE -or [IO.File]::ReadAllText($receipt) -cne 'owner-verified') { throw 'The owner probe failed.' }

        if ($VerifyHarness) {
            Set-FixtureAcl $restricted @('/deny', "${trustee}:(OI)(CI)(RX)")
            $denyApplied = $true
        }
        Set-ProbeScope $fixture $observerSid $false
        if ($VerifyHarness) {
            & $probe @probeArgs
            $observerExitCode = $LASTEXITCODE
        } else {
            $child = Start-Process -FilePath $probe -WorkingDirectory $fixture -ArgumentList $probeArgs `
                -Credential $ObserverCredential -WindowStyle Hidden -Wait -PassThru
            $observerExitCode = $child.ExitCode
        }
        if ($observerExitCode -ne 0 -or [IO.File]::ReadAllText($receipt) -cne 'observer-verified') {
            throw "The denied-access probe failed (exit $observerExitCode)."
        }
        if ($VerifyHarness) {
            'Harness check passed under one account with two ACL states.'
        } else {
            'M5 passed under two distinct Windows account SIDs.'
        }
    } finally {
        $resolved = (Resolve-Path -LiteralPath $fixture).Path
        if ($resolved -ine $fixture -or [IO.Path]::GetDirectoryName($resolved) -ine $target) {
            throw 'Unexpected scope fixture path.'
        }
        if ($denyApplied) { Set-FixtureAcl $restricted @('/remove:d', $trustee) }
        Remove-Item -LiteralPath $resolved -Force -Recurse
    }
} finally {
    Pop-Location
}
