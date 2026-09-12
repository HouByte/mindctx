# One-line uninstaller for mindctx (Windows).
#
# Reverses what the installer did: strips the marker blocks it wrote, removes the MCP
# registration through the host CLIs, and deletes only its own binary. Content outside those
# blocks is never touched. Pass -Purge (or --purge) to also drop ~/.mindctx/record/ (receipts
# left behind by the removed `mindctx apply`), which is kept by default.
param([switch]$Purge)

# `irm ... | iex` cannot pass arguments, and PowerShell binds `--purge` as a positional
# argument rather than to the -Purge switch, so accept both spellings.
$purge = $false
foreach ($arg in $args) {
    if ($arg -eq '--purge' -or $arg -eq '-purge') {
        $purge = $true
    } else {
        Write-Error "Unknown option: $arg (supported: -Purge)" -ErrorAction Stop
        exit 2
    }
}
if ($Purge) { $purge = $true }

# Markers delimit the chunks the installer owns. Prompt blocks live in markdown files (HTML
# comments); the [core] block lives in TOML and uses comments — HTML comments are not TOML.
$PromptBegin = '<!-- mindctx:begin -->'
$PromptEnd = '<!-- mindctx:end -->'
$CoreBegin = '# mindctx:begin:core'
$CoreEnd = '# mindctx:end:core'

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

# Whole-line marker test: only lines the installer wrote count, as in the bash version.
function Test-LineMarker {
    param([string]$Text, [string]$Marker)
    return [regex]::IsMatch($Text, '(?m)^' + [regex]::Escape($Marker) + '$')
}

# Remove every inclusive begin..end block. An unterminated block is left alone rather than
# truncating the file.
function Remove-MarkedBlock {
    param([string]$Text, [string]$Begin, [string]$End)

    if (-not (Test-LineMarker -Text $Text -Marker $End)) {
        Write-Warning "  warning: unterminated $Begin block - left as-is"
        return $Text
    }

    $pattern = '(?ms)^' + [regex]::Escape($Begin) + '\r?\n.*?^' + [regex]::Escape($End) + '\r?\n?'
    return [regex]::Replace($Text, $pattern, '')
}

# Strip the block, drop the blank lines left behind, then remove the file when nothing else was
# in it (the installer creates these files when the host has none).
function Remove-BlockAndPrune {
    param([string]$Path, [string]$Begin, [string]$End)

    if (-not (Test-Path $Path)) { return }

    $text = Read-TextFile -Path $Path
    if (-not (Test-LineMarker -Text $text -Marker $Begin)) { return }

    $text = Remove-MarkedBlock -Text $text -Begin $Begin -End $End
    $text = [regex]::Replace($text, '(\r?\n\s*)+$', '')

    if ($text.Length -eq 0) {
        Remove-Item $Path -Force
        Write-Output "Removed $Path (no content left after stripping the mindctx block)"
        return
    }

    if (-not $text.EndsWith("`n")) { $text += "`n" }
    Write-TextFile -Path $Path -Text $text
    Write-Output "Stripped the mindctx block from $Path"
}

# Configuration cleanup first: the host CLIs below may rewrite their own config files, and the
# marker blocks live in plain files next to them.
Remove-BlockAndPrune -Path (Join-Path $HOME '.claude\CLAUDE.md') -Begin $PromptBegin -End $PromptEnd
Remove-BlockAndPrune -Path (Join-Path $HOME '.codex\AGENTS.md') -Begin $PromptBegin -End $PromptEnd
Remove-BlockAndPrune -Path (Join-Path $HOME '.mindctx\config.toml') -Begin $CoreBegin -End $CoreEnd

# Remove MCP registrations (idempotent — don't fail if not registered)
try {
    $claudeCmd = Get-Command claude -ErrorAction SilentlyContinue
    if ($claudeCmd) {
        claude mcp remove mindctx --scope user 2>$null
        if ($LASTEXITCODE -eq 0) {
            Write-Output "Removed mindctx MCP server from Claude Code"
        }
    }
} catch { }
try {
    $codexCmd = Get-Command codex -ErrorAction SilentlyContinue
    if ($codexCmd) {
        codex mcp remove mindctx 2>$null
        if ($LASTEXITCODE -eq 0) {
            Write-Output "Removed mindctx MCP server from Codex"
        }
    }
} catch { }

$target = Join-Path $env:LOCALAPPDATA 'Programs\mindctx\mindctx.exe'
if (Test-Path $target) {
    Remove-Item $target -Force
    Write-Output "Removed $target"
} else {
    Write-Output "mindctx is not installed (no file at $target)"
}

if ($purge) {
    $record = Join-Path $HOME '.mindctx\record'
    if (Test-Path $record) {
        Remove-Item $record -Recurse -Force
        Write-Output "Purged $record"
    } else {
        Write-Output "Nothing to purge at $record"
    }
}
