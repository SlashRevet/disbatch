<#
    parser-tricky.ps1 — constructs that separate the two parsers.

    Open this NORMALLY first. PowerShell's AST parser reads it correctly — note
    that "Mode" is a DROPDOWN with three bracketed options, and "Source Dir" is
    a required folder picker (even though its [Parameter()] spans several lines
    and is preceded by a comment full of traps).

    Now force the regex fallback and reopen it:

        PowerShell:  $env:DISBATCH_NO_AST = "1"     # force the regex fallback
        (reopen parser-tricky.ps1)
        PowerShell:  Remove-Item Env:\DISBATCH_NO_AST  # back to the AST parser

    The note now says "(regex fallback)" and "Mode" DEGRADES to a plain text box
    with a garbled value: the regex parser can't see the ']' inside the
    ValidateSet strings. That single visible difference is exactly why the AST
    parser is primary and the regex parser is only the backup for when PowerShell
    can't be invoked at all.
#>
param(
    # A comment with an unbalanced ) paren and a fake $Variable = "trap" to
    # throw off a naive scanner — the AST parser ignores it entirely.
    [Parameter(
        Mandatory = $true,
        HelpMessage = "Where to read from"
    )]
    [string]$SourceDir,

    # ValidateSet values that contain ']' — a clean dropdown under the AST
    # parser, lost under the regex fallback.
    [ValidateSet("[1] Fast", "[2] Balanced", "[3] Thorough")]
    [string]$Mode = "[2] Balanced",

    [int]$MaxItems = 50,

    # A default value containing parentheses (both parsers handle this one).
    [string]$Caption = "Report (final)"
)

Write-Host "@status Working"
Write-Host "SourceDir = $SourceDir"
Write-Host "Mode      = $Mode"
Write-Host "MaxItems  = $MaxItems"
Write-Host "Caption   = $Caption"
Write-Host "@progress 100"
Write-Host "@status Done"
