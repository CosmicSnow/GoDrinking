# Coleta completa de logs do repro "Windows transmite, ninguem ve" (PASSO 1).
# - Roda os testes automatizados do repro com --nocapture (evidencia no stdout).
# - Copia os session logs do app (%APPDATA%\godrinking\logs) para um diretorio de coleta.
# Uso: powershell -ExecutionPolicy Bypass -File scripts/collect-windows-repro-logs.ps1
$ErrorActionPreference = "Continue"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$out = Join-Path ([System.IO.Path]::GetTempPath()) "godrinking-repro-$stamp"
New-Item -ItemType Directory -Path $out -Force | Out-Null

Write-Host "== 1/2 cargo test win_stunar_repro -- --nocapture =="
$testLog = Join-Path $out "cargo-test-win_stunar_repro.log"
Set-Location (Join-Path $root "src-tauri")
cargo test win_stunar_repro -- --nocapture 2>&1 | Tee-Object -FilePath $testLog
Write-Host "teste gravado em $testLog"

Write-Host "== 2/2 copiando session logs do app =="
$appLogs = Join-Path $env:APPDATA "godrinking\logs"
if (Test-Path -LiteralPath $appLogs) {
  Copy-Item -Recurse -Force -LiteralPath $appLogs -Destination (Join-Path $out "app-logs")
  Get-ChildItem -LiteralPath (Join-Path $out "app-logs") | Select-Object Name, Length, LastWriteTime | Format-Table | Out-String | Tee-Object -FilePath (Join-Path $out "app-logs-manifest.log")
} else {
  "SEM app-logs em $appLogs (app ainda nao rodou nesta maquina)" | Out-File -FilePath (Join-Path $out "app-logs-manifest.log")
}
Write-Host "COLETA COMPLETA EM: $out"
