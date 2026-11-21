<#
.SYNOPSIS
    Teste de longa duração do wintop (Etapa 8): amostra o consumo do wintop e do host do console
    e avalia a tendência (vazamento de memória ou de handles, CPU acima da meta).

.DESCRIPTION
    Rodar num terminal elevado (o wintop é elevado: sem elevação o tempo de CPU dele não é
    legível), com o wintop já aberto. A cada -IntervalSec, grava uma linha no CSV com o wintop e
    o host do console dele (conhost/OpenConsole filho do wintop). Ctrl+C encerra a coleta e
    imprime o resumo; -Summary <csv> só resume um CSV já gravado.

    Critérios (ignorando os primeiros -WarmupMin minutos: gráficos e históricos enchendo):
      - memória privada do wintop: crescimento < 1 MB/h (regressão linear);
      - handles do wintop: crescimento < 20/h;
      - CPU média (wintop + host) < 1% (meta interna 0,3%);
      - o wintop não fechou antes do fim, o host do console foi medido e há pelo menos 3
        amostras depois do aquecimento.
    Saída 0 = OK, 1 = falha.

.EXAMPLE
    .\scripts\soak.ps1 -Hours 8
    .\scripts\soak.ps1 -Summary .\soak-20261121-2300.csv
#>
[CmdletBinding()]
param(
    [double]$Hours = 8,
    [int]$IntervalSec = 60,
    [int]$WarmupMin = 10,
    [string]$Out = "",
    [string]$Summary = ""
)

$ErrorActionPreference = 'Stop'

function Get-Host-Of([int]$ProcessId) {
    Get-CimInstance Win32_Process -Filter "ParentProcessId = $ProcessId" |
        Where-Object { $_.Name -in 'conhost.exe', 'OpenConsole.exe' } |
        Select-Object -First 1
}

# Inclinação (por hora) da regressão linear de y sobre t (horas).
function Get-Slope([double[]]$t, [double[]]$y) {
    $n = $t.Count
    if ($n -lt 3) { return [double]::NaN }
    $mt = ($t | Measure-Object -Average).Average
    $my = ($y | Measure-Object -Average).Average
    $num = 0.0; $den = 0.0
    for ($i = 0; $i -lt $n; $i++) {
        $num += ($t[$i] - $mt) * ($y[$i] - $my)
        $den += ($t[$i] - $mt) * ($t[$i] - $mt)
    }
    if ($den -eq 0) { return [double]::NaN }
    $num / $den
}

