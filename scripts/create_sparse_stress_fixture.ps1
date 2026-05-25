param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Path,

    [string]$LogicalSize = "500GB",

    [string]$HeadText = "QEM_SPARSE_HEAD`n",

    [string]$MiddleText = "QEM_SPARSE_MIDDLE`n",

    [string]$TailText = "QEM_SPARSE_TAIL`n",

    [switch]$Force
)

$ErrorActionPreference = "Stop"

function Parse-SizeBytes {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Value
    )

    $normalized = $Value.Trim().ToUpperInvariant()
    if ($normalized -notmatch '^(\d+)(B|KB|MB|GB|TB|KIB|MIB|GIB|TIB)?$') {
        throw "Unsupported size '$Value'. Use values like 1073741824, 1GB, 50GB, or 1TB."
    }

    $number = [UInt64]$matches[1]
    $suffix = $matches[2]
    switch ($suffix) {
        "" { return $number }
        "B" { return $number }
        "KB" { return $number * 1000 }
        "MB" { return $number * 1000 * 1000 }
        "GB" { return $number * 1000 * 1000 * 1000 }
        "TB" { return $number * 1000 * 1000 * 1000 * 1000 }
        "KIB" { return $number * 1024 }
        "MIB" { return $number * 1024 * 1024 }
        "GIB" { return $number * 1024 * 1024 * 1024 }
        "TIB" { return $number * 1024 * 1024 * 1024 * 1024 }
        default { throw "Unsupported size suffix '$suffix'." }
    }
}

function Write-Utf8TextAt {
    param(
        [Parameter(Mandatory = $true)]
        [System.IO.FileStream]$Stream,

        [Parameter(Mandatory = $true)]
        [UInt64]$LogicalSizeBytes,

        [Parameter(Mandatory = $true)]
        [UInt64]$Offset,

        [Parameter(Mandatory = $true)]
        [string]$Text
    )

    if ([string]::IsNullOrEmpty($Text)) {
        return
    }

    $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
    if ([UInt64]$bytes.Length -gt $LogicalSizeBytes) {
        throw "Text payload is larger than the requested logical size."
    }

    $maxOffset = $LogicalSizeBytes - [UInt64]$bytes.Length
    $safeOffset = [Math]::Min([double]$Offset, [double]$maxOffset)
    [void]$Stream.Seek([Int64][UInt64]$safeOffset, [System.IO.SeekOrigin]::Begin)
    $Stream.Write($bytes, 0, $bytes.Length)
}

$logicalSizeBytes = Parse-SizeBytes $LogicalSize
if ($logicalSizeBytes -lt 1024) {
    throw "Logical size must be at least 1024 bytes."
}

$fullPath = [System.IO.Path]::GetFullPath($Path)
$parent = Split-Path -Parent $fullPath
if (-not [string]::IsNullOrWhiteSpace($parent) -and -not (Test-Path $parent)) {
    New-Item -ItemType Directory -Path $parent | Out-Null
}

if ((Test-Path $fullPath) -and -not $Force) {
    throw "File already exists: $fullPath. Pass -Force to overwrite it."
}

if (-not (Test-Path $fullPath)) {
    New-Item -ItemType File -Path $fullPath | Out-Null
}

& fsutil sparse setflag $fullPath | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "fsutil sparse setflag failed for $fullPath"
}

$stream = [System.IO.File]::Open($fullPath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
try {
    $stream.SetLength([Int64]$logicalSizeBytes)

    Write-Utf8TextAt -Stream $stream -LogicalSizeBytes $logicalSizeBytes -Offset 0 -Text $HeadText
    Write-Utf8TextAt -Stream $stream -LogicalSizeBytes $logicalSizeBytes -Offset ([UInt64]($logicalSizeBytes / 2)) -Text $MiddleText
    Write-Utf8TextAt -Stream $stream -LogicalSizeBytes $logicalSizeBytes -Offset ([UInt64]($logicalSizeBytes - 1)) -Text $TailText
}
finally {
    $stream.Dispose()
}

$created = Get-Item $fullPath
Write-Host ("Created sparse stress fixture: {0}" -f $fullPath)
Write-Host ("Logical size bytes: {0}" -f $created.Length)
Write-Host "Markers:"
Write-Host ("  head   = {0}" -f $HeadText.TrimEnd())
Write-Host ("  middle = {0}" -f $MiddleText.TrimEnd())
Write-Host ("  tail   = {0}" -f $TailText.TrimEnd())
Write-Host "This fixture is useful for structural huge-file stress and mmap/viewport envelope checks."
Write-Host "It is not a representative replacement for a real 1TB text file when publishing throughput claims."
