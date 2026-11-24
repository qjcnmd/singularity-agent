<#
.SYNOPSIS
    Thin deterministic workflow wrapper over scripts/check.ps1.
.DESCRIPTION
    Commands:
      gate          Auto-scoped check runner with HEAD green-stamp cache, 40-line failure tail, and ledger logging.
      radar         Merge-base diff summary with sensitive-area radar and post-review bloat check.
      record-review Validate and append structured review findings/verdicts to ~/.singularity-dev/ledger.jsonl.
      digest        Zero-token aggregation of recent ledger entries for outer-loop meta-system analysis.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet('gate', 'radar', 'record-review', 'digest')]
    [string]$Command,

    [ValidateSet('auto', 'rust', 'protocol', 'desktop', 'size', 'all')]
    [string]$Scope = 'auto',

    [switch]$SkipE2E,
    [switch]$Force,
    [switch]$Json,

    [string]$Base = '',
    [string]$InputPath = '',
    [string]$JsonString = '',
    [string]$Reviewer = '',
    [string]$HandoffVersion = 'v1',
    [int]$Round = 1,
    [int]$Days = 14
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$repoName = Split-Path -Leaf $repoRoot
$devHome = if ($env:SINGULARITY_DEV_HOME) { $env:SINGULARITY_DEV_HOME } else { Join-Path $HOME '.singularity-dev' }
$ledgerPath = Join-Path $devHome 'ledger.jsonl'
$stampsDir = Join-Path $devHome 'stamps'
$logsDir = Join-Path $devHome 'logs'
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$gitOpts = @('-c', 'core.quotepath=false', '-c', 'core.safecrlf=false')

function Get-Prop($Obj, [string]$Name, $Default = $null) {
    if ($null -eq $Obj) { return $Default }
    $p = $Obj.PSObject.Properties[$Name]
    if ($null -eq $p) { return $Default }
    return $p.Value
}

function Ensure-Dir([string]$DirPath) {
    if (-not (Test-Path -LiteralPath $DirPath)) {
        New-Item -ItemType Directory -Path $DirPath -Force | Out-Null
    }
}

function Append-Ledger([System.Collections.IDictionary]$Entry) {
    Ensure-Dir $devHome
    $line = ($Entry | ConvertTo-Json -Depth 10 -Compress)
    [System.IO.File]::AppendAllText($ledgerPath, "$line`n", $utf8NoBom)
}

function Get-GitInfo {
    $branch = (& git @gitOpts rev-parse --abbrev-ref HEAD 2>$null | Out-String).Trim()
    if (-not $branch) { $branch = 'unknown' }
    $head = (& git @gitOpts rev-parse HEAD 2>$null | Out-String).Trim()
    if (-not $head) { $head = 'unknown' }
    $porcelain = @(& git @gitOpts status --porcelain 2>$null | Where-Object { $_ })
    $global:LASTEXITCODE = 0
    return [pscustomobject]@{ Branch = $branch; Head = $head; Dirty = ($porcelain.Count -gt 0) }
}

function Resolve-BaseRef([string]$RequestedBase) {
    if ($RequestedBase) { return $RequestedBase }
    foreach ($candidate in @('origin/main', 'main')) {
        $null = & git rev-parse --verify $candidate 2>$null
        if ($LASTEXITCODE -eq 0) { return $candidate }
    }
    $global:LASTEXITCODE = 0
    return 'HEAD'
}

