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

$WorkspaceRoot = (Resolve-Path -LiteralPath $WorkspaceRoot).Path
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
$stableSbomPaths = @{
    "sg" = Join-Path $OutputDirectory "sbom-sg.cdx.json"
    "singularity_app_server" = Join-Path $OutputDirectory "sbom-singularity-app-server.cdx.json"
    "singularity-command-runner" = Join-Path $OutputDirectory "sbom-singularity-command-runner.cdx.json"
    "singularity-windows-sandbox-setup" = Join-Path $OutputDirectory "sbom-singularity-windows-sandbox-setup.cdx.json"
}

if ($DryRun) {
    Write-Output "dry-run: package=$name"
    Write-Output "dry-run: signing=$SigningStatus"
    Write-Output "dry-run: sbom-tool=$SbomToolPath"
    return
}

if (-not (Test-Path -LiteralPath $SbomToolPath -PathType Leaf)) {
    throw "cargo-cyclonedx executable was not found: $SbomToolPath"
}

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
New-Item -ItemType Directory -Force -Path $directory | Out-Null

$binaryNames = @(
    "sg"
    "singularity_app_server"
    "singularity-command-runner"
    "singularity-windows-sandbox-setup"
)
$binaryFiles = @(
    $binaryNames | ForEach-Object {
        Join-Path $WorkspaceRoot ("target/release/{0}.exe" -f $_)
    }
)
foreach ($source in $binaryFiles) {
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "missing release binary: $source"
    }
    Copy-Item -LiteralPath $source -Destination $directory
}

$metadataFiles = @(
    (Join-Path $WorkspaceRoot "README.md")
    (Join-Path $WorkspaceRoot "LICENSE")
    (Join-Path $WorkspaceRoot "THIRD_PARTY_NOTICES.md")
    (Join-Path $WorkspaceRoot "docs/INSTALL.md")
)
Copy-Item -LiteralPath $metadataFiles[0..2] -Destination $directory
Copy-Item -LiteralPath $metadataFiles[3] -Destination (Join-Path $directory "INSTALL.md")

Compress-Archive -LiteralPath $directory -DestinationPath $archive
$checksum = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
"$checksum  $name.zip" | Set-Content -LiteralPath $checksumPath -Encoding ascii

$sbomRequests = @(
    [ordered]@{
        Manifest = Join-Path $WorkspaceRoot "crates/cli/Cargo.toml"
        Directory = Join-Path $WorkspaceRoot "crates/cli"
        ExpectedNames = @("sg")
    }
    [ordered]@{
        Manifest = Join-Path $WorkspaceRoot "crates/app-server/Cargo.toml"
        Directory = Join-Path $WorkspaceRoot "crates/app-server"
        ExpectedNames = @("singularity_app_server")
    }
    [ordered]@{
        Manifest = Join-Path $WorkspaceRoot "crates/windows-sandbox/Cargo.toml"
        Directory = Join-Path $WorkspaceRoot "crates/windows-sandbox"
        ExpectedNames = @("singularity-command-runner", "singularity-windows-sandbox-setup")
    }
)
$generatedBomFiles = @()
$validatedBoms = @()

try {
    foreach ($request in $sbomRequests) {
        $existing = @(Get-ChildItem -LiteralPath $request.Directory -Filter "*.cdx.json" -File)
        if ($existing.Count -ne 0) {
            throw "unexpected pre-existing CycloneDX files in $($request.Directory)"
        }

        $sbomArguments = @(
            "cyclonedx"
            "--manifest-path"
            $request.Manifest
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
            throw "cargo-cyclonedx failed for $($request.Manifest)"
        }
        $outputs = @(Get-ChildItem -LiteralPath $request.Directory -Filter "*.cdx.json" -File)
        $generatedBomFiles += $outputs
        if ($outputs.Count -ne $request.ExpectedNames.Count) {
            throw "expected $($request.ExpectedNames.Count) binary BOMs in $($request.Directory), found $($outputs.Count)"
        }
    }
    if ($generatedBomFiles.Count -ne $binaryNames.Count) {
        throw "expected four generated binary BOMs, found $($generatedBomFiles.Count)"
    }

    foreach ($bomFile in @($generatedBomFiles | Sort-Object FullName)) {
        try {
            $bom = Get-Content -Raw -LiteralPath $bomFile.FullName | ConvertFrom-Json
        } catch {
            throw "invalid CycloneDX JSON: $($bomFile.FullName): $($_.Exception.Message)"
        }
        if ([string]$bom.bomFormat -ne "CycloneDX" -or [string]$bom.specVersion -ne "1.5") {
            throw "unexpected CycloneDX format or spec version: $($bomFile.FullName)"
        }
        if ($null -eq $bom.metadata -or $null -eq $bom.metadata.component) {
            throw "missing CycloneDX metadata component: $($bomFile.FullName)"
        }
        $binaryName = [string]$bom.metadata.component.name
        if ([string]$bom.metadata.component.type -ne "application" -or $stableSbomPaths.Keys -notcontains $binaryName) {
            throw "metadata component does not identify a release binary: $($bomFile.FullName)"
        }
        $request = @($sbomRequests | Where-Object { $_.Directory -eq $bomFile.DirectoryName })
        if ($request.Count -ne 1 -or $request[0].ExpectedNames -notcontains $binaryName) {
            throw "metadata binary does not match its manifest: $($bomFile.FullName)"
        }
        if (@($validatedBoms | Where-Object { $_.BinaryName -eq $binaryName }).Count -ne 0) {
            throw "duplicate binary BOM metadata: $binaryName"
        }
        $validatedBoms += [PSCustomObject]@{
            BinaryName = $binaryName
            File = $bomFile
        }
    }
    if ($validatedBoms.Count -ne $binaryNames.Count) {
        throw "expected four validated binary BOMs, found $($validatedBoms.Count)"
    }

    foreach ($binaryName in $binaryNames) {
        $validated = @($validatedBoms | Where-Object { $_.BinaryName -eq $binaryName })
        if ($validated.Count -ne 1) {
            throw "missing expected binary BOM: $binaryName"
        }
        $stablePath = $stableSbomPaths[$binaryName]
        if (Test-Path -LiteralPath $stablePath) {
            throw "stable SBOM path already exists: $stablePath"
        }
        Copy-Item -LiteralPath $validated[0].File.FullName -Destination $stablePath
    }

    $generatedBomFiles | Remove-Item -Force -ErrorAction Stop
    $generatedBomFiles = @()
} finally {
    if (@($generatedBomFiles).Count -gt 0) {
        try {
            $generatedBomFiles | Remove-Item -Force -ErrorAction Stop
            $generatedBomFiles = @()
        } catch {
            throw "SBOM temporary-file cleanup failed."
        }
    }
}

Set-WorkflowOutput -Name "name" -Value $name
Set-WorkflowOutput -Name "archive" -Value $archive
Set-WorkflowOutput -Name "checksum" -Value $checksumPath
Set-WorkflowOutput -Name "sbom_sg" -Value $stableSbomPaths["sg"]
Set-WorkflowOutput -Name "sbom_app_server" -Value $stableSbomPaths["singularity_app_server"]
Set-WorkflowOutput -Name "sbom_command_runner" -Value $stableSbomPaths["singularity-command-runner"]
Set-WorkflowOutput -Name "sbom_windows_sandbox_setup" -Value $stableSbomPaths["singularity-windows-sandbox-setup"]
