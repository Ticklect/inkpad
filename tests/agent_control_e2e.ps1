param(
    [string]$GuiExe = 'D:\chat\inkpad\target\release\inkpad.exe',
    [string]$AgentExe = 'D:\chat\inkpad\target\release\inkpad-agent.exe',
    [string]$OutputRoot = 'D:\chat\inkpad\dist\agent-control-audit'
)
$ErrorActionPreference = 'Stop'
$folder = Join-Path $OutputRoot ([Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $folder | Out-Null
$env:INKPAD_DATA_DIR = $folder
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

function Invoke-Agent {
    param([string[]]$AgentArgs)
    $output = & $AgentExe @AgentArgs
    if ($LASTEXITCODE -ne 0) {
        throw ("Failed agent command '{0}': {1}" -f ($AgentArgs -join ' '), ($output -join [Environment]::NewLine))
    }
    return ($output -join "`n" | ConvertFrom-Json)
}

$gui = Start-Process -FilePath $GuiExe -PassThru
try {
    Start-Sleep -Milliseconds 750
    $note = Invoke-Agent @('create','--title','AI control E2E','--body','This page belongs to a test workspace')
    $id = [string]$note.id
    if ([string]::IsNullOrWhiteSpace($id)) { throw 'No note ID returned' }

    $null = Invoke-Agent @('rename',$id,'Agent renamed this note')
    $null = Invoke-Agent @('pin',$id)
    $null = Invoke-Agent @('font-size',$id,'21')
    $null = Invoke-Agent @('font-choice',$id,'Mono')
    $null = Invoke-Agent @('activate',$id)
    $stroke = '{"points":[[20,35],[72,90]],"width":8,"color":"#345678","eraser":false}'
    $strokePath = Join-Path $folder 'agent-stroke.json'
    [System.IO.File]::WriteAllText($strokePath, $stroke)
    $null = Invoke-Agent @('add-stroke',$id,('@' + $strokePath))

    $textPath = Join-Path $folder 'exported-note.txt'
    $pngPath = Join-Path $folder 'exported-drawing.png'
    $null = Invoke-Agent @('export-text',$id,$textPath)
    $null = Invoke-Agent @('export-png',$id,$pngPath)
    if (!(Test-Path -LiteralPath $textPath) -or !(Test-Path -LiteralPath $pngPath)) {
        throw 'Agent export did not create both formats'
    }
    if ((Get-Item -LiteralPath $pngPath).Length -lt 200) {
        throw 'PNG export appears empty'
    }

    $current = Get-Content -LiteralPath (Join-Path $folder 'workspace.json') -Raw | ConvertFrom-Json
    $stored = @($current.notes | Where-Object { $_.id -eq $id })[0]
    if (!$stored -or $stored.title -ne 'Agent renamed this note' -or !$stored.pinned -or
        $stored.font_size -ne 21 -or $stored.font_choice -ne 'Mono' -or
        $stored.strokes.Count -ne 1 -or $current.active_id -ne $id) {
        throw 'Renaming, pinning, font settings, active selection or stroke not saved'
    }

    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    $visible = $false
    while ($clock.ElapsedMilliseconds -lt 5000) {
        foreach ($window in [System.Windows.Automation.AutomationElement]::RootElement.FindAll(
            [System.Windows.Automation.TreeScope]::Children,
            [System.Windows.Automation.Condition]::TrueCondition)) {
            if ($window.Current.ProcessId -ne $gui.Id -or $window.Current.Name -notlike 'Inkpad*') { continue }
            $nodes = $window.FindAll(
                [System.Windows.Automation.TreeScope]::Descendants,
                [System.Windows.Automation.Condition]::TrueCondition)
            foreach ($node in $nodes) {
                if ($node.Current.Name -like '*Agent renamed this note*') { $visible = $true;break }
            }
            if ($visible) { break }
        }
        if ($visible) {break}
        Start-Sleep -Milliseconds 70
    }
    if (!$visible) { throw 'Renamed page never became visible in the open GUI' }

    $null = Invoke-Agent @('undo-stroke',$id,'--confirm')
    $fresh = Get-Content -LiteralPath (Join-Path $folder 'workspace.json') -Raw | ConvertFrom-Json
    $afterUndo = @($fresh.notes | Where-Object { $_.id -eq $id })[0]
    if ($afterUndo.strokes.Count -ne 0 -or $afterUndo.body -ne 'This page belongs to a test workspace') {
        throw 'Agent stroke undo did not preserve text'
    }
    Write-Output "PASS headless create/rename/pin/font/activate/strokes/undo/exports with desktop open"
    Write-Output ("GUI_REFRESH_MS={0:N1}" -f $clock.Elapsed.TotalMilliseconds)
    Write-Output "WORKSPACE=$folder"
} finally {
    $gui.Refresh()
    if (!$gui.HasExited) { [void]$gui.CloseMainWindow() }
}
