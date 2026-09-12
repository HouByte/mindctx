# One-line installer for mindctx (Windows).
# Requires PowerShell 5.1+.

# The agent prompt template is fetched from the same release tag as the binary (see Main), so
# the block written into a host always matches the version being installed.
# MINDCTX_PROMPT_URL overrides that source.

# Markers delimit the chunks this installer owns. Prompt blocks live in markdown files (HTML
# comments); the [core] block lives in TOML and uses comments — HTML comments are not TOML.
$PromptBegin = '<!-- mindctx:begin -->'
$PromptEnd = '<!-- mindctx:end -->'
$CoreBegin = '# mindctx:begin:core'
$CoreEnd = '# mindctx:end:core'

# Release download base. Defaults to GitHub; point MINDCTX_RELEASE_BASE_URL at a mirror
# (e.g. object storage) when GitHub is unreachable. The mirror must preserve the
# /releases/download/vX.Y.Z/{asset,SHA256SUMS} and /releases/latest path layout.
$ReleaseBaseUrl = if ($env:MINDCTX_RELEASE_BASE_URL) { $env:MINDCTX_RELEASE_BASE_URL } else { 'https://github.com/HouByte/mindctx' }

function Get-Platform {
    $arch = $env:PROCESSOR_ARCHITECTURE
    switch ($arch) {
        'AMD64' { return 'x86_64-pc-windows-msvc' }
        'ARM64' {
            Write-Error 'ARM64 Windows is not supported yet.' -ErrorAction Stop
            exit 1
        }
        default {
            Write-Error "Unsupported architecture: $arch" -ErrorAction Stop
            exit 1
        }
    }
}

function Get-Version {
    if ($env:MINDCTX_VERSION) {
        if ($env:MINDCTX_VERSION -notmatch '^v?\d+\.\d+\.\d+$') {
            Write-Error "Invalid MINDCTX_VERSION: $($env:MINDCTX_VERSION) (expected vX.Y.Z or X.Y.Z)" -ErrorAction Stop
            exit 1
        }
        return $env:MINDCTX_VERSION.TrimStart('v')
    }

    # Follow redirect to latest release tag
    try {
        $resp = Invoke-WebRequest -Uri "${ReleaseBaseUrl}/releases/latest" -Method Head -MaximumRedirection 0 -ErrorAction SilentlyContinue
        $location = $resp.Headers['Location']
        if ($location) {
            $tag = Split-Path -Leaf $location
            return $tag.TrimStart('v')
        }
    } catch {
        # Fallback for PS 5.1 where -MaximumRedirection 0 throws
    }

    # Fallback: use HttpClient to read Location header without following redirect
    try {
        $handler = New-Object System.Net.Http.HttpClientHandler
        $handler.AllowAutoRedirect = $false
        $client = New-Object System.Net.Http.HttpClient($handler)
        $client.DefaultRequestHeaders.Add('User-Agent', 'mindctx-install')
        $resp = $client.GetAsync("${ReleaseBaseUrl}/releases/latest").Result
        $location = $null
        if ($null -ne $resp.Headers.Location) { $location = $resp.Headers.Location.ToString() }
        if ($location) {
            $tag = Split-Path -Leaf $location
            return $tag.TrimStart('v')
        }
    } catch { }

    Write-Error 'Could not resolve latest release version.' -ErrorAction Stop
    exit 1
}

