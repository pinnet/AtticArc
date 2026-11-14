<#
.SYNOPSIS
  Makes a Hyper-V virtual machine with Ubuntu 24.04 Desktop for testing AtticARC and Plugin²
  on Linux.

.DESCRIPTION
  Downloads the newest Ubuntu 24.04 Desktop image (about 6 GB, checked against Ubuntu's
  SHA256SUMS), then makes a Generation 2 VM with Secure Boot set for Linux, 4 processors,
  8 GB of memory (more when free), a 60 GB disk that grows as used, and the Default Switch
  for the network. It starts the VM and opens its window; install Ubuntu from there, then run
  scripts/linux-test.sh inside it (docs/testing-vm.md).

  Run from an elevated PowerShell (Run as administrator):
    powershell -ExecutionPolicy Bypass -File scripts\new-test-vm.ps1

  Nothing is deleted. Running it again reuses the downloaded image and refuses to replace a
  VM that is already there.

.PARAMETER Iso
  An Ubuntu desktop image you already have (any version, e.g. 26.04.1). Skips the download;
  you check its checksum yourself.
    powershell -ExecutionPolicy Bypass -File scripts\new-test-vm.ps1 -Iso D:\Downloads\ubuntu-26.04.1-desktop-amd64.iso

.PARAMETER Folder
  Where the image and the VM's disk go. D:\Dev\VMs by default (inside D:\Dev, but outside
  any repository; about 15 GB once Ubuntu is installed).
#>
param(
  [string]$Iso = "",
  [string]$Name = "AtticARC Linux test",
  [string]$Folder = "D:\Dev\VMs",
  [int]$Processors = 4,
  [long]$MemoryBytes = 8GB,
  [long]$DiskBytes = 60GB
)

$ErrorActionPreference = "Stop"
function Say($text) { Write-Host "`n$text" -ForegroundColor Cyan }
function Ok($text) { Write-Host "  ok  $text" -ForegroundColor Green }
function Stop-With($text) { Write-Host "  !!  $text" -ForegroundColor Red; exit 1 }

Say "1. Hyper-V"
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { Stop-With "Run this from PowerShell opened with 'Run as administrator'." }
if (-not (Get-Command New-VM -ErrorAction SilentlyContinue)) {
  Stop-With ("Hyper-V isn't switched on. Turn it on (needs Windows Pro), restart, and run this again:`n" +
             "      Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All")
}
Ok "Hyper-V is on"
if (Get-VM -Name $Name -ErrorAction SilentlyContinue) {
  Stop-With "A VM called '$Name' is already there. Open it from Hyper-V Manager, or give another -Name."
}
$switch = Get-VMSwitch -Name "Default Switch" -ErrorAction SilentlyContinue
if (-not $switch) { Stop-With "Hyper-V's 'Default Switch' (the network) isn't there; make one in Hyper-V Manager → Virtual Switch Manager." }
Ok "network: Default Switch"

if ($Iso) {
  Say "2. The Ubuntu image"
  if (-not (Test-Path -LiteralPath $Iso -PathType Leaf)) { Stop-With "$Iso isn't there (a path with spaces needs quotes)" }
  if ($Iso -notmatch '\.iso$') { Stop-With "$Iso isn't an .iso" }
  $isoPath = (Resolve-Path -LiteralPath $Iso).Path
  Ok "using $isoPath (check its checksum against Ubuntu's SHA256SUMS yourself)"
} else {
  Say "2. The Ubuntu 24.04 Desktop image"
  $isoDir = Join-Path $Folder "iso"
  New-Item -ItemType Directory -Force $isoDir | Out-Null
  $base = "https://releases.ubuntu.com/noble"
  $list = (Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS").Content
  if ($list -is [byte[]]) { $list = [Text.Encoding]::ASCII.GetString($list) }
  # Newest first: 24.04.5.1 before 24.04.5 before 24.04.4.
  $sums = $list -split "`n" |
    Where-Object { $_ -match '^([0-9a-f]{64}) \*?(ubuntu-[0-9.]+-desktop-amd64\.iso)\s*$' } |
    ForEach-Object {
      $parts = (($Matches[2] -replace '^ubuntu-|-desktop-amd64\.iso$', '').Split('.') + @('0', '0', '0'))[0..3]
      [pscustomobject]@{ Hash = $Matches[1]; File = $Matches[2]; Version = [version]($parts -join '.') }
    } |
    Sort-Object Version -Descending
  if (-not $sums) { Stop-With "couldn't read the image list at $base/SHA256SUMS" }
  $image = $sums[0]
  $isoPath = Join-Path $isoDir $image.File
  if (Test-Path $isoPath) {
    Ok "already downloaded: $($image.File)"
  } else {
    Write-Host "  Downloading $($image.File) (about 6 GB) to $isoDir ..."
    Start-BitsTransfer -Source "$base/$($image.File)" -Destination $isoPath -DisplayName "Ubuntu image"
  }
  Write-Host "  Checking it against Ubuntu's checksum ..."
  $got = (Get-FileHash -Algorithm SHA256 $isoPath).Hash.ToLower()
  if ($got -ne $image.Hash) {
    Stop-With "$($image.File) doesn't match Ubuntu's checksum (a broken download). Delete it and run this again."
  }
  Ok "$($image.File) is intact"
}

Say "3. The VM"
$vmDir = Join-Path $Folder "vms"
New-Item -ItemType Directory -Force $vmDir | Out-Null
$vhd = Join-Path $vmDir "$Name.vhdx"
if (Test-Path $vhd) { Stop-With "$vhd is already there, from an earlier VM. Move it away first; nothing was changed." }
$vm = New-VM -Name $Name -Generation 2 -MemoryStartupBytes $MemoryBytes -NewVHDPath $vhd -NewVHDSizeBytes $DiskBytes -SwitchName "Default Switch" -Path $vmDir
Set-VMProcessor -VM $vm -Count $Processors
Set-VMMemory -VM $vm -DynamicMemoryEnabled $true -MinimumBytes 2GB -StartupBytes $MemoryBytes -MaximumBytes ([math]::Max($MemoryBytes, 12GB))
# Linux needs the Microsoft UEFI CA template, not the Windows one, to boot with Secure Boot on.
Set-VMFirmware -VM $vm -EnableSecureBoot On -SecureBootTemplate "MicrosoftUEFICertificateAuthority"
$dvd = Add-VMDvdDrive -VM $vm -Path $isoPath -Passthru
Set-VMFirmware -VM $vm -FirstBootDevice $dvd
# Checkpoints by hand only (one after Ubuntu is installed is worth keeping).
Set-VM -VM $vm -AutomaticCheckpointsEnabled $false -CheckpointType Standard
Ok "'$Name': $Processors processors, $([math]::Round($MemoryBytes/1GB)) GB memory, $([math]::Round($DiskBytes/1GB)) GB disk at $vhd"

Say "4. Starting it"
Start-VM -VM $vm
vmconnect.exe localhost $Name
Write-Host @"

  In the window that opens: Install Ubuntu (default choices are fine; tick 'Install third-party
  software'). When it asks to remove the install medium, just press Enter.

  Then in Hyper-V Manager: right-click the VM → Checkpoint, and name it 'Fresh Ubuntu', so you
  can always go back to a clean machine.

  Inside Ubuntu, open Terminal and run:
    curl -fsSL https://raw.githubusercontent.com/pinnet/AtticArc/main/scripts/linux-test.sh | bash

  (docs/testing-vm.md has the rest.)
"@
