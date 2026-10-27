# Installs (or with -Uninstall removes) the DisplaySwarm WinUSB driver package for
# the Android accessory interface (18D1:2D00 / 18D1:2D01). Run elevated; normally
# started by `displayswarm-hostd --install-usb-driver`, which asks for the UAC
# prompt and extracts this folder to a temporary directory.
#
# What it does, and why (see host/src/transport/usb_driver.rs for the full story):
#   1. makes a one-off self-signed *code signing* certificate whose private key is
#      NON-EXPORTABLE and is deleted again before this script ends,
#   2. signs a catalog covering displayswarm-usb.inf with it (Windows x64 refuses
#      even an INF-only third-party driver package without a signed catalog),
#   3. trusts that certificate (public part only) in the machine's Root and
#      TrustedPublisher stores, which is what lets Windows accept the catalog,
#   4. installs the package with pnputil. winusb.sys itself is Microsoft's.
# Because the key is destroyed, the trusted certificate cannot sign anything else.
# The certificate stays so the driver keeps validating when the phone is replugged;
# `-Uninstall` removes the certificate and the driver package.
param(
    [Parameter(Mandatory)][string]$Dir,
    [switch]$Uninstall,
    [string]$LogFile
)
$ErrorActionPreference = 'Stop'
$Subject = 'CN=DisplaySwarm USB driver (WinUSB for Android accessory; key destroyed after signing)'
$InfName = 'displayswarm-usb.inf'

if ($LogFile) { Start-Transcript -Path $LogFile -Force | Out-Null }
function Remove-TrustedCerts {
    foreach ($store in 'Root', 'TrustedPublisher') {
        Get-ChildItem "Cert:\LocalMachine\$store" |
            Where-Object { $_.Subject -eq $Subject } |
            ForEach-Object { Remove-Item $_.PSPath -Force; "removed certificate $($_.Thumbprint) from $store" }
    }
}

try {
    $principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'must run as administrator'
    }

    if ($Uninstall) {
        # pnputil lists "Published Name" then "Original Name" per package.
        $published = $null
        foreach ($line in (pnputil /enum-drivers)) {
            if ($line -match 'Published Name:\s*(\S+)') { $published = $Matches[1] }
            elseif ($line -match "Original Name:\s*$([regex]::Escape($InfName))") {
                pnputil /delete-driver $published /uninstall /force
            }
        }
        Remove-TrustedCerts
        'uninstalled'
        return
    }

    $work = Join-Path ([IO.Path]::GetTempPath()) ("displayswarm-usb-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $work | Out-Null
    Copy-Item (Join-Path $Dir $InfName) $work
    $inf = Join-Path $work $InfName
    $cat = Join-Path $work 'displayswarm-usb.cat'

    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $Subject `
        -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 `
        -KeyExportPolicy NonExportable -NotAfter (Get-Date).AddYears(10) `
        -CertStoreLocation Cert:\CurrentUser\My
    try {
        New-FileCatalog -Path $inf -CatalogFilePath $cat -CatalogVersion 2 | Out-Null
        $sig = Set-AuthenticodeSignature -FilePath $cat -Certificate $cert -HashAlgorithm SHA256
        if (-not $sig.SignerCertificate) { throw "signing failed: $($sig.StatusMessage)" }

        $cer = Join-Path $work 'signer.cer'
        Export-Certificate -Cert $cert -FilePath $cer | Out-Null
        foreach ($store in 'Root', 'TrustedPublisher') {
            Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
        }
    } finally {
        # Destroy the private key whatever happened above.
        Remove-Item "Cert:\CurrentUser\My\$($cert.Thumbprint)" -DeleteKey -Force -ErrorAction SilentlyContinue
    }

    try {
        pnputil /add-driver $inf /install
        if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 3010) { throw "pnputil failed with exit code $LASTEXITCODE" }
    } catch {
        Remove-TrustedCerts
        throw
    }
    pnputil /scan-devices | Out-Null
    'installed'
} catch {
    "ERROR: $_"
    if ($LogFile) { Stop-Transcript | Out-Null }
    exit 1
}
if ($LogFile) { Stop-Transcript | Out-Null }
exit 0
