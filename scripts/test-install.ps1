# Behavioral smoke for install.ps1 pure marker/block functions (pwsh 7, any platform).
#
# Tests the string/regex core — whole-line marker test, inclusive block strip, trailing-blank
# trim — without touching $HOME or the network. The $HOME-dependent wrappers (Add-CoreConfig /
# Add-PromptBlock) are thin file-I/O around these functions; their idempotency/ownership logic is
# the same as the bash scripts, which test-install.sh covers end-to-end.
$ErrorActionPreference = 'Stop'

$here = Split-Path -Parent $MyInvocation.MyCommand.Path

# Dot-source install.ps1 without the trailing Main call (which would download + install).
$src = Get-Content -Raw (Join-Path $here 'install.ps1')
$src = $src.TrimEnd() -replace '(?s)\r?\nMain\s*$', ''
. ([scriptblock]::Create($src))

function Fail([string]$msg) { throw "FAIL: $msg" }

$begin = '<!-- mindctx:begin -->'
$end = '<!-- mindctx:end -->'

# Test-LineMarker: whole-line match only, never a substring match.
if (-not (Test-LineMarker -Text "$begin`n" -Marker $begin)) { Fail 'begin marker not detected' }
if (Test-LineMarker -Text "prefix $begin suffix" -Marker $begin) { Fail 'marker matched as substring' }

# Remove-MarkedBlock: strips the inclusive block, preserves surrounding content.
$text = "# header`n$begin`nblock body`n$end`n# footer`n"
$out = Remove-MarkedBlock -Text $text -Begin $begin -End $end
if ($out -match [regex]::Escape($begin)) { Fail 'strip left the begin marker' }
if ($out -notmatch '# header') { Fail 'strip dropped content before the block' }
if ($out -notmatch '# footer') { Fail 'strip dropped content after the block' }

# Remove-MarkedBlock: an unterminated block (no end marker) is left alone.
$unterminated = "keep`n$begin`nbody`n"
$out2 = Remove-MarkedBlock -Text $unterminated -Begin $begin -End $end
if ($out2 -ne $unterminated) { Fail 'unterminated block should be left as-is' }

# Remove-TrailingBlankLines: drops trailing blanks only, keeps leading/middle content.
if ((Remove-TrailingBlankLines -Text "a`nb`n`n`n") -ne "a`nb") { Fail 'trailing blank trim wrong' }

# Get-Newline: CRLF is detected and preserved; otherwise LF.
if ((Get-Newline -Text "a`r`nb") -ne "`r`n") { Fail 'CRLF newline not detected' }
if ((Get-Newline -Text "a`nb") -ne "`n") { Fail 'LF newline not detected' }

Write-Output 'PASS: install.ps1 behavioral smoke'
