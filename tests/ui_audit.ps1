param(
    [string]$Executable = 'D:\chat\inkpad\target\release\inkpad.exe',
    [string]$OutputDir = 'D:\chat\inkpad\dist\ui-audit',
    [string]$DataDir = 'D:\chat\inkpad\dist\ui-audit-data',
    [double]$ScaleFactor = 1.0
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $OutputDir,$DataDir | Out-Null
$env:INKPAD_DATA_DIR = $DataDir
$env:SLINT_SCALE_FACTOR = [string]$ScaleFactor
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class InkpadAuditNative {
    [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr hwnd, int x, int y, int width, int height, bool repaint);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr dc, uint options);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int left,top,right,bottom; }
}
'@
$process = Start-Process -FilePath $Executable -PassThru
try {
    Start-Sleep -Milliseconds 1200
    $process.Refresh()
    if (!$process.MainWindowHandle) { throw "Inkpad did not launch a visible main window." }
    # Slint's min-width/height are logical pixels. At non-100% scale, a
    # physical 1010x660 window would be forcibly smaller than the legal
    # minimum, producing artificial clipping. Audit supported physical sizes.
    $smallWidth = if ($ScaleFactor -gt 1) { [int][Math]::Ceiling(1010*$ScaleFactor)+12 } else { 1010 }
    $smallHeight = if ($ScaleFactor -gt 1) { [int][Math]::Ceiling(660*$ScaleFactor)+20 } else { 660 }
    $sizes = @(@($smallWidth,$smallHeight),@([Math]::Max(1330,$smallWidth),[Math]::Max(830,$smallHeight)),@([Math]::Max(1600,$smallWidth),[Math]::Max(900,$smallHeight)))
    foreach ($size in $sizes) {
        $width,$height=$size
        [void][InkpadAuditNative]::MoveWindow($process.MainWindowHandle,35,35,$width,$height,$true)
        Start-Sleep -Milliseconds 250
        $rect = New-Object InkpadAuditNative+Rect
        [void][InkpadAuditNative]::GetWindowRect($process.MainWindowHandle,[ref]$rect)
        $w=$rect.right-$rect.left
        $h=$rect.bottom-$rect.top
        $bitmap=New-Object System.Drawing.Bitmap($w,$h)
        $graphics=[System.Drawing.Graphics]::FromImage($bitmap)
        $dc=$graphics.GetHdc()
        $ok=[InkpadAuditNative]::PrintWindow($process.MainWindowHandle,$dc,2)
        $graphics.ReleaseHdc($dc)
        $graphics.Dispose()
        $path=Join-Path $OutputDir ("inkpad-{0}x{1}.png" -f $width,$height)
        $bitmap.Save($path,[System.Drawing.Imaging.ImageFormat]::Png)
        $bitmap.Dispose()
        if (!$ok) { throw "PrintWindow failed for $width x $height" }
        Write-Output "CAPTURE $path ACTUAL ${w}x${h}"

        $window=$null
        $desktop=[System.Windows.Automation.AutomationElement]::RootElement
        foreach($candidate in $desktop.FindAll([System.Windows.Automation.TreeScope]::Children,[System.Windows.Automation.Condition]::TrueCondition)) {
            if ($candidate.Current.ProcessId -eq $process.Id -and $candidate.Current.Name -like 'Inkpad*') { $window=$candidate;break }
        }
        if ($window) {
            $interesting=$window.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition)
            $names=@('New page','FONT','Sans','Serif','Mono','SIZE','Text','Draw','Rename','Save')
            foreach ($item in $interesting) {
                if ($names -contains $item.Current.Name -and $item.Current.ControlType.ProgrammaticName -notmatch 'Button') {
                    $r=$item.Current.BoundingRectangle
                    Write-Output ("BOX {0} {1:N1},{2:N1} {3:N1}x{4:N1}" -f $item.Current.Name,$r.X,$r.Y,$r.Width,$r.Height)
                }
            }
        }
    }
} finally {
    $process.Refresh()
    if (!$process.HasExited) { [void]$process.CloseMainWindow(); Start-Sleep -Milliseconds 450 }
}
