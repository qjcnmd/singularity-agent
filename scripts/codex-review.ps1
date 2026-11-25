<#
.SYNOPSIS
    Run an ephemeral local Codex CLI review with suspended Job-Object launch, file redirection, and stall recovery.
.PARAMETER PromptFile
    Path to the UTF-8 review brief file passed to codex exec via stdin.
.PARAMETER Images
    Optional image paths attached via repeated -i arguments (invoke with `& ./scripts/codex-review.ps1 ... -Images @(...)`).
.PARAMETER Model
    Codex model slug (default: codex/gpt-6.2-sol).
.PARAMETER Effort
    Reasoning effort override (default: high).
.PARAMETER AllowWebSearch
    Allow Codex built-in web search (disabled by default for local repository reviews).
.PARAMETER DeletePrompt
    Delete PromptFile in finally cleanup after the run finishes.
.PARAMETER StallSeconds
    Abort if stderr log shows no progress for this many seconds (default: 480).
#>
[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Mandatory)]
    [string]$PromptFile,

    [string[]]$Images = @(),

    [string]$Model = 'codex/gpt-6.2-sol',

    [string]$Effort = 'high',

    [switch]$AllowWebSearch,

    [switch]$DeletePrompt,

    [ValidateRange(30, 3600)]
    [int]$StallSeconds = 480
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$utf8 = [System.Text.UTF8Encoding]::new($false)
try { [Console]::OutputEncoding = $utf8 } catch {}

