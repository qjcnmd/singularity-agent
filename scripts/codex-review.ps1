<#
.SYNOPSIS
    Run an ephemeral local Codex CLI review with file redirection, hang/stall recovery, and .reviews/ archiving.
#>
[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Mandatory)]
    [string]$PromptFile,

    [string[]]$Images = @(),

    [string]$Model = '',

    [string]$Effort = '',

    [switch]$AllowWebSearch,

    [switch]$DeletePrompt,

    [ValidateRange(30, 3600)]
    [int]$StallSeconds = 480,

    [ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9._-]{0,31}\z')]
    [string]$Kind = 'review',

    [ValidateRange(0, 99)]
    [int]$Round = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$utf8 = [System.Text.UTF8Encoding]::new($false)
try { [Console]::OutputEncoding = $utf8 } catch {}

function Format-CliArg([string]$Value) {
    if ($Value.Length -eq 0 -or $Value -match '[\s"]') {
        $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
        $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
        return '"' + $escaped + '"'
    }
    return $Value
}

function Format-YamlScalar($Value) {
    if ($null -eq $Value) { return 'null' }
    $json = [string]$Value | ConvertTo-Json -Compress
    return [regex]::Replace($json, '[\x7F-\x9F]', { param($m) '\u{0:X4}' -f [int][char]$m.Value[0] })
}

function Resolve-CodexInvocation {
    $exe = Get-Command codex.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($exe) { return @{ FilePath = $exe.Source; PrefixArgs = @() } }

    $cmd = Get-Command codex.cmd -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $cmd) { throw 'codex executable (codex.exe / codex.cmd) not found in PATH.' }

    # Bypass cmd.exe (which expands %VAR% in paths) by invoking node.exe + codex.js directly.
    $shimDir = Split-Path -Parent $cmd.Source
    $codexJs = Join-Path $shimDir 'node_modules\@openai\codex\bin\codex.js'
    if (Test-Path -LiteralPath $codexJs -PathType Leaf) {
        $localNode = Join-Path $shimDir 'node.exe'
        $nodeExe = if (Test-Path -LiteralPath $localNode -PathType Leaf) {
            $localNode
        } else {
            $nodeCmd = Get-Command node.exe -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($nodeCmd) { $nodeCmd.Source } else { $null }
        }
        if ($nodeExe) { return @{ FilePath = $nodeExe; PrefixArgs = @($codexJs) } }
    }

    throw "Found $($cmd.Source) but could not locate node.exe + codex.js (refusing cmd.exe fallback)."
}

function Invoke-GitQuiet([string[]]$GitArgs) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $global:LASTEXITCODE = 0
        $out = @(& git @GitArgs 2>$null)
        return @{ Ok = ($LASTEXITCODE -eq 0); Lines = $out }
    } finally {
        $ErrorActionPreference = $prev
        $global:LASTEXITCODE = 0
    }
}

function Get-RepoRoot {
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) { return $null }
    $r = Invoke-GitQuiet @('rev-parse', '--show-toplevel')
    if (-not $r.Ok -or $r.Lines.Count -eq 0 -or -not $r.Lines[0]) { return $null }
    return [System.IO.Path]::GetFullPath($r.Lines[0].Trim())
}