function Get-Asset {
    param([string]$Version, [string]$Platform)

    $baseUrl = "${ReleaseBaseUrl}/releases/download/v${Version}"
    $sha256Url = "${baseUrl}/SHA256SUMS"

    $tmpDir = Join-Path $env:TEMP ([System.Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

    Invoke-WebRequest -Uri $sha256Url -OutFile (Join-Path $tmpDir 'SHA256SUMS') -UseBasicParsing

    $assetName = "mindctx-${Platform}.exe"
    $assetPath = Join-Path $tmpDir $assetName
    Invoke-WebRequest -Uri "${baseUrl}/${assetName}" -OutFile $assetPath -UseBasicParsing

    $expectedHash = $null
    foreach ($line in (Get-Content (Join-Path $tmpDir 'SHA256SUMS'))) {
        if ($line -match "/$([regex]::Escape($assetName))$") {
            $expectedHash = ($line -split ' ')[0]
            break
        }
    }

    if (-not $expectedHash) {
        Write-Error "Asset ${assetName} not found in SHA256SUMS" -ErrorAction Stop
        exit 1
    }

    $actualHash = (Get-FileHash -Path $assetPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $expectedHash.ToLowerInvariant()) {
        Write-Error "SHA256 mismatch for ${assetName}:`n  expected: ${expectedHash}`n  actual:   ${actualHash}" -ErrorAction Stop
        exit 1
    }

    return @{ AssetPath = $assetPath; TmpDir = $tmpDir; AssetName = $assetName }
}

function Install-Binary {
    param([hashtable]$Result)

    $installDir = Join-Path $env:LOCALAPPDATA 'Programs\mindctx'
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null

    $binPath = Join-Path $installDir $Result.AssetName
    if (Test-Path $binPath) {
        Remove-Item $binPath -Force
    }
    Move-Item $Result.AssetPath (Join-Path $installDir 'mindctx.exe') -Force

    Write-Output "Installed mindctx to $installDir\mindctx.exe"
}

function Test-PathInEnv {
    $installDir = Join-Path $env:LOCALAPPDATA 'Programs\mindctx'
    $paths = $env:Path -split ';'
    foreach ($p in $paths) {
        if ($p -eq $installDir) { return $true }
    }
    return $false
}

function Register-Mcp {
    $hasClaude = $null -ne (Get-Command claude -ErrorAction SilentlyContinue)
    $hasCodex  = $null -ne (Get-Command codex  -ErrorAction SilentlyContinue)

    if (-not $hasClaude -and -not $hasCodex) {
        Write-Output "No coding tools detected on PATH, skipping MCP registration."
        return
    }

    Write-Output "Registering mindctx as MCP server in detected coding tools..."

    if ($hasClaude) {
        Write-Output "Registering mindctx as MCP server in Claude Code (user scope)..."
        try {
            claude mcp add --scope user --transport stdio mindctx -- mindctx serve 2>$null
            if ($LASTEXITCODE -eq 0) {
                Write-Output "  ok: Claude Code MCP registered"
            } else {
                Write-Warning "  warning: claude mcp add failed (exit $LASTEXITCODE) — register manually later"
            }
        } catch {
            Write-Warning "  warning: claude mcp add failed — register manually later"
        }
    }

    if ($hasCodex) {
        Write-Output "Registering mindctx as MCP server in Codex..."
        try {
            codex mcp add mindctx -- mindctx serve 2>$null
            if ($LASTEXITCODE -eq 0) {
                Write-Output "  ok: Codex MCP registered"
            } else {
                Write-Warning "  warning: codex mcp add failed (exit $LASTEXITCODE) — register manually later"
            }
        } catch {
            Write-Warning "  warning: codex mcp add failed — register manually later"
        }
    }
}

# Set-Content -Encoding UTF8 writes a BOM on PowerShell 5.1, which would corrupt the leading
# bytes of a TOML or markdown file. The .NET file APIs read and write UTF-8 (BOM detected on
# read, never written), on 5.1 and 7 alike.
function Read-TextFile {
    param([string]$Path)
    return [System.IO.File]::ReadAllText($Path)
}

function Write-TextFile {
    param([string]$Path, [string]$Text)
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $Text, $utf8NoBom)
}

# Fetch the agent prompt template. A missing template is fatal: a host must never be left
# with a half-written guidance block.
function Get-PromptTemplate {
    param([string]$Url, [string]$Dest)

    try {
        Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing
    } catch {
        Write-Error "Failed to download the agent prompt template: $Url" -ErrorAction Stop
        exit 1
    }

    $text = Read-TextFile -Path $Dest
    if (-not (Test-LineMarker -Text $text -Marker $PromptBegin) -or -not (Test-LineMarker -Text $text -Marker $PromptEnd)) {
        Write-Error "Agent prompt template is missing its own marker lines: $Url" -ErrorAction Stop
        exit 1
    }
}

# Whole-line marker test: only lines this installer wrote count, as in the bash version.
function Test-LineMarker {
    param([string]$Text, [string]$Marker)
    return [regex]::IsMatch($Text, '(?m)^' + [regex]::Escape($Marker) + '$')
}

# Remove every inclusive begin..end block, so repeated runs converge instead of stacking
# blocks. Markers are matched as whole lines.
function Remove-MarkedBlock {
    param([string]$Text, [string]$Begin, [string]$End)

    $pattern = '(?ms)^' + [regex]::Escape($Begin) + '\r?\n.*?^' + [regex]::Escape($End) + '\r?\n?'
    return [regex]::Replace($Text, $pattern, '')
}

# Drop trailing blank lines, so re-installing cannot accumulate blank-line drift.
function Remove-TrailingBlankLines {
    param([string]$Text)
    return [regex]::Replace($Text, '(\r?\n\s*)+$', '')
}

function Get-Newline {
    param([string]$Text)
    if ($Text -match "`r`n") { return "`r`n" }
    return "`n"
}

# Idempotent upsert of the agent prompt block: replace the previous block in place, append
# when there is none, create the file when the host has none yet.
function Add-PromptBlock {
    param([string]$Path, [string]$Template)

    $dir = Split-Path -Parent $Path
    if ($dir -and -not (Test-Path $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    }

    $text = ''
    if (Test-Path $Path) {
        $text = Read-TextFile -Path $Path
        if (Test-LineMarker -Text $text -Marker $PromptBegin) {
            if (-not (Test-LineMarker -Text $text -Marker $PromptEnd)) {
                Write-Warning "  warning: $Path has an unterminated $PromptBegin block - left as-is, block not written"
                return
            }
            $text = Remove-MarkedBlock -Text $text -Begin $PromptBegin -End $PromptEnd
        }
        $text = Remove-TrailingBlankLines -Text $text
    }

    $newline = Get-Newline -Text $text
    if ($text.Length -gt 0) {
        $text += $newline + $newline
    }
    $text += $Template
    if (-not $text.EndsWith($newline)) {
        $text += $newline
    }

    Write-TextFile -Path $Path -Text $text
    Write-Output "  ok: agent prompt block written to $Path"
}

# Idempotent upsert of the managed [core] section in ~/.mindctx/config.toml. Everything
# outside the marker block — user sections and comments — is preserved.
function Add-CoreConfig {
    $config = Join-Path $HOME '.mindctx\config.toml'
    $dir = Split-Path -Parent $config
    if (-not (Test-Path $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    }

    $text = ''
    if (Test-Path $config) {
        $text = Read-TextFile -Path $config
        if (Test-LineMarker -Text $text -Marker $CoreBegin) {
            if (-not (Test-LineMarker -Text $text -Marker $CoreEnd)) {
                Write-Warning "  warning: $config has an unterminated $CoreBegin block - left as-is, block not written"
                return
            }
            $text = Remove-MarkedBlock -Text $text -Begin $CoreBegin -End $CoreEnd
        }
        $text = Remove-TrailingBlankLines -Text $text

        # A bare, unmarked [core] can only come from `mindctx apply`, which wrote exactly that
        # and nothing else. Drop it so the file keeps one [core] table — the block below
        # carries it. A [core] holding keys is the user's; leave the file alone instead of
        # emitting a duplicate table, which is invalid TOML.
        if (Test-LineMarker -Text $text -Marker '[core]') {
            $meaningful = ($text -split "\r?\n" | Where-Object { $_ -notmatch '^\s*$' -and $_ -notmatch '^\s*#' }) -join "`n"
            if ($meaningful -eq '[core]') {
                $text = ''
            } else {
                Write-Warning "  warning: $config already has an unmanaged [core] section - left as-is, block not written"
                return
            }
        }
    }

    $newline = Get-Newline -Text $text
    if ($text.Length -gt 0) {
        $text += $newline + $newline
    }
    $text += $CoreBegin + $newline + '[core]' + $newline + '# managed by installer' + $newline + $CoreEnd + $newline

    Write-TextFile -Path $config -Text $text
    Write-Output "  ok: [core] block written to $config"
}

function Main {
    $installDir = Join-Path $env:LOCALAPPDATA 'Programs\mindctx'
    $platform = Get-Platform
    $version = Get-Version
    $result = Get-Asset -Version $version -Platform $platform
    Install-Binary -Result $result

    if (-not (Test-PathInEnv)) {
        Write-Output ""
        Write-Output "NOTE: ${installDir} is not in your PATH."
        Write-Output "Add it via System Properties > Environment Variables > Path (user) > Edit > New."
        Write-Output "Restart your shell for PATH changes to take effect."
    }

    Register-Mcp

    # Host configuration init, formerly `mindctx apply`'s job: the guidance block for each
    # detected host plus the shared runtime config. The template is pinned to the version
    # being installed, so the URL is built after Get-Version has run.
    $promptUrl = if ($env:MINDCTX_PROMPT_URL) { $env:MINDCTX_PROMPT_URL } else { "https://raw.githubusercontent.com/HouByte/mindctx/v${version}/scripts/agent-prompt.md" }
    $templatePath = Join-Path $env:TEMP ('mindctx-agent-prompt-' + [System.Guid]::NewGuid().ToString() + '.md')
    Get-PromptTemplate -Url $promptUrl -Dest $templatePath
    try {
        $template = Read-TextFile -Path $templatePath
        Add-CoreConfig
        if (Get-Command claude -ErrorAction SilentlyContinue) {
            Add-PromptBlock -Path (Join-Path $HOME '.claude\CLAUDE.md') -Template $template
        }
        if (Get-Command codex -ErrorAction SilentlyContinue) {
            Add-PromptBlock -Path (Join-Path $HOME '.codex\AGENTS.md') -Template $template
        }
    } finally {
        Remove-Item $templatePath -Force -ErrorAction SilentlyContinue
    }
}

Main