if (-not ('SingularityReviewProcess' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public sealed class SingularityReviewProcess : IDisposable {
    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_ATTRIBUTES {
        public int nLength;
        public IntPtr lpSecurityDescriptor;
        public bool bInheritHandle;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO {
        public int cb;
        public string lpReserved, lpDesktop, lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2;
        public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION {
        public IntPtr hProcess, hThread;
        public uint dwProcessId, dwThreadId;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
        public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize, MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass, SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct IO_COUNTERS {
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
        public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
    }

    static readonly IntPtr INVALID_HANDLE = new IntPtr(-1);
    const uint GENERIC_READ = 0x80000000u, GENERIC_WRITE = 0x40000000u;
    const uint SHARE_ALL = 0x7u; // READ | WRITE | DELETE
    const uint OPEN_EXISTING = 3u, CREATE_ALWAYS = 2u;
    const int STARTF_USESTDHANDLES = 0x00000100;
    const uint CREATE_SUSPENDED = 0x00000004u, CREATE_NO_WINDOW = 0x08000000u;
    const uint JOB_KILL_ON_CLOSE = 0x2000u;

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateFileW(string name, uint access, uint share, ref SECURITY_ATTRIBUTES sa, uint disp, uint flags, IntPtr tmpl);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessW(string app, StringBuilder cmdLine, IntPtr procAttr, IntPtr threadAttr, bool inherit, uint flags, IntPtr env, string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateJobObjectW(IntPtr attrs, string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetInformationJobObject(IntPtr job, int infoClass, ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION info, uint len);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr proc);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint ResumeThread(IntPtr thread);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForSingleObject(IntPtr handle, uint ms);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetExitCodeProcess(IntPtr proc, out uint code);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool TerminateJobObject(IntPtr job, uint code);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool TerminateProcess(IntPtr proc, uint code);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CloseHandle(IntPtr handle);

    IntPtr _job = IntPtr.Zero;
    IntPtr _proc = IntPtr.Zero;

    public uint ProcessId { get; private set; }

    public static SingularityReviewProcess StartInJob(string exePath, string commandLine, string workDir, string stdinPath, string stdoutPath, string stderrPath) {
        IntPtr job = CreateJobObjectW(IntPtr.Zero, null);
        if (job == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateJobObjectW failed");

        var limit = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
        limit.BasicLimitInformation.LimitFlags = JOB_KILL_ON_CLOSE;
        if (!SetInformationJobObject(job, 9, ref limit, (uint)Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION)))) {
            int err = Marshal.GetLastWin32Error();
            CloseHandle(job);
            throw new Win32Exception(err, "SetInformationJobObject failed");
        }

        var sa = new SECURITY_ATTRIBUTES { nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES)), bInheritHandle = true };
        IntPtr hIn = INVALID_HANDLE, hOut = INVALID_HANDLE, hErr = INVALID_HANDLE;
        var pi = new PROCESS_INFORMATION();
        bool started = false;
        try {
            hIn = CreateFileW(stdinPath, GENERIC_READ, SHARE_ALL, ref sa, OPEN_EXISTING, 0, IntPtr.Zero);
            if (hIn == INVALID_HANDLE) throw new Win32Exception(Marshal.GetLastWin32Error(), "Failed to open stdin file");
            hOut = CreateFileW(stdoutPath, GENERIC_WRITE, SHARE_ALL, ref sa, CREATE_ALWAYS, 0, IntPtr.Zero);
            if (hOut == INVALID_HANDLE) throw new Win32Exception(Marshal.GetLastWin32Error(), "Failed to create stdout file");
            hErr = CreateFileW(stderrPath, GENERIC_WRITE, SHARE_ALL, ref sa, CREATE_ALWAYS, 0, IntPtr.Zero);
            if (hErr == INVALID_HANDLE) throw new Win32Exception(Marshal.GetLastWin32Error(), "Failed to create stderr file");

            var si = new STARTUPINFO {
                cb = Marshal.SizeOf(typeof(STARTUPINFO)),
                dwFlags = STARTF_USESTDHANDLES,
                hStdInput = hIn,
                hStdOutput = hOut,
                hStdError = hErr
            };
            var sb = new StringBuilder(commandLine);
            if (!CreateProcessW(exePath, sb, IntPtr.Zero, IntPtr.Zero, true, CREATE_SUSPENDED | CREATE_NO_WINDOW, IntPtr.Zero, workDir, ref si, out pi)) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateProcessW failed");
            }
            started = true;

            if (!AssignProcessToJobObject(job, pi.hProcess)) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "AssignProcessToJobObject failed");
            }
            if (ResumeThread(pi.hThread) == uint.MaxValue) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "ResumeThread failed");
            }
            CloseHandle(pi.hThread);
            pi.hThread = IntPtr.Zero;

            var runner = new SingularityReviewProcess { _job = job, _proc = pi.hProcess, ProcessId = pi.dwProcessId };
            job = IntPtr.Zero;
            pi.hProcess = IntPtr.Zero;
            return runner;
        } catch {
            if (started && pi.hProcess != IntPtr.Zero) TerminateProcess(pi.hProcess, 1);
            if (pi.hThread != IntPtr.Zero) CloseHandle(pi.hThread);
            if (pi.hProcess != IntPtr.Zero) CloseHandle(pi.hProcess);
            if (job != IntPtr.Zero) { TerminateJobObject(job, 1); CloseHandle(job); }
            throw;
        } finally {
            if (hIn != INVALID_HANDLE) CloseHandle(hIn);
            if (hOut != INVALID_HANDLE) CloseHandle(hOut);
            if (hErr != INVALID_HANDLE) CloseHandle(hErr);
        }
    }

    public bool HasExited { get { return _proc == IntPtr.Zero || WaitForSingleObject(_proc, 0) == 0; } }

    public bool WaitForExit(uint timeoutMs) {
        return _proc == IntPtr.Zero || WaitForSingleObject(_proc, timeoutMs) == 0;
    }

    public int GetExitCode() {
        uint code;
        if (_proc == IntPtr.Zero || !GetExitCodeProcess(_proc, out code)) return -1;
        return unchecked((int)code);
    }

    public void TerminateTree() {
        if (_job != IntPtr.Zero) {
            TerminateJobObject(_job, 1);
            CloseHandle(_job);
            _job = IntPtr.Zero;
        }
        if (_proc != IntPtr.Zero) {
            WaitForSingleObject(_proc, 3000);
        }
    }

    public void Dispose() {
        TerminateTree();
        if (_proc != IntPtr.Zero) {
            CloseHandle(_proc);
            _proc = IntPtr.Zero;
        }
    }
}
'@
}

