<#
.SYNOPSIS
Rename EEPROM write-history YAML files to the decimal SNID filename format.

.DESCRIPTION
The target filename format is:
  <non-batch-snid-prefix>_<YYMMDD>_<decimal-sequence>.yaml

For a 14-byte YG stereo SNID:
  bytes 0..4  : resolution/vendor/module prefix
  bytes 5..8  : encoded batch date YY + month + day
  byte  9     : optical-axis class
  bytes 10..11: base-62 sequence, decoded to 1..3844
  bytes 12..13: algorithm/reserved suffix

Example:
  2T233268101a00 -> 2T233000_260801_73.yaml

Run with -WhatIf first on Windows:
  powershell -ExecutionPolicy Bypass -File scripts/convert_eeprom_history_names.ps1 -HistoryDir .\write_history -WhatIf
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$HistoryDir = "write_history"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Decode-SnidMonth {
    param([char]$Value)
    $code = [int][char]$Value
    if ($code -ge [int][char]'1' -and $code -le [int][char]'9') {
        return $code - [int][char]'0'
    }
    if ($code -ge [int][char]'A' -and $code -le [int][char]'C') {
        return $code - [int][char]'A' + 10
    }
    throw "invalid encoded SNID month '$Value'"
}

function Decode-SnidDay {
    param([char]$Value)
    $code = [int][char]$Value
    if ($code -ge [int][char]'1' -and $code -le [int][char]'9') {
        return $code - [int][char]'0'
    }
    if ($code -ge [int][char]'A' -and $code -le [int][char]'V') {
        return $code - [int][char]'A' + 10
    }
    throw "invalid encoded SNID day '$Value'"
}

function Decode-Base62Digit {
    param([char]$Value)
    $code = [int][char]$Value
    if ($code -ge [int][char]'0' -and $code -le [int][char]'9') {
        return $code - [int][char]'0'
    }
    if ($code -ge [int][char]'a' -and $code -le [int][char]'z') {
        return $code - [int][char]'a' + 10
    }
    if ($code -ge [int][char]'A' -and $code -le [int][char]'Z') {
        return $code - [int][char]'A' + 36
    }
    throw "invalid encoded SNID sequence digit '$Value'"
}

function Convert-SnidToHistoryFileName {
    param([string]$Snid)
    $serial = $Snid.Trim()
    if ($serial.Length -ne 14) {
        throw "SNID '$serial' must contain exactly 14 ASCII characters"
    }
    if ($serial -notmatch '^[0-9A-Za-z_-]+$') {
        throw "SNID '$serial' contains characters that are unsafe for history filenames"
    }

    $prefix = $serial.Substring(0, 5) + $serial.Substring(9, 1) + $serial.Substring(12, 2)
    $year = $serial.Substring(5, 2)
    $month = Decode-SnidMonth $serial[7]
    $day = Decode-SnidDay $serial[8]
    $sequence = (Decode-Base62Digit $serial[10]) * 62 + (Decode-Base62Digit $serial[11]) + 1
    if ($sequence -lt 1 -or $sequence -gt 3844) {
        throw "decoded SNID sequence $sequence is outside 1..3844"
    }

    return ('{0}_{1}{2:D2}{3:D2}_{4}.yaml' -f $prefix, $year, $month, $day, $sequence)
}

function Read-HistorySnid {
    param([string]$Path)
    $text = Get-Content -LiteralPath $Path -Raw

    $yamlMatch = [regex]::Match($text, '(?m)^\s*serial_number\s*:\s*[''\"]?([0-9A-Za-z_-]{14})[''\"]?\s*$')
    if ($yamlMatch.Success) {
        return $yamlMatch.Groups[1].Value
    }

    $jsonMatch = [regex]::Match($text, '\"serial_number\"\s*:\s*\"([^\"]+)\"')
    if ($jsonMatch.Success) {
        return $jsonMatch.Groups[1].Value
    }

    return $null
}

if (-not (Test-Path -LiteralPath $HistoryDir -PathType Container)) {
    throw "History directory '$HistoryDir' does not exist"
}

$files = Get-ChildItem -LiteralPath $HistoryDir -File | Where-Object { $_.Extension -ieq ".yaml" }
foreach ($file in $files) {
    $snid = Read-HistorySnid $file.FullName
    if ([string]::IsNullOrWhiteSpace($snid)) {
        Write-Warning "Skipping '$($file.Name)': cannot find request serial_number"
        continue
    }

    try {
        $targetName = Convert-SnidToHistoryFileName $snid
    } catch {
        Write-Warning "Skipping '$($file.Name)': $($_.Exception.Message)"
        continue
    }

    if ($file.Name -ceq $targetName) {
        Write-Host "OK '$($file.Name)'"
        continue
    }

    $targetPath = Join-Path -Path $file.DirectoryName -ChildPath $targetName
    if (Test-Path -LiteralPath $targetPath) {
        $existingSnid = Read-HistorySnid $targetPath
        if ($existingSnid -eq $snid) {
            Write-Warning "Skipping '$($file.Name)': target '$targetName' already records the same SNID"
            continue
        }
        throw "Cannot rename '$($file.Name)' to '$targetName': target exists and records SNID '$existingSnid'"
    }

    if ($PSCmdlet.ShouldProcess($file.FullName, "Rename to $targetName")) {
        Rename-Item -LiteralPath $file.FullName -NewName $targetName
    }
}
