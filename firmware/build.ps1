param([ValidateSet("wifi", "ethernet")][string]$Variant = "wifi", [string]$Port = "")
$Idf = if ($env:IDF_PATH) { $env:IDF_PATH } else { "C:\clones\esp\esp-idf" }
. "$Idf\export.ps1"
function Step($Name) { if ($LASTEXITCODE -ne 0) { Pop-Location; throw "$Name failed" } }
Push-Location "$Idf\examples\openthread\ot_rcp"
idf.py set-target esp32h2; Step "ot_rcp set-target"
idf.py build; Step "ot_rcp build"
Pop-Location
Push-Location "$PSScriptRoot\br"
$Build = "build-$Variant"
$IdfArgs = @("-B", $Build, "-DSDKCONFIG=$Build/sdkconfig", "-DSDKCONFIG_DEFAULTS=sdkconfig.defaults;sdkconfig.$Variant$(if (Test-Path "sdkconfig.$Variant.local") { ";sdkconfig.$Variant.local" })")
if (Test-Path $Build) { Remove-Item -Recurse -Force $Build }
idf.py @IdfArgs set-target esp32s3; Step "br set-target"
idf.py @IdfArgs build; Step "br build"
Select-String -Path "$Build/sdkconfig" -Pattern '^CONFIG_(OPENTHREAD_BR_AUTO_START|EXAMPLE_CONNECT_ETHERNET|EXAMPLE_CONNECT_WIFI|OPENTHREAD_BR_SOFTAP_SETUP|AUTO_UPDATE_RCP)='
if ($Port) {
    idf.py @IdfArgs -p $Port erase-flash flash; Step "flash"
}
Pop-Location
