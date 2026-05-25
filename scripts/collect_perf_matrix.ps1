param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string[]]$InputPaths,

    [string]$Needle,

    [int]$FindAllLimit = 0,

    [int]$FindAllRangeLines = 0,

    [string[]]$States = @("clean", "edited"),

    [string[]]$ViewportAnchors = @("middle"),

    [ValidateRange(1, 1000)]
    [int]$Repeats = 2,

    [ValidateRange(0, 3600)]
    [int]$WaitSecs = 20,

    [string]$SeedEdit = "[probe]`n",

    [string]$MatrixLabel = "",

    [string]$OutputJsonl = "target\perf-matrix.jsonl",

    [switch]$MeasureSave,

    [string]$SaveDir = "target\perf-matrix-saves",

    [switch]$KeepSaveOutputs,

    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

function Get-SafePerfLabel {
    param(
        [string]$Value
    )

    $sanitized = $Value -replace '[^A-Za-z0-9._-]+', '_'
    if ([string]::IsNullOrWhiteSpace($sanitized)) {
        return "perf"
    }

    return $sanitized.Trim('_')
}

function Invoke-PerfProbe {
    param(
        [string]$ExePath,
        [string[]]$Arguments,
        [string]$WorkingDirectory
    )

    $token = [Guid]::NewGuid().ToString("N")
    $stdoutPath = Join-Path $WorkingDirectory ("perf-probe-stdout-{0}.log" -f $token)
    $stderrPath = Join-Path $WorkingDirectory ("perf-probe-stderr-{0}.log" -f $token)

    try {
        $process = Start-Process `
            -FilePath $ExePath `
            -ArgumentList $Arguments `
            -WorkingDirectory $WorkingDirectory `
            -NoNewWindow `
            -PassThru `
            -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath

        $peakWorkingSetBytes = 0L
        while (-not $process.HasExited) {
            $process.Refresh()
            if ($process.WorkingSet64 -gt $peakWorkingSetBytes) {
                $peakWorkingSetBytes = [int64]$process.WorkingSet64
            }
            Start-Sleep -Milliseconds 10
        }
        $process.WaitForExit()
        $process.Refresh()
        if ($process.WorkingSet64 -gt $peakWorkingSetBytes) {
            $peakWorkingSetBytes = [int64]$process.WorkingSet64
        }

        $outputLines = @()
        if (Test-Path $stdoutPath) {
            $outputLines += Get-Content $stdoutPath
        }
        if (Test-Path $stderrPath) {
            $outputLines += Get-Content $stderrPath
        }

        return @{
            ExitCode = $process.ExitCode
            OutputLines = $outputLines
            PeakWorkingSetBytes = $peakWorkingSetBytes
        }
    }
    finally {
        if (Test-Path $stdoutPath) {
            Remove-Item -Force $stdoutPath
        }
        if (Test-Path $stderrPath) {
            Remove-Item -Force $stderrPath
        }
    }
}

if ($FindAllLimit -gt 0 -and [string]::IsNullOrWhiteSpace($Needle)) {
    throw "--FindAllLimit requires -Needle."
}

if ($FindAllRangeLines -gt 0 -and $FindAllLimit -le 0) {
    throw "--FindAllRangeLines requires -FindAllLimit."
}

$States = @(
    $States |
        ForEach-Object { $_ -split "," } |
        ForEach-Object { $_.Trim() } |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
)

foreach ($state in $States) {
    if ($state -notin @("clean", "edited")) {
        throw "Unsupported state '$state'. Expected clean and/or edited."
    }
}

$ViewportAnchors = @(
    $ViewportAnchors |
        ForEach-Object { $_ -split "," } |
        ForEach-Object { $_.Trim().ToLowerInvariant() } |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
)

