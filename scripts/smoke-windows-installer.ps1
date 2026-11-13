param(
    [Parameter(Mandatory)]
    [string]$Installer,

    [Parameter(Mandatory)]
    [ValidateSet('plain', 'bundled')]
    [string]$Variant,

    [Parameter(Mandatory)]
    [string]$AppVersion,

    [string]$BundledDshVersion,

    [string]$NodeVersion,

    [Parameter(Mandatory)]
    [string]$ScreenshotPath
)

$ErrorActionPreference = 'Stop'

function Save-ClientScreenshot([Diagnostics.Process]$App, [string]$Path) {
    Add-Type -AssemblyName System.Drawing.Common
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class DshSmokeWindow
{
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect
    {
        public int Left, Top, Right, Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct Point
    {
        public int X, Y;
    }

    [DllImport("user32.dll")]
    public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")]
    public static extern bool SystemParametersInfoW(uint action, uint param, out Rect rect, uint flags);
    [DllImport("user32.dll")]
    public static extern bool SetWindowPos(IntPtr window, IntPtr after, int x, int y, int width, int height, uint flags);
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")]
    public static extern bool GetClientRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")]
    public static extern bool ClientToScreen(IntPtr window, ref Point point);
    [DllImport("user32.dll")]
    private static extern IntPtr WindowFromPoint(Point point);
    [DllImport("user32.dll")]
    private static extern IntPtr GetAncestor(IntPtr window, uint flags);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, IntPtr process);
    [DllImport("user32.dll")]
    private static extern bool AttachThreadInput(uint first, uint second, bool attach);
    [DllImport("kernel32.dll")]
    private static extern uint GetCurrentThreadId();

    public static void Focus(IntPtr window)
    {
        if (SetForegroundWindow(window))
            return;
        uint current = GetCurrentThreadId();
        uint foreground = GetWindowThreadProcessId(GetForegroundWindow(), IntPtr.Zero);
        if (foreground == 0 || foreground == current || !AttachThreadInput(current, foreground, true))
            return;
        try
        {
            SetForegroundWindow(window);
        }
        finally
        {
            AttachThreadInput(current, foreground, false);
        }
    }

    public static bool OwnsArea(IntPtr window, int x, int y, int width, int height)
    {
        for (int row = 1; row < 10; row++)
        {
            for (int column = 1; column < 10; column++)
            {
                var point = new Point { X = x + width * column / 10, Y = y + height * row / 10 };
                IntPtr hit = WindowFromPoint(point);
                // WebView2 child windows count as part of the same top-level window.
                if (hit != window && GetAncestor(hit, 2) != window)
                    return false;
            }
        }
        return true;
    }
}
'@

    $App.Refresh()
    $window = $App.MainWindowHandle
    if ($window -eq [IntPtr]::Zero) { throw 'Client window disappeared before screenshot' }

    # Use physical coordinates for both Win32 window bounds and screen pixels.
    $oldDpiContext = [DshSmokeWindow]::SetThreadDpiAwarenessContext([IntPtr](-4))
    if ($oldDpiContext -eq [IntPtr]::Zero) { throw 'Could not select per-monitor DPI awareness' }
    try {
        $workArea = [DshSmokeWindow+Rect]::new()
        if (![DshSmokeWindow]::SystemParametersInfoW(0x30, 0, [ref]$workArea, 0)) {
            throw 'Could not read the runner desktop work area'
        }

        # Resize only the test window and keep it ahead of the runner's own windows.
        $screenWidth = $workArea.Right - $workArea.Left
        $screenHeight = $workArea.Bottom - $workArea.Top
        if (![DshSmokeWindow]::SetWindowPos($window, [IntPtr](-1),
                $workArea.Left, $workArea.Top, [Math]::Min(1440, $screenWidth), [Math]::Min(900, $screenHeight), 0x0240)) {
            throw 'Could not position the client window for screenshot'
        }

        $deadline = [DateTime]::UtcNow.AddSeconds(45)
        do {
            if ($App.HasExited) { throw 'Client exited before screenshot' }
            [DshSmokeWindow]::Focus($window)
            Start-Sleep -Milliseconds 750
            $bounds = [DshSmokeWindow+Rect]::new()
            $origin = [DshSmokeWindow+Point]::new()
            if (![DshSmokeWindow]::GetClientRect($window, [ref]$bounds) -or
                ![DshSmokeWindow]::ClientToScreen($window, [ref]$origin)) {
                throw 'Could not locate the client window on the runner desktop'
            }
            $width = $bounds.Right - $bounds.Left
            $height = $bounds.Bottom - $bounds.Top
            if ($width -lt 500 -or $height -lt 300 -or $origin.X -lt $workArea.Left -or
                $origin.Y -lt $workArea.Top -or ($origin.X + $width) -gt $workArea.Right -or
                ($origin.Y + $height) -gt $workArea.Bottom) {
                throw "Client window ($width x $height) does not fit the runner desktop ($screenWidth x $screenHeight)"
            }
            if ([DshSmokeWindow]::GetForegroundWindow() -ne $window -or
                ![DshSmokeWindow]::OwnsArea($window, $origin.X, $origin.Y, $width, $height)) {
                continue
            }

            $bitmap = [Drawing.Bitmap]::new($width, $height)
            try {
                $graphics = [Drawing.Graphics]::FromImage($bitmap)
                try {
                    # Capture the composed WebView2 surface. PrintWindow can omit it.
                    $graphics.CopyFromScreen($origin.X, $origin.Y, 0, 0, $bitmap.Size)
                } finally {
                    $graphics.Dispose()
                }
                if ([DshSmokeWindow]::GetForegroundWindow() -ne $window -or
                    ![DshSmokeWindow]::OwnsArea($window, $origin.X, $origin.Y, $width, $height)) {
                    continue
                }

                # The centered green status pill appears only after the shell shows its running view.
                $middle = [int]($width / 2)
                $running = $false
                for ($y = 6; $y -lt 65 -and !$running; $y += 2) {
                    for ($x = $middle - 65; $x -lt $middle + 65; $x += 2) {
                        $color = $bitmap.GetPixel($x, $y)
                        if ($color.G -gt 100 -and ($color.G - $color.R) -gt 65 -and ($color.G - $color.B) -gt 40) {
                            $running = $true
                            break
                        }
                    }
                }
                if (!$running) { continue }

                # Require visible detail in the content area as well as the status pill.
                # A blank WebView or a single-line authentication error is not a useful screenshot.
                $detailedRows = 0
                for ($y = 85; $y -lt ($height - 10) -and $detailedRows -lt 4; $y += 6) {
                    $previous = $bitmap.GetPixel(0, $y)
                    for ($x = 3; $x -lt $width; $x += 3) {
                        $color = $bitmap.GetPixel($x, $y)
                        $difference = [Math]::Abs($color.R - $previous.R) +
                            [Math]::Abs($color.G - $previous.G) + [Math]::Abs($color.B - $previous.B)
                        if ($difference -gt 150) {
                            $detailedRows++
                            break
                        }
                        $previous = $color
                    }
                }
                if ($detailedRows -lt 4) { continue }

                $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($Path))
                $bitmap.Save($Path, [Drawing.Imaging.ImageFormat]::Png)
                Write-Host "Saved client window screenshot ($width x $height): $Path"
                return
            } finally {
                $bitmap.Dispose()
            }
        } while ([DateTime]::UtcNow -lt $deadline)
    } finally {
        $null = [DshSmokeWindow]::SetThreadDpiAwarenessContext($oldDpiContext)
    }
    throw 'Client did not render a visible, unobstructed running window before screenshot deadline'
}

