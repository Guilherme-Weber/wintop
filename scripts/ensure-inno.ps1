<#
.SYNOPSIS
    Garante o Inno Setup 6 para o package.ps1 (CI e máquina local).

.DESCRIPTION
    Usa o ISCC.exe já instalado (as imagens Windows do GitHub costumam trazer); sem ele, baixa o
    instalador oficial da versão fixada, confere o SHA-256 e a assinatura Authenticode antes de
    executar e instala em modo silencioso. Sempre registra qual ISCC será usado, para a versão
    do compilador do setup ficar no log da release.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

# Versão fixada: o hash e o assinante são conferidos antes de executar o instalador.
$version = '6.7.3'
$url = "https://github.com/jrsoftware/issrc/releases/download/is-$($version.Replace('.', '_'))/innosetup-$version.exe"
$sha256 = '9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732'
$signer = 'CN=Pyrsys B.V.'

$iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
if (-not (Test-Path $iscc)) {
    $dir = Join-Path ([IO.Path]::GetTempPath()) "inno-$([guid]::NewGuid())"
    New-Item -ItemType Directory $dir | Out-Null
    $exe = Join-Path $dir "innosetup-$version.exe"
    Invoke-WebRequest $url -OutFile $exe -UseBasicParsing
    $actual = (Get-FileHash $exe -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $sha256) { throw "innosetup-$version.exe com SHA-256 $actual (esperado: $sha256)" }
    $sig = Get-AuthenticodeSignature $exe
    if ($sig.Status -ne 'Valid' -or -not $sig.SignerCertificate.Subject.StartsWith($signer)) {
        throw "assinatura inválida do instalador do Inno Setup: $($sig.Status) $($sig.SignerCertificate.Subject)"
    }
    $p = Start-Process $exe -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', '/ALLUSERS' -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "instalador do Inno Setup saiu com $($p.ExitCode)" }
    Remove-Item $dir -Recurse -Force
    if (-not (Test-Path $iscc)) { throw "Inno Setup instalado, mas $iscc não existe" }
}
# O ISCC.exe não traz versão no recurso do executável: vem da chave de desinstalação.
$keys = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1',
    'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1'
$v = $keys | ForEach-Object { (Get-ItemProperty $_ -ErrorAction SilentlyContinue).DisplayVersion } |
    Where-Object { $_ } | Select-Object -First 1
if (-not $v) { $v = 'versão desconhecida' }
Write-Host "Inno Setup: $iscc ($v)"
