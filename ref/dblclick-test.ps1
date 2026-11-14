$f = Join-Path $env:TEMP 'dblclick-test.md'
Remove-Item "$env:LOCALAPPDATA\mdv\cache\dblclick-test-*.html" -Force -ErrorAction SilentlyContinue

$sw = [Diagnostics.Stopwatch]::StartNew()
Start-Process -FilePath $f           # 等价于资源管理器里双击
$title = $null
while ($sw.ElapsedMilliseconds -lt 8000) {
  foreach ($p in Get-Process msedge -ErrorAction SilentlyContinue) {
    if ($p.MainWindowTitle -like '*dblclick-test*') { $title = $p.MainWindowTitle; $p.CloseMainWindow() | Out-Null; break }
  }
  if ($title) { break }
  Start-Sleep -Milliseconds 25
}
$sw.Stop()
if ($title) { "OK: 窗口出现 $($sw.ElapsedMilliseconds) ms | 标题: $title" }
else {
  "FAIL: 8 秒内没看到窗口。当前 msedge 窗口标题："
  Get-Process msedge -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowTitle } | Select-Object -First 5 -ExpandProperty MainWindowTitle
}
