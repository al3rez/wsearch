$exe = "C:\Users\User\wsearch\ui\bin\Release\net8.0-windows\WSearch.Ui.exe"
$p = Start-Process -FilePath $exe -PassThru -WindowStyle Normal
Start-Sleep -Seconds 4
if ($null -eq $p) { Write-Output "NO_PROCESS"; exit 1 }
$p.Refresh()
if ($p.HasExited) {
    Write-Output ("EXITED code=" + $p.ExitCode)
    exit 1
}
Write-Output ("PID=" + $p.Id)
Write-Output ("TITLE=" + $p.MainWindowTitle)
Write-Output ("HWND=" + $p.MainWindowHandle)
if ($p.MainWindowHandle -ne [IntPtr]::Zero) { Write-Output "WINDOW_OK" } else { Write-Output "NO_HWND" }
# also confirm via process name
$ui = Get-Process -Name "WSearch.Ui" -ErrorAction SilentlyContinue
if ($ui) { Write-Output ("PROC=" + $ui.ProcessName + " title=" + $ui.MainWindowTitle) }
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
