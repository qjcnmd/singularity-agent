param([int]$OwnerProcessId, [string]$Folder, [switch]$Cancel)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class NativeDialogFocus {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
}
'@
$condition = [System.Windows.Automation.AndCondition]::new(
  [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ProcessIdProperty, $OwnerProcessId),
  [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty, '选择工作区文件夹')
)
$deadline = [DateTime]::UtcNow.AddSeconds(15)
do {
  $dialog = [System.Windows.Automation.AutomationElement]::RootElement.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition)
  if ($dialog) { break }
  Start-Sleep -Milliseconds 100
} while ([DateTime]::UtcNow -lt $deadline)
if (-not $dialog) { throw 'Native folder dialog did not appear for the Electron process' }
[NativeDialogFocus]::SetForegroundWindow([IntPtr]$dialog.Current.NativeWindowHandle) | Out-Null
if ($Cancel) {
  [System.Windows.Forms.SendKeys]::SendWait('{ESC}')
  Write-Output 'Native folder dialog cancelled with Escape'
  exit
}
# 窗口出现时文件名输入框可能仍在初始化，等待它真正提供可写的 ValuePattern。
$fieldCondition = [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::AutomationIdProperty, '1152')
$valuePattern = $null
do {
  $dialog = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$dialog.Current.NativeWindowHandle)
  $field = $dialog.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $fieldCondition)
  if ($field -and $field.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$valuePattern)) { break }
  Start-Sleep -Milliseconds 100
} while ([DateTime]::UtcNow -lt $deadline)
if (-not $valuePattern) { throw 'Native folder input did not become writable' }
([System.Windows.Automation.ValuePattern]$valuePattern).SetValue($Folder)
# 原生按钮随对话框初始化才暴露 InvokePattern；等待可调用的按钮后执行。
$buttonCondition = [System.Windows.Automation.AndCondition]::new(
  [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::AutomationIdProperty, '1'),
  [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::IsInvokePatternAvailableProperty, $true)
)
$invokePattern = $null
do {
  $button = $dialog.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $buttonCondition)
  if ($button -and $button.Current.IsEnabled -and $button.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$invokePattern)) { break }
  Start-Sleep -Milliseconds 100
} while ([DateTime]::UtcNow -lt $deadline)
if (-not $invokePattern) { throw 'Native folder button did not become invokable' }
([System.Windows.Automation.InvokePattern]$invokePattern).Invoke()
Write-Output 'Native folder selected'
