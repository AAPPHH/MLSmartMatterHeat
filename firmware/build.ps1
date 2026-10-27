param([string]$Port = "")
$Idf = if ($env:IDF_PATH) { $env:IDF_PATH } else { "C:\clones\esp\esp-idf" }
. "$Idf\export.ps1"
$Defaults = "sdkconfig.defaults;sdkconfig.ethernet"
function Step($Name) { if ($LASTEXITCODE -ne 0) { Pop-Location; throw "$Name failed" } }
Push-Location "$Idf\examples\openthread\ot_rcp"
idf.py set-target esp32h2; Step "ot_rcp set-target"
idf.py build; Step "ot_rcp build"
Pop-Location
Push-Location "$PSScriptRoot\br"
if (Test-Path build) { Remove-Item -Recurse -Force build }
if (Test-Path sdkconfig) { Remove-Item -Force sdkconfig }
idf.py "-DSDKCONFIG_DEFAULTS=$Defaults" set-target esp32s3; Step "br set-target"
idf.py "-DSDKCONFIG_DEFAULTS=$Defaults" build; Step "br build"
Select-String -Path sdkconfig -Pattern '^CONFIG_(OPENTHREAD_BR_AUTO_START|EXAMPLE_CONNECT_ETHERNET|EXAMPLE_USE_W5500|AUTO_UPDATE_RCP)='
if ($Port) {
    idf.py -p $Port erase-flash flash; Step "flash"
}
Pop-Location
