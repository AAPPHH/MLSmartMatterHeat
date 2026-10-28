param([string]$Ip = "192.168.178.21")
$adb = (Get-Command adb -ErrorAction SilentlyContinue).Source
if (-not $adb) { $adb = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\Google.PlatformTools*\platform-tools\adb.exe" | Select-Object -First 1 -ExpandProperty FullName }
$fixed = "${Ip}:5555"
if ((& $adb connect $fixed) -match "^connected") { $fixed; exit 0 }
$tls = & $adb mdns services | Select-String "_adb-tls-connect\._tcp\s+(\S+)" | ForEach-Object { $_.Matches[0].Groups[1].Value } | Select-Object -First 1
if (-not $tls) { throw "Uhr nicht gefunden: WLAN an und Kabelloses Debugging aktiv?" }
& $adb connect $tls | Out-Null
& $adb -s $tls tcpip 5555 | Out-Null
Start-Sleep 4
if ((& $adb connect $fixed) -match "^connected") { $fixed } else { $tls }
