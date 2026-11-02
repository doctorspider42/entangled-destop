# Test-signs a build of entangled-gpu (ADR-0009): a catalog with Inf2Cat and
# signatures with signtool, under a self-signed code-signing certificate kept
# as a file in the output directory (created once, removed from the host's
# certificate store right after). The guest needs test-signing on and the
# .cer in its Root and TrustedPublisher stores.
# Usage: powershell -File sign.ps1 <output directory>
param([Parameter(Mandatory)][string]$Out)
$ErrorActionPreference = 'Stop'
$kit = 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0'
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
foreach ($file in 'entangled-gpu.sys', 'entangled-gpu.cat') {
    & "$kit\x64\signtool.exe" sign /fd sha256 /f $pfx /p 'entangled-test' (Join-Path $Out $file) | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "signtool failed on $file ($LASTEXITCODE)" }
}
Write-Output "signed $Out"
