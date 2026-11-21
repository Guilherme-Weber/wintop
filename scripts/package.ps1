<#
.SYNOPSIS
    Monta o pacote de distribuição do wintop (zip + SHA-256), o mesmo na máquina e na CI.

.DESCRIPTION
    Compila em release (a menos que -NoBuild), confere o módulo do PawnIO contra o SHA-256
    documentado em modules/README.md e monta dist\wintop-<versão>-windows-x64.zip com:
      wintop.exe, modules\ (binário LGPL, licença, README e código-fonte), LICENSE, README.md e
      CHANGELOG.md.
    Ao lado do zip, wintop-<versão>-windows-x64.zip.sha256 (formato do sha256sum).

.EXAMPLE
    .\scripts\package.ps1
    .\scripts\package.ps1 -NoBuild
#>
[CmdletBinding()]
param(
    [switch]$NoBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw 'versão não encontrada no Cargo.toml' }

if (-not $NoBuild) {
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo build falhou' }
}
$exe = Join-Path $root 'target\release\wintop.exe'
if (-not (Test-Path $exe)) { throw "não encontrado: $exe" }
$reported = & $exe --version
if ($reported -ne "wintop $version") { throw "o executável diz '$reported', o Cargo.toml diz $version" }

# O módulo distribuído tem de ser o documentado (binário oficial e assinado, sem modificações).
$doc = Select-String -Path modules\README.md -Pattern 'SHA-256 do binário: `([0-9a-f]{64})`'
$expected = $doc.Matches[0].Groups[1].Value
$actual = (Get-FileHash modules\AMDFamily17.bin -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "AMDFamily17.bin com SHA-256 $actual (documentado: $expected)" }

$name = "wintop-$version-windows-x64"
$dist = Join-Path $root 'dist'
$stage = Join-Path $dist $name
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item $exe $stage
Copy-Item modules (Join-Path $stage 'modules') -Recurse
Copy-Item LICENSE, README.md, CHANGELOG.md $stage

$zip = Join-Path $dist "$name.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
# Formato do sha256sum (LF, sem BOM): `sha256sum -c` confere direto.
[IO.File]::WriteAllText("$zip.sha256", "$hash  $name.zip`n")

# Notas da release: a seção "## [<versão>]" do CHANGELOG (sem ela, sem notas: o workflow de
# release falha de propósito, para a versão não sair sem o que mudou).
$notes = Join-Path $dist 'notes.md'
if (Test-Path $notes) { Remove-Item $notes -Force }
$section = [regex]::Match((Get-Content CHANGELOG.md -Raw),
    "(?ms)^## \[$([regex]::Escape($version))\][^\n]*\n(.*?)(?=^## \[|\z)")
if ($section.Success) {
    $verify = @"

## Instalação e verificação

Extraia o zip, abra um terminal como administrador na pasta extraída e rode
``.\wintop.exe --install`` (copia para ``C:\Program Files\wintop`` e cria o atalho sem UAC no
Menu Iniciar), ou rode ``.\wintop.exe`` direto. Requisitos no README.

Integridade e procedência do download:

``````powershell
(Get-FileHash $name.zip -Algorithm SHA256).Hash   # igual ao $name.zip.sha256
gh attestation verify $name.zip -R Guilherme-Weber/wintop
``````
"@
    [IO.File]::WriteAllText($notes, $section.Groups[1].Value.Trim() + "`n" + $verify)
    Write-Host "notas: $notes"
} else {
    Write-Warning "CHANGELOG sem a seção [$version]: sem notas da release"
}

Write-Host "pacote: $zip"
Write-Host "sha256: $hash"
Get-ChildItem $stage -Recurse -File | ForEach-Object {
    '  {0} ({1:n0} bytes)' -f $_.FullName.Substring($stage.Length + 1), $_.Length
}