function Format-CliArg([string]$Value) {
    if ($Value.Length -eq 0 -or $Value -match '[\s"]') {
        $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
        $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
        return '"' + $escaped + '"'
    }
    return $Value
}

function Resolve-CodexInvocation {
    $exe = Get-Command codex.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($exe) { return @{ FilePath = $exe.Source; PrefixArgs = @() } }

    $cmd = Get-Command codex.cmd -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $cmd) {
        throw 'codex executable (codex.exe / npm codex.cmd) not found in PATH.'
    }

    # Resolve native codex.exe (or node.exe + codex.js) and never fall back to cmd.exe, which expands %VAR% in paths.
    $shimDir = Split-Path -Parent $cmd.Source
    $archPkg = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'codex-win32-arm64' } else { 'codex-win32-x64' }
    $triple = if ($archPkg -eq 'codex-win32-arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
    $vendorCandidates = @(
        (Join-Path $shimDir "node_modules\@openai\$archPkg\vendor\$triple\bin\codex.exe"),
        (Join-Path $shimDir "node_modules\@openai\codex\node_modules\@openai\$archPkg\vendor\$triple\bin\codex.exe"),
        (Join-Path $shimDir "node_modules\@openai\codex\vendor\$triple\bin\codex.exe")
    )
    foreach ($candidate in $vendorCandidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return @{ FilePath = $candidate; PrefixArgs = @() }
        }
    }

    $codexJs = Join-Path $shimDir 'node_modules\@openai\codex\bin\codex.js'
    if (Test-Path -LiteralPath $codexJs -PathType Leaf) {
        $localNode = Join-Path $shimDir 'node.exe'
        $nodeExe = if (Test-Path -LiteralPath $localNode -PathType Leaf) {
            $localNode
        } else {
            (Get-Command node.exe -ErrorAction SilentlyContinue | Select-Object -First 1).Source
        }
        if ($nodeExe) {
            return @{ FilePath = $nodeExe; PrefixArgs = @($codexJs) }
        }
    }

    throw "Found $($cmd.Source) but could not locate native codex.exe or node.exe + codex.js (refusing cmd.exe fallback)."
}

function Invoke-GitRaw([string]$WorkDir, [string[]]$GitArgs) {
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = (Get-Command git.exe, git -ErrorAction Stop | Select-Object -First 1).Source
    $psi.Arguments = (@($GitArgs | ForEach-Object { Format-CliArg $_ }) -join ' ')
    $psi.WorkingDirectory = $WorkDir
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    try {
        $errTask = $p.StandardError.ReadToEndAsync()
        $ms = [System.IO.MemoryStream]::new()
        $p.StandardOutput.BaseStream.CopyTo($ms)
        $errText = $errTask.GetAwaiter().GetResult()
        $p.WaitForExit()
        # Wrap byte[] in hashtable so PowerShell never unrolls an empty byte[0] into $null.
        return @{ Ok = ($p.ExitCode -eq 0); Bytes = $ms.ToArray(); Error = $errText.Trim() }
    } finally {
        $p.Dispose()
    }
}

