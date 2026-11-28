param(
    [Parameter(Mandatory)]
    [long]$WindowHandle,

    [Parameter(Mandatory)]
    [string]$EvidencePath
)

# Run with Windows PowerShell 5.1, whose .NET Framework includes UIAutomation.
# No CDP/debug port or relaxed WebView security is needed to inspect the page.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

$root = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$WindowHandle)
if (!$root) { throw 'The installed client has no automation window' }

function Read-DshPage {
    $condition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
        [System.Windows.Automation.ControlType]::Document)
    $documents = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $condition)
    foreach ($document in $documents) {
        # The trusted shell is titled DeepSeekHarness (without the space).
        # Only inspect the actual DSH document, not toolbar/log/splash text.
        if ($document.Current.Name -notmatch '(?i)^DeepSeek\s+Harness(?:\b|$)') { continue }
        $nodes = @()
        foreach ($element in $document.FindAll([System.Windows.Automation.TreeScope]::Descendants,
                [System.Windows.Automation.Condition]::TrueCondition)) {
            try {
                $current = $element.Current
                if (!$current.IsOffscreen -and $current.Name) {
                    $nodes += [PSCustomObject]@{
                        Element = $element
                        Name = $current.Name
                        ControlType = $current.ControlType
                        Enabled = $current.IsEnabled
                        Width = $current.BoundingRectangle.Width
                        Height = $current.BoundingRectangle.Height
                    }
                }
            } catch [System.Windows.Automation.ElementNotAvailableException] {
                # SPA navigation can replace elements during enumeration.
            }
        }
        $names = @($nodes | ForEach-Object { $_.Name })
        # The navigation sidebar is also present on plugin/settings pages.
        # Require the actual task composer, not just a sidebar search box.
        $composer = $nodes | Where-Object {
            $_.ControlType -eq [System.Windows.Automation.ControlType]::Edit -and
            ($_.Name -match '(?i)(describe.*(?:build|task)|what can i help|描述.*(?:构建|内容)|选择工作区|select.*workspace)' -or
             ($_.Width -ge 200 -and $_.Height -ge 60))
        }
        $kind = 'loading'
        if ($names -match '^(Add an API key to get started|添加一个 API Key 开始使用)$') {
            $kind = 'onboarding'
        } elseif ($names -match '^(Preview Notice|预览版说明|预览须知|预览说明)$') {
            $kind = 'preview'
        } elseif (($names -match '^(Plugins|插件)$') -and
            ($names -match '(?i)(built.?in plug|installed plug|内置插件|已安装插件)')) {
            $kind = 'plugins'
        } elseif (($names -match '^(New (session|chat)|新(建)?(会话|对话))$') -and
            $composer) {
            $kind = 'workspace'
        }
        return [PSCustomObject]@{ Document = $document; Nodes = $nodes; Kind = $kind }
    }
    return $null
}

function Invoke-PageControl($Nodes, [string]$NamePattern) {
    foreach ($node in $Nodes) {
        if (!$node.Enabled -or $node.Name -notmatch $NamePattern) { continue }
        $pattern = $null
        if ($node.Element.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$pattern)) {
            $pattern.Invoke()
            return $true
        }
        if ($node.Element.TryGetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern, [ref]$pattern)) {
            $pattern.Select()
            return $true
        }
    }
    return $false
}

function Wait-DshWorkspace([int]$Seconds) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        try {
            $page = Read-DshPage
            if ($page -and $page.Kind -eq 'workspace') { return $page }
            if ($page -and $page.Kind -eq 'preview') {
                # This is a disposable first-run profile and a non-binding
                # product preview notice, not a user account or terms flow.
                $null = Invoke-PageControl $page.Nodes '^(Continue|继续)$'
            } elseif ($page -and $page.Kind -eq 'onboarding') {
                # Do not enter credentials or send a model request.
                $null = Invoke-PageControl $page.Nodes '^(Configure later|稍后配置)$'
            }
        } catch [System.Windows.Automation.ElementNotAvailableException] {
        } catch [System.InvalidOperationException] {
        }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'The actual DSH WebView did not render its workspace controls (blank or authentication error pages cannot pass)'
}

$workspacePage = Wait-DshWorkspace 90
Write-Host 'UIA: Actual DSH workspace and task composer are visible'
if (!(Invoke-PageControl $workspacePage.Nodes '^(Plugins|插件)$')) {
    throw 'The rendered DSH page did not expose an invokable plugin navigation control'
}
$pluginDeadline = [DateTime]::UtcNow.AddSeconds(30)
$plugins = $null
do {
    try { $plugins = Read-DshPage } catch [System.Windows.Automation.ElementNotAvailableException] {}
    if ($plugins -and $plugins.Kind -eq 'plugins') { break }
    Start-Sleep -Milliseconds 500
} while ([DateTime]::UtcNow -lt $pluginDeadline)
if (!$plugins -or $plugins.Kind -ne 'plugins') {
    throw 'DSH plugin navigation did not render its actual plugin configuration/list'
}
Write-Host 'UIA: Plugin navigation rendered its actual configuration/list'

$shellNodes = @()
foreach ($document in $root.FindAll([System.Windows.Automation.TreeScope]::Descendants,
        [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::Document))) {
    if ($document.Current.Name -ne 'DeepSeekHarness') { continue }
    foreach ($element in $document.FindAll([System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition)) {
        try {
            if (!$element.Current.IsOffscreen -and $element.Current.Name -match '^(Refresh(?: page)?|刷新页面)$') {
                $shellNodes += [PSCustomObject]@{
                    Element = $element
                    Name = $element.Current.Name
                    Enabled = $element.Current.IsEnabled
                }
            }
        } catch [System.Windows.Automation.ElementNotAvailableException] {}
    }
}
if (!(Invoke-PageControl $shellNodes '^(Refresh(?: page)?|刷新页面)$')) {
    throw 'The local toolbar did not expose its refresh-page control'
}
$null = Wait-DshWorkspace 60
Write-Host 'UIA: DSH workspace restored after the local toolbar refresh'

$evidence = [ordered]@{
    result = 'passed'
    actual_dsh_document = $true
    workspace_controls = $true
    plugin_navigation = $true
    plugin_configuration_rendered = $true
    toolbar_refresh = $true
    workspace_after_refresh = $true
    checked_at_utc = [DateTime]::UtcNow.ToString('o')
}
$null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($EvidencePath))
[IO.File]::WriteAllText($EvidencePath, ($evidence | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
Write-Host 'PASS: Actual WebView2 DSH workspace, plugin navigation and toolbar refresh verified with UI Automation'
