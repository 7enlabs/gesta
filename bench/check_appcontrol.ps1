# Which App Control policy is blocking execution from D:\Hermes\gesta\target?
# Run this in an elevated PowerShell. It only reads policy state.
$ErrorActionPreference = 'SilentlyContinue'

Write-Host "=== WDAC / App Control policies in effect ==="
$policies = Get-CimInstance -Namespace root\Microsoft\Windows\CI -ClassName MSFT_DeviceControlPolicies
if (-not $policies) { Write-Host "none reported" }
foreach ($p in $policies) {
    Write-Host "  Name        : $($p.FriendlyName)"
    Write-Host "  PolicyId    : $($p.PolicyID)"
    Write-Host "  Enforced    : $($p.EnforcementMode)  1=enforced 0=audit"
    Write-Host "  Version     : $($p.Version)"
    Write-Host ""
}

Write-Host "=== Smart App Control (consumer path) ==="
$sac = Get-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy' -Name VerifiedAndReputablePolicyState
Write-Host "  VerifiedAndReputablePolicyState = $($sac.VerifiedAndReputablePolicyState)  0=off 1=on 2=eval"

Write-Host ""
Write-Host "=== Code Integrity event for the blocked file ==="
Get-WinEvent -LogName 'Microsoft-Windows-CodeIntegrity/Operational' -MaxEvents 5 |
    Select-Object TimeCreated, Id, @{n='Msg';e={$_.Message.Substring(0, [Math]::Min(300, $_.Message.Length))}} |
    Format-List