function Get-DshListener {
    Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort 3080 -State Listen -ErrorAction SilentlyContinue |
        Select-Object -First 1
}

function Read-PeMachine([string]$Path) {
    $reader = [IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        if ($reader.ReadUInt16() -ne 0x5A4D) { throw 'Installed application is missing the MZ header' }
        $reader.BaseStream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        $reader.BaseStream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x4550) { throw 'Installed application is missing the PE header' }
        return $reader.ReadUInt16()
    } finally {
        $reader.Dispose()
    }
}

function Get-DshSessionCookie([Net.Http.HttpClient]$Client, [string]$Token) {
    $reply = $Client.GetAsync("http://127.0.0.1:3080/?token=$Token").GetAwaiter().GetResult()
    try {
        if ([int]$reply.StatusCode -ne 303) { return }
        $header = $reply.Headers.GetValues('Set-Cookie') |
            Where-Object { $_ -like 'dsh-auth-*=*' } |
            Select-Object -First 1
        if ($header) { return ($header -split ';', 2)[0] }
    } finally {
        $reply.Dispose()
    }
}

function Read-DshHttpStatus([Net.Http.HttpClient]$Client, [string]$Cookie) {
    $request = [Net.Http.HttpRequestMessage]::new([Net.Http.HttpMethod]::Get, 'http://127.0.0.1:3080/')
    try {
        if ($Cookie) {
            $null = $request.Headers.TryAddWithoutValidation('Cookie', $Cookie)
        }
        $reply = $Client.SendAsync($request).GetAwaiter().GetResult()
        try {
            return [int]$reply.StatusCode
        } finally {
            $reply.Dispose()
        }
    } finally {
        $request.Dispose()
    }
}

