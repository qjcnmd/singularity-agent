<#
.SYNOPSIS
    Run local quality checks aligned with .github/workflows/ci.yml.
.PARAMETER Scope
    Check scope: rust, protocol, desktop, size, or all (default: all).
.PARAMETER SkipE2E
    Skip the Electron Playwright smoke test in desktop/all scopes.
#>
[CmdletBinding()]
param(
    [ValidateSet('rust', 'protocol', 'desktop', 'size', 'all')]
    [string]$Scope = 'all',

    [switch]$SkipE2E
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
    if ($p -match '(^|/)(tests|e2e|node_modules|dist|dist-desktop)(/|$)') { return $false }
    ($p -match '^crates/[^/]+/src/.+\.rs$' -and $p -notmatch '(^|/)property_tests\.rs$') -or
        ($p -match '^apps/desktop/desktop/.+\.(ts|tsx|cts)$') -or
        ($p -match '^apps/desktop/src/.+\.(ts|tsx)$' -and $p -ne 'apps/desktop/src/protocol.generated.ts')
}

function Invoke-SizeCheck {
    $maxLines = 600
    $roots = @('crates', 'apps/desktop/desktop', 'apps/desktop/src')
    $files = @(Get-ChildItem -Path $roots -Recurse -File | ForEach-Object {
        $rel = [System.IO.Path]::GetRelativePath($repoRoot, $_.FullName).Replace('\', '/')
        if (Test-ProdSourcePath $rel) {
            [pscustomobject]@{ Path = $rel; Lines = [System.IO.File]::ReadAllLines($_.FullName).Length }
        }
    })
    $totalLines = [int]($files | Measure-Object -Property Lines -Sum).Sum
    $largest = $files | Sort-Object Lines -Descending | Select-Object -First 1
    $largestText = if ($largest) { "$($largest.Path): $($largest.Lines)/$maxLines" } else { 'none' }
    Write-Host "  Production source: $($files.Count) files, $totalLines lines (largest: $largestText)"

    if (Get-Command git -ErrorAction SilentlyContinue) {
        $null = & git rev-parse --verify HEAD 2>$null
        if ($LASTEXITCODE -eq 0) {
            $gitOpts = @('-c', 'core.quotepath=false', '-c', 'core.safecrlf=false')
            $ins = 0; $del = 0; $newFiles = 0
            foreach ($line in @(& git @gitOpts diff --numstat --no-renames HEAD -- @roots)) {
                $parts = $line -split "`t", 3
                if ($parts.Count -eq 3 -and $parts[0] -match '^\d+$' -and (Test-ProdSourcePath $parts[2])) {
                    $ins += [int]$parts[0]; $del += [int]$parts[1]
                }
            }
            foreach ($rel in @(& git @gitOpts diff --name-only --diff-filter=A HEAD -- @roots)) {
                if (Test-ProdSourcePath $rel) { $newFiles++ }
            }
            foreach ($rel in @(& git @gitOpts ls-files --others --exclude-standard -- @roots)) {
                if (Test-ProdSourcePath $rel) {
                    $newFiles++
                    $ins += [System.IO.File]::ReadAllLines((Join-Path $repoRoot $rel)).Length
                }
            }
            $net = $ins - $del
            $netText = if ($net -gt 0) { "+$net" } else { "$net" }
            Write-Host "  Working-tree delta vs HEAD: +$ins / -$del (net $netText), new files: $newFiles"
        }
        $global:LASTEXITCODE = 0
    }

    $overLimit = @($files | Where-Object { $_.Lines -gt $maxLines } | Sort-Object Lines -Descending)
    if ($overLimit.Count -gt 0) {
        foreach ($item in $overLimit) {
            Write-Host "  $($item.Path) ($($item.Lines) > $maxLines)" -ForegroundColor Red
        }
        throw "Production source files exceed $maxLines-line limit ($($overLimit.Count) file(s))."
    }
}

Push-Location $repoRoot
try {
    if ($Scope -in @('rust', 'desktop', 'size', 'all')) {
        Invoke-Step 'Check production source size (<= 600 lines)' {
            Invoke-SizeCheck
        }
    }

    if ($Scope -in @('protocol', 'all')) {
        Invoke-Step 'Export TypeScript protocol bindings' {
            cargo run -p protocol --features typescript --example export_types --locked
        }
        Invoke-Step 'Verify protocol.generated.ts has no drift' {
            git diff --ignore-cr-at-eol --exit-code -- apps/desktop/src/protocol.generated.ts
        }
    }

    if ($Scope -in @('rust', 'all')) {
        Invoke-Step 'Format Rust workspace' {
            cargo fmt --all
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
