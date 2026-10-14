param(
    [string]$GuiExe = 'D:\chat\inkpad\target\release\inkpad.exe',
    [string]$AgentExe = 'D:\chat\inkpad\target\release\inkpad-agent.exe',
    [string]$DataDir = 'D:\chat\inkpad\dist\agent-gui-smoke'
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
$env:INKPAD_DATA_DIR = $DataDir
$gui = Start-Process -FilePath $GuiExe -PassThru
try {
    Start-Sleep -Milliseconds 950
    Add-Type -AssemblyName UIAutomationClient
    Add-Type -AssemblyName UIAutomationTypes
    $title='Headless live check ' + ([Guid]::NewGuid().ToString('N').Substring(0,10))
    $clock=[System.Diagnostics.Stopwatch]::StartNew()
    $output = & $AgentExe create --title $title --body 'Created from a separate headless agent with Inkpad GUI already running'
    if ($LASTEXITCODE -ne 0) { throw 'Agent failed to create a note.' }
    $agentMs=$clock.Elapsed.TotalMilliseconds
    $found=$false
    while ($clock.Elapsed.TotalMilliseconds -lt 5000) {
        $root=[System.Windows.Automation.AutomationElement]::RootElement
        foreach ($w in $root.FindAll([System.Windows.Automation.TreeScope]::Children,[System.Windows.Automation.Condition]::TrueCondition)) {
            if ($w.Current.ProcessId -ne $gui.Id -or $w.Current.Name -notlike 'Inkpad*') { continue }
            $items=$w.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition)
            foreach ($item in $items) {
                if ($item.Current.Name -eq "$title, page") { $found=$true;break }
            }
            if ($found) { break }
        }
        if ($found) { break }
        Start-Sleep -Milliseconds 45
    }
    $clock.Stop()
    Write-Output ('AGENT_PROCESS_CREATE_MS={0:N1}' -f $agentMs)
    Write-Output ('GUI_SIDEBAR_UPDATE_MS={0:N1}' -f $clock.Elapsed.TotalMilliseconds)
    Write-Output ('LIVE_NOTE_FOUND={0}' -f $found)
    if (!$found) { throw "New note not visible in desktop sidebar" }
} finally {
    $gui.Refresh()
    if (!$gui.HasExited) { [void]$gui.CloseMainWindow() }
}