function Get-WorkspaceFingerprint {
    if (-not (Get-Command git.exe, git -ErrorAction SilentlyContinue)) {
        return @{ InRepo = $false; Ok = $true; Hash = $null; Error = $null }
    }
    $cwd = (Get-Location).ProviderPath
    $top = Invoke-GitRaw $cwd @('rev-parse', '--show-toplevel')
    if (-not $top.Ok) {
        return @{ InRepo = $false; Ok = $true; Hash = $null; Error = $null }
    }
    $repoRoot = $utf8.GetString($top.Bytes).Trim()
    if (-not $repoRoot) {
        return @{ InRepo = $false; Ok = $true; Hash = $null; Error = $null }
    }

    $statusRes = Invoke-GitRaw $repoRoot @('-c', 'core.quotepath=false', 'status', '--porcelain', '--untracked-files=all', '-z')
    $workDiff = Invoke-GitRaw $repoRoot @('-c', 'core.quotepath=false', 'diff', '--binary', '--no-ext-diff')
    $indexDiff = Invoke-GitRaw $repoRoot @('-c', 'core.quotepath=false', 'diff', '--cached', '--binary', '--no-ext-diff')
    $untrackedRes = Invoke-GitRaw $repoRoot @('-c', 'core.quotepath=false', 'ls-files', '--others', '--exclude-standard', '-z')
    foreach ($res in @($statusRes, $workDiff, $indexDiff, $untrackedRes)) {
        if (-not $res.Ok) {
            return @{ InRepo = $true; Ok = $false; Hash = $null; Error = "git snapshot failed: $($res.Error)" }
        }
    }

    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $sep = [byte[]]@(0, 255, 0)
        foreach ($chunk in @($statusRes.Bytes, $sep, $workDiff.Bytes, $sep, $indexDiff.Bytes, $sep)) {
            if ($chunk.Length -gt 0) {
                [void]$sha.TransformBlock($chunk, 0, $chunk.Length, $null, 0)
            }
        }
        if ($untrackedRes.Bytes.Length -gt 0) {
            $relPaths = $utf8.GetString($untrackedRes.Bytes).Split([char]0, [System.StringSplitOptions]::RemoveEmptyEntries)
            foreach ($rel in $relPaths) {
                $nameBytes = $utf8.GetBytes($rel)
                [void]$sha.TransformBlock($nameBytes, 0, $nameBytes.Length, $null, 0)
                [void]$sha.TransformBlock($sep, 0, $sep.Length, $null, 0)
                $full = Join-Path $repoRoot $rel
                if (Test-Path -LiteralPath $full -PathType Leaf) {
                    $fileBytes = [System.IO.File]::ReadAllBytes($full)
                    if ($fileBytes.Length -gt 0) {
                        [void]$sha.TransformBlock($fileBytes, 0, $fileBytes.Length, $null, 0)
                    }
                }
                [void]$sha.TransformBlock($sep, 0, $sep.Length, $null, 0)
            }
        }
        [void]$sha.TransformFinalBlock([byte[]]@(), 0, 0)
        return @{ InRepo = $true; Ok = $true; Hash = ([BitConverter]::ToString($sha.Hash).Replace('-', '')); Error = $null }
    } finally {
        $sha.Dispose()
    }
}

if (-not (Test-Path -LiteralPath $PromptFile -PathType Leaf)) {
    throw "PromptFile must be an existing file: $PromptFile"
}
$resolvedPrompt = (Resolve-Path -LiteralPath $PromptFile).ProviderPath
$runId = [guid]::NewGuid().ToString('N')
$outFile = Join-Path $env:TEMP "codex-review-$runId.md"
$stdoutFile = Join-Path $env:TEMP "codex-review-$runId.out"
$logFile = Join-Path $env:TEMP "codex-review-$runId.log"