function Get-ChangedFiles([string]$BaseRef) {
    $mergeBase = (& git @gitOpts merge-base $BaseRef HEAD 2>$null | Out-String).Trim()
    if (-not $mergeBase) { $mergeBase = 'HEAD' }
    $set = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($f in @(& git @gitOpts diff --name-only $mergeBase 2>$null)) {
        if ($f) { [void]$set.Add($f.Replace('\', '/')) }
    }
    foreach ($f in @(& git @gitOpts ls-files --others --exclude-standard 2>$null)) {
        if ($f) { [void]$set.Add($f.Replace('\', '/')) }
    }
    $global:LASTEXITCODE = 0
    return [pscustomobject]@{ MergeBase = $mergeBase; Files = @($set | Sort-Object) }
}

function Test-ProdSourcePath([string]$RelPath) {
    if (-not $RelPath) { return $false }
    $p = $RelPath.Replace('\', '/')
    if ($p -match '(^|/)(tests|e2e|node_modules|dist|dist-desktop)(/|$)') { return $false }
    ($p -match '^crates/[^/]+/src/.+\.rs$' -and $p -notmatch '(^|/)property_tests\.rs$') -or
        ($p -match '^apps/desktop/desktop/.+\.(ts|tsx|cts)$') -or
        ($p -match '^apps/desktop/src/.+\.(ts|tsx)$' -and $p -ne 'apps/desktop/src/protocol.generated.ts')
}

function Resolve-AutoScope([string[]]$Files) {
    if ($Files.Count -eq 0) { return 'docs-only' }
    $touchesProtocol = $false
    $touchesRust = $false
    $touchesDesktop = $false
    $touchesOtherCode = $false

    foreach ($f in $Files) {
        if ($f -match '^crates/protocol/') { $touchesProtocol = $true; $touchesRust = $true }
        elseif ($f -match '^crates/|^Cargo\.(toml|lock)$|^deny\.toml$|^rustfmt\.toml$|^rust-toolchain\.toml$') { $touchesRust = $true }
        elseif ($f -match '^apps/desktop/') { $touchesDesktop = $true }
        elseif ($f -match '^(docs/|\.agents/|\.github/|README\.md$|AGENTS\.md$|LICENSE$|[^/]+\.html$|[^/]+\.md$)') { continue }
        else { $touchesOtherCode = $true }
    }

    if ($touchesProtocol -or ($touchesRust -and $touchesDesktop)) { return 'all' }
    if ($touchesRust) { return 'rust' }
    if ($touchesDesktop) { return 'desktop' }
    if ($touchesOtherCode) { return 'size' }
    return 'docs-only'
}

function Test-ScopeCovered([string]$StampedScope, [bool]$StampedSkipE2E, [string]$WantedScope, [bool]$WantedSkipE2E) {
    $coversScope = ($StampedScope -eq $WantedScope) -or ($StampedScope -eq 'all')
    if (-not $coversScope) { return $false }
    if (($WantedScope -in @('desktop', 'all')) -and (-not $WantedSkipE2E) -and $StampedSkipE2E) { return $false }
    return $true
}

function Get-ErrorSignature([string[]]$Lines) {
    for ($i = $Lines.Count - 1; $i -ge 0; $i--) {
        $l = $Lines[$i]
        if ($l -match 'clippy::([a-z0-9_]+)') { return "clippy:$($Matches[1])" }
        if ($l -match '\berror\[(E\d{4})\]') { return "rustc:$($Matches[1])" }
        if ($l -match '\berror (TS\d{4}):') { return "ts:$($Matches[1])" }
        if ($l -match 'exceed 600-line limit') { return 'size:over_600' }
        if ($l -match 'Check failed \(\d+\): (.+)$') { return "step:$($Matches[1].Trim())" }
    }
    $tail = ($Lines | Select-Object -Last 6) -join "`n"
    $norm = ($tail -replace '\d+', 'N' -replace '[A-Za-z]:\\[^\s:]+', '<PATH>').Trim()
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($norm)
    $hash = [System.BitConverter]::ToString([System.Security.Cryptography.SHA256]::HashData($bytes)).Replace('-', '').Substring(0, 12).ToLowerInvariant()
    return "err:$hash"
}

function Get-BranchDelta([string]$MergeBase) {
    $ins = 0; $del = 0; $newProd = 0
    foreach ($line in @(& git @gitOpts diff --numstat --no-renames $MergeBase 2>$null)) {
        $parts = $line -split "`t", 3
        if ($parts.Count -eq 3 -and $parts[0] -match '^\d+$' -and (Test-ProdSourcePath $parts[2])) {
            $ins += [int]$parts[0]; $del += [int]$parts[1]
        }
    }
    foreach ($rel in @(& git @gitOpts diff --name-only --diff-filter=A $MergeBase 2>$null)) {
        if (Test-ProdSourcePath $rel) { $newProd++ }
    }
    foreach ($rel in @(& git @gitOpts ls-files --others --exclude-standard 2>$null)) {
        if (Test-ProdSourcePath $rel) {
            $newProd++
            $full = Join-Path $repoRoot $rel
            if (Test-Path -LiteralPath $full) { $ins += [System.IO.File]::ReadAllLines($full).Length }
        }
    }
    $global:LASTEXITCODE = 0
    return [pscustomobject]@{ Ins = $ins; Del = $del; Net = ($ins - $del); NewProdFiles = $newProd }
}

function Read-LedgerEntries([int]$WindowDays) {
    if (-not (Test-Path -LiteralPath $ledgerPath)) { return @() }
    $cutoff = [DateTimeOffset]::UtcNow.AddDays(-$WindowDays)
    $items = [System.Collections.Generic.List[object]]::new()
    foreach ($line in [System.IO.File]::ReadLines($ledgerPath)) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        try {
            $obj = $line | ConvertFrom-Json
            if ($WindowDays -le 0) { $items.Add($obj); continue }
            if ($obj.ts -and ([DateTimeOffset]::Parse($obj.ts) -ge $cutoff)) { $items.Add($obj) }
        } catch { continue }
    }
    return @($items)
}

function Invoke-Gate {
    $git = Get-GitInfo
    $baseRef = Resolve-BaseRef $Base
    $changed = Get-ChangedFiles $baseRef
    $resolvedScope = if ($Scope -eq 'auto') { Resolve-AutoScope $changed.Files } else { $Scope }
    $safeBranch = ($git.Branch -replace '[^A-Za-z0-9._-]', '_')
    $stampPath = Join-Path $stampsDir "$repoName-$safeBranch.json"

    if ($resolvedScope -eq 'docs-only') {
        Ensure-Dir $stampsDir
        $stamp = [ordered]@{
            repo = $repoName; branch = $git.Branch; head = $git.Head; scope = 'docs-only'
            skip_e2e = $true; ok = $true; dirty = $git.Dirty; ts = [DateTimeOffset]::UtcNow.ToString('o')
        }
        [System.IO.File]::WriteAllText($stampPath, ($stamp | ConvertTo-Json -Depth 5), $utf8NoBom)
        Append-Ledger ([ordered]@{
            ts = $stamp.ts; type = 'gate'; repo = $repoName; branch = $git.Branch; head = $git.Head
            dirty = $git.Dirty; scope = 'docs-only'; ok = $true; cache_hit = $false; duration_ms = 0
        })
        Write-Host "Gate PASSED (scope=docs-only, 0 production code changes)." -ForegroundColor Green
        return
    }

    if ((-not $Force) -and (-not $git.Dirty) -and (Test-Path -LiteralPath $stampPath)) {
        try {
            $existing = Get-Content -LiteralPath $stampPath -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($existing.ok -and ($existing.head -eq $git.Head) -and (-not $existing.dirty) -and
                (Test-ScopeCovered $existing.scope ([bool]$existing.skip_e2e) $resolvedScope ([bool]$SkipE2E))) {
                Append-Ledger ([ordered]@{
                    ts = [DateTimeOffset]::UtcNow.ToString('o'); type = 'gate'; repo = $repoName
                    branch = $git.Branch; head = $git.Head; dirty = $false; scope = $resolvedScope
                    ok = $true; cache_hit = $true; duration_ms = 0
                })
                Write-Host "Gate stamp valid (HEAD=$($git.Head.Substring(0, [Math]::Min(8, $git.Head.Length))), scope=$resolvedScope) - skipped redundant run." -ForegroundColor Green
                return
            }
        } catch { }
    }

    Ensure-Dir $stampsDir
    Ensure-Dir $logsDir
    $logFile = Join-Path $logsDir ("gate-" + [DateTimeOffset]::UtcNow.ToString('yyyyMMdd-HHmmss') + "-$resolvedScope.log")
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $argsList = @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'check.ps1'), '-Scope', $resolvedScope)
    if ($SkipE2E) { $argsList += '-SkipE2E' }

    $outputLines = @(& pwsh @argsList 2>&1 | ForEach-Object { $_.ToString() })
    $exitCode = $LASTEXITCODE
    $sw.Stop()
    [System.IO.File]::WriteAllLines($logFile, $outputLines, $utf8NoBom)

    $ok = ($exitCode -eq 0)
    $errSig = if ($ok) { $null } else { Get-ErrorSignature $outputLines }
    $nowIso = [DateTimeOffset]::UtcNow.ToString('o')

    $stamp = [ordered]@{
        repo = $repoName; branch = $git.Branch; head = $git.Head; scope = $resolvedScope
        skip_e2e = [bool]$SkipE2E; ok = $ok; dirty = $git.Dirty; error_sig = $errSig
        log_path = $logFile; duration_ms = [int]$sw.ElapsedMilliseconds; ts = $nowIso
    }
    [System.IO.File]::WriteAllText($stampPath, ($stamp | ConvertTo-Json -Depth 5), $utf8NoBom)
    Append-Ledger ([ordered]@{
        ts = $nowIso; type = 'gate'; repo = $repoName; branch = $git.Branch; head = $git.Head
        dirty = $git.Dirty; scope = $resolvedScope; skip_e2e = [bool]$SkipE2E; ok = $ok
        cache_hit = $false; error_sig = $errSig; duration_ms = [int]$sw.ElapsedMilliseconds; log_path = $logFile
    })

    if ($ok) {
        Write-Host "Gate PASSED (scope=$resolvedScope, $([math]::Round($sw.Elapsed.TotalSeconds, 1))s, HEAD=$($git.Head.Substring(0, [Math]::Min(8, $git.Head.Length))))." -ForegroundColor Green
    } else {
        Write-Host "Gate FAILED (scope=$resolvedScope, sig=$errSig, full log: $logFile)" -ForegroundColor Red
        Write-Host '--- Last 40 lines ---' -ForegroundColor Yellow
        $outputLines | Select-Object -Last 40 | ForEach-Object { Write-Host $_ }
        exit $exitCode
    }
}

function Invoke-Radar {
    $git = Get-GitInfo
    $baseRef = Resolve-BaseRef $Base
    $changed = Get-ChangedFiles $baseRef
    $delta = Get-BranchDelta $changed.MergeBase

    $sensitive = [ordered]@{
        tests        = @($changed.Files | Where-Object { $_ -match '(^crates/.+/tests/|property_tests\.rs$|^apps/desktop/e2e/)' })
        dependencies = @($changed.Files | Where-Object { $_ -match '(^Cargo\.(toml|lock)$|^apps/desktop/package(-lock)?\.json$)' })
        protocol     = @($changed.Files | Where-Object { $_ -match '^crates/protocol/' })
        workflow_ci  = @($changed.Files | Where-Object { $_ -match '^(\.github/|scripts/|\.agents/)' })
        ui           = @($changed.Files | Where-Object { $_ -match '^apps/desktop/src/' })
    }

    $priorReview = @(Read-LedgerEntries 0 | Where-Object {
        (Get-Prop $_ 'type') -eq 'review' -and
        (Get-Prop $_ 'repo') -eq $repoName -and
        (Get-Prop $_ 'branch') -eq $git.Branch -and
        (Get-Prop $_ 'merge_base') -eq $changed.MergeBase -and
        $null -ne (Get-Prop $_ 'net_lines_at_review')
    }) | Select-Object -Last 1
    $postReviewDelta = if ($priorReview) { $delta.Net - [int](Get-Prop $priorReview 'net_lines_at_review') } else { $null }

    $report = [ordered]@{
        repo                     = $repoName
        branch                   = $git.Branch
        head                     = $git.Head
        merge_base               = $changed.MergeBase
        changed_files_count      = $changed.Files.Count
        prod_ins                 = $delta.Ins
        prod_del                 = $delta.Del
        prod_net                 = $delta.Net
        new_prod_files           = $delta.NewProdFiles
        post_review_net_delta    = $postReviewDelta
        post_review_bloat_alert  = ($null -ne $postReviewDelta -and $postReviewDelta -gt 50)
        large_change_alert       = ($changed.Files.Count -gt 5 -or $delta.Net -gt 200)
        sensitive_hits           = $sensitive
    }

    if ($Json) {
        $report | ConvertTo-Json -Depth 6
        return
    }

    $netText = if ($delta.Net -gt 0) { "+$($delta.Net)" } else { "$($delta.Net)" }
    Write-Host "Radar ($($git.Branch) vs $($changed.MergeBase.Substring(0, [Math]::Min(8, $changed.MergeBase.Length)))): $($changed.Files.Count) file(s), prod +$($delta.Ins)/-$($delta.Del) (net $netText, new prod files: $($delta.NewProdFiles))" -ForegroundColor Cyan
    $hitNames = @($sensitive.Keys | Where-Object { $sensitive[$_].Count -gt 0 })
    if ($hitNames.Count -gt 0) {
        Write-Host "  Sensitive areas touched: $($hitNames -join ', ')" -ForegroundColor Yellow
        foreach ($k in $hitNames) { Write-Host "    - ${k}: $($sensitive[$k] -join ', ')" }
    } else {
        Write-Host '  Sensitive areas touched: none' -ForegroundColor Green
    }
    if ($report.post_review_bloat_alert) {
        Write-Host "  WARNING: Post-review net line growth is +$postReviewDelta (> +50 threshold)." -ForegroundColor Yellow
    }
}

function Invoke-RecordReview {
    $raw = if ($InputPath) {
        Get-Content -LiteralPath $InputPath -Raw -Encoding UTF8
    } elseif ($JsonString) {
        $JsonString
    } else {
        throw 'record-review requires -InputPath or -JsonString.'
    }

    $parsed = $raw | ConvertFrom-Json
    $findings = if ($parsed -is [System.Array]) { @($parsed) }
        elseif ($parsed.PSObject.Properties['findings']) { @($parsed.findings) }
        else { throw 'Review JSON must be an array or an object with a "findings" array.' }

    $validSeverities = @('blocker', 'nit')
    $validVerdicts = @('fixed', 'rebutted', 'escalated', 'ignored')
    foreach ($f in $findings) {
        $fid = [string](Get-Prop $f 'id' '?')
        $sev = [string](Get-Prop $f 'severity' '')
        $ver = [string](Get-Prop $f 'verdict' '')
        $rea = [string](Get-Prop $f 'reason' '')
        if ($sev -notin $validSeverities) { throw "Invalid severity '$sev' on finding '$fid' (expected blocker|nit)." }
        if ($ver -notin $validVerdicts) { throw "Invalid verdict '$ver' on finding '$fid' (expected fixed|rebutted|escalated|ignored)." }
        if ($sev -eq 'blocker' -and $ver -eq 'ignored') { throw "Blocker '$fid' cannot have verdict 'ignored'." }
        if ($ver -eq 'rebutted' -and [string]::IsNullOrWhiteSpace($rea)) {
            throw "Rebutted finding '$fid' MUST include a non-empty plain-language 'reason'."
        }
    }

    $git = Get-GitInfo
    $baseRef = Resolve-BaseRef $Base
    $changed = Get-ChangedFiles $baseRef
    $delta = Get-BranchDelta $changed.MergeBase
    $reviewerName = if ($Reviewer) { $Reviewer } else { [string](Get-Prop $parsed 'reviewer' 'unknown') }

    Append-Ledger ([ordered]@{
        ts                  = [DateTimeOffset]::UtcNow.ToString('o')
        type                = 'review'
        repo                = $repoName
        branch              = $git.Branch
        head                = $git.Head
        merge_base          = $changed.MergeBase
        reviewer            = $reviewerName
        handoff_version     = $HandoffVersion
        round               = $Round
        net_lines_at_review = $delta.Net
        findings            = $findings
    })
    $rebuttedCount = @($findings | Where-Object { (Get-Prop $_ 'verdict') -eq 'rebutted' }).Count
    Write-Host "Recorded review ($($findings.Count) finding(s), $rebuttedCount rebutted) for branch '$($git.Branch)'." -ForegroundColor Green
}

function Group-ReviewPatterns($Findings, [string]$TargetVerdict) {
    return @($Findings | Where-Object { $_.verdict -eq $TargetVerdict -and $_.pattern } | Group-Object pattern | ForEach-Object {
        [ordered]@{ pattern = $_.Name; count = $_.Count; distinct_branches = @($_.Group | Select-Object -ExpandProperty branch -Unique).Count }
    } | Sort-Object { $_.distinct_branches }, { $_.count } -Descending | Select-Object -First 10)
}

function Invoke-Digest {
    $entries = Read-LedgerEntries $Days
    $handoffs = @($entries | Where-Object { (Get-Prop $_ 'type') -eq 'handoff' })
    $gates = @($entries | Where-Object { (Get-Prop $_ 'type') -eq 'gate' })
    $reviews = @($entries | Where-Object { (Get-Prop $_ 'type') -eq 'review' })

    $cleanRuns = @($gates | Where-Object { (-not [bool](Get-Prop $_ 'dirty' $false)) -and (-not [bool](Get-Prop $_ 'cache_hit' $false)) })
    $redundantGroups = @($cleanRuns | Group-Object { "$(Get-Prop $_ 'head')|$(Get-Prop $_ 'scope')" } | Where-Object { $_.Count -gt 1 })
    $redundantExtraRuns = 0; $redundantMs = 0
    foreach ($g in $redundantGroups) {
        $redundantExtraRuns += ($g.Count - 1)
        foreach ($extra in ($g.Group | Select-Object -Skip 1)) { $redundantMs += [int](Get-Prop $extra 'duration_ms' 0) }
    }

    $failedGates = @($gates | Where-Object { (-not [bool](Get-Prop $_ 'ok' $true)) -and (Get-Prop $_ 'error_sig') })
    $topErrors = @($failedGates | Group-Object { Get-Prop $_ 'error_sig' } | ForEach-Object {
        [ordered]@{
            error_sig = $_.Name
            total_hits = $_.Count
            distinct_branches = @($_.Group | ForEach-Object { Get-Prop $_ 'branch' 'unknown' } | Select-Object -Unique).Count
        }
    } | Sort-Object { $_.distinct_branches }, { $_.total_hits } -Descending | Select-Object -First 10)

    $allFindings = [System.Collections.Generic.List[object]]::new()
    foreach ($r in $reviews) {
        foreach ($f in @(Get-Prop $r 'findings' @())) {
            $allFindings.Add([pscustomobject]@{
                branch          = [string](Get-Prop $r 'branch' 'unknown')
                reviewer        = [string](Get-Prop $r 'reviewer' 'unknown')
                handoff_version = [string](Get-Prop $r 'handoff_version' 'unknown')
                severity        = [string](Get-Prop $f 'severity' '')
                pattern         = [string](Get-Prop $f 'pattern' '')
                verdict         = [string](Get-Prop $f 'verdict' '')
            })
        }
    }
    $totalF = $allFindings.Count
    $rebuttedF = @($allFindings | Where-Object { $_.verdict -eq 'rebutted' }).Count
    $fpRate = if ($totalF -gt 0) { [math]::Round($rebuttedF / $totalF, 3) } else { 0.0 }
    $topFixedPatterns = Group-ReviewPatterns $allFindings 'fixed'
    $topRebuttedPatterns = Group-ReviewPatterns $allFindings 'rebutted'

    $byVersion = @($reviews | Group-Object { Get-Prop $_ 'handoff_version' 'unknown' } | ForEach-Object {
        $verFindings = @($_.Group | ForEach-Object { @(Get-Prop $_ 'findings' @()) })
        [ordered]@{
            handoff_version = $_.Name
            review_count    = $_.Count
            blockers        = @($verFindings | Where-Object { (Get-Prop $_ 'severity') -eq 'blocker' }).Count
            fixed           = @($verFindings | Where-Object { (Get-Prop $_ 'verdict') -eq 'fixed' }).Count
            rebutted        = @($verFindings | Where-Object { (Get-Prop $_ 'verdict') -eq 'rebutted' }).Count
        }
    })

    $summary = [ordered]@{
        window_days           = $Days
        ledger_path           = $ledgerPath
        counts                = [ordered]@{ handoffs = $handoffs.Count; gates = $gates.Count; reviews = $reviews.Count }
        redundant_gate_runs   = [ordered]@{ extra_runs = $redundantExtraRuns; wasted_ms = $redundantMs }
        top_gate_errors       = $topErrors
        review_metrics        = [ordered]@{
            total_findings       = $totalF
            rebutted             = $rebuttedF
            false_positive_rate  = $fpRate
            by_handoff_version   = $byVersion
            top_fixed_patterns   = $topFixedPatterns
            top_rebutted_patterns = $topRebuttedPatterns
        }
    }

    if ($Json) {
        $summary | ConvertTo-Json -Depth 8
        return
    }

    Write-Host "Digest (last $Days day(s)): $($handoffs.Count) handoffs, $($gates.Count) gates, $($reviews.Count) reviews" -ForegroundColor Cyan
    Write-Host "  Redundant gate runs (same clean HEAD+scope): $redundantExtraRuns (wasted $([math]::Round($redundantMs / 1000.0, 1))s)"
    Write-Host "  Review findings: $totalF total, $rebuttedF rebutted (FP rate: $([math]::Round($fpRate * 100, 1))%)"
    if ($topFixedPatterns.Count -gt 0) {
        Write-Host '  Top recurring FIXED patterns:' -ForegroundColor Yellow
        foreach ($p in $topFixedPatterns) { Write-Host "    - $($p.pattern): $($p.count)x across $($p.distinct_branches) branch(es)" }
    }
    if ($topRebuttedPatterns.Count -gt 0) {
        Write-Host '  Top recurring REBUTTED patterns:' -ForegroundColor Yellow
        foreach ($p in $topRebuttedPatterns) { Write-Host "    - $($p.pattern): $($p.count)x across $($p.distinct_branches) branch(es)" }
    }
    if ($topErrors.Count -gt 0) {
        Write-Host '  Top recurring gate errors:' -ForegroundColor Yellow
        foreach ($e in $topErrors) { Write-Host "    - $($e.error_sig): $($e.total_hits)x across $($e.distinct_branches) branch(es)" }
    }
}

Push-Location $repoRoot
try {
    switch ($Command) {
        'gate'          { Invoke-Gate }
        'radar'         { Invoke-Radar }
        'record-review' { Invoke-RecordReview }
        'digest'        { Invoke-Digest }
    }
}
finally {
    Pop-Location
}
