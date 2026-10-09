# Forward paper run of strategy v5 on this machine, detached from any shell.
#
#   powershell -File sidecar\forward_v5.ps1 install   # build, copy, register the task, start it
#   powershell -File sidecar\forward_v5.ps1 status    # task state, status.json, last signals
#   powershell -File sidecar\forward_v5.ps1 stop      # stop the run (state is kept)
#   powershell -File sidecar\forward_v5.ps1 start     # start it again (resumes from its log)
#   powershell -File sidecar\forward_v5.ps1 remove    # stop and unregister the task
#
# The task runs at logon and restarts on failure; no admin rights are needed.
# The binary is a copy under results/forward_v5/bin, so rebuilding the repo
# never fights a running process for the file. Paper only: the program holds
# no keys and has no order code; it reads public candles.

param([ValidateSet("install", "status", "stop", "start", "remove", "run")] [string]$Action = "status")

$ErrorActionPreference = "Stop"
$Repo = Split-Path -Parent $PSScriptRoot
$Dir = Join-Path $Repo "results\forward_v5"
$Bin = Join-Path $Dir "bin\mft-engine.exe"
$Log = Join-Path $Dir "run.log"
$Task = "mft-engine-v5-forward"

switch ($Action) {
    "run" {
        # What the task executes: the engine, output appended to run.log.
        Set-Location $Repo
        & $Bin paper-hourly --strategy v5 --coins BTC,ETH --dir $Dir *>> $Log
        exit $LASTEXITCODE
    }
    "install" {
        Set-Location $Repo
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "build failed" }
        New-Item -ItemType Directory -Force (Split-Path $Bin) | Out-Null
        Copy-Item -Force (Join-Path $Repo "target\release\mft-engine.exe") $Bin
        $script = Join-Path $PSScriptRoot "forward_v5.ps1"
        $taskAction = New-ScheduledTaskAction -Execute "powershell.exe" `
            -Argument "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$script`" run" `
            -WorkingDirectory $Repo
        $taskTrigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
        $taskSettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
            -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 5) `
            -MultipleInstances IgnoreNew -StartWhenAvailable
        $taskPrincipal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
        Register-ScheduledTask -TaskName $Task -Action $taskAction -Trigger $taskTrigger -Settings $taskSettings -Principal $taskPrincipal -Force | Out-Null
        Start-ScheduledTask -TaskName $Task
        Write-Output "registered and started $Task; logs in $Dir"
    }
    "start" { Start-ScheduledTask -TaskName $Task; Write-Output "started $Task" }
    "stop" {
        Stop-ScheduledTask -TaskName $Task
        Get-Process mft-engine -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Bin } | Stop-Process -Force
        Write-Output "stopped $Task (logs and state kept; 'start' resumes)"
    }
    "remove" {
        Stop-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue
        Get-Process mft-engine -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Bin } | Stop-Process -Force
        Unregister-ScheduledTask -TaskName $Task -Confirm:$false
        Write-Output "removed $Task"
    }
    "status" {
        $t = Get-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue
        if ($t) { Write-Output "task $Task : $($t.State)" } else { Write-Output "task $Task : not registered" }
        Get-Process mft-engine -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Bin } |
            ForEach-Object { Write-Output "process mft-engine pid $($_.Id), started $($_.StartTime)" }
        $status = Join-Path $Dir "status.json"
        if (Test-Path $status) { Get-Content $status }
        $signals = Join-Path $Dir "signals.log"
        if (Test-Path $signals) { Write-Output "last signals:"; Get-Content $signals -Tail 5 }
    }
}
