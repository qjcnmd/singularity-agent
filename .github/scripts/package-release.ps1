[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]*$')]
    [string]$Version,

    [string]$WorkspaceRoot = (Get-Location).Path,
    [string]$OutputDirectory = (Join-Path (Get-Location).Path 'dist')
)

$ErrorActionPreference = 'Stop'
$WorkspaceRoot = (Resolve-Path -LiteralPath $WorkspaceRoot).Path
Push-Location -LiteralPath $WorkspaceRoot
try {
    $metadata = (& cargo metadata --locked --no-deps --format-version 1 | Out-String) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed while resolving the release directory.' }
} finally {
    Pop-Location
}
$binary = Join-Path $metadata.target_directory 'release/singularity.exe'
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Missing release binary: $binary" }

$name = "singularity-$Version-windows-x86_64"
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$directory = Join-Path $OutputDirectory $name
$archive = Join-Path $OutputDirectory "$name.zip"
$checksum = Join-Path $OutputDirectory 'SHA256SUMS.txt'
if ((Test-Path -LiteralPath $directory) -or (Test-Path -LiteralPath $archive)) {
    throw "Package already exists: $name"
}
New-Item -ItemType Directory -Path $directory -Force | Out-Null
Copy-Item -LiteralPath $binary -Destination $directory
Copy-Item -LiteralPath (Join-Path $WorkspaceRoot 'README.md'), (Join-Path $WorkspaceRoot 'LICENSE') -Destination $directory
Copy-Item -LiteralPath (Join-Path $WorkspaceRoot 'docs/INSTALL.md') -Destination (Join-Path $directory 'INSTALL.md')
Compress-Archive -LiteralPath $directory -DestinationPath $archive
$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $name.zip" | Set-Content -LiteralPath $checksum -Encoding ascii

foreach ($line in @("name=$name", "archive=$archive", "checksum=$checksum")) {
    if ($env:GITHUB_OUTPUT) { Add-Content -LiteralPath $env:GITHUB_OUTPUT -Value $line }
    else { Write-Output $line }
}
