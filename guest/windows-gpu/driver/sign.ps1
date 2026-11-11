# Test-signs a build of entangled-gpu (ADR-0009): a catalog with Inf2Cat and
# signatures with signtool, under a self-signed code-signing certificate kept
# as a file in the output directory (created once, removed from the host's
# certificate store right after). The guest needs test-signing on and the
# .cer in its Root and TrustedPublisher stores.
# Usage: powershell -File sign.ps1 <output directory>
#
# CI (windows-gpu.yml) signs each release under a certificate made for that
# run and deletes the .pfx before publishing: the private key exists nowhere
# afterwards, so trusting the .cer lets no one sign anything else.
param(
    [Parameter(Mandatory)][string]$Out,
    [string]$Kit = 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0'
)
$ErrorActionPreference = 'Stop'
$kit = $Kit
$pfx = Join-Path $Out 'entangled-gpu-test.pfx'
$cer = Join-Path $Out 'entangled-gpu-test.cer'
$password = ConvertTo-SecureString -String 'entangled-test' -Force -AsPlainText
if (-not (Test-Path $pfx)) {
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=Entangled GPU test' `
        -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddYears(5)
    Export-PfxCertificate -Cert $cert -FilePath $pfx -Password $password | Out-Null
    Export-Certificate -Cert $cert -FilePath $cer | Out-Null
    Remove-Item -Path "Cert:\CurrentUser\My\$($cert.Thumbprint)"
}
& "$kit\x86\Inf2Cat.exe" "/driver:$Out" /os:10_X64 /uselocaltime | Out-Null
if ($LASTEXITCODE -ne 0) { throw "Inf2Cat failed ($LASTEXITCODE)" }
# Every driver built into this directory: entangled-gpu, and entangled-display
# (guest/windows-gpu/display) when its build.cmd wrote here too. One
# certificate for both, so a guest trusts one.
foreach ($file in Get-ChildItem -Path $Out -File | Where-Object { $_.Extension -in '.sys', '.cat' }) {
    & "$kit\x64\signtool.exe" sign /fd sha256 /f $pfx /p 'entangled-test' $file.FullName | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "signtool failed on $($file.Name) ($LASTEXITCODE)" }
}
Write-Output "signed $Out"