function Get-WorkspaceSnapshot([string]$RepoRoot) {
    if (-not $RepoRoot) {
        return @{ Head = $null; ShortHead = 'nogit'; Branch = $null; Dirty = $null; Status = ''; Sig = $null }
    }
    $h = Invoke-GitQuiet @('-C', $RepoRoot, 'rev-parse', '--verify', 'HEAD')
    $head = if ($h.Ok -and $h.Lines.Count -gt 0 -and $h.Lines[0]) { $h.Lines[0].Trim() } else { $null }
    $b = Invoke-GitQuiet @('-C', $RepoRoot, 'symbolic-ref', '--short', 'HEAD')
    $branch = if ($b.Ok -and $b.Lines.Count -gt 0 -and $b.Lines[0]) { $b.Lines[0].Trim() } elseif ($head) { 'HEAD' } else { $null }
    $status = (Invoke-GitQuiet @('-C', $RepoRoot, '-c', 'core.quotepath=false', 'status', '--porcelain', '--untracked-files=all')).Lines -join "`n"
    $workDiff = (Invoke-GitQuiet @('-C', $RepoRoot, '-c', 'core.quotepath=false', 'diff', '--no-ext-diff')).Lines -join "`n"
    $indexDiff = (Invoke-GitQuiet @('-C', $RepoRoot, '-c', 'core.quotepath=false', 'diff', '--cached', '--no-ext-diff')).Lines -join "`n"
    return @{
        Head = $head
        ShortHead = if ($head) { $head.Substring(0, [Math]::Min(8, $head.Length)) } else { 'nohead' }
        Branch = $branch
        Dirty = ($status.Length -gt 0)
        Status = $status
        Sig = "$(if ($head) { $head } else { '<UNBORN>' })`n$status`n---`n$workDiff`n---`n$indexDiff"
    }
}

function Write-ReviewArchive([hashtable]$Meta, [string]$PromptText, [string]$ReportText, [string]$StderrTail) {
    $archiveDir = Join-Path $Meta.ArchiveBase '.reviews'
    [void][System.IO.Directory]::CreateDirectory($archiveDir)

    $stamp = $Meta.StartedAt.ToString('yyyy-MM-dd_HHmmss')
    $roundPart = if ($Meta.Round -gt 0) { "_r$($Meta.Round)" } else { '' }
    $baseName = "${stamp}_$($Meta.Kind)${roundPart}_$($Meta.ShortHead)"
    $archivePath = Join-Path $archiveDir "$baseName.md"
    if (Test-Path -LiteralPath $archivePath) {
        $archivePath = Join-Path $archiveDir "${baseName}_$($Meta.RunId.Substring(0, 6)).md"
    }

    $fmtBool = { param($v) if ($null -eq $v) { 'null' } elseif ($v) { 'true' } else { 'false' } }
    $normPrompt = $PromptText.Replace("`r`n", "`n").Replace("`r", "`n").TrimEnd()
    $normReport = if ([string]::IsNullOrWhiteSpace($ReportText)) { '_(empty)_' } else { $ReportText.Replace("`r`n", "`n").Replace("`r", "`n").TrimEnd() }
    $lines = [System.Collections.Generic.List[string]]::new()
    $lines.AddRange([string[]]@(
        '---',
        "ts: $(Format-YamlScalar $Meta.StartedAt.ToString('yyyy-MM-ddTHH:mm:ssK'))",
        'reviewer: "codex"',
        "model: $(Format-YamlScalar $(if ($Meta.Model) { $Meta.Model } else { 'default' }))",
        "effort: $(Format-YamlScalar $(if ($Meta.Effort) { $Meta.Effort } else { 'default' }))",
        "kind: $(Format-YamlScalar $Meta.Kind)",
        "round: $(if ($Meta.Round -gt 0) { $Meta.Round } else { 'null' })",
        "branch: $(Format-YamlScalar $Meta.Branch)",
        "head: $(Format-YamlScalar $Meta.Head)",
        "dirty: $(& $fmtBool $Meta.Dirty)",
        "duration_ms: $($Meta.DurationMs)",
        "exit_code: $(if ($null -eq $Meta.ExitCode) { 'null' } else { $Meta.ExitCode })",
        "recovered: $(& $fmtBool $Meta.Recovered)",
        "stalled: $(& $fmtBool $Meta.Stalled)",
        "workspace_changed: $(& $fmtBool $Meta.WorkspaceChanged)",
        "error: $(Format-YamlScalar $Meta.Error)",
        '---', '', '# Review Brief', '', $normPrompt, '', '---', '', '# Review Report', '', $normReport
    ))
    if ($Meta.Error -and -not [string]::IsNullOrWhiteSpace($StderrTail)) {
        $normTail = $StderrTail.Replace("`r`n", "`n").Replace("`r", "`n").TrimEnd()
        $lines.AddRange([string[]]@('', '---', '', '# Stderr Tail', '', '~~~~~~text', $normTail, '~~~~~~'))
    }
    [System.IO.File]::WriteAllText($archivePath, ($lines -join "`n") + "`n", $utf8)
    return $archivePath
}

