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
    if ($LASTEXITCODE -ne 0 -or -not $branch) { throw 'Failed to resolve current git branch.' }
    $head = (& git @gitOpts rev-parse HEAD 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $head) { throw 'Failed to resolve current git HEAD.' }
    $porcelain = @(& git @gitOpts status --porcelain --untracked-files=all 2>$null | Where-Object { $_ })
    if ($LASTEXITCODE -ne 0) { throw 'Failed to read git status.' }
    return [pscustomobject]@{ Branch = $branch; Head = $head; Dirty = ($porcelain.Count -gt 0) }
}

function Resolve-BaseRef([string]$RequestedBase) {
    if ($RequestedBase) {
        $null = & git rev-parse --verify $RequestedBase 2>$null
        if ($LASTEXITCODE -ne 0) { throw "Base ref '$RequestedBase' could not be resolved." }
        return $RequestedBase
    }
    foreach ($candidate in @('origin/main', 'main')) {
        $null = & git rev-parse --verify $candidate 2>$null
        if ($LASTEXITCODE -eq 0) { return $candidate }
    }
    $global:LASTEXITCODE = 0
    throw "Could not resolve default base ref ('origin/main' or 'main'); pass -Base <ref>."
}

function Get-ChangedFiles([string]$BaseRef) {
    $mergeBase = (& git @gitOpts merge-base $BaseRef HEAD 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $mergeBase) {
        throw "Failed to compute git merge-base between '$BaseRef' and HEAD."
    }
    $set = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($diffArgs in @(
        @('diff', '--name-only', '--no-renames', $mergeBase),
        @('diff', '--cached', '--name-only', '--no-renames', $mergeBase)
    )) {
        $lines = @(& git @gitOpts @diffArgs 2>$null)
        if ($LASTEXITCODE -ne 0) { throw "git $($diffArgs -join ' ') failed." }
        foreach ($f in $lines) {
            if ($f) { [void]$set.Add($f.Replace('\', '/')) }
        }
    }
    $untracked = @(& git @gitOpts ls-files --others --exclude-standard 2>$null)
    if ($LASTEXITCODE -ne 0) { throw 'git ls-files --others failed.' }
    foreach ($f in $untracked) {
        if ($f) { [void]$set.Add($f.Replace('\', '/')) }
    }
    return [pscustomobject]@{ MergeBase = $mergeBase; Files = @($set | Sort-Object) }
}

function Assert-NoIndexWorktreeConflict {
    $staged = @(& git @gitOpts diff --cached --name-only --no-renames 2>$null | Where-Object { $_ })
    if ($LASTEXITCODE -ne 0) { throw 'git diff --cached failed.' }
    if ($staged.Count -eq 0) { return }
    $unstaged = @(& git @gitOpts diff --name-only --no-renames 2>$null | Where-Object { $_ })
    if ($LASTEXITCODE -ne 0) { throw 'git diff failed.' }
    $untracked = @(& git @gitOpts ls-files --others --exclude-standard 2>$null | Where-Object { $_ })
    if ($LASTEXITCODE -ne 0) { throw 'git ls-files --others failed.' }
    $stagedSet = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($s in $staged) { [void]$stagedSet.Add($s.Replace('\', '/')) }
    $conflicts = @((@($unstaged) + @($untracked)) | ForEach-Object { $_.Replace('\', '/') } | Where-Object { $stagedSet.Contains($_) } | Select-Object -Unique)
    if ($conflicts.Count -gt 0) {
        throw "Staged index differs from working tree on: $($conflicts -join ', '). Re-stage or unstage before running gate/commit."
    }
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
    }
    for ($i = $Lines.Count - 1; $i -ge 0; $i--) {
        if ($Lines[$i] -match 'Check failed \(\d+\): (.+)$') { return "step:$($Matches[1].Trim())" }
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
    foreach ($rel in @(& git @gitOpts diff --name-only --no-renames --diff-filter=A $MergeBase 2>$null)) {
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

function Parse-TimestampUtc($Value) {
    if ($null -eq $Value) { return $null }
    if ($Value -is [DateTimeOffset]) { return $Value.ToUniversalTime() }
    if ($Value -is [DateTime]) {
        if ($Value.Kind -eq [DateTimeKind]::Utc) { return [DateTimeOffset]::new($Value, [TimeSpan]::Zero) }
        return [DateTimeOffset]::new($Value).ToUniversalTime()
    }
    return [DateTimeOffset]::Parse(
        [string]$Value,
        [System.Globalization.CultureInfo]::InvariantCulture,
        [System.Globalization.DateTimeStyles]::RoundtripKind
    ).ToUniversalTime()
}

function Read-LedgerEntries([int]$WindowDays) {
    if (-not (Test-Path -LiteralPath $ledgerPath)) { return @() }
    $cutoff = [DateTimeOffset]::UtcNow.AddDays(-$WindowDays)
    $items = [System.Collections.Generic.List[object]]::new()
    foreach ($line in [System.IO.File]::ReadLines($ledgerPath)) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        try {
            $obj = $line | ConvertFrom-Json -NoEnumerate
            if ($WindowDays -le 0) { $items.Add($obj); continue }
            $parsedTs = Parse-TimestampUtc (Get-Prop $obj 'ts')
            if ($null -ne $parsedTs -and $parsedTs -ge $cutoff) { $items.Add($obj) }
        } catch { continue }
    }
    return @($items)
}

function Invoke-Gate {
    $git = Get-GitInfo
    Assert-NoIndexWorktreeConflict
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
            dirty = $git.Dirty; scope = 'docs-only'; skip_e2e = $true; ok = $true; cache_hit = $false; duration_ms = 0
        })
        Write-Host "Gate PASSED (scope=docs-only, 0 production code changes)." -ForegroundColor Green
        return
    }

    if ((-not $Force) -and (-not $git.Dirty) -and (Test-Path -LiteralPath $stampPath)) {
        try {
            $existing = Get-Content -LiteralPath $stampPath -Raw -Encoding UTF8 | ConvertFrom-Json
            if ((Get-Prop $existing 'ok' $false) -and ((Get-Prop $existing 'head') -eq $git.Head) -and (-not [bool](Get-Prop $existing 'dirty' $true)) -and
                (Test-ScopeCovered ([string](Get-Prop $existing 'scope' '')) ([bool](Get-Prop $existing 'skip_e2e' $false)) $resolvedScope ([bool]$SkipE2E))) {
                Append-Ledger ([ordered]@{
                    ts = [DateTimeOffset]::UtcNow.ToString('o'); type = 'gate'; repo = $repoName
                    branch = $git.Branch; head = $git.Head; dirty = $false; scope = $resolvedScope
                    skip_e2e = [bool]$SkipE2E; ok = $true; cache_hit = $true; duration_ms = 0
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

    $gitAfter = Get-GitInfo
    $effectiveDirty = $git.Dirty -or $gitAfter.Dirty -or ($git.Head -ne $gitAfter.Head)
    $ok = ($exitCode -eq 0)
    $indexConflictErr = $null
    if ($ok) {
        try { Assert-NoIndexWorktreeConflict }
        catch { $ok = $false; $indexConflictErr = $_.Exception.Message; $exitCode = 1 }
    }
    $errSig = if ($ok) { $null } elseif ($indexConflictErr) { 'git:index_worktree_mismatch' } else { Get-ErrorSignature $outputLines }
    $nowIso = [DateTimeOffset]::UtcNow.ToString('o')

    $stamp = [ordered]@{
        repo = $repoName; branch = $gitAfter.Branch; head = $gitAfter.Head; scope = $resolvedScope
        skip_e2e = [bool]$SkipE2E; ok = $ok; dirty = $effectiveDirty; error_sig = $errSig
        log_path = $logFile; duration_ms = [int]$sw.ElapsedMilliseconds; ts = $nowIso
    }
    [System.IO.File]::WriteAllText($stampPath, ($stamp | ConvertTo-Json -Depth 5), $utf8NoBom)
    Append-Ledger ([ordered]@{
        ts = $nowIso; type = 'gate'; repo = $repoName; branch = $gitAfter.Branch; head = $gitAfter.Head
        dirty = $effectiveDirty; scope = $resolvedScope; skip_e2e = [bool]$SkipE2E; ok = $ok
        cache_hit = $false; error_sig = $errSig; duration_ms = [int]$sw.ElapsedMilliseconds; log_path = $logFile
    })

    if ($ok) {
        $dirtyNote = if ($effectiveDirty -and (-not $git.Dirty)) { ' (working tree modified by formatter; commit before caching)' } else { '' }
        Write-Host "Gate PASSED (scope=$resolvedScope, $([math]::Round($sw.Elapsed.TotalSeconds, 1))s, HEAD=$($gitAfter.Head.Substring(0, [Math]::Min(8, $gitAfter.Head.Length))))$dirtyNote." -ForegroundColor Green
    } elseif ($indexConflictErr) {
        Write-Host "Gate FAILED: $indexConflictErr" -ForegroundColor Red
        exit 1
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
        tests        = [object[]]@($changed.Files | Where-Object { $_ -match '(^crates/.+/tests/|property_tests\.rs$|^apps/desktop/e2e/)' })
        dependencies = [object[]]@($changed.Files | Where-Object { $_ -match '(^Cargo\.(toml|lock)$|^apps/desktop/package(-lock)?\.json$)' })
        protocol     = [object[]]@($changed.Files | Where-Object { $_ -match '^crates/protocol/' })
        workflow_ci  = [object[]]@($changed.Files | Where-Object { $_ -match '^(\.github/|scripts/|\.agents/)' })
        ui           = [object[]]@($changed.Files | Where-Object { $_ -match '^apps/desktop/src/' })
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
        repo                    = $repoName
        branch                  = $git.Branch
        head                    = $git.Head
        merge_base              = $changed.MergeBase
        changed_files_count     = $changed.Files.Count
        prod_ins                = $delta.Ins
        prod_del                = $delta.Del
        prod_net                = $delta.Net
        new_prod_files          = $delta.NewProdFiles
        post_review_net_delta   = $postReviewDelta
        post_review_bloat_alert = ($null -ne $postReviewDelta -and $postReviewDelta -gt 50)
        large_change_alert      = ($changed.Files.Count -gt 5 -or $delta.Net -gt 200)
        sensitive_hits          = $sensitive
    }

    if ($Json) {
        $report | ConvertTo-Json -Depth 6
        return
    }

    $netText = if ($delta.Net -gt 0) { "+$($delta.Net)" } else { "$($delta.Net)" }
    Write-Host "Radar ($($git.Branch) vs $($changed.MergeBase.Substring(0, [Math]::Min(8, $changed.MergeBase.Length)))): $($changed.Files.Count) file(s), prod +$($delta.Ins)/-$($delta.Del) (net $netText, new prod files: $($delta.NewProdFiles))" -ForegroundColor Cyan
    $hitNames = @($sensitive.Keys | Where-Object { @($sensitive[$_]).Count -gt 0 })
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

    $parsed = $raw | ConvertFrom-Json -NoEnumerate
    $findingsList = [System.Collections.Generic.List[object]]::new()
    if ($parsed -is [System.Array]) {
        foreach ($item in $parsed) { $findingsList.Add($item) }
    } elseif ($parsed -is [pscustomobject] -and $parsed.PSObject.Properties['findings']) {
        $propVal = $parsed.findings
        if (-not ($propVal -is [System.Array])) {
            throw 'Review JSON property "findings" must be an array.'
        }
        foreach ($item in $propVal) { $findingsList.Add($item) }
    } else {
        throw 'Review JSON must be an array or an object with a "findings" array.'
    }

    $validSeverities = @('blocker', 'nit')
    $validVerdicts = @('fixed', 'rebutted', 'escalated', 'ignored')
    $rebuttedCount = 0
    foreach ($f in $findingsList) {
        if (-not ($f -is [pscustomobject])) { throw 'Each review finding must be a JSON object.' }
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
        if ($ver -eq 'rebutted') { $rebuttedCount++ }
    }

    $git = Get-GitInfo
    $baseRef = Resolve-BaseRef $Base
    $changed = Get-ChangedFiles $baseRef
    $delta = Get-BranchDelta $changed.MergeBase
    $reviewerName = if ($Reviewer) { $Reviewer } elseif ($parsed -is [pscustomobject]) { [string](Get-Prop $parsed 'reviewer' 'unknown') } else { 'unknown' }

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
        findings            = [object[]]$findingsList.ToArray()
    })
    Write-Host "Recorded review ($($findingsList.Count) finding(s), $rebuttedCount rebutted) for branch '$($git.Branch)'." -ForegroundColor Green
}

function Group-ReviewPatterns($Findings, [string]$TargetVerdict) {
    if ($null -eq $Findings -or @($Findings).Count -eq 0) { return @() }
    return @($Findings | Where-Object { $_.verdict -eq $TargetVerdict -and $_.pattern } | Group-Object pattern | ForEach-Object {
        [ordered]@{ pattern = $_.Name; count = $_.Count; distinct_branches = @($_.Group | Select-Object -ExpandProperty branch -Unique).Count }
    } | Sort-Object { $_.distinct_branches }, { $_.count } -Descending | Select-Object -First 10)
}

function Invoke-Digest {
    $entries = @(Read-LedgerEntries $Days)
    $handoffs = @($entries | Where-Object { (Get-Prop $_ 'type') -eq 'handoff' })
    $gates = @($entries | Where-Object { (Get-Prop $_ 'type') -eq 'gate' })
    $reviews = @($entries | Where-Object { (Get-Prop $_ 'type') -eq 'review' })

    $cleanPassedByHead = @{}
    $redundantExtraRuns = 0
    $redundantMs = 0
    foreach ($g in @($gates | Where-Object { (-not [bool](Get-Prop $_ 'dirty' $false)) -and (-not [bool](Get-Prop $_ 'cache_hit' $false)) })) {
        $gHead = [string](Get-Prop $g 'head' '')
        $gScope = [string](Get-Prop $g 'scope' '')
        $gSkip = [bool](Get-Prop $g 'skip_e2e' $false)
        $gOk = [bool](Get-Prop $g 'ok' $false)
        if (-not $cleanPassedByHead.ContainsKey($gHead)) {
            $cleanPassedByHead[$gHead] = [System.Collections.Generic.List[object]]::new()
        }
        $alreadyCovered = $false
        foreach ($prev in $cleanPassedByHead[$gHead]) {
            if (Test-ScopeCovered $prev.Scope $prev.SkipE2E $gScope $gSkip) { $alreadyCovered = $true; break }
        }
        if ($alreadyCovered) {
            $redundantExtraRuns++
            $redundantMs += [int](Get-Prop $g 'duration_ms' 0)
        } elseif ($gOk) {
            $cleanPassedByHead[$gHead].Add([pscustomobject]@{ Scope = $gScope; SkipE2E = $gSkip })
        }
    }

    $failedGates = @($gates | Where-Object { (-not [bool](Get-Prop $_ 'ok' $true)) -and (Get-Prop $_ 'error_sig') })
    $topErrors = [object[]]@($failedGates | Group-Object { Get-Prop $_ 'error_sig' } | ForEach-Object {
        [ordered]@{
            error_sig = $_.Name
            total_hits = $_.Count
            distinct_branches = @($_.Group | ForEach-Object { Get-Prop $_ 'branch' 'unknown' } | Select-Object -Unique).Count
        }
    } | Sort-Object { $_.distinct_branches }, { $_.total_hits } -Descending | Select-Object -First 10)

    $allFindings = [System.Collections.Generic.List[object]]::new()
    foreach ($r in $reviews) {
        foreach ($f in @(Get-Prop $r 'findings' @())) {
            if ($null -eq $f) { continue }
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
    $topFixedPatterns = [object[]]@(Group-ReviewPatterns $allFindings 'fixed')
    $topRebuttedPatterns = [object[]]@(Group-ReviewPatterns $allFindings 'rebutted')

    $byVersion = [object[]]@($reviews | Group-Object { Get-Prop $_ 'handoff_version' 'unknown' } | ForEach-Object {
        $verFindings = @($_.Group | ForEach-Object { @(Get-Prop $_ 'findings' @()) } | Where-Object { $null -ne $_ })
        [ordered]@{
            handoff_version = $_.Name
            review_count    = $_.Count
            blockers        = @($verFindings | Where-Object { (Get-Prop $_ 'severity') -eq 'blocker' }).Count
            fixed           = @($verFindings | Where-Object { (Get-Prop $_ 'verdict') -eq 'fixed' }).Count
            rebutted        = @($verFindings | Where-Object { (Get-Prop $_ 'verdict') -eq 'rebutted' }).Count
        }
    })

    $summary = [ordered]@{
        window_days         = $Days
        ledger_path         = $ledgerPath
        counts              = [ordered]@{ handoffs = $handoffs.Count; gates = $gates.Count; reviews = $reviews.Count }
        redundant_gate_runs = [ordered]@{ extra_runs = $redundantExtraRuns; wasted_ms = $redundantMs }
        top_gate_errors     = $topErrors
        review_metrics      = [ordered]@{
            total_findings        = $totalF
            rebutted              = $rebuttedF
            false_positive_rate   = $fpRate
            by_handoff_version    = $byVersion
            top_fixed_patterns    = $topFixedPatterns
            top_rebutted_patterns = $topRebuttedPatterns
        }
    }

    if ($Json) {
        $summary | ConvertTo-Json -Depth 8
        return
    }

    Write-Host "Digest (last $Days day(s)): $($handoffs.Count) handoffs, $($gates.Count) gates, $($reviews.Count) reviews" -ForegroundColor Cyan
    Write-Host "  Redundant gate runs (covered by prior passed clean HEAD): $redundantExtraRuns (wasted $([math]::Round($redundantMs / 1000.0, 1))s)"
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