foreach ($viewportAnchor in $ViewportAnchors) {
    if ($viewportAnchor -notin @("head", "middle", "tail")) {
        throw "Unsupported viewport anchor '$viewportAnchor'. Expected head, middle, and/or tail."
    }
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$outputPath = Join-Path $repoRoot $OutputJsonl
$outputDir = Split-Path -Parent $outputPath
if (-not (Test-Path $outputDir)) {
    New-Item -ItemType Directory -Path $outputDir | Out-Null
}
if (Test-Path $outputPath) {
    Remove-Item -Force $outputPath
}

$perfProbeExe = Join-Path $repoRoot "target\debug\examples\perf_probe.exe"
$saveRoot = Join-Path $repoRoot $SaveDir

if ($MeasureSave -and -not (Test-Path $saveRoot)) {
    New-Item -ItemType Directory -Path $saveRoot | Out-Null
}

Push-Location $repoRoot
try {
    if (-not $SkipBuild -or -not (Test-Path $perfProbeExe)) {
        cargo build --example perf_probe
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --example perf_probe failed."
        }
    }

    if (-not (Test-Path $perfProbeExe)) {
        throw "perf_probe binary was not found at $perfProbeExe"
    }

    foreach ($inputPath in $InputPaths) {
        $resolvedInput = Resolve-Path $inputPath
        $inputFullPath = $resolvedInput.Path
        $inputLabel = Split-Path -Leaf $inputFullPath

        foreach ($viewportAnchor in $ViewportAnchors) {
            foreach ($state in $States) {
                for ($runIndex = 1; $runIndex -le $Repeats; $runIndex++) {
                    $args = @(
                        $inputFullPath,
                        "--json",
                        "--wait-secs",
                        $WaitSecs.ToString(),
                        "--viewport-anchor",
                        $viewportAnchor
                    )

                    if (-not [string]::IsNullOrWhiteSpace($Needle)) {
                        $args += @("--needle", $Needle)
                    }
                    if ($FindAllLimit -gt 0) {
                        $args += @("--find-all-limit", $FindAllLimit.ToString())
                    }
                    if ($FindAllRangeLines -gt 0) {
                        $args += @("--find-all-range-lines", $FindAllRangeLines.ToString())
                    }
                    if ($state -eq "edited") {
                        $args += @("--seed-edit", $SeedEdit)
                    }

                    $saveOutput = $null
                    if ($MeasureSave) {
                        $label = Get-SafePerfLabel ("{0}-{1}-{2}-{3}-{4}" -f $inputLabel, $viewportAnchor, $state, $runIndex, $MatrixLabel)
                        $saveOutput = Join-Path $saveRoot ("{0}.out" -f $label)
                        $args += @("--save", $saveOutput)
                    }

                    Write-Host ("[{0}/{1}] anchor={2} state={3} file={4}" -f $runIndex, $Repeats, $viewportAnchor, $state, $inputLabel)

                    $probeRun = Invoke-PerfProbe -ExePath $perfProbeExe -Arguments $args -WorkingDirectory $repoRoot
                    if ($probeRun.ExitCode -ne $null -and $probeRun.ExitCode -ne 0) {
                        throw "perf_probe failed for anchor=$viewportAnchor state=$state file=$inputFullPath"
                    }

                    $rawOutput = $probeRun.OutputLines
                    $jsonLine = $rawOutput | Where-Object { $_.TrimStart().StartsWith("{") } | Select-Object -Last 1
                    if (-not $jsonLine) {
                        throw "perf_probe did not emit JSON for anchor=$viewportAnchor state=$state file=$inputFullPath"
                    }

                    $record = $jsonLine | ConvertFrom-Json
                    $record | Add-Member -NotePropertyName "matrix_state" -NotePropertyValue $state
                    $record | Add-Member -NotePropertyName "matrix_run" -NotePropertyValue $runIndex
                    $record | Add-Member -NotePropertyName "matrix_input_label" -NotePropertyValue $inputLabel
                    $record | Add-Member -NotePropertyName "matrix_wait_secs" -NotePropertyValue $WaitSecs
                    $record | Add-Member -NotePropertyName "matrix_label" -NotePropertyValue $MatrixLabel
                    $record | Add-Member -NotePropertyName "matrix_viewport_anchor" -NotePropertyValue $viewportAnchor
                    $record | Add-Member -NotePropertyName "matrix_peak_working_set_bytes" -NotePropertyValue $probeRun.PeakWorkingSetBytes

                    ($record | ConvertTo-Json -Compress) | Add-Content -Path $outputPath

                    if ($MeasureSave -and -not $KeepSaveOutputs -and $saveOutput -and (Test-Path $saveOutput)) {
                        Remove-Item -Force $saveOutput
                    }
                }
            }
        }
    }
}
finally {
    Pop-Location
}

Write-Host ("Wrote JSONL matrix to {0}" -f $outputPath)
