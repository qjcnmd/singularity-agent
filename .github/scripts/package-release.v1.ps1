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

function Set-IsolatedWorkspaceMembers {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ManifestPath,

        [Parameter(Mandatory = $true)]
        [string[]]$MemberPaths
    )

    $lines = @(Get-Content -LiteralPath $ManifestPath)
    $membersStart = -1
    $membersEnd = -1
    for ($index = 0; $index -lt $lines.Count; $index++) {
        if ($lines[$index].Trim() -eq "members = [") {
            if ($membersStart -ne -1) {
                throw "workspace manifest has multiple members arrays: $ManifestPath"
            }
            $membersStart = $index
            for ($end = $index + 1; $end -lt $lines.Count; $end++) {
                if ($lines[$end].Trim() -eq "]") {
                    $membersEnd = $end
                    break
                }
            }
            break
        }
    }
    if ($membersStart -eq -1 -or $membersEnd -eq -1) {
        throw "workspace manifest members array was not found: $ManifestPath"
    }

    $replacement = New-Object System.Collections.Generic.List[string]
    $replacement.Add("members = [")
    foreach ($memberPath in $MemberPaths) {
        $replacement.Add(('    "{0}",' -f $memberPath.Replace("\", "/")))
    }
    $replacement.Add("]")

    $rewritten = New-Object System.Collections.Generic.List[string]
    for ($index = 0; $index -lt $membersStart; $index++) {
        $rewritten.Add($lines[$index])
    }
    foreach ($line in $replacement) {
        $rewritten.Add($line)
    }
    for ($index = $membersEnd + 1; $index -lt $lines.Count; $index++) {
        $rewritten.Add($lines[$index])
    }
    Set-Content -LiteralPath $ManifestPath -Value $rewritten -Encoding utf8
}

function Rewrite-IsolatedManifestPathDependencies {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ManifestPath,

        [Parameter(Mandatory = $true)]
        [string]$OriginalPackageDirectory,

        [Parameter(Mandatory = $true)]
        [string]$OriginalCratesRoot,

        [Parameter(Mandatory = $true)]
        [string]$DependencyRoot
    )

    $lines = @(Get-Content -LiteralPath $ManifestPath)
    $rewritten = foreach ($line in $lines) {
        if ($line -match 'path\s*=\s*"(?<relative>\.\./[^"]+)"') {
            $relative = $Matches.relative
            $originalDependency = [IO.Path]::GetFullPath((Join-Path $OriginalPackageDirectory $relative))
            $cratesPrefix = "$([IO.Path]::GetFullPath($OriginalCratesRoot))$([IO.Path]::DirectorySeparatorChar)"
            if (-not $originalDependency.StartsWith($cratesPrefix, [StringComparison]::OrdinalIgnoreCase)) {
                throw "path dependency escapes the workspace crates directory: $ManifestPath"
            }
            $dependencyRelative = [IO.Path]::GetRelativePath(
                [IO.Path]::GetFullPath($OriginalCratesRoot),
                $originalDependency
            )
            $dependency = Join-Path $DependencyRoot $dependencyRelative
            if (-not (Test-Path -LiteralPath (Join-Path $dependency "Cargo.toml") -PathType Leaf)) {
                throw "path dependency manifest is missing: $dependency"
            }
            $newRelative = [IO.Path]::GetRelativePath(
                (Split-Path -Parent $ManifestPath),
                $dependency
            ).Replace("\", "/")
            $line.Replace($relative, $newRelative)
        } else {
            $line
        }
    }
    Set-Content -LiteralPath $ManifestPath -Value $rewritten -Encoding utf8
}

function Remove-TaskStagingDirectory {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        return
    }
    $resolved = (Resolve-Path -LiteralPath $Path).Path
    $tempRoot = (Resolve-Path -LiteralPath ([IO.Path]::GetTempPath())).Path
    $leaf = Split-Path -Leaf $resolved
    if (-not $resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        $leaf -notmatch '^singularity-release-sbom-[0-9]+-[0-9a-f]{32}$') {
        throw "refusing to remove unexpected SBOM staging path: $resolved"
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction Stop
}

$WorkspaceRoot = (Resolve-Path -LiteralPath $WorkspaceRoot).Path
$releaseRoot = Get-CargoReleaseRoot -Root $WorkspaceRoot
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
    "singularity" = Join-Path $OutputDirectory "sbom-singularity.cdx.json"
}

if ($DryRun) {
    Write-Output "dry-run: package=$name"
    Write-Output "dry-run: signing=$SigningStatus"
    Write-Output "dry-run: sbom-tool=$SbomToolPath"
    Write-Output "dry-run: release-root=$releaseRoot"
    return
}

if (-not (Test-Path -LiteralPath $SbomToolPath -PathType Leaf)) {
    throw "cargo-cyclonedx executable was not found: $SbomToolPath"
}

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
New-Item -ItemType Directory -Force -Path $directory | Out-Null

$binaryNames = @(
    "singularity"
)
$binaryFiles = @(
    $binaryNames | ForEach-Object {
        Join-Path $releaseRoot ("{0}.exe" -f $_)
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
    (Join-Path $WorkspaceRoot "docs/INSTALL.md")
)
Copy-Item -LiteralPath $metadataFiles[0..1] -Destination $directory
Copy-Item -LiteralPath $metadataFiles[2] -Destination (Join-Path $directory "INSTALL.md")

Compress-Archive -LiteralPath $directory -DestinationPath $archive
$checksum = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
"$checksum  $name.zip" | Set-Content -LiteralPath $checksumPath -Encoding ascii

$sbomRequests = @(
    [ordered]@{
        Manifest = Join-Path $WorkspaceRoot "crates/cli/Cargo.toml"
        Directory = Join-Path $WorkspaceRoot "crates/cli"
        ExpectedNames = @("singularity")
    }
)
$generatedBomFiles = @()
$validatedBoms = @()
$stagingParent = Join-Path ([IO.Path]::GetTempPath()) ("singularity-release-sbom-{0}-{1}" -f $PID, [Guid]::NewGuid().ToString("N"))

try {
    foreach ($request in $sbomRequests) {
        $existing = @(Get-ChildItem -LiteralPath $request.Directory -Filter "*.cdx.json" -File)
        if ($existing.Count -ne 0) {
            throw "unexpected pre-existing CycloneDX files in $($request.Directory)"
        }
    }

    # cargo-cyclonedx 0.5.9 emits binaries for every workspace member, so each
    # release crate is described from a temporary single-member workspace.
    $stagedWorkspaceRoot = Join-Path $stagingParent "workspaces"
    New-Item -ItemType Directory -Force -Path $stagedWorkspaceRoot | Out-Null

    $stagedSbomRequests = @()
    foreach ($request in $sbomRequests) {
        $packageDirectoryName = Split-Path -Leaf $request.Directory
        $workspaceDirectory = Join-Path $stagedWorkspaceRoot $packageDirectoryName
        $stagedPackageDirectory = Join-Path $workspaceDirectory "packages/$packageDirectoryName"
        New-Item -ItemType Directory -Force -Path $workspaceDirectory, (Join-Path $workspaceDirectory "packages") | Out-Null
        Copy-Item -LiteralPath @(
            (Join-Path $WorkspaceRoot "Cargo.toml")
            (Join-Path $WorkspaceRoot "Cargo.lock")
        ) -Destination $workspaceDirectory
        Copy-Item -LiteralPath $request.Directory -Destination $stagedPackageDirectory -Recurse

        $stagedWorkspaceManifest = Join-Path $workspaceDirectory "Cargo.toml"
        Set-IsolatedWorkspaceMembers -ManifestPath $stagedWorkspaceManifest -MemberPaths @(
            "packages/$packageDirectoryName"
        )
        Rewrite-IsolatedManifestPathDependencies `
            -ManifestPath (Join-Path $stagedPackageDirectory "Cargo.toml") `
            -OriginalPackageDirectory $request.Directory `
            -OriginalCratesRoot (Join-Path $WorkspaceRoot "crates") `
            -DependencyRoot (Join-Path $WorkspaceRoot "crates")

        $stagedSbomRequests += [ordered]@{
            Manifest = $stagedWorkspaceManifest
            Directory = $stagedPackageDirectory
            ExpectedNames = $request.ExpectedNames
        }
    }

    foreach ($request in $stagedSbomRequests) {
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
        throw "expected $($binaryNames.Count) generated binary BOMs, found $($generatedBomFiles.Count)"
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
        $request = @($stagedSbomRequests | Where-Object { $_.Directory -eq $bomFile.DirectoryName })
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
        throw "expected $($binaryNames.Count) validated binary BOMs, found $($validatedBoms.Count)"
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

    $binaryBomPath = $stableSbomPaths["singularity"]
    $binaryBom = Get-Content -Raw -LiteralPath $binaryBomPath | ConvertFrom-Json
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
    $binaryBom | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $binaryBomPath -Encoding utf8

    $mergedBom = Get-Content -Raw -LiteralPath $binaryBomPath | ConvertFrom-Json
    if (@($mergedBom.components | Where-Object { [string]$_.'bom-ref' -eq $webRef }).Count -ne 1 -or
        @($mergedBom.dependencies | Where-Object {
            [string]$_.ref -eq $binaryRef -and @($_.dependsOn) -contains $webRef
        }).Count -ne 1) {
        throw "embedded WebUI dependencies were not linked into the binary SBOM."
    }

    $generatedBomFiles | Remove-Item -Force -ErrorAction Stop
    $generatedBomFiles = @()
} finally {
    $cleanupFailure = $null
    if (@($generatedBomFiles).Count -gt 0) {
        try {
            $generatedBomFiles | Remove-Item -Force -ErrorAction Stop
            $generatedBomFiles = @()
        } catch {
            $cleanupFailure = "SBOM temporary-file cleanup failed."
        }
    }
    try {
        Remove-TaskStagingDirectory -Path $stagingParent
    } catch {
        if ($null -eq $cleanupFailure) {
            $cleanupFailure = "SBOM temporary workspace cleanup failed."
        } else {
            $cleanupFailure = "$cleanupFailure SBOM temporary workspace cleanup failed."
        }
    }
    if ($null -ne $cleanupFailure) {
        throw $cleanupFailure
    }
}

Set-WorkflowOutput -Name "name" -Value $name
Set-WorkflowOutput -Name "archive" -Value $archive
Set-WorkflowOutput -Name "checksum" -Value $checksumPath
Set-WorkflowOutput -Name "sbom_singularity" -Value $stableSbomPaths["singularity"]
