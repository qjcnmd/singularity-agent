<#
.SYNOPSIS
    Run local quality checks aligned with .github/workflows/ci.yml.
.PARAMETER Scope
    Check scope: rust, protocol, desktop, or all (default: all).
.PARAMETER SkipE2E
    Skip the Electron Playwright smoke test in desktop/all scopes.
#>
[CmdletBinding()]
param(
    [ValidateSet('rust', 'protocol', 'desktop', 'all')]
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

Push-Location $repoRoot
try {
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
