# Downloads the runtimes listed in runtimes.json into bin\.
#
#   fetch-runtimes.bat                      everything that is missing
#   fetch-runtimes.bat -List                show what is installed, download nothing
#   fetch-runtimes.bat -Only apache,mysql   just these
#   fetch-runtimes.bat -IncludeOptional     also the optional ones (Git)
#   fetch-runtimes.bat -Root D:\Other       fill another OmniPHP folder
#
# Every download is checked against the SHA-256 in runtimes.json before it is
# unpacked. What is already in place is left alone, so the script can be run
# again at any time. Works with Windows PowerShell 5.1 and PowerShell 7.
param(
    [string]$Root,
    [string[]]$Only,
    [switch]$IncludeOptional,
    [switch]$List
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # the progress bar makes downloads many times slower
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$repo = Split-Path -Parent $PSScriptRoot
if (-not $Root) { $Root = $repo }
$Root = [IO.Path]::GetFullPath($Root)
$manifest = Join-Path $repo 'runtimes.json'
$cache = Join-Path $Root 'tmp\downloads'
$userAgent = 'omniphp-fetch-runtimes'

if ($Root.Contains(' ')) {
    Write-Warning "The folder $Root has a space in its path. PHP's FastCGI launcher cannot run from it."
}

$entries = @((Get-Content -Raw -LiteralPath $manifest | ConvertFrom-Json).runtimes)

# "-Only a,b" arrives as one string when the script is started through the .bat file.
$wanted = @($Only | Where-Object { $_ } | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$unknown = @($wanted | Where-Object { $name = $_; -not ($entries | Where-Object { $_.name -eq $name }) })
if ($unknown.Count -gt 0) {
    Write-Host "Not in runtimes.json: $($unknown -join ', ')" -ForegroundColor Red
    Write-Host "Known names: $(($entries | ForEach-Object { $_.name }) -join ', ')"
    exit 2
}

# Not Get-FileHash: Windows PowerShell cannot load the module it lives in when
# it is started from a PowerShell 7 prompt.
function Get-Sha256([string]$file) {
    $stream = [IO.File]::OpenRead($file)
    try {
        $sha = [Security.Cryptography.SHA256]::Create()
        try {
            [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '').ToLowerInvariant()
        } finally {
            $sha.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function Get-TargetDir($entry) {
    Join-Path $Root ($entry.dir -replace '/', '\')
}

function Test-Installed($entry) {
    Test-Path -LiteralPath (Join-Path (Get-TargetDir $entry) ($entry.check -replace '/', '\')) -PathType Leaf
}

# Returns the downloaded file, fetching it unless a good copy is already cached.
function Get-Download($entry) {
    New-Item -ItemType Directory -Force -Path $cache | Out-Null
    $name = [IO.Path]::GetFileName(([Uri]$entry.urls[0]).AbsolutePath)
    $target = Join-Path $cache $name
    if ((Test-Path -LiteralPath $target) -and (Get-Sha256 $target) -eq $entry.sha256) {
        return $target
    }
    $problems = @()
    foreach ($url in $entry.urls) {
        try {
            Invoke-WebRequest -Uri $url -OutFile $target -UserAgent $userAgent -UseBasicParsing
        } catch {
            $problems += "$url : $($_.Exception.Message)"
            continue
        }
        $actual = Get-Sha256 $target
        if ($actual -eq $entry.sha256) {
            return $target
        }
        Remove-Item -LiteralPath $target -Force
        $problems += "$url : checksum mismatch, the file was discarded (expected $($entry.sha256), got $actual)"
    }
    throw "the download failed.`n    $($problems -join "`n    ")"
}

# Unpacks straight into the destination. Staging the files elsewhere and moving
# them afterwards fails on Windows with "Access is denied" while a virus
# scanner still holds them.
function Expand-Zip([string]$zip, [string]$dir, [string]$strip, [string[]]$exclude) {
    $base = [IO.Path]::GetFullPath($dir).TrimEnd('\') + '\'
    $prefix = if ($strip) { $strip.Trim('/') + '/' } else { '' }
    $archive = [IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $count = 0
        foreach ($item in $archive.Entries) {
            $name = $item.FullName -replace '\\', '/'
            if ($prefix) {
                if (-not $name.StartsWith($prefix)) { continue }
                $name = $name.Substring($prefix.Length)
            }
            if (-not $name) { continue }
            if (@($exclude | Where-Object { $name -like $_ }).Count -gt 0) { continue }
            $path = [IO.Path]::GetFullPath((Join-Path $base ($name -replace '/', '\')))
            if (-not $path.StartsWith($base, [StringComparison]::OrdinalIgnoreCase)) {
                throw "the archive holds a path outside its folder: $($item.FullName)"
            }
            if ($name.EndsWith('/')) {
                [IO.Directory]::CreateDirectory($path) | Out-Null
                continue
            }
            [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($path)) | Out-Null
            [IO.Compression.ZipFileExtensions]::ExtractToFile($item, $path, $true)
            $count++
        }
        if ($count -eq 0) { throw "nothing in the archive matched '$strip'" }
    } finally {
        $archive.Dispose()
    }
}

function Install-Runtime($entry) {
    $dir = Get-TargetDir $entry
    $download = Get-Download $entry
    New-Item -ItemType Directory -Force -Path $dir | Out-Null

    if ($entry.file) {
        Copy-Item -LiteralPath $download -Destination (Join-Path $dir $entry.file) -Force
    } elseif ($entry.self_extracting) {
        # A 7-Zip self-extractor: -o<folder> -y unpacks without asking.
        $run = Start-Process -FilePath $download -ArgumentList "-o`"$dir`"", '-y' -Wait -PassThru
        if ($run.ExitCode -ne 0) { throw "the self-extractor ended with code $($run.ExitCode)" }
    } else {
        Expand-Zip $download $dir $entry.strip @($entry.exclude | Where-Object { $_ })
    }

    if ($entry.write) {
        foreach ($extra in $entry.write.PSObject.Properties) {
            if ($extra.Name -match '[\\/:]') { throw "unsafe file name in 'write': $($extra.Name)" }
            [IO.File]::WriteAllText((Join-Path $dir $extra.Name), [string]$extra.Value, [Text.Encoding]::ASCII)
        }
    }

    if (-not (Test-Installed $entry)) {
        throw "unpacked, but $($entry.check) is not in $dir"
    }
    Remove-Item -LiteralPath $download -Force -ErrorAction SilentlyContinue
}

Write-Host "OmniPHP runtimes -> $Root\bin"
Write-Host ''
$failed = 0
foreach ($entry in $entries) {
    $label = '{0,-12} {1}' -f $entry.name, $entry.version
    $selected = if ($wanted.Count -gt 0) { $wanted -contains $entry.name } else { $IncludeOptional -or -not $entry.optional }

    if (Test-Installed $entry) {
        Write-Host "  ok       $label"
        continue
    }
    if (-not $selected) {
        if ($entry.optional -and $wanted.Count -eq 0) {
            Write-Host "  skipped  $label  (optional: -IncludeOptional)" -ForegroundColor DarkGray
        }
        continue
    }
    if ($List) {
        Write-Host "  missing  $label" -ForegroundColor Yellow
        continue
    }

    Write-Host "  getting  $label"
    try {
        Install-Runtime $entry
        Write-Host "  added    $label" -ForegroundColor Green
    } catch {
        Write-Host "  FAILED   $label" -ForegroundColor Red
        Write-Host "    $($_.Exception.Message)" -ForegroundColor Red
        $failed++
    }
}

Write-Host ''
if ($failed -gt 0) {
    Write-Host "$failed could not be installed. An old version may have been removed from its download site;"
    Write-Host 'put a newer version, its address and its SHA-256 in runtimes.json and run this again.'
    exit 1
}
if (-not $List) { Write-Host 'Done.' }
