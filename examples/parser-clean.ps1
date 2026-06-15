<#
    parser-clean.ps1 — a conventional param block where BOTH parsers agree.

    Open this in Disbatch: you get a folder picker (required), a dropdown, a
    number field, a checkbox and a text field. The status note reads
    "Detected 5 parameter(s)." — parsed by PowerShell's AST.

    Now force the regex fallback and reopen it:

        PowerShell:  $env:DISBATCH_NO_AST = "1"     # force the regex fallback
        (reopen parser-clean.ps1)
        PowerShell:  Remove-Item Env:\DISBATCH_NO_AST  # back to the AST parser

    The note now says "(regex fallback)" — but the controls are IDENTICAL. On
    ordinary scripts the backup matches the AST parser exactly; that parity is
    enforced by a unit test (psparse::tests::regex_fallback_matches_ast_*).
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$InputFolder,

    [ValidateSet("Low", "Medium", "High")]
    [string]$Quality = "Medium",

    [int]$Retries = 3,

    [switch]$Overwrite,

    [string]$Note = "nightly run"
)

Write-Host "@status Starting"
Write-Host "InputFolder = $InputFolder"
Write-Host "Quality     = $Quality"
Write-Host "Retries     = $Retries"
Write-Host "Overwrite   = $Overwrite"
Write-Host "Note        = $Note"
foreach ($p in 0, 25, 50, 75, 100) { Write-Host "@progress $p"; Start-Sleep -Milliseconds 120 }
Write-Host "@status Done"
