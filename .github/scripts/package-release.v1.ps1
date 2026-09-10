[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [switch]$IsFormalRelease,

    [Parameter(Mandatory = $true)]
    [string]$RefName,

    [Parameter(Mandatory = $true)]
    [int]$RunNumber,

    [Parameter(Mandatory = $true)]
    [ValidateSet("signed", "unsigned-dev")]
    [string]$SigningStatus,

    [Parameter(Mandatory = $true)]
    [string]$SbomToolPath,

    [Parameter(Mandatory = $false)]
    [string]$WorkspaceRoot = (Get-Location).Path,

    [Parameter(Mandatory = $false)]
    [string]$OutputDirectory = (Join-Path (Get-Location).Path "dist"),

    [Parameter(Mandatory = $false)]
    [string]$OutputFile = $env:GITHUB_OUTPUT,

    [Parameter(Mandatory = $false)]
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

. (Join-Path $PSScriptRoot 'release-common.ps1')

$WorkspaceRoot = (Resolve-Path -LiteralPath $WorkspaceRoot).Path
$binaryPath = Get-CargoReleaseBinary -Root $WorkspaceRoot
if ($IsFormalRelease) {
    if ($RefName -notmatch '^v') {
        throw "Formal releases must use a v-prefixed tag."
    }
    if ($SigningStatus -ne "signed") {
        throw "Formal tagged releases must be signed."
    }
    $version = $RefName
} else {
    if ($RunNumber -le 0) {
        throw "workflow_dispatch run number must be positive."
    }
    $version = "dev-$RunNumber"
}

$name = "singularity-$version-windows-x86_64"
if ($SigningStatus -eq "unsigned-dev") {
    $name = "$name-unsigned"
}
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$directory = Join-Path $OutputDirectory $name
$archive = Join-Path $OutputDirectory "$name.zip"
$checksumPath = Join-Path $OutputDirectory "SHA256SUMS.txt"
$stableSbomPath = Join-Path $OutputDirectory "sbom-singularity.cdx.json"

if ($DryRun) {
    Write-Output "dry-run: package=$name"
    Write-Output "dry-run: signing=$SigningStatus"
    Write-Output "dry-run: sbom-tool=$SbomToolPath"
    Write-Output "dry-run: release-binary=$binaryPath"
    return
}

if (-not (Test-Path -LiteralPath $SbomToolPath -PathType Leaf)) {
    throw "cargo-cyclonedx executable was not found: $SbomToolPath"
}

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
New-Item -ItemType Directory -Force -Path $directory | Out-Null

if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
    throw "missing release binary: $binaryPath"
}
Copy-Item -LiteralPath $binaryPath -Destination $directory

$metadataFiles = @(
    (Join-Path $WorkspaceRoot "README.md")
    (Join-Path $WorkspaceRoot "LICENSE")
    (Join-Path $WorkspaceRoot "docs/INSTALL.md")
)
Copy-Item -LiteralPath $metadataFiles[0..1] -Destination $directory
Copy-Item -LiteralPath $metadataFiles[2] -Destination (Join-Path $directory "INSTALL.md")

Compress-Archive -LiteralPath $directory -DestinationPath $archive
$checksum = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
"$checksum  $name.zip" | Set-Content -LiteralPath $checksumPath -Encoding ascii

$packageDirectory = Join-Path $WorkspaceRoot "crates/cli"
$generatedBomPath = Join-Path $packageDirectory "singularity_bin.cdx.json"
foreach ($path in @($generatedBomPath, $stableSbomPath)) {
    if (Test-Path -LiteralPath $path) {
        throw "SBOM path already exists: $path"
    }
}

