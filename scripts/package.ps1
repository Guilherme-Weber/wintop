<#
.SYNOPSIS
    Monta o pacote de distribuição do wintop (zip + SHA-256), o mesmo na máquina e na CI.

.DESCRIPTION
    Compila em release (a menos que -NoBuild), confere o módulo do PawnIO contra o SHA-256
    documentado em modules/README.md e monta dist\wintop-<versão>-windows-x64.zip com:
      wintop.exe, modules\ (binário LGPL, licença, README e código-fonte), LICENSE, README.md,
      CHANGELOG.md e THIRD-PARTY-NOTICES.txt (cargo-about).
    O exe é compilado sem a pasta do usuário nos caminhos e o zip é determinístico (o mesmo exe
    dá o mesmo zip). -RequireNotes (a release) falha se o CHANGELOG não tiver a seção da versão.
    Ao lado do zip, wintop-<versão>-windows-x64.zip.sha256 (formato do sha256sum).

.EXAMPLE
    .\scripts\package.ps1
    .\scripts\package.ps1 -NoBuild
#>
[CmdletBinding()]
param(
    [switch]$NoBuild,
    [switch]$RequireNotes
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw 'versão não encontrada no Cargo.toml' }

# Caminhos das dependências (mensagens de pânico) sem a pasta do usuário ou do runner: o
# `--config` soma ao rustflags do .cargo/config.toml (crt-static), não substitui.
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
if (-not $NoBuild) {
    cargo build --release --locked --config "target.x86_64-pc-windows-msvc.rustflags=['--remap-path-prefix=$cargoHome=cargo']"
    if ($LASTEXITCODE -ne 0) { throw 'cargo build falhou' }
}
$exe = Join-Path $root 'target\release\wintop.exe'
if (-not (Test-Path $exe)) { throw "não encontrado: $exe" }
$text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($exe))
if ($text.Contains($cargoHome)) { throw "o exe contém o caminho $cargoHome (compilado sem o remapeamento?)" }
if ($text -match '(?i)vcruntime140\.dll') { throw 'o exe depende do vcruntime140.dll (sem crt-static?)' }
# --version não precisa de elevação: sem isto, o manifesto pediria UAC (terminal comum).
$env:__COMPAT_LAYER = 'RunAsInvoker'
try { $reported = & $exe --version } finally { Remove-Item Env:__COMPAT_LAYER }
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
Copy-Item LICENSE, README.md, CHANGELOG.md, SECURITY.md $stage
# Avisos de licença das crates ligadas no exe (MIT/Apache exigem os avisos na distribuição
# binária). Offline e com falha em licença não reconhecida (about.toml).
# O `cargo metadata` dele precisa de todas as crates do Cargo.lock baixadas (não só as compiladas).
cargo fetch --locked
if ($LASTEXITCODE -ne 0) { throw 'cargo fetch falhou' }
cargo about generate --frozen --fail about.hbs -o (Join-Path $stage 'THIRD-PARTY-NOTICES.txt')
if ($LASTEXITCODE -ne 0) { throw 'cargo about falhou (instalar: cargo install cargo-about --locked --features cli)' }

$zip = Join-Path $dist "$name.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
# Zip determinístico: entradas em ordem fixa, barras normais e a data do commit em todas (o mesmo
# exe dá o mesmo zip, na máquina ou na CI).
$stamp = [DateTimeOffset]::FromUnixTimeSeconds([long](git log -1 --format=%ct))
Add-Type -AssemblyName System.IO.Compression
$fs = [IO.File]::Open($zip, [IO.FileMode]::CreateNew)
try {
    $archive = [IO.Compression.ZipArchive]::new($fs, [IO.Compression.ZipArchiveMode]::Create)
    # Ordem ordinal (byte a byte): não depende da cultura da máquina (o -Culture do Sort-Object
    # não aceita a invariante).
    $files = [IO.FileInfo[]](Get-ChildItem $stage -Recurse -File)
    $keys = [string[]]($files | ForEach-Object { $_.FullName.Substring($stage.Length + 1).Replace('\', '/') })
    [Array]::Sort($keys, $files, [StringComparer]::Ordinal)
    foreach ($f in $files) {
        $rel = "$name/" + $f.FullName.Substring($stage.Length + 1).Replace('\', '/')
        $entry = $archive.CreateEntry($rel, [IO.Compression.CompressionLevel]::Optimal)
        $entry.LastWriteTime = $stamp
        $out = $entry.Open()
        try {
            $in = [IO.File]::OpenRead($f.FullName)
            try { $in.CopyTo($out) } finally { $in.Dispose() }
        } finally { $out.Dispose() }
    }
    $archive.Dispose()
} finally { $fs.Dispose() }
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

O executável não tem assinatura Authenticode: o Windows pode mostrar o aviso do SmartScreen
("Mais informações" > "Executar assim mesmo"; em inglês, "More info" > "Run anyway") e o UAC
mostra editor desconhecido ("Unknown publisher").
Confira o download antes:

``````powershell
(Get-FileHash $name.zip -Algorithm SHA256).Hash   # igual ao $name.zip.sha256
gh attestation verify $name.zip -R Guilherme-Weber/wintop
``````
"@
    [IO.File]::WriteAllText($notes, $section.Groups[1].Value.Trim() + "`n" + $verify)
    Write-Host "notas: $notes"
} elseif ($RequireNotes) {
    throw "CHANGELOG sem a seção [$version]: renomeie [Não lançado] para [$version] - <data>"
} else {
    Write-Warning "CHANGELOG sem a seção [$version]: sem notas da release"
}

Write-Host "pacote: $zip"
Write-Host "sha256: $hash"
Get-ChildItem $stage -Recurse -File | ForEach-Object {
    '  {0} ({1:n0} bytes)' -f $_.FullName.Substring($stage.Length + 1), $_.Length
}
