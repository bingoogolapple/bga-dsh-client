param(
    [Parameter(Mandatory)]
    [long]$WindowHandle,

    [Parameter(Mandatory)]
    [string]$EvidencePath,

    [switch]$WorkspaceOnly
)

# Run with Windows PowerShell 5.1, whose .NET Framework includes UIAutomation.
# No CDP/debug port or relaxed WebView security is needed to inspect the page.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

$root = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$WindowHandle)
if (!$root) { throw 'The installed client has no automation window' }
$script:accountPromptDismissals = 0

function Get-VisiblePageNodes($Container) {
    foreach ($element in $Container.FindAll([System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition)) {
        try {
            $current = $element.Current
            if (!$current.IsOffscreen) {
                [PSCustomObject]@{
                    Element = $element
                    Name = $current.Name
                    ControlType = $current.ControlType
                    Enabled = $current.IsEnabled
                    Width = $current.BoundingRectangle.Width
                    Height = $current.BoundingRectangle.Height
                    # .NET Framework UIAutomation does not expose the newer
                    # native AriaRole property identifier. Use its supported
                    # control type/localized role description instead.
                    IsDialog = ($current.ControlType -eq [System.Windows.Automation.ControlType]::Window -or
                        $current.LocalizedControlType -match '(?i)(dialog|对话)')
                }
            }
        } catch [System.Windows.Automation.ElementNotAvailableException] {
            # SPA navigation can replace elements during enumeration.
        }
    }
}

function Find-AccountDialogNodes($Document, $Nodes) {
    $documentId = [string]::Join(':', $Document.GetRuntimeId())
    foreach ($title in @($Nodes | Where-Object { $_.Name -match '^(Get started|开始使用)$' })) {
        $container = $title.Element
        for ($depth = 0; $depth -lt 12 -and $container; $depth++) {
            if ([string]::Join(':', $container.GetRuntimeId()) -eq $documentId) { break }
            $dialogNodes = @(Get-VisiblePageNodes $container)
            $dialogNames = @($dialogNodes | ForEach-Object { $_.Name })
            # Scope Close to the optional account dialog, never an unrelated
            # workspace/sidebar close action or an account sign-in control.
            if (($dialogNames -match '^(Add API Key|添加 API Key)$') -and
                ($dialogNames -match '^(Sign in|登录)$') -and
                ($dialogNames -match '^(Close|关闭)$')) {
                return $dialogNodes
            }
            $container = [System.Windows.Automation.TreeWalker]::ControlViewWalker.GetParent($container)
        }
    }
}

function Read-DshPage {
    $condition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
        [System.Windows.Automation.ControlType]::Document)
    $documents = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $condition)
    foreach ($document in $documents) {
        # The trusted shell is titled DeepSeekHarness (without the space).
        # Only inspect the actual DSH document, not toolbar/log/splash text.
        if ($document.Current.Name -notmatch '(?i)^DeepSeek\s+Harness(?:\b|$)') { continue }
        $nodes = @(Get-VisiblePageNodes $document)
        $names = @($nodes | ForEach-Object { $_.Name })
        # The navigation sidebar is also present on plugin/settings pages.
        # Require the actual task composer, not just a sidebar search box.
        $composer = $nodes | Where-Object {
            $_.ControlType -eq [System.Windows.Automation.ControlType]::Edit -and
            ($_.Name -match '(?i)(describe.*(?:build|task)|what can i help|描述.*(?:构建|内容)|选择工作区|select.*workspace)' -or
             ($_.Width -ge 200 -and $_.Height -ge 60))
        }
        $kind = 'loading'
        $accountDialogNodes = @()
        if (($names -match '^(Get started|开始使用)$') -and
            ($names -match '^(Add API Key|添加 API Key)$') -and
            ($names -match '^(Sign in|登录)$')) {
            $kind = 'account-prompt'
            $accountDialogNodes = @(Find-AccountDialogNodes $document $nodes)
        } elseif ($names -match '^(Add an API key to get started|添加一个 API Key 开始使用)$') {
            $kind = 'onboarding'
        } elseif ($names -match '^(Preview Notice|预览版说明|预览须知|预览说明)$') {
            $kind = 'preview'
        } elseif ($nodes | Where-Object { $_.IsDialog }) {
            # IsOffscreen does not detect modal occlusion. Do not count the
            # visible-to-UIA sidebar/editor underneath an unhandled dialog.
            $kind = 'blocked-dialog'
        } elseif (($names -match '^(Plugins|插件)$') -and
            ($names -match '(?i)(built.?in plug|installed plug|内置插件|已安装插件)')) {
            $kind = 'plugins'
        } elseif (($names -match '^(New (session|chat)|新(建)?(会话|对话))$') -and
            $composer) {
            $kind = 'workspace'
        }
        return [PSCustomObject]@{
            Document = $document
            Nodes = $nodes
            Kind = $kind
            AccountDialogNodes = $accountDialogNodes
        }
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

function Dismiss-OptionalAccountPrompt($Page) {
    if (!$Page -or $Page.Kind -ne 'account-prompt') { return $Page }
    if (!(Invoke-PageControl $Page.AccountDialogNodes '^(Close|关闭)$')) {
        throw 'Could not identify the optional account dialog Close control'
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        Start-Sleep -Milliseconds 200
        try {
            $currentPage = Read-DshPage
        } catch [System.Windows.Automation.ElementNotAvailableException] {
            continue
        }
        if ($currentPage -and $currentPage.Kind -ne 'account-prompt') {
            $script:accountPromptDismissals++
            Write-Host 'UIA: Closed the optional Get started account prompt without signing in'
            return $currentPage
        }
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'The optional account dialog remained visible after Close'
}

function Wait-DshWorkspace([int]$Seconds) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    $workspaceSince = $null
    do {
        try {
            $page = Dismiss-OptionalAccountPrompt (Read-DshPage)
            if ($page -and $page.Kind -eq 'workspace') {
                # Give asynchronous first-run overlays time to appear. A
                # workspace hidden under a dialog is never a successful read.
                if (!$workspaceSince) { $workspaceSince = [DateTime]::UtcNow }
                if (([DateTime]::UtcNow - $workspaceSince).TotalSeconds -ge 2) { return $page }
            } else {
                $workspaceSince = $null
            }
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

if ($WorkspaceOnly) {
    # This final check follows the composed-window screenshot. It is strictly
    # read-only: do not close a dialog after it was captured and then call the
    # screenshot a successful workspace image.
    $evidence = Get-Content -LiteralPath $EvidencePath -Raw | ConvertFrom-Json
    if ($evidence.result -notin @('pending_capture', 'passed')) {
        throw 'The final screenshot is missing a successful DSH interaction sequence'
    }
    $capturedPage = Read-DshPage
    $capturedWorkspace = $capturedPage -and $capturedPage.Kind -eq 'workspace'
    $evidence | Add-Member -NotePropertyName captured_workspace_unblocked -NotePropertyValue ([bool]$capturedWorkspace) -Force
    $evidence.result = if ($capturedWorkspace) { 'passed' } else { 'failed' }
    $evidence.checked_at_utc = [DateTime]::UtcNow.ToString('o')
    [IO.File]::WriteAllText($EvidencePath, ($evidence | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
    if (!$capturedWorkspace) { throw 'The captured client window was not an unobstructed DSH workspace' }
    Write-Host 'PASS: The final screenshot corresponds to an unobstructed DSH workspace'
    exit 0
}

$workspacePage = Wait-DshWorkspace 90
Write-Host 'UIA: Actual DSH workspace and task composer are visible'
if (!(Invoke-PageControl $workspacePage.Nodes '^(Plugins|插件)$')) {
    throw 'The rendered DSH page did not expose an invokable plugin navigation control'
}
$pluginDeadline = [DateTime]::UtcNow.AddSeconds(30)
$plugins = $null
do {
    try { $plugins = Dismiss-OptionalAccountPrompt (Read-DshPage) } catch [System.Windows.Automation.ElementNotAvailableException] {}
    if ($plugins -and $plugins.Kind -eq 'plugins') { break }
    if ($plugins -and $plugins.Kind -eq 'workspace') {
        # A late first-run prompt can complete by returning to the workspace.
        # Reopen the requested read-only panel only after that prompt is gone.
        $null = Invoke-PageControl $plugins.Nodes '^(Plugins|插件)$'
    }
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
    result = 'pending_capture'
    actual_dsh_document = $true
    workspace_controls = $true
    plugin_navigation = $true
    plugin_configuration_rendered = $true
    toolbar_refresh = $true
    workspace_after_refresh = $true
    workspace_unblocked_by_dialog = $true
    captured_workspace_unblocked = $false
    optional_account_prompts_closed = $script:accountPromptDismissals
    checked_at_utc = [DateTime]::UtcNow.ToString('o')
}
$null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($EvidencePath))
[IO.File]::WriteAllText($EvidencePath, ($evidence | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
Write-Host 'PASS: Actual WebView2 DSH workspace, plugin navigation and toolbar refresh verified with UI Automation'