if (-not (Test-Path -LiteralPath $PromptFile -PathType Leaf)) {
    throw "PromptFile must be an existing file: $PromptFile"
}
$resolvedPrompt = (Resolve-Path -LiteralPath $PromptFile).ProviderPath
$promptText = [System.IO.File]::ReadAllText($resolvedPrompt, $utf8)
$runId = [guid]::NewGuid().ToString('N')
$outFile = Join-Path $env:TEMP "codex-review-$runId.md"
$stdoutFile = Join-Path $env:TEMP "codex-review-$runId.out"
$logFile = Join-Path $env:TEMP "codex-review-$runId.log"

$repoRoot = Get-RepoRoot
$archiveBase = if ($repoRoot) { $repoRoot } else { (Get-Location).ProviderPath }
$before = Get-WorkspaceSnapshot $repoRoot
$proc = $null; $stopwatch = $null; $startedAt = $null; $attempted = $false
$exitCode = $null; $recovered = $false; $stalled = $false; $workspaceChanged = $null
$report = ''; $runError = $null

try {
    $resolvedImages = @($Images | ForEach-Object {
        if (-not (Test-Path -LiteralPath $_ -PathType Leaf)) { throw "Image path must be an existing file: $_" }
        (Resolve-Path -LiteralPath $_).ProviderPath
    })
    $inv = Resolve-CodexInvocation

    $cliArgs = [System.Collections.Generic.List[string]]::new()
    foreach ($prefix in $inv.PrefixArgs) { $cliArgs.Add($prefix) }
    $cliArgs.AddRange([string[]]@('exec', '-c', 'notify=[]', '-c', 'features.memories=false'))
    if ($Model) { $cliArgs.AddRange([string[]]@('-m', $Model)) }
    if ($Effort) { $cliArgs.AddRange([string[]]@('-c', "model_reasoning_effort=$Effort")) }
    if (-not $AllowWebSearch) { $cliArgs.AddRange([string[]]@('-c', 'web_search=disabled')) }
    $cliArgs.AddRange([string[]]@('--ephemeral', '--skip-git-repo-check', '-o', $outFile))
    foreach ($img in $resolvedImages) { $cliArgs.AddRange([string[]]@('-i', $img)) }
    $cliArgs.AddRange([string[]]@('--', '-'))
    $argLine = (@($cliArgs | ForEach-Object { Format-CliArg $_ }) -join ' ')

    $startedAt = Get-Date
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $attempted = $true
    $proc = Start-Process -FilePath $inv.FilePath -ArgumentList $argLine `
        -WorkingDirectory (Get-Location).ProviderPath `
        -RedirectStandardInput $resolvedPrompt -RedirectStandardOutput $stdoutFile -RedirectStandardError $logFile `
        -NoNewWindow -PassThru
    $null = $proc.Handle # Ensure ExitCode stays populated on Windows PowerShell 5.1.

    Write-Host "OUT_FILE=$outFile"
    Write-Host "LOG_FILE=$logFile"

    $stableSince = $null; $lastOutSig = ''; $lastLogSig = ''; $lastActivity = Get-Date
    while (-not $proc.HasExited) {
        Start-Sleep -Seconds 1
        if (Test-Path -LiteralPath $logFile) {
            $li = Get-Item -LiteralPath $logFile
            $logSig = "$($li.Length)|$($li.LastWriteTimeUtc.Ticks)"
            if ($logSig -ne $lastLogSig) { $lastLogSig = $logSig; $lastActivity = Get-Date }
        }
        if ((Test-Path -LiteralPath $outFile) -and ($fi = Get-Item -LiteralPath $outFile).Length -gt 0) {
            $outSig = "$($fi.Length)|$($fi.LastWriteTimeUtc.Ticks)"
            if ($outSig -ne $lastOutSig) { $lastOutSig = $outSig; $stableSince = Get-Date }
            elseif (((Get-Date) - $stableSince).TotalSeconds -ge 15) { $recovered = $true; break }
        } elseif (((Get-Date) - $lastActivity).TotalSeconds -ge $StallSeconds) {
            $stalled = $true; break
        }
    }

    if ($recovered -or $stalled) {
        & taskkill /PID $proc.Id /T /F 2>$null | Out-Null
    }
    $null = $proc.WaitForExit(3000)
    $exitCode = if ($recovered) { 0 } elseif ($proc.HasExited) { $proc.ExitCode } else { -1 }

    $report = if (Test-Path -LiteralPath $outFile -PathType Leaf) { [System.IO.File]::ReadAllText($outFile, $utf8) } else { '' }
    if ($stalled) {
        Get-Content -LiteralPath $logFile -Tail 60 -Encoding UTF8 -ErrorAction SilentlyContinue | Write-Host
        throw "codex exec stalled (no stderr progress for ${StallSeconds}s)."
    }
    if ($exitCode -ne 0 -or [string]::IsNullOrWhiteSpace($report)) {
        Get-Content -LiteralPath $logFile -Tail 60 -Encoding UTF8 -ErrorAction SilentlyContinue | Write-Host
        throw "codex exec failed (exit=$exitCode)."
    }
    Write-Output $report
} catch {
    $runError = $_.Exception.Message
    throw
} finally {
    if ($null -ne $proc) {
        if (-not $proc.HasExited) { & taskkill /PID $proc.Id /T /F 2>$null | Out-Null }
        $proc.Dispose()
    }
    if ($null -ne $stopwatch) { $stopwatch.Stop() }

    if ($repoRoot -and $null -ne $before.Sig) {
        $after = Get-WorkspaceSnapshot $repoRoot
        if ($before.Sig -cne $after.Sig) {
            $workspaceChanged = $true
            $headNote = if ($before.Head -cne $after.Head) { "HEAD: $($before.Head) -> $($after.Head)`n" } else { '' }
            Write-Warning "Workspace changed during review.`n$headNote$($after.Status)"
        } else {
            $workspaceChanged = $false
        }
    }

    if ($attempted) {
        try {
            if ($null -eq $exitCode -and $null -eq $runError) { $runError = 'review interrupted before completion' }
            $stderrTail = if ($runError -and (Test-Path -LiteralPath $logFile -PathType Leaf)) {
                (Get-Content -LiteralPath $logFile -Tail 60 -Encoding UTF8 -ErrorAction SilentlyContinue) -join "`n"
            } else { '' }
            $archivePath = Write-ReviewArchive @{
                ArchiveBase = $archiveBase; StartedAt = $startedAt; RunId = $runId
                Kind = $Kind; Round = $Round; Branch = $before.Branch; Head = $before.Head
                ShortHead = $before.ShortHead; Dirty = $before.Dirty; Model = $Model; Effort = $Effort
                DurationMs = if ($stopwatch) { [long]$stopwatch.ElapsedMilliseconds } else { 0L }
                ExitCode = $exitCode; Recovered = $recovered; Stalled = $stalled
                WorkspaceChanged = $workspaceChanged; Error = $runError
            } $promptText $report $stderrTail
            Write-Host "ARCHIVE_FILE=$archivePath"
        } catch {
            Write-Warning "Failed to write review archive: $_"
        }
    }

    $cleanup = @($outFile, $stdoutFile, $logFile)
    if ($DeletePrompt) { $cleanup += $resolvedPrompt }
    foreach ($f in $cleanup) {
        if (Test-Path -LiteralPath $f -PathType Leaf) { Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue }
    }
}
