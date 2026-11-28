<#
.SYNOPSIS
    Run local quality checks aligned with .github/workflows/ci.yml.
.PARAMETER Scope
    Check scope: rust, protocol, desktop, size, arch, or all (default: all).
.PARAMETER SkipE2E
    Skip the Electron Playwright smoke test in desktop/all scopes.
.PARAMETER Check
    Run in read-only mode without modifying working-tree files (cargo fmt --check, export_types --check).
#>
[CmdletBinding()]
param(
    [ValidateSet('rust', 'protocol', 'desktop', 'size', 'arch', 'all')]
    [string]$Scope = 'all',

    [switch]$SkipE2E,

    [switch]$Check
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot

function Invoke-Step {
    param(
        [string]$Label,
        [scriptblock]$Command
    )
    Write-Host "==> $Label" -ForegroundColor Cyan
    $global:LASTEXITCODE = 0
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "Check failed ($LASTEXITCODE): $Label"
    }
}

function Test-ProdSourcePath([string]$RelPath) {
    if (-not $RelPath) { return $false }
    $p = $RelPath.Replace('\', '/')
    if ($p -match '(^|/)(tests|e2e|node_modules|dist)(/|$)') { return $false }
    ($p -match '^crates/[^/]+/src/.+\.rs$' -and $p -notmatch '(^|/)property_tests\.rs$') -or
        ($p -match '^apps/desktop/desktop/.+\.(ts|tsx|cts)$') -or
        ($p -match '^apps/desktop/src/.+\.(ts|tsx)$' -and $p -ne 'apps/desktop/src/protocol.generated.ts') -or
        ($p -match '^apps/desktop/src/styles/.+\.css$') -or
        ($p -match '^scripts/.+\.ps1$')
}

function Test-TestSourcePath([string]$RelPath) {
    if (-not $RelPath) { return $false }
    $p = $RelPath.Replace('\', '/')
    if ($p -match '(^|/)(node_modules|dist)(/|$)') { return $false }
    ($p -match '^crates/[^/]+/src/(?:.+/)?property_tests\.rs$') -or
        ($p -match '^crates/[^/]+/tests/.+\.rs$') -or
        ($p -match '^apps/desktop/e2e/.+\.(mjs|js|ts)$')
}

function Test-WorkflowPath([string]$RelPath) {
    if (-not $RelPath) { return $false }
    $p = $RelPath.Replace('\', '/')
    ($p -eq 'AGENTS.md') -or
        ($p -match '^scripts/.+\.ps1$') -or
        ($p -match '^\.agents/.+') -or
        ($p -match '^\.github/workflows/.+')
}

function Test-BusinessPath([string]$RelPath) {
    if (-not $RelPath) { return $false }
    $p = $RelPath.Replace('\', '/')
    $p -match '^(crates|apps/desktop)/'
}

function Get-RepoRelativePath([string]$BaseDir, [string]$FullPath) {
    $prefix = $BaseDir.TrimEnd('\', '/') + [System.IO.Path]::DirectorySeparatorChar
    if ($FullPath.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $FullPath.Substring($prefix.Length).Replace('\', '/')
    }
    return $FullPath.Replace('\', '/')
}

function Invoke-SizeCheck {
    $warnLines = 600
    $maxLines = 800
    $testFileWarnLines = 800
    $testDeltaWarnLines = 150
    $batchFileWarnCount = 20
    $batchNetWarnLines = 500

    $roots = @('crates', 'apps/desktop/desktop', 'apps/desktop/src', 'scripts')
    $files = @(Get-ChildItem -Path $roots -Recurse -File | ForEach-Object {
        $rel = Get-RepoRelativePath $repoRoot $_.FullName
        if (Test-ProdSourcePath $rel) {
            [pscustomobject]@{ Path = $rel; Lines = [System.IO.File]::ReadAllLines($_.FullName).Length }
        }
    })
    $totalLines = [int]($files | Measure-Object -Property Lines -Sum).Sum
    $largest = $files | Sort-Object Lines -Descending | Select-Object -First 1
    $largestText = if ($largest) { "$($largest.Path): $($largest.Lines)/$maxLines" } else { 'none' }
    Write-Host "  Production source: $($files.Count) files, $totalLines lines (largest: $largestText)"

    $testRoots = @(@('crates', 'apps/desktop/e2e') | Where-Object { Test-Path -LiteralPath $_ })
    $testFiles = @(if ($testRoots.Count -gt 0) {
        Get-ChildItem -Path $testRoots -Recurse -File | ForEach-Object {
            $rel = Get-RepoRelativePath $repoRoot $_.FullName
            if (Test-TestSourcePath $rel) {
                [pscustomobject]@{ Path = $rel; Lines = [System.IO.File]::ReadAllLines($_.FullName).Length }
            }
        }
    })
    $totalTestLines = if ($testFiles.Count -gt 0) { [int]($testFiles | Measure-Object -Property Lines -Sum).Sum } else { 0 }
    $largestTest = $testFiles | Sort-Object Lines -Descending | Select-Object -First 1
    $largestTestText = if ($largestTest) { "$($largestTest.Path): $($largestTest.Lines)/$testFileWarnLines" } else { 'none' }
    Write-Host "  Test source: $($testFiles.Count) files, $totalTestLines lines (largest: $largestTestText)"

    $net = 0; $testNet = 0; $changedProdFiles = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    $hasBusinessChange = $false; $hasWorkflowChange = $false

    if (Get-Command git -ErrorAction SilentlyContinue) {
        $null = & git rev-parse --verify HEAD 2>$null
        if ($LASTEXITCODE -eq 0) {
            $gitOpts = @('-c', 'core.quotepath=false', '-c', 'core.safecrlf=false')
            $ins = 0; $del = 0; $newFiles = 0
            $testIns = 0; $testDel = 0; $newTestFiles = 0
            foreach ($line in @(& git @gitOpts diff --numstat --no-renames HEAD)) {
                $parts = $line -split "`t", 3
                if ($parts.Count -ne 3) { continue }
                $rel = $parts[2]
                if (Test-BusinessPath $rel) { $hasBusinessChange = $true }
                if (Test-WorkflowPath $rel) { $hasWorkflowChange = $true }
                if ($parts[0] -match '^\d+$') {
                    if (Test-ProdSourcePath $rel) {
                        $ins += [int]$parts[0]; $del += [int]$parts[1]
                        $null = $changedProdFiles.Add($rel)
                    } elseif (Test-TestSourcePath $rel) {
                        $testIns += [int]$parts[0]; $testDel += [int]$parts[1]
                    }
                }
            }
            foreach ($rel in @(& git @gitOpts diff --name-only --diff-filter=A HEAD)) {
                if (Test-ProdSourcePath $rel) { $newFiles++; $null = $changedProdFiles.Add($rel) }
                elseif (Test-TestSourcePath $rel) { $newTestFiles++ }
            }
            foreach ($rel in @(& git @gitOpts ls-files --others --exclude-standard)) {
                if (Test-BusinessPath $rel) { $hasBusinessChange = $true }
                if (Test-WorkflowPath $rel) { $hasWorkflowChange = $true }
                if (Test-ProdSourcePath $rel) {
                    $newFiles++
                    $null = $changedProdFiles.Add($rel)
                    $ins += [System.IO.File]::ReadAllLines((Join-Path $repoRoot $rel)).Length
                } elseif (Test-TestSourcePath $rel) {
                    $newTestFiles++
                    $testIns += [System.IO.File]::ReadAllLines((Join-Path $repoRoot $rel)).Length
                }
            }
            $net = $ins - $del
            $netText = if ($net -gt 0) { "+$net" } else { "$net" }
            Write-Host "  Working-tree delta vs HEAD: +$ins / -$del (net $netText), touched prod files: $($changedProdFiles.Count), new files: $newFiles"

            $testNet = $testIns - $testDel
            $testNetText = if ($testNet -gt 0) { "+$testNet" } else { "$testNet" }
            Write-Host "  Test delta vs HEAD: +$testIns / -$testDel (net $testNetText), new test files: $newTestFiles"
        }
        $global:LASTEXITCODE = 0
    }

    $overWarn = @($files | Where-Object { $_.Lines -gt $warnLines -and $_.Lines -le $maxLines } | Sort-Object Lines -Descending)
    if ($overWarn.Count -gt 0) {
        Write-Host "  Warning: $($overWarn.Count) production source file(s) exceed $warnLines-line soft target (hard limit: $maxLines):" -ForegroundColor Yellow
        foreach ($item in $overWarn) {
            Write-Host "    $($item.Path) ($($item.Lines) > $warnLines)" -ForegroundColor Yellow
        }
    }

    $overTestWarn = @($testFiles | Where-Object { $_.Lines -gt $testFileWarnLines } | Sort-Object Lines -Descending)
    if ($overTestWarn.Count -gt 0) {
        Write-Host "  Warning: $($overTestWarn.Count) test file(s) exceed $testFileWarnLines-line review threshold (check docs/development.md test retention standard):" -ForegroundColor Yellow
        foreach ($item in $overTestWarn) {
            Write-Host "    $($item.Path) ($($item.Lines) > $testFileWarnLines)" -ForegroundColor Yellow
        }
    }

    if ($testNet -gt $testDeltaWarnLines) {
        Write-Host "  Warning: Test net delta (+$testNet > +$testDeltaWarnLines lines); verify user confirmation and docs/development.md test retention standard." -ForegroundColor Yellow
    }

    if ($changedProdFiles.Count -gt $batchFileWarnCount -or $net -gt $batchNetWarnLines) {
        Write-Host "  Warning: Large working-tree batch ($($changedProdFiles.Count) prod files, net $netText lines; threshold: >$batchFileWarnCount files or >+$batchNetWarnLines lines). Consider splitting by topic." -ForegroundColor Yellow
    }

    if ($hasBusinessChange -and $hasWorkflowChange) {
        Write-Host "  Warning: Mixed business code and workflow/rule changes (AGENTS.md / scripts / .agents / .github/workflows); keep single-topic commits separate." -ForegroundColor Yellow
    }

    $overLimit = @($files | Where-Object { $_.Lines -gt $maxLines } | Sort-Object Lines -Descending)
    if ($overLimit.Count -gt 0) {
        foreach ($item in $overLimit) {
            Write-Host "  $($item.Path) ($($item.Lines) > $maxLines)" -ForegroundColor Red
        }
        throw "Production source files exceed $maxLines-line hard limit ($($overLimit.Count) file(s))."
    }
}

function Invoke-WorkspaceDagCheck {
    $allowedEdges = @{
        app_core = @()
        protocol = @()
        model    = @('app_core', 'protocol')
        mcp      = @('app_core', 'protocol')
        agent    = @('app_core', 'protocol', 'model', 'mcp')
        app      = @('app_core', 'protocol', 'model', 'mcp', 'agent')
    }
    $raw = (& cargo metadata --locked --no-deps --format-version 1) -join "`n"
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed ($LASTEXITCODE)"
    }
    $meta = $raw | ConvertFrom-Json
    $workspaceNames = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::Ordinal)
    foreach ($pkg in @($meta.packages)) { $null = $workspaceNames.Add([string]$pkg.name) }

    $violations = [System.Collections.Generic.List[string]]::new()
    foreach ($pkg in @($meta.packages)) {
        $name = [string]$pkg.name
        if (-not $allowedEdges.ContainsKey($name)) {
            $violations.Add("unexpected workspace crate '$name'")
            continue
        }
        $allowed = @($allowedEdges[$name])
        $seen = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::Ordinal)
        foreach ($dep in @($pkg.dependencies)) {
            if ($null -ne $dep.kind -and $dep.kind -ne 'normal') { continue }
            $depName = [string]$dep.name
            if ($workspaceNames.Contains($depName) -and $seen.Add($depName) -and ($depName -cnotin $allowed)) {
                $allowedText = if ($allowed.Count -gt 0) { $allowed -join ', ' } else { 'none' }
                $violations.Add("forbidden workspace dependency: $name -> $depName (allowed: $allowedText)")
            }
        }
    }
    foreach ($expected in $allowedEdges.Keys) {
        if (-not $workspaceNames.Contains($expected)) {
            $violations.Add("missing expected workspace crate '$expected'")
        }
    }
    if ($violations.Count -gt 0) {
        foreach ($v in $violations) { Write-Host "  $v" -ForegroundColor Red }
        throw "Workspace crate-DAG check failed ($($violations.Count) violation(s))."
    }
    Write-Host "  Workspace crate DAG verified ($($workspaceNames.Count) crates)"
}

function Invoke-IdentityParityCheck {
    $rsText = [System.IO.File]::ReadAllText((Join-Path $repoRoot 'crates/core/src/identity.rs')).Replace("`r`n", "`n")
    $tsText = [System.IO.File]::ReadAllText((Join-Path $repoRoot 'apps/desktop/src/identity.ts')).Replace("`r`n", "`n")
    $pairs = @(
        @{ Rust = 'APP_NAME';      Ts = 'APP_NAME' }
        @{ Rust = 'APP_SLUG';      Ts = 'APP_SCHEME' }
        @{ Rust = 'HOME_ENV_VAR';  Ts = 'HOME_ENV_VAR' }
        @{ Rust = 'DATA_DIR_NAME'; Ts = 'DATA_DIR_NAME' }
    )
    $issues = [System.Collections.Generic.List[string]]::new()
    foreach ($pair in $pairs) {
        $rsMatch = [regex]::Match($rsText, "(?m)^\s*pub\s+const\s+$([regex]::Escape($pair.Rust))\s*:\s*&str\s*=\s*`"([^`"\r\n]*)`"\s*;")
        $tsMatch = [regex]::Match($tsText, "(?m)^\s*export\s+const\s+$([regex]::Escape($pair.Ts))(?:\s*:\s*\w+)?\s*=\s*(['`"])([^'`"\r\n]*)\1\s*;?\s*$")
        if (-not $rsMatch.Success) { $issues.Add("missing Rust constant $($pair.Rust) in crates/core/src/identity.rs") }
        if (-not $tsMatch.Success) { $issues.Add("missing TypeScript constant $($pair.Ts) in apps/desktop/src/identity.ts") }
        if ($rsMatch.Success -and $tsMatch.Success -and ($rsMatch.Groups[1].Value -cne $tsMatch.Groups[2].Value)) {
            $issues.Add("identity mismatch: Rust $($pair.Rust)='$($rsMatch.Groups[1].Value)' != TS $($pair.Ts)='$($tsMatch.Groups[2].Value)'")
        }
    }
    if ($issues.Count -gt 0) {
        foreach ($issue in $issues) { Write-Host "  $issue" -ForegroundColor Red }
        throw "Cross-language identity parity check failed ($($issues.Count) issue(s))."
    }
    Write-Host "  Cross-language identity constants verified ($($pairs.Count) pairs)"
}

Push-Location $repoRoot
try {
    if ($Scope -in @('rust', 'desktop', 'size', 'all')) {
        Invoke-Step 'Check production source size (warn > 600, hard limit <= 800 lines)' {
            Invoke-SizeCheck
        }
    }

    if ($Scope -in @('arch', 'rust', 'all')) {
        Invoke-Step 'Verify Rust workspace crate DAG' {
            Invoke-WorkspaceDagCheck
        }
    }

    if ($Scope -in @('arch', 'protocol', 'desktop', 'all')) {
        Invoke-Step 'Verify Rust/TypeScript identity constant parity' {
            Invoke-IdentityParityCheck
        }
    }

    if ($Scope -in @('protocol', 'all')) {
        if ($Check) {
            Invoke-Step 'Verify TypeScript protocol bindings are up to date' {
                cargo run -p protocol --features typescript --example export_types --locked -- --check
            }
        } else {
            $generatedPath = Join-Path $repoRoot 'apps/desktop/src/protocol.generated.ts'
            $before = if (Test-Path -LiteralPath $generatedPath) {
                [System.IO.File]::ReadAllText($generatedPath).Replace("`r`n", "`n")
            } else {
                $null
            }
            Invoke-Step 'Export TypeScript protocol bindings' {
                cargo run -p protocol --features typescript --example export_types --locked
            }
            Invoke-Step 'Verify protocol.generated.ts has no drift' {
                $after = if (Test-Path -LiteralPath $generatedPath) {
                    [System.IO.File]::ReadAllText($generatedPath).Replace("`r`n", "`n")
                } else {
                    $null
                }
                if ($before -cne $after) {
                    throw 'apps/desktop/src/protocol.generated.ts drifted from crates/protocol and was updated; re-run after reviewing or updating callers.'
                }
            }
        }
    }

    if ($Scope -in @('rust', 'all')) {
        if ($Check) {
            Invoke-Step 'Check Rust workspace formatting' {
                cargo fmt --all -- --check
            }
        } else {
            Invoke-Step 'Format Rust workspace' {
                cargo fmt --all
            }
        }
        Invoke-Step 'Clippy Rust workspace (CI flags)' {
            cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
        }
        Invoke-Step 'Run Rust workspace tests' {
            cargo test --workspace --locked
        }
    }

    if ($Scope -in @('desktop', 'all')) {
        Invoke-Step 'Lint desktop sources' {
            npm --prefix apps/desktop run lint
        }
        Invoke-Step 'Build desktop assets (strict TypeScript + Vite)' {
            npm --prefix apps/desktop run build
        }
        if (-not $SkipE2E) {
            Invoke-Step 'Build Rust AppServer binary' {
                cargo build -p app --locked
            }
            Invoke-Step 'Prepare desktop runtime binary' {
                npm --prefix apps/desktop run prepare:runtime
            }
            Invoke-Step 'Run desktop E2E smoke suite' {
                npm --prefix apps/desktop run test:e2e
            }
        }
    }

    Write-Host "All requested checks passed ($Scope)." -ForegroundColor Green
}
finally {
    Pop-Location
}
