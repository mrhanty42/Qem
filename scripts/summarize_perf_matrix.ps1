param(
    [string]$InputJsonl = "target\perf-matrix.jsonl",
    [string]$OutputMarkdown = "target\perf-matrix-summary.md"
)

$ErrorActionPreference = "Stop"
$InvariantCulture = [System.Globalization.CultureInfo]::InvariantCulture

function Format-InvariantNumber {
    param(
        [double]$Value
    )

    return $Value.ToString("0.000", $InvariantCulture)
}

function Format-SizeLabel {
    param(
        [double]$Bytes
    )

    if ($Bytes -ge 1GB) {
        return ('{0} GiB' -f (Format-InvariantNumber ($Bytes / 1GB)))
    }
    if ($Bytes -ge 1MB) {
        return ('{0} MiB' -f (Format-InvariantNumber ($Bytes / 1MB)))
    }
    if ($Bytes -ge 1KB) {
        return ('{0} KiB' -f (Format-InvariantNumber ($Bytes / 1KB)))
    }

    return ('{0} B' -f [int64]$Bytes)
}

function Get-StatsRow {
    param(
        [object[]]$Values
    )

    $filtered = @($Values | Where-Object { $_ -ne $null })
    if ($filtered.Count -eq 0) {
        return @{
            median = "-"
            min = "-"
            max = "-"
        }
    }

    $sorted = @($filtered | Sort-Object)
    $count = $sorted.Count
    if ($count % 2 -eq 1) {
        $median = [double]$sorted[[int]($count / 2)]
    }
    else {
        $median = ([double]$sorted[($count / 2) - 1] + [double]$sorted[$count / 2]) / 2.0
    }

    return @{
        median = (Format-InvariantNumber ([double]$median))
        min = (Format-InvariantNumber ([double]$sorted[0]))
        max = (Format-InvariantNumber ([double]$sorted[$count - 1]))
    }
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$inputPath = Join-Path $repoRoot $InputJsonl
$outputPath = Join-Path $repoRoot $OutputMarkdown

if (-not (Test-Path $inputPath)) {
    throw "Input JSONL not found: $inputPath"
}

$rows = Get-Content $inputPath |
    Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
    ForEach-Object { $_ | ConvertFrom-Json }

if ($rows.Count -eq 0) {
    throw "Input JSONL is empty: $inputPath"
}

$groups = $rows | Group-Object {
    "{0}|{1}|{2}|{3}|{4}" -f $_.matrix_input_label, $_.matrix_state, $_.backing, $_.matrix_label, $_.matrix_viewport_anchor
}

$lines = New-Object System.Collections.Generic.List[string]
$lines.Add("# Perf Matrix Summary")
$lines.Add("")
$lines.Add(('Source: `{0}`' -f $InputJsonl))
$lines.Add("")
$lines.Add('| input | size | label | anchor | state | backing | runs | open ms | index wait ms | exact line wait ms | edit ms | viewport ms | save ms | next ms | prev ms | find_all ms | peak WS MiB |')
$lines.Add('| --- | --- | --- | --- | --- | --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- | --- | ---: |')

foreach ($group in ($groups | Sort-Object Name)) {
    $parts = $group.Name.Split('|', 5)
    $inputLabel = $parts[0]
    $state = $parts[1]
    $backing = $parts[2]
    $matrixLabel = $parts[3]
    $viewportAnchor = $parts[4]
    if ([string]::IsNullOrWhiteSpace($matrixLabel)) {
        $matrixLabel = "-"
    }
    if ([string]::IsNullOrWhiteSpace($viewportAnchor)) {
        $viewportAnchor = "middle"
    }

    $firstRow = $group.Group | Select-Object -First 1
    $sizeLabel = Format-SizeLabel ([double]$firstRow.file_len_bytes)

    $openStats = Get-StatsRow ($group.Group | ForEach-Object { $_.open_ms })
    $indexWaitStats = Get-StatsRow ($group.Group | ForEach-Object { $_.index_wait_ms })
    $exactLineWaitStats = Get-StatsRow ($group.Group | ForEach-Object { $_.exact_line_count_wait_ms })
    $seedEditStats = Get-StatsRow ($group.Group | ForEach-Object { $_.seed_edit_ms })
    $viewportStats = Get-StatsRow ($group.Group | ForEach-Object { $_.viewport_ms })
    $saveStats = Get-StatsRow ($group.Group | ForEach-Object { $_.save_ms })
    $nextStats = Get-StatsRow ($group.Group | ForEach-Object { $_.next_ms })
    $prevStats = Get-StatsRow ($group.Group | ForEach-Object { $_.prev_ms })
    $findAllStats = Get-StatsRow ($group.Group | ForEach-Object { $_.find_all_ms })
    $peakWorkingSetStats = Get-StatsRow (
        $group.Group | ForEach-Object {
            if ($_.PSObject.Properties.Name -contains 'matrix_peak_working_set_bytes' -and $_.matrix_peak_working_set_bytes -ne $null) {
                ([double]$_.matrix_peak_working_set_bytes) / 1MB
            }
            else {
                $null
            }
        }
    )

    $row = @(
        $inputLabel,
        $sizeLabel,
        $matrixLabel,
        $viewportAnchor,
        $state,
        $backing,
        [string]$group.Count,
        ('{0} [{1}-{2}]' -f $openStats.median, $openStats.min, $openStats.max),
        ('{0} [{1}-{2}]' -f $indexWaitStats.median, $indexWaitStats.min, $indexWaitStats.max),
        ('{0} [{1}-{2}]' -f $exactLineWaitStats.median, $exactLineWaitStats.min, $exactLineWaitStats.max),
        ('{0} [{1}-{2}]' -f $seedEditStats.median, $seedEditStats.min, $seedEditStats.max),
        ('{0} [{1}-{2}]' -f $viewportStats.median, $viewportStats.min, $viewportStats.max),
        ('{0} [{1}-{2}]' -f $saveStats.median, $saveStats.min, $saveStats.max),
        ('{0} [{1}-{2}]' -f $nextStats.median, $nextStats.min, $nextStats.max),
        ('{0} [{1}-{2}]' -f $prevStats.median, $prevStats.min, $prevStats.max),
        ('{0} [{1}-{2}]' -f $findAllStats.median, $findAllStats.min, $findAllStats.max),
        ('{0} [{1}-{2}]' -f $peakWorkingSetStats.median, $peakWorkingSetStats.min, $peakWorkingSetStats.max)
    )
    $lines.Add('| ' + ($row -join ' | ') + ' |')
}

$outputDir = Split-Path -Parent $outputPath
if (-not (Test-Path $outputDir)) {
    New-Item -ItemType Directory -Path $outputDir | Out-Null
}

$lines | Set-Content -Path $outputPath
Write-Host ("Wrote markdown summary to {0}" -f $outputPath)
