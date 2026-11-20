# Скачивает модели рядом с exe (sherpa-onnx): CED-small — звуки (AudioSet, ~22 МБ),
# Zipformer (vosk-model-ru) — русская речь (~73 МБ). Уже скачанные пропускаются.
# Запуск:  powershell -ExecutionPolicy Bypass -File scripts\fetch-model.ps1 [-Dest папка_с_exe]
param([string]$Dest = (Join-Path $PSScriptRoot "..\target\release"))

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$base = "https://github.com/k2-fsa/sherpa-onnx/releases/download"
$models = @(
    @{ Dir = "ced-small"; Name = "sherpa-onnx-ced-small-audio-tagging-2024-04-19"; Tag = "audio-tagging-models"
       Files = "model.int8.onnx", "class_labels_indices.csv"; Check = "model.int8.onnx" },
    @{ Dir = "zipformer-ru"; Name = "sherpa-onnx-zipformer-ru-int8-2025-04-20"; Tag = "asr-models"
       Files = "encoder.int8.onnx", "decoder.onnx", "joiner.int8.onnx", "tokens.txt"; Check = "encoder.int8.onnx" }
)

New-Item -ItemType Directory -Force -Path $Dest | Out-Null
$Dest = (Resolve-Path $Dest).Path
foreach ($m in $models) {
    $target = Join-Path $Dest "models\$($m.Dir)"
    if (Test-Path (Join-Path $target $m.Check)) {
        Write-Host "Модель уже есть: $target"
        continue
    }
    $tmp = Join-Path $env:TEMP "vox-model-$($m.Dir)"
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    $archive = Join-Path $tmp "model.tar.bz2"
    Write-Host "Скачиваю $($m.Name) ..."
    Invoke-WebRequest "$base/$($m.Tag)/$($m.Name).tar.bz2" -OutFile $archive
    tar -xjf $archive -C $tmp
    if ($LASTEXITCODE -ne 0) { throw "tar не смог распаковать архив" }
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    Copy-Item ($m.Files | ForEach-Object { Join-Path $tmp "$($m.Name)\$_" }) $target -Force
    Remove-Item -Recurse -Force $tmp
    Write-Host "Готово: $target"
}
