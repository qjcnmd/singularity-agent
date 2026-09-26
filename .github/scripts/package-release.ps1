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
$desktop = Join-Path $WorkspaceRoot 'apps/desktop/release/win-unpacked'
if (-not (Test-Path -LiteralPath (Join-Path $desktop 'Singularity.exe'))) { throw "Missing desktop package: $desktop" }

$name = "singularity-$Version-windows-x86_64"
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$directory = Join-Path $OutputDirectory $name
$archive = Join-Path $OutputDirectory "$name.zip"
$checksum = Join-Path $OutputDirectory 'SHA256SUMS.txt'
if ((Test-Path -LiteralPath $directory) -or (Test-Path -LiteralPath $archive)) {
    throw "Package already exists: $name"
}
New-Item -ItemType Directory -Path $directory -Force | Out-Null
Get-ChildItem -LiteralPath $desktop | Copy-Item -Destination $directory -Recurse
Copy-Item -LiteralPath (Join-Path $WorkspaceRoot 'README.md'), (Join-Path $WorkspaceRoot 'LICENSE') -Destination $directory
Copy-Item -LiteralPath (Join-Path $WorkspaceRoot 'docs/INSTALL.md') -Destination (Join-Path $directory 'INSTALL.md')
Compress-Archive -LiteralPath $directory -DestinationPath $archive
$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $name.zip" | Set-Content -LiteralPath $checksum -Encoding ascii

foreach ($line in @("name=$name", "archive=$archive", "checksum=$checksum")) {
    if ($env:GITHUB_OUTPUT) { Add-Content -LiteralPath $env:GITHUB_OUTPUT -Value $line }
    else { Write-Output $line }
}
