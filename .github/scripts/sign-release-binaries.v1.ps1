[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [switch]$IsFormalRelease,

    [Parameter(Mandatory = $false)]
    [string]$WorkspaceRoot = (Get-Location).Path,

    [Parameter(Mandatory = $false)]
    [string]$OutputFile = $env:GITHUB_OUTPUT
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

$binaryNames = @(
    "sg"
)
$WorkspaceRoot = (Resolve-Path -LiteralPath $WorkspaceRoot).Path
$binaryPaths = @(
    $binaryNames | ForEach-Object {
        Join-Path $WorkspaceRoot ("target/release/{0}.exe" -f $_)
    }
)

$pfxBase64 = [string]$env:WINDOWS_CODESIGNING_PFX_BASE64
$pfxPassword = [string]$env:WINDOWS_CODESIGNING_PFX_PASSWORD
$timestampUrl = [string]$env:WINDOWS_CODESIGNING_TIMESTAMP_URL
$configuredValues = @($pfxBase64, $pfxPassword, $timestampUrl)
$configuredCount = @(
    $configuredValues | Where-Object {
        -not [string]::IsNullOrWhiteSpace($_)
    }
).Count

if (-not $IsFormalRelease -and $configuredCount -eq 0) {
    Write-Host "::warning::No Windows code-signing configuration was provided; this workflow_dispatch artifact is unsigned and is not a release-signed build."
    Set-WorkflowOutput -Name "status" -Value "unsigned-dev"
    return
}
if ($configuredCount -ne 3) {
    throw "Windows code-signing configuration must provide the PFX, PFX password, and RFC3161 timestamp URL."
}

[Uri]$timestampUri = $null
if (-not [Uri]::TryCreate($timestampUrl.Trim(), [UriKind]::Absolute, [ref]$timestampUri)) {
    throw "Windows code-signing timestamp URL must be an absolute HTTP(S) RFC3161 endpoint."
}
if (@("http", "https") -notcontains $timestampUri.Scheme.ToLowerInvariant()) {
    throw "Windows code-signing timestamp URL must be an absolute HTTP(S) RFC3161 endpoint."
}

foreach ($binaryPath in $binaryPaths) {
    if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
        throw "missing release binary: $binaryPath"
    }
}

$signtoolPath = $null
$programFilesX86 = [Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
$sdkRoot = if ([string]::IsNullOrWhiteSpace($programFilesX86)) {
    $null
} else {
    Join-Path $programFilesX86 "Windows Kits\10\bin"
}
if ($null -ne $sdkRoot -and (Test-Path -LiteralPath $sdkRoot -PathType Container)) {
    $signtoolCandidate = Get-ChildItem -LiteralPath $sdkRoot -Filter "signtool.exe" -File -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' } |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if ($null -ne $signtoolCandidate) {
        $signtoolPath = $signtoolCandidate.FullName
    }
}
if ([string]::IsNullOrWhiteSpace($signtoolPath)) {
    $signtoolCommand = Get-Command signtool.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $signtoolCommand) {
        $signtoolPath = $signtoolCommand.Source
    }
}
if ([string]::IsNullOrWhiteSpace($signtoolPath)) {
    throw "signtool.exe was not found on the Windows runner."
}

$pfxPath = Join-Path $env:RUNNER_TEMP ("singularity-codesign-{0}.pfx" -f [Guid]::NewGuid().ToString("N"))
$toolLogPath = Join-Path $env:RUNNER_TEMP ("singularity-codesign-{0}.log" -f [Guid]::NewGuid().ToString("N"))
$store = $null
$securePassword = $null
$certificates = $null
$importedThumbprints = @()
$cleanupFailed = $false

