# Real Win32 GUI workload with isolated notebook data. Never touches the installed app.
# powershell -NoProfile -ExecutionPolicy Bypass -File tests/benchmark_gui_sustained.ps1
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $root
Add-Type -AssemblyName UIAutomationClient
$agent = (Resolve-Path 'target\release\inkpad-agent.exe').Path
$paths = @(
    @{ name='before'; exe=(Resolve-Path 'tools\perf_baseline\inkpad-gui-before-secondpass.exe').Path },
    @{ name='after'; exe=(Resolve-Path 'target\release\inkpad.exe').Path }
)
if($env:INKPAD_GUI_BEFORE_EXE) {
    $paths[0].exe = (Resolve-Path -LiteralPath $env:INKPAD_GUI_BEFORE_EXE).Path
}
if($env:INKPAD_GUI_MODES) {
    $selectedModes = @($env:INKPAD_GUI_MODES.Split(',') | ForEach-Object { $_.Trim() })
    $paths = @($paths | Where-Object { $_.name -in $selectedModes })
    if($paths.Count -eq 0) {throw 'No selected GUI benchmark modes'}
}
if($env:INKPAD_GUI_REVERSE_ORDER -eq '1') {
    [Array]::Reverse($paths)
}
$tag = Get-Date -Format 'yyyyMMddTHHmmss'
$results = @()
$counts = @(24,100,256,512)
if($env:INKPAD_GUI_SCENE_SIZES) {
    $counts = @($env:INKPAD_GUI_SCENE_SIZES.Split(',') | ForEach-Object { [int]$_.Trim() })
    foreach($count in $counts) { if($count -notin @(24,100,256,512)) {throw "Invalid scene count $count"} }
}
$gestures = 6
if($env:INKPAD_GUI_GESTURES) { $gestures = [int]$env:INKPAD_GUI_GESTURES }
if($gestures -lt 1 -or $gestures -gt 60) { throw "Gesture count must be 1..60" }

function Find-UIA($window, $name, $controlType) {
    $items=$window.FindAll([System.Windows.Automation.TreeScope]::Descendants,
        [System.Windows.Automation.Condition]::TrueCondition)
    for($i=0;$i -lt $items.Count;$i++) {
        $entry=$items.Item($i)
        if($entry.Current.Name -eq $name -and
            $entry.Current.ControlType.ProgrammaticName -eq $controlType) { return $entry }
    }
    return $null
}
foreach($binary in $paths) {
    foreach($count in $counts) {
        $folder=Join-Path $root "target\perf-gui-$($tag)-$($binary.name)-$($count)"
        if(Test-Path -LiteralPath $folder) {throw "Refusing to replace $folder"}
        & python 'tests\prepare_gui_benchmark.py' $agent $folder $count
        if($LASTEXITCODE -ne 0) {throw 'Fixture preparation failed'}
        $env:INKPAD_DATA_DIR=$folder
        $proc=Start-Process -FilePath $binary.exe -WorkingDirectory $root -PassThru
        try {
            for($j=0;$j -lt 50;$j++) {
                Start-Sleep -Milliseconds 100
                $proc.Refresh()
                if($proc.MainWindowHandle -ne [IntPtr]::Zero) {break}
            }
            if($proc.MainWindowHandle -eq [IntPtr]::Zero) {throw 'No GUI window'}
            foreach($name in @('Draw','Select')) {
                $button=$null
                for($retry=0;$retry -lt 80;$retry++) {
                    $proc.Refresh()
                    if($proc.MainWindowHandle -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 100; continue }
                    $window=[System.Windows.Automation.AutomationElement]::FromHandle($proc.MainWindowHandle)
                    $button=Find-UIA $window $name 'ControlType.Button'
                    if($button) {break}
                    Start-Sleep -Milliseconds 100
                }
                if(-not $button) {
                    $observed=@()
                    if($window) {
                        $all=$window.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition)
                        for($index=0;$index -lt [Math]::Min($all.Count,100);$index++) {
                            if($all.Item($index).Current.ControlType.ProgrammaticName -eq 'ControlType.Button') {
                                $observed+= $all.Item($index).Current.Name
                            }
                        }
                    }
                    throw "UIA button missing: $name; pid=$($proc.Id) title=$($proc.MainWindowTitle) observed=$($observed -join ',')"
                }
                $button.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
            }
            # On a 2,200-note sidebar Slint may still be laying out thousands
            # of rows after UI Automation reports a button invocation. Wait
            # for those queued UI callbacks to settle before starting the
            # measured pointer workload; this wait is outside all timings.
            Start-Sleep -Milliseconds 1000
            $selectConfirmed=$false
            for($attempt=0;$attempt -lt 25;$attempt++) {
                $items=$window.FindAll([System.Windows.Automation.TreeScope]::Descendants,
                    [System.Windows.Automation.Condition]::TrueCondition)
                for($i=0;$i -lt $items.Count;$i++) {
                    if($items.Item($i).Current.Name -match '^Objects: [0-9]+ .*select') {
                        $selectConfirmed=$true
                        break
                    }
                }
                if($selectConfirmed) {break}
                $select=Find-UIA $window 'Select' 'ControlType.Button'
                if($select) { $select.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke() }
                Start-Sleep -Milliseconds 180
            }
            if(-not $selectConfirmed) {throw "Select mode was never confirmed by GUI after clicking Select"}
            $items=$window.FindAll([System.Windows.Automation.TreeScope]::Descendants,
                [System.Windows.Automation.Condition]::TrueCondition)
            $rect=$null
            for($i=0;$i -lt $items.Count;$i++) {
                if($items.Item($i).Current.ControlType.ProgrammaticName -eq 'ControlType.Image') {
                    $rect=$items.Item($i).Current.BoundingRectangle
                    break
                }
            }
            if($null -eq $rect) {throw 'No accessible canvas'}
            $x=[int][Math]::Round($rect.X+700*$rect.Width/1100)
            $y=[int][Math]::Round($rect.Y+350*$rect.Height/700)
            $report=& python 'tests\benchmark_gui_sustained.py' $proc.Id $folder $x $y $gestures $rect.Width $rect.Height
            if($LASTEXITCODE -ne 0) {throw "GUI benchmark failed: $report"}
            $parsed=$report | ConvertFrom-Json
            $row=[pscustomobject]@{
                mode=$binary.name; objects=$count
                cpu_p50_ms=$parsed.cpu_ms_per_gesture.p50
                cpu_p95_ms=$parsed.cpu_ms_per_gesture.p95
                commit_p50_ms=$parsed.mouse_up_to_durable_commit_ms.p50
                commit_p95_ms=$parsed.mouse_up_to_durable_commit_ms.p95
                gestures=$gestures
                rss_initial_mib=$parsed.rss_initial_mib
                rss_peak_mib=$parsed.rss_peak_mib
                rss_after_idle_mib=$parsed.rss_after_idle_mib
                private_after_idle_mib=$parsed.private_after_idle_mib
                rss_final_mib=$parsed.rss_final_mib
            }
            $results += $row
            Write-Output ($row|ConvertTo-Json -Compress)
        } finally {
            $proc.Refresh()
            if(-not $proc.HasExited) {
                $proc.CloseMainWindow()|Out-Null
                if(-not $proc.WaitForExit(8000)) {throw "Test app $($proc.Id) did not close"}
            }
        }
    }
}
$results|ConvertTo-Json|Out-File -LiteralPath (
    Join-Path $root "target\perf-gui-$($tag)-results.json") -Encoding utf8