function Show-Summary([string]$Csv, [string[]]$Problems = @()) {
    $rows = @(Import-Csv $Csv)
    if ($rows.Count -eq 0) { Write-Host "sem amostras em $Csv"; return $false }
    $inv = [Globalization.CultureInfo]::InvariantCulture
    $num = { param($v) [double]::Parse($v, $inv) }
    $all = @($rows | ForEach-Object {
        [pscustomobject]@{
            H       = (& $num $_.elapsed_h)
            Cpu     = (& $num $_.cpu_total_pct)
            Priv    = (& $num $_.wintop_private_mb)
            Ws      = (& $num $_.wintop_ws_mb)
            Handles = (& $num $_.wintop_handles)
            Threads = (& $num $_.wintop_threads)
            HostPid = $_.host_pid
        }
    })
    $problems = @($Problems)
    $steady = @($all | Where-Object { $_.H -ge $WarmupMin / 60 })
    $afterWarmup = $steady.Count
    if ($steady.Count -lt 3) {
        $problems += "só $($steady.Count) amostras depois do aquecimento (mínimo 3)"
        $steady = @($all)
    }
    if (@($all | Where-Object { -not $_.HostPid }).Count -gt 0) {
        $problems += 'host do console não medido em parte das amostras (CPU só do wintop)'
    }
    $t = [double[]]($steady | ForEach-Object H)
    $privSlope = Get-Slope $t ([double[]]($steady | ForEach-Object Priv))
    $hSlope = Get-Slope $t ([double[]]($steady | ForEach-Object Handles))
    $cpu = $steady | Measure-Object Cpu -Average -Maximum
    $priv = $all | Measure-Object Priv -Minimum -Maximum
    $last = $all[-1]

    $fmt = { param($v, $d) $v.ToString("F$d", $inv) }
    Write-Host ""
    Write-Host ("duração: {0} h, {1} amostras ({2} após o aquecimento de {3} min)" -f `
        (& $fmt $last.H 2), $all.Count, $afterWarmup, $WarmupMin)
    Write-Host ("CPU (wintop + host): média {0}%, pico {1}%" -f (& $fmt $cpu.Average 3), (& $fmt $cpu.Maximum 3))
    Write-Host ("memória privada: {0}-{1} MB, tendência {2} MB/h" -f `
        (& $fmt $priv.Minimum 1), (& $fmt $priv.Maximum 1), (& $fmt $privSlope 3))
    Write-Host ("handles: {0} no fim, tendência {1}/h; threads: {2}" -f `
        $last.Handles, (& $fmt $hSlope 1), $last.Threads)
    if (-not ($privSlope -lt 1)) { $problems += 'memória privada crescendo (>= 1 MB/h)' }
    if (-not ($hSlope -lt 20)) { $problems += 'handles crescendo (>= 20/h)' }
    if (-not ($cpu.Average -lt 1)) { $problems += 'CPU média >= 1%' }
    foreach ($p in $problems) { Write-Host "  - $p" }
    $ok = $problems.Count -eq 0
    Write-Host ("resultado: {0}" -f $(if ($ok) { 'OK' } else { 'FALHA' }))
    $ok
}

if ($Summary) { if (Show-Summary $Summary) { exit 0 } else { exit 1 } }

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { throw "rodar num terminal elevado (o wintop é elevado)" }

$w = @(Get-Process wintop -ErrorAction SilentlyContinue)
if ($w.Count -ne 1) { throw "abra o wintop (uma instância) antes; encontradas: $($w.Count)" }
$w = $w[0]
$hostInfo = Get-Host-Of $w.Id
$hostProc = if ($hostInfo) { Get-Process -Id $hostInfo.ProcessId } else { $null }
if (-not $Out) { $Out = Join-Path (Get-Location) ("soak-{0}.csv" -f (Get-Date -Format 'yyyyMMdd-HHmm')) }

Write-Host ("wintop PID {0}; host {1}; CSV {2}; Ctrl+C encerra e resume" -f `
    $w.Id, $(if ($hostProc) { "$($hostProc.ProcessName) PID $($hostProc.Id)" } else { 'não encontrado (Windows Terminal?)' }), $Out)
"time,elapsed_h,cpu_total_pct,wintop_cpu_pct,host_cpu_pct,wintop_private_mb,wintop_ws_mb,wintop_handles,wintop_threads,host_pid,host_private_mb" |
    Set-Content -Encoding utf8 $Out

$inv = [Globalization.CultureInfo]::InvariantCulture
$cores = [Environment]::ProcessorCount
# Relógio monotônico: NTP e horário de verão não mexem nos intervalos.
$clock = [Diagnostics.Stopwatch]::StartNew()
$problems = @()
$prev = @{ T = $clock.Elapsed; W = $w.TotalProcessorTime; H = $(if ($hostProc) { $hostProc.TotalProcessorTime } else { [TimeSpan]::Zero }) }
try {
    while ($clock.Elapsed.TotalHours -lt $Hours) {
        Start-Sleep -Seconds $IntervalSec
        $w.Refresh()
        if ($w.HasExited) { $problems += 'o wintop fechou antes do fim'; Write-Host 'o wintop fechou'; break }
        $hostAlive = $hostProc -and -not $hostProc.HasExited
        if ($hostAlive) { $hostProc.Refresh() }
        # Cada contador lido uma vez por amostra (a mesma leitura vira a base da próxima).
        $now = $clock.Elapsed
        $wCpu = $w.TotalProcessorTime
        $hCpu = if ($hostAlive) { $hostProc.TotalProcessorTime } else { $prev.H }
        $dt = ($now - $prev.T).TotalSeconds
        if ($dt -le 0) { continue }
        $wall = $dt * $cores
        $wPct = 100 * ($wCpu - $prev.W).TotalSeconds / $wall
        $hPct = 100 * ($hCpu - $prev.H).TotalSeconds / $wall
        $hostPriv = if ($hostAlive) { $hostProc.PrivateMemorySize64 / 1MB } else { 0 }
        $line = @(
            (Get-Date).ToString('s'),
            $now.TotalHours.ToString('F4', $inv),
            ($wPct + $hPct).ToString('F4', $inv),
            $wPct.ToString('F4', $inv),
            $hPct.ToString('F4', $inv),
            ($w.PrivateMemorySize64 / 1MB).ToString('F2', $inv),
            ($w.WorkingSet64 / 1MB).ToString('F2', $inv),
            $w.HandleCount,
            $w.Threads.Count,
            $(if ($hostAlive) { $hostProc.Id } else { '' }),
            $hostPriv.ToString('F2', $inv)
        ) -join ','
        Add-Content -Encoding utf8 $Out $line
        $prev = @{ T = $now; W = $wCpu; H = $hCpu }
    }
}
finally {
    $ok = Show-Summary $Out $problems
}
if ($ok) { exit 0 } else { exit 1 }