try {
    try {
        $pfxBytes = [Convert]::FromBase64String($pfxBase64)
    } catch {
        throw "Windows code-signing PFX is not valid base64."
    }
    if ($pfxBytes.Length -eq 0) {
        throw "Windows code-signing PFX is empty."
    }
    [IO.File]::WriteAllBytes($pfxPath, $pfxBytes)

    $certificates = [System.Security.Cryptography.X509Certificates.X509Certificate2Collection]::new()
    $certificates.Import(
        $pfxPath,
        $pfxPassword,
        [System.Security.Cryptography.X509Certificates.X509KeyStorageFlags]::EphemeralKeySet
    )
    $leafCandidates = @($certificates | Where-Object { $_.HasPrivateKey })
    if ($leafCandidates.Count -ne 1) {
        throw "Windows code-signing PFX must contain exactly one private-key leaf certificate."
    }
    $leaf = $leafCandidates[0]
    $now = [DateTime]::UtcNow
    if ($leaf.NotBefore.ToUniversalTime() -gt $now -or $leaf.NotAfter.ToUniversalTime() -le $now) {
        throw "Windows code-signing leaf certificate is not currently valid."
    }
    $hasCodeSigningEku = $false
    foreach ($extension in $leaf.Extensions) {
        if ($extension.Oid.Value -ne "2.5.29.37") {
            continue
        }
        $ekuExtension = [System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]$extension
        if (@($ekuExtension.EnhancedKeyUsages | Where-Object { $_.Value -eq "1.3.6.1.5.5.7.3.3" }).Count -gt 0) {
            $hasCodeSigningEku = $true
        }
    }
    if (-not $hasCodeSigningEku) {
        throw "Windows code-signing leaf certificate lacks the Code Signing EKU."
    }
    $thumbprint = ($leaf.Thumbprint -replace '\s', '').ToUpperInvariant()
    if ($thumbprint -notmatch '^[0-9A-F]{40}$') {
        throw "Windows code-signing leaf certificate has an invalid thumbprint."
    }

    $store = [System.Security.Cryptography.X509Certificates.X509Store]::new(
        [System.Security.Cryptography.X509Certificates.StoreName]::My,
        [System.Security.Cryptography.X509Certificates.StoreLocation]::CurrentUser
    )
    $store.Open([System.Security.Cryptography.X509Certificates.OpenFlags]::ReadOnly)
    $preexistingThumbprints = @(
        $store.Certificates |
            ForEach-Object { $_.Thumbprint } |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
            Sort-Object -Unique
    )
    $preexistingLeaf = @($store.Certificates | Where-Object { $_.Thumbprint -eq $thumbprint })
    $store.Close()

    if ($preexistingLeaf.Count -gt 1) {
        throw "Windows code-signing certificate selection is ambiguous."
    }
    if ($preexistingLeaf.Count -eq 1) {
        if (-not $preexistingLeaf[0].HasPrivateKey) {
            throw "The existing Windows code-signing certificate has no private key."
        }
    } else {
        $importedThumbprints = @(
            $certificates |
                ForEach-Object { $_.Thumbprint } |
                Where-Object {
                    -not [string]::IsNullOrWhiteSpace($_) -and $preexistingThumbprints -notcontains $_
                } |
                Sort-Object -Unique
        )
        $securePassword = ConvertTo-SecureString -String $pfxPassword -AsPlainText -Force
        $importArguments = @{
            FilePath = $pfxPath
            CertStoreLocation = "Cert:\CurrentUser\My"
            Password = $securePassword
        }
        Import-PfxCertificate @importArguments | Out-Null
    }

    $store.Open([System.Security.Cryptography.X509Certificates.OpenFlags]::ReadOnly)
    $storedLeaf = @($store.Certificates | Where-Object { $_.Thumbprint -eq $thumbprint })
    $store.Close()
    if ($storedLeaf.Count -ne 1 -or -not $storedLeaf[0].HasPrivateKey) {
        throw "Windows code-signing leaf certificate was not available with a private key."
    }

    foreach ($binaryPath in $binaryPaths) {
        $signArguments = @(
            "sign"
            "/fd"
            "SHA256"
            "/td"
            "SHA256"
            "/tr"
            $timestampUrl.Trim()
            "/s"
            "My"
            "/sha1"
            $thumbprint
            $binaryPath
        )
        & $signtoolPath @signArguments *> $toolLogPath
        if ($LASTEXITCODE -ne 0) {
            throw "Authenticode signing failed."
        }
    }

    foreach ($binaryPath in $binaryPaths) {
        $verifyArguments = @(
            "verify"
            "/pa"
            "/all"
            "/tw"
            $binaryPath
        )
        & $signtoolPath @verifyArguments *> $toolLogPath
        if ($LASTEXITCODE -ne 0) {
            throw "Authenticode policy verification failed."
        }
    }
    Set-WorkflowOutput -Name "status" -Value "signed"
} finally {
    try {
        if ($null -ne $store) {
            $store.Close()
        }
    } catch {
        $cleanupFailed = $true
    }

    if (@($importedThumbprints).Count -gt 0) {
        $cleanupStore = $null
        try {
            $cleanupStore = [System.Security.Cryptography.X509Certificates.X509Store]::new(
                [System.Security.Cryptography.X509Certificates.StoreName]::My,
                [System.Security.Cryptography.X509Certificates.StoreLocation]::CurrentUser
            )
            $cleanupStore.Open([System.Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite)
            foreach ($importedThumbprint in $importedThumbprints) {
                $certificatesToRemove = @(
                    $cleanupStore.Certificates | Where-Object { $_.Thumbprint -eq $importedThumbprint }
                )
                foreach ($certificateToRemove in $certificatesToRemove) {
                    $cleanupStore.Remove($certificateToRemove)
                }
            }
        } catch {
            $cleanupFailed = $true
        } finally {
            try {
                if ($null -ne $cleanupStore) {
                    $cleanupStore.Close()
                }
            } catch {
                $cleanupFailed = $true
            }
        }
    }

    try {
        if ($null -ne $pfxPath -and (Test-Path -LiteralPath $pfxPath -PathType Leaf)) {
            Remove-Item -LiteralPath $pfxPath -Force
        }
    } catch {
        $cleanupFailed = $true
    }
    try {
        if ($null -ne $toolLogPath -and (Test-Path -LiteralPath $toolLogPath -PathType Leaf)) {
            Remove-Item -LiteralPath $toolLogPath -Force
        }
    } catch {
        $cleanupFailed = $true
    }
    try {
        if ($null -ne $securePassword) {
            $securePassword.Dispose()
        }
    } catch {
        $cleanupFailed = $true
    }
    try {
        if ($null -ne $certificates) {
            foreach ($certificate in $certificates) {
                $certificate.Dispose()
            }
        }
    } catch {
        $cleanupFailed = $true
    }

    $pfxBase64 = $null
    $pfxPassword = $null
    $pfxBytes = $null
    foreach ($variableName in @(
        "WINDOWS_CODESIGNING_PFX_BASE64"
        "WINDOWS_CODESIGNING_PFX_PASSWORD"
        "WINDOWS_CODESIGNING_TIMESTAMP_URL"
    )) {
        Remove-Item -LiteralPath "Env:$variableName" -ErrorAction SilentlyContinue
    }
    if ($cleanupFailed) {
        throw "Windows code-signing cleanup failed."
    }
}
