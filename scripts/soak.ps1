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
      - CPU média (wintop + host) < 1% (meta interna 0,3%).

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

function Show-Summary([string]$Csv) {
    $rows = @(Import-Csv $Csv)
    if ($rows.Count -eq 0) { Write-Host "sem amostras em $Csv"; return }
    $inv = [Globalization.CultureInfo]::InvariantCulture
    $num = { param($v) [double]::Parse($v, $inv) }
    $all = $rows | ForEach-Object {
        [pscustomobject]@{
            H       = (& $num $_.elapsed_h)
            Cpu     = (& $num $_.cpu_total_pct)
            Priv    = (& $num $_.wintop_private_mb)
            Ws      = (& $num $_.wintop_ws_mb)
            Handles = (& $num $_.wintop_handles)
            Threads = (& $num $_.wintop_threads)
        }
    }
    $steady = @($all | Where-Object { $_.H -ge $WarmupMin / 60 })
    if ($steady.Count -lt 3) { $steady = @($all) }
    $t = [double[]]($steady | ForEach-Object H)
    $privSlope = Get-Slope $t ([double[]]($steady | ForEach-Object Priv))
    $hSlope = Get-Slope $t ([double[]]($steady | ForEach-Object Handles))
    $cpu = $steady | Measure-Object Cpu -Average -Maximum
    $priv = $all | Measure-Object Priv -Minimum -Maximum
    $last = $all[-1]

    $fmt = { param($v, $d) $v.ToString("F$d", $inv) }
    Write-Host ""
    Write-Host ("duração: {0} h, {1} amostras ({2} após o aquecimento de {3} min)" -f `
        (& $fmt $last.H 2), $all.Count, $steady.Count, $WarmupMin)
    Write-Host ("CPU (wintop + host): média {0}%, pico {1}%" -f (& $fmt $cpu.Average 3), (& $fmt $cpu.Maximum 3))
    Write-Host ("memória privada: {0}-{1} MB, tendência {2} MB/h" -f `
        (& $fmt $priv.Minimum 1), (& $fmt $priv.Maximum 1), (& $fmt $privSlope 3))
    Write-Host ("handles: {0} no fim, tendência {1}/h; threads: {2}" -f `
        $last.Handles, (& $fmt $hSlope 1), $last.Threads)
    $ok = ($privSlope -lt 1) -and ($hSlope -lt 20) -and ($cpu.Average -lt 1)
    Write-Host ("resultado: {0}" -f $(if ($ok) { 'OK' } else { 'FALHA' }))
}

if ($Summary) { Show-Summary $Summary; return }

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
"time,elapsed_h,cpu_total_pct,wintop_cpu_pct,host_cpu_pct,wintop_private_mb,wintop_ws_mb,wintop_handles,wintop_threads,host_private_mb" |
    Set-Content -Encoding utf8 $Out

$inv = [Globalization.CultureInfo]::InvariantCulture
$cores = [Environment]::ProcessorCount
$start = Get-Date
$prev = @{ T = $start; W = $w.TotalProcessorTime; H = $(if ($hostProc) { $hostProc.TotalProcessorTime } else { [TimeSpan]::Zero }) }
try {
    while (((Get-Date) - $start).TotalHours -lt $Hours) {
        Start-Sleep -Seconds $IntervalSec
        $w.Refresh()
        if ($w.HasExited) { Write-Host "o wintop fechou"; break }
        if ($hostProc) { $hostProc.Refresh() }
        $now = Get-Date
        $wall = ($now - $prev.T).TotalSeconds * $cores
        $hCpu = if ($hostProc -and -not $hostProc.HasExited) { $hostProc.TotalProcessorTime } else { $prev.H }
        $wPct = 100 * ($w.TotalProcessorTime - $prev.W).TotalSeconds / $wall
        $hPct = 100 * ($hCpu - $prev.H).TotalSeconds / $wall
        $hostPriv = if ($hostProc -and -not $hostProc.HasExited) { $hostProc.PrivateMemorySize64 / 1MB } else { 0 }
        $line = @(
            $now.ToString('s'),
            (($now - $start).TotalHours).ToString('F4', $inv),
            ($wPct + $hPct).ToString('F4', $inv),
            $wPct.ToString('F4', $inv),
            $hPct.ToString('F4', $inv),
            ($w.PrivateMemorySize64 / 1MB).ToString('F2', $inv),
            ($w.WorkingSet64 / 1MB).ToString('F2', $inv),
            $w.HandleCount,
            $w.Threads.Count,
            $hostPriv.ToString('F2', $inv)
        ) -join ','
        Add-Content -Encoding utf8 $Out $line
        $prev = @{ T = $now; W = $w.TotalProcessorTime; H = $hCpu }
    }
}
finally {
    Show-Summary $Out
}