try {
    # The workspace has one binary; pinned cargo-cyclonedx emits its BOM beside the manifest.
    $sbomArguments = @(
        "cyclonedx"
        "--manifest-path"
        (Join-Path $WorkspaceRoot "Cargo.toml")
        "--format"
        "json"
        "--describe"
        "binaries"
        "--all-features"
        "--target"
        "x86_64-pc-windows-msvc"
        "--spec-version"
        "1.5"
        "--quiet"
    )
    & $SbomToolPath @sbomArguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo-cyclonedx failed for $WorkspaceRoot"
    }
    try {
        $binaryBom = Get-Content -Raw -LiteralPath $generatedBomPath | ConvertFrom-Json
    } catch {
        throw "invalid CycloneDX JSON: ${generatedBomPath}: $($_.Exception.Message)"
    }
    if ([string]$binaryBom.bomFormat -ne "CycloneDX" -or [string]$binaryBom.specVersion -ne "1.5") {
        throw "unexpected CycloneDX format or spec version: $generatedBomPath"
    }
    if ($null -eq $binaryBom.metadata -or $null -eq $binaryBom.metadata.component -or
        [string]$binaryBom.metadata.component.type -ne "application" -or
        [string]$binaryBom.metadata.component.name -ne "singularity") {
        throw "CycloneDX metadata does not identify the release binary: $generatedBomPath"
    }

    $webPackageDirectory = Join-Path $WorkspaceRoot "crates/cli/web"
    $npmCommand = Get-Command npm -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $npmCommand) {
        throw "npm was not found while generating the embedded WebUI SBOM."
    }
    $npmArguments = @(
        "--prefix"
        $webPackageDirectory
        "sbom"
        "--omit"
        "dev"
        "--package-lock-only"
        "--sbom-format"
        "cyclonedx"
        "--sbom-type"
        "application"
    )
    $npmBomJson = (& $npmCommand.Source @npmArguments | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "npm sbom failed for the embedded WebUI."
    }
    try {
        $npmBom = $npmBomJson | ConvertFrom-Json
    } catch {
        throw "npm returned invalid CycloneDX JSON: $($_.Exception.Message)"
    }
    if ([string]$npmBom.bomFormat -ne "CycloneDX" -or [string]$npmBom.specVersion -ne "1.5" -or
        $null -eq $npmBom.metadata -or $null -eq $npmBom.metadata.component) {
        throw "npm returned an unexpected CycloneDX document."
    }

    $binaryRef = [string]$binaryBom.metadata.component.'bom-ref'
    $webRef = [string]$npmBom.metadata.component.'bom-ref'
    if ([string]::IsNullOrWhiteSpace($binaryRef) -or [string]::IsNullOrWhiteSpace($webRef)) {
        throw "CycloneDX root components must provide bom-ref values."
    }
    $webComponent = $npmBom.metadata.component
    $webComponent | Add-Member -NotePropertyName "properties" -NotePropertyValue @(
        @($webComponent.properties) + [PSCustomObject]@{
            name = "singularity:delivery"
            value = "embedded-webui"
        }
    ) -Force
    $binaryBom.components = @($binaryBom.components) + @($webComponent) + @($npmBom.components)
    $rootDependency = @($binaryBom.dependencies | Where-Object { [string]$_.ref -eq $binaryRef })
    if ($rootDependency.Count -eq 0) {
        $binaryBom.dependencies = @($binaryBom.dependencies) + @(
            [PSCustomObject]@{ ref = $binaryRef; dependsOn = @($webRef) }
        )
    } elseif ($rootDependency.Count -eq 1) {
        $rootDependency[0].dependsOn = @($rootDependency[0].dependsOn) + @($webRef) | Sort-Object -Unique
    } else {
        throw "CycloneDX binary root dependency is ambiguous."
    }
    $binaryBom.dependencies = @($binaryBom.dependencies) + @($npmBom.dependencies)
    $binaryBom | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $stableSbomPath -Encoding utf8

    $mergedBom = Get-Content -Raw -LiteralPath $stableSbomPath | ConvertFrom-Json
    if (@($mergedBom.components | Where-Object { [string]$_.'bom-ref' -eq $webRef }).Count -ne 1 -or
        @($mergedBom.dependencies | Where-Object {
            [string]$_.ref -eq $binaryRef -and @($_.dependsOn) -contains $webRef
        }).Count -ne 1) {
        throw "embedded WebUI dependencies were not linked into the binary SBOM."
    }

} finally {
    if (Test-Path -LiteralPath $generatedBomPath) {
        Remove-Item -LiteralPath $generatedBomPath -Force -ErrorAction Stop
    }
}

Set-WorkflowOutput -Name "name" -Value $name
Set-WorkflowOutput -Name "archive" -Value $archive
Set-WorkflowOutput -Name "checksum" -Value $checksumPath
Set-WorkflowOutput -Name "sbom_singularity" -Value $stableSbomPath
