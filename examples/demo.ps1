<#
    Disbatch demo script.
    Open this file in Disbatch to see every control type generated automatically,
    then click Run to watch the console + progress bar.
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$InputFolder,                       # -> folder picker (required *)

    [string]$OutFile = "result.txt",            # -> file field, pre-filled

    [ValidateSet("Low", "Medium", "High")]
    [string]$Quality = "Medium",                # -> dropdown

    [int]$Iterations = 5,                        # -> number field

    [switch]$Recurse,                            # -> checkbox

    [switch]$DryRun                              # -> checkbox
)

Write-Host "Disbatch demo starting..."
Write-Host "  InputFolder = $InputFolder"
Write-Host "  OutFile     = $OutFile"
Write-Host "  Quality     = $Quality"
Write-Host "  Iterations  = $Iterations"
Write-Host "  Recurse     = $Recurse"
Write-Host "  DryRun      = $DryRun"
Write-Host "@status Working through $Iterations iterations"

for ($i = 1; $i -le $Iterations; $i++) {
    $pct = [int](($i / $Iterations) * 100)
    Write-Host "@progress $pct"
    Write-Host "  step $i of $Iterations ..."
    Start-Sleep -Milliseconds 600
}

Write-Host "@status Done"
Write-Host "@progress 100"
Write-Host "Finished successfully."