function Stop-SmokeProcess([Diagnostics.Process]$Process) {
    if (!$Process) { return }
    try {
        if (!$Process.HasExited) {
            $Process.Kill($true)
            $null = $Process.WaitForExit(10000)
        }
    } catch {
        Write-Warning "Could not cleanly stop smoke process $($Process.Id)"
    }
}

function Test-SmokeService($Process) {
    if (!$Process -or $Process.Name -ine 'node.exe') { return $false }
    $exePath = [string]$Process.ExecutablePath
    if ($exePath.StartsWith('\\?\')) { $exePath = $exePath.Substring(4) }
    $commandLine = ([string]$Process.CommandLine).Replace('/', '\')
    if ($Variant -eq 'bundled') {
        return $exePath -ieq $bundledNode -and
            $commandLine.Contains($bundledDsh, [StringComparison]::OrdinalIgnoreCase)
    }
    return $exePath -ieq $systemNode -and
        $commandLine.Contains($npxCachePrefix, [StringComparison]::OrdinalIgnoreCase) -and
        ($commandLine -like '*\@deepseek-ai\dsh\lib\bin.js*' -or
            $commandLine -like '*\node_modules\.bin\dsh*')
}

$installerPath = (Resolve-Path -LiteralPath $Installer).Path
$ScreenshotPath = [IO.Path]::GetFullPath($ScreenshotPath, (Get-Location).ProviderPath)
if ($Variant -eq 'bundled' -and !$BundledDshVersion) { throw 'BundledDshVersion is required for the bundled installer' }
$probeDir = Join-Path ([IO.Path]::GetTempPath()) "dsh-smoke-$([Guid]::NewGuid().ToString('N'))"
$installDir = Join-Path $env:LOCALAPPDATA 'DeepSeekHarness'
if (Test-Path -LiteralPath $installDir) { throw 'Expected a fresh install location' }
$appPath = Join-Path $installDir 'DeepSeekHarness.exe'
$bundledNode = Join-Path $installDir 'runtime\nd\node.exe'
$bundledDsh = Join-Path $installDir 'runtime\rt\node_modules\@deepseek-ai\dsh\lib\bin.js'
$appConfig = Join-Path ([Environment]::GetFolderPath('ApplicationData')) 'cn.bingoogolapple.dsh'
$settingsDir = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.dsh\bga-dsh-client'
$npxCachePrefix = Join-Path $appConfig 'npm-cache\_npx'
if ((Test-Path -LiteralPath $appConfig) -or (Test-Path -LiteralPath $settingsDir) -or (Get-DshListener)) {
    throw 'Expected a fresh Windows runner with no client data and no listener on port 3080'
}
$systemNode = $null
if ($Variant -eq 'plain') {
    $systemNode = (Get-Command node.exe -CommandType Application | Select-Object -First 1).Source
    $actualNodeVersion = (& $systemNode --version)
    if ($LASTEXITCODE -ne 0 -or ($NodeVersion -and $actualNodeVersion.TrimStart('v') -ne $NodeVersion.TrimStart('v'))) {
        throw "Expected working system Node $NodeVersion, got $actualNodeVersion"
    }
    $null = Get-Command npx.cmd -CommandType Application
}

$envChanges = @{
    DSH_HOME = Join-Path $probeDir 'dsh'
    DSH_AGENTS_HOME = Join-Path $probeDir 'agents'
    DO_NOT_TRACK = '1'
    npm_config_cache = Join-Path $probeDir 'empty-external-npm-cache'
    WEBVIEW2_USER_DATA_FOLDER = Join-Path $probeDir 'webview'
}
$savedEnv = @{}
$setup = $null
$app = $null
$http = $null

try {
    $null = New-Item -ItemType Directory -Path $probeDir
    $setup = Start-Process -FilePath $installerPath -ArgumentList '/S' -PassThru
    if (!$setup.WaitForExit(300000)) { throw 'Silent NSIS installation timed out' }
    if ($setup.ExitCode -ne 0) { throw "Silent NSIS installation exited with $($setup.ExitCode)" }
    if (!(Test-Path -LiteralPath $appPath -PathType Leaf)) { throw 'NSIS did not install DeepSeekHarness.exe' }
    $productVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo($appPath).ProductVersion
    if ($productVersion -ne $AppVersion -and $productVersion -ne "$AppVersion.0") {
        throw "Expected application version $AppVersion, got $productVersion"
    }
    if ((Read-PeMachine $appPath) -ne 0x8664) { throw 'Installed executable is not amd64' }
    Write-Host "Installed $Variant application $productVersion (amd64)"

    foreach ($name in $envChanges.Keys) {
        $savedEnv[$name] = [Environment]::GetEnvironmentVariable($name)
        [Environment]::SetEnvironmentVariable($name, $envChanges[$name])
    }

    if ($Variant -eq 'bundled') {
        if (!(Test-Path -LiteralPath $bundledNode -PathType Leaf) -or
            !(Test-Path -LiteralPath $bundledDsh -PathType Leaf)) {
            throw 'Bundled installer is missing the runtime'
        }
        $dshVersion = (& $bundledNode $bundledDsh --version)
        if ($LASTEXITCODE -ne 0 -or $dshVersion -ne $BundledDshVersion) {
            throw "Expected bundled dsh version $BundledDshVersion, got $dshVersion"
        }
        Write-Host "Installed bundled CLI returns $dshVersion when run directly"
    } elseif (Test-Path -LiteralPath $bundledNode) {
        throw 'Plain installer unexpectedly includes the bundled Node runtime'
    }

    if ((Test-Path -LiteralPath $appConfig) -or (Test-Path -LiteralPath $settingsDir)) {
        throw 'Installer unexpectedly created app configuration or an npm cache'
    }
    Write-Host "Cold launch in the default install directory: $installDir"
    $startup = [Diagnostics.Stopwatch]::StartNew()
    $app = Start-Process -FilePath $appPath -WorkingDirectory $installDir -PassThru
    if ($app.SessionId -ne (Get-Process -Id $PID).SessionId) {
        throw 'Application did not start in the runner desktop session'
    }

    $serviceLog = Join-Path $appConfig 'service.log'
    $windowReported = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(780)
    do {
        $app.Refresh()
        if ($app.HasExited) { throw "Application exited early with code $($app.ExitCode)" }
        $listener = Get-DshListener
        if ($app.MainWindowHandle -ne [IntPtr]::Zero -and !$windowReported) {
            Write-Host 'Installed desktop window is visible'
            $windowReported = $true
        }
        if ($listener -and $windowReported) { break }
        if ((Test-Path -LiteralPath $serviceLog) -and
            (Select-String -LiteralPath $serviceLog -Pattern 'Service failed to start|服务启动失败' -Quiet)) {
            throw 'Installed app reported that dsh failed to start'
        }
        Start-Sleep -Seconds 1
    } while ([DateTime]::UtcNow -lt $deadline)
    if (!$listener -or $app.MainWindowHandle -eq [IntPtr]::Zero) {
        throw 'Desktop window or dsh listener did not appear'
    }

    $service = Get-CimInstance Win32_Process -Filter "ProcessId=$($listener.OwningProcess)"
    if (!(Test-SmokeService $service)) { throw "Port 3080 does not belong to the installed $Variant dsh" }
    $handler = [Net.Http.HttpClientHandler]::new()
    $handler.UseProxy = $false
    $handler.UseCookies = $false
    $handler.AllowAutoRedirect = $false
    $http = [Net.Http.HttpClient]::new($handler)
    $http.Timeout = [TimeSpan]::FromSeconds(3)

    $tokenPath = Join-Path $appConfig 'launch-token'
    $pidPath = Join-Path $appConfig 'service.pid'
    $authenticated = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(60)
    do {
        try {
            if ((Read-DshHttpStatus $http) -eq 401 -and
                (Test-Path -LiteralPath $tokenPath) -and (Test-Path -LiteralPath $pidPath)) {
                $token = (Get-Content -LiteralPath $tokenPath -Raw).Trim()
                if ($token -match '^[A-Za-z0-9_-]{32,}$') {
                    $cookie = Get-DshSessionCookie $http $token
                    if ($cookie) {
                        $authenticated = (Read-DshHttpStatus $http $cookie) -eq 200
                    }
                }
            }
        } catch {
            # The service may not be ready yet. Errors can contain the token URL.
        }
        if (!$authenticated) { Start-Sleep -Seconds 1 }
    } until ($authenticated -or [DateTime]::UtcNow -ge $deadline)
    if (!$authenticated) { throw 'dsh did not return unauthenticated HTTP 401 and authenticated HTTP 200' }
    $app.Refresh()
    if ($app.HasExited) { throw "Application exited early with code $($app.ExitCode)" }

    if ($Variant -eq 'plain') {
        $serviceText = Get-Content -LiteralPath $serviceLog -Raw
        if ($serviceText -notmatch 'npx --yes @deepseek-ai/dsh@latest web --no-open') {
            throw 'App did not use the default npx latest command'
        }
        $packages = @(Get-ChildItem -LiteralPath $npxCachePrefix -Directory | ForEach-Object {
            $manifest = Join-Path $_.FullName 'node_modules\@deepseek-ai\dsh\package.json'
            if (Test-Path -LiteralPath $manifest -PathType Leaf) { $manifest }
        })
        if ($packages.Count -ne 1) { throw 'Expected one dsh package in the newly populated app npm cache' }
        $dshVersion = (Get-Content -LiteralPath $packages[0] -Raw | ConvertFrom-Json).version
        if (!$dshVersion) { throw 'dsh package version was not recorded' }
    }
    $seconds = [Math]::Round($startup.Elapsed.TotalSeconds, 1)
    Save-ClientScreenshot -App $app -Path $ScreenshotPath
    Write-Host "PASS: $AppVersion $Variant desktop window opened, dsh $dshVersion returned authenticated HTTP 200 after $seconds seconds"
} catch {
    if ($Variant -eq 'plain') {
        $npmCache = Join-Path $appConfig 'npm-cache'
        $npmDebug = Get-ChildItem -LiteralPath (Join-Path $npmCache '_logs') -Filter '*-debug-0.log' -File -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($npmDebug) {
            Write-Host 'Last npm debug messages from the app launch'
            Get-Content -LiteralPath $npmDebug.FullName -Tail 30 |
                ForEach-Object { $_ -replace '(?i)(?:_auth(?:token)?|authorization|password|passwd|token)\s*[=:].*', '[redacted credential]' } |
                Write-Host
        }
        foreach ($npxDir in Get-ChildItem -LiteralPath $npxCachePrefix -Directory -ErrorAction SilentlyContinue) {
            $lock = Test-Path -LiteralPath (Join-Path $npxDir.FullName 'package-lock.json')
            $shim = Test-Path -LiteralPath (Join-Path $npxDir.FullName 'node_modules\.bin\dsh.cmd')
            $cli = Test-Path -LiteralPath (Join-Path $npxDir.FullName 'node_modules\@deepseek-ai\dsh\lib\bin.js')
            Write-Host "npx cache state: package-lock=$lock dsh.cmd=$shim dsh lib/bin.js=$cli"
        }
    }
    $serviceLog = Join-Path $appConfig 'service.log'
    if (Test-Path -LiteralPath $serviceLog) {
        Get-Content -LiteralPath $serviceLog -Tail 25 -ErrorAction SilentlyContinue |
            ForEach-Object { $_ -replace '(?i)(token=)[A-Za-z0-9_-]+', '$1[redacted]' } |
            Write-Host
    }
    throw
} finally {
    if ($http) { $http.Dispose() }
    $listener = Get-DshListener
    if ($listener) {
        $service = Get-CimInstance Win32_Process -Filter "ProcessId=$($listener.OwningProcess)" -ErrorAction SilentlyContinue
        if (Test-SmokeService $service) {
            Stop-Process -Id $service.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
    Stop-SmokeProcess $app
    Stop-SmokeProcess $setup
    foreach ($name in $savedEnv.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnv[$name])
    }
    $uninstaller = Join-Path $installDir 'uninstall.exe'
    if (Test-Path -LiteralPath $uninstaller -PathType Leaf) {
        $uninstall = Start-Process -FilePath $uninstaller -ArgumentList '/S' -PassThru -ErrorAction SilentlyContinue
        if ($uninstall -and !$uninstall.WaitForExit(120000)) { Stop-SmokeProcess $uninstall }
    }
    foreach ($path in @($probeDir, $appConfig, $settingsDir)) {
        if (Test-Path -LiteralPath $path) {
            Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}
