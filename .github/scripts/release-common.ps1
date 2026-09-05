function Set-WorkflowOutput {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,

        [Parameter(Mandatory = $true)]
        [string]$Value
    )

    $line = "$Name=$Value"
    if ([string]::IsNullOrWhiteSpace($OutputFile)) {
        Write-Output $line
    } else {
        Add-Content -LiteralPath $OutputFile -Value $line
    }
}

function Get-CargoReleaseRoot {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Root
    )

    Push-Location -LiteralPath $Root
    try {
        $metadataJson = (& cargo metadata --no-deps --format-version 1 | Out-String)
        if ($LASTEXITCODE -ne 0) {
            throw "cargo metadata failed while resolving the release directory."
        }
        $metadata = $metadataJson | ConvertFrom-Json
        if ([string]::IsNullOrWhiteSpace([string]$metadata.target_directory)) {
            throw "cargo metadata did not return target_directory."
        }
        return [IO.Path]::GetFullPath((Join-Path ([string]$metadata.target_directory) "release"))
    } finally {
        Pop-Location
    }
}