$beforeFp = $null
$runner = $null
try {
    try {
        $resolvedImages = @($Images | ForEach-Object {
            if (-not (Test-Path -LiteralPath $_ -PathType Leaf)) {
                throw "Image path must be an existing file: $_"
            }
            (Resolve-Path -LiteralPath $_).ProviderPath
        })
        $invocation = Resolve-CodexInvocation

        $rawArgs = [System.Collections.Generic.List[string]]::new()
        $rawArgs.Add($invocation.FilePath)
        foreach ($prefix in $invocation.PrefixArgs) { $rawArgs.Add($prefix) }
        $rawArgs.AddRange([string[]]@(
            'exec',
            '-m', $Model,
            '-c', "model_reasoning_effort=$Effort",
            '-c', 'notify=[]',
            '-c', 'features.memories=false'
        ))
        if (-not $AllowWebSearch) {
            $rawArgs.AddRange([string[]]@('-c', 'web_search=disabled'))
        }
        $rawArgs.AddRange([string[]]@('--ephemeral', '--skip-git-repo-check', '-o', $outFile))
        foreach ($img in $resolvedImages) {
            $rawArgs.AddRange([string[]]@('-i', $img))
        }
        $rawArgs.AddRange([string[]]@('--', '-'))
        $cmdLine = (@($rawArgs | ForEach-Object { Format-CliArg $_ }) -join ' ')

        try {
            $beforeFp = Get-WorkspaceFingerprint
            if ($beforeFp.InRepo -and -not $beforeFp.Ok) {
                Write-Warning "Failed to capture pre-review workspace fingerprint: $($beforeFp.Error)"
            }
        } catch {
            Write-Warning "Failed to capture pre-review workspace fingerprint: $_"
        }

        $runner = [SingularityReviewProcess]::StartInJob(
            $invocation.FilePath,
            $cmdLine,
            (Get-Location).ProviderPath,
            $resolvedPrompt,
            $stdoutFile,
            $logFile
        )

        Write-Host "OUT_FILE=$outFile"
        Write-Host "LOG_FILE=$logFile"

        $stableSince = $null
        $lastOutSig = ''
        $lastLogSig = ''
        $lastActivity = Get-Date
        $recovered = $false
        $stalled = $false

        while (-not $runner.HasExited) {
            Start-Sleep -Seconds 1
            if (Test-Path -LiteralPath $logFile) {
                $li = Get-Item -LiteralPath $logFile
                $logSig = "$($li.Length)|$($li.LastWriteTimeUtc.Ticks)"
                if ($logSig -ne $lastLogSig) {
                    $lastLogSig = $logSig
                    $lastActivity = Get-Date
                }
            }
            if ((Test-Path -LiteralPath $outFile) -and ($fi = Get-Item -LiteralPath $outFile).Length -gt 0) {
                $outSig = "$($fi.Length)|$($fi.LastWriteTimeUtc.Ticks)"
                if ($outSig -ne $lastOutSig) {
                    $lastOutSig = $outSig
                    $stableSince = Get-Date
                } elseif (((Get-Date) - $stableSince).TotalSeconds -ge 15) {
                    $recovered = $true
                    break
                }
            } elseif (((Get-Date) - $lastActivity).TotalSeconds -ge $StallSeconds) {
                $stalled = $true
                break
            }
        }

        if ($recovered -or $stalled) {
            $runner.TerminateTree()
        } else {
            $null = $runner.WaitForExit(3000)
        }

        $exitCode = if ($recovered) { 0 } else { $runner.GetExitCode() }
        $runner.Dispose()
        $runner = $null

        $report = if (Test-Path -LiteralPath $outFile) { [System.IO.File]::ReadAllText($outFile, $utf8) } else { '' }

        if ($stalled) {
            Get-Content -LiteralPath $logFile -Tail 60 -Encoding UTF8 -ErrorAction SilentlyContinue | Write-Host
            throw "codex exec stalled (no stderr progress for ${StallSeconds}s)."
        }
        if ($exitCode -ne 0 -or [string]::IsNullOrWhiteSpace($report)) {
            Get-Content -LiteralPath $logFile -Tail 60 -Encoding UTF8 -ErrorAction SilentlyContinue | Write-Host
            throw "codex exec failed (exit=$exitCode)."
        }

        Write-Output $report
    } finally {
        if ($null -ne $runner) {
            $runner.Dispose()
            $runner = $null
        }
        if ($null -ne $beforeFp -and $beforeFp.InRepo -and $beforeFp.Ok) {
            try {
                $afterFp = Get-WorkspaceFingerprint
                if (-not $afterFp.Ok) {
                    Write-Warning "Failed to verify post-review workspace fingerprint: $($afterFp.Error)"
                } elseif ($beforeFp.Hash -cne $afterFp.Hash) {
                    $statusSummary = (& git -c core.quotepath=false status --porcelain --untracked-files=all 2>$null) -join "`n"
                    Write-Warning "Workspace changed during review.`n$statusSummary"
                }
            } catch {
                Write-Warning "Failed to verify post-review workspace fingerprint: $_"
            }
        }
    }
} finally {
    $cleanup = @($outFile, $stdoutFile, $logFile)
    if ($DeletePrompt) { $cleanup += $resolvedPrompt }
    for ($attempt = 0; $attempt -lt 5; $attempt++) {
        $remaining = @($cleanup | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf })
        if ($remaining.Count -eq 0) { break }
        Remove-Item -LiteralPath $remaining -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 150
    }
}
