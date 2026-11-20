# Собирает установщики: release-сборка + модели звуков и слов + Inno Setup →
#   dist\Vox-Setup-<версия>.exe   — полный, с моделями (для первой установки);
#   dist\Vox-Update-<версия>.exe  — без моделей (его качает автообновление).
# Запуск:  powershell -ExecutionPolicy Bypass -File scripts\build-installer.ps1
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $root

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Не найден Inno Setup 6 (ISCC.exe). Установите: winget install JRSoftware.InnoSetup --scope user" }

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$rcVersion = (Select-String -Path assets\app.rc -Pattern 'VALUE "FileVersion", "(.+)"').Matches[0].Groups[1].Value
if ($rcVersion -ne $version) { throw "Версия в assets\app.rc ($rcVersion) не совпадает с Cargo.toml ($version)" }

Write-Host "Сборка release $version ..."
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build завершился с ошибкой" }

foreach ($m in "ced-small", "zipformer-ru") {
    $model = Join-Path $root "models\$m"
    $dest = Join-Path $root "target\release\models\$m"
    if (Test-Path (Join-Path $model "model.int8.onnx")) {
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        Copy-Item (Join-Path $model "*") $dest -Force
    }
}
# чего нет в папке models — скачает
& (Join-Path $PSScriptRoot "fetch-model.ps1") -Dest (Join-Path $root "target\release")

foreach ($kind in @("Setup", "Update")) {
    Write-Host "Inno Setup: $kind ..."
    $defs = @("/DAppVersion=$version")
    if ($kind -eq "Update") { $defs += "/DUpdateOnly" }
    & $iscc /Q @defs "installer\Vox.iss"
    if ($LASTEXITCODE -ne 0) { throw "ISCC завершился с ошибкой" }
    $out = Get-Item "dist\Vox-$kind-$version.exe"
    Write-Host ("Готово: {0} ({1:N1} МБ)" -f $out.FullName, ($out.Length / 1MB))
}
