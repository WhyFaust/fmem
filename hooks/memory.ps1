param([string] $EventName, [string] $EncodedWorkingDirectory)
$ErrorActionPreference = 'Stop'
try {
    if ($env:PLUGIN_ROOT) {
        $binary = [IO.Path]::Combine($env:PLUGIN_ROOT, 'fmem.exe')
        if ([IO.File]::Exists($binary)) {
            $start = New-Object Diagnostics.ProcessStartInfo
            $start.FileName = $binary
            $start.WorkingDirectory = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($EncodedWorkingDirectory))
            $start.UseShellExecute = $false
            $start.Arguments = if ($EventName -ceq 'WorkerHook') {
                'codex worker-hook'
            } else {
                'codex hook ' + $EventName
            }
            $process = [Diagnostics.Process]::Start($start)
            try {
                $process.WaitForExit()
                if ($process.ExitCode -eq 0) { exit 0 }
            } finally {
                $process.Dispose()
            }
        }
    }
} catch {}
[Console]::WriteLine('{}')
