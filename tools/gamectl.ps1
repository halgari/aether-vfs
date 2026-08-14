# Drive the Skyrim window: screenshot it, and send scancode-level key input.
#
# Games read input via DirectInput / raw input, which ignores SendKeys and
# WM_KEYDOWN posts. SendInput with KEYEVENTF_SCANCODE is what actually
# registers, so that is what this uses.
#
#   gamectl.ps1 shot  <out.png>
#   gamectl.ps1 key   <NAME> [repeat]
#   gamectl.ps1 focus
#   gamectl.ps1 click <x> <y>    (window-relative pixels, matching a `shot` screenshot)
#   gamectl.ps1 launch [shim-stats-log-path]
#   gamectl.ps1 stats  <shim-stats-log-path> [skyrim-live-stderr-log-path]
#
# `launch`/`stats` drive `skyrim-live.exe` for the bypass-baseline measurement
# (rust/docs/bypass-baseline.md): `launch` starts it fully detached via
# Start-Process (NOT a job-object child of this script's own process — a
# `skyrim-live` launched as a backgrounded child dies when its parent task is
# reaped, which is not a durable session), with VFS_SHIM_STATS_LOG set so the
# injected shim turns on outcome classification. `stats` dumps the shim's
# "under-root open outcomes" section and, if given skyrim-live's own stderr
# log, the director-side `vfs-io opens: ok=.../err=...` line it prints itself
# (skyrim-live embeds the director directly — there is no separate `vfs-directord`
# gRPC daemon for a live game run, so there is no `vfs stats` endpoint to query;
# this reads the same `io_stats::open_totals()` numbers skyrim-live now prints
# to its own stderr).
param(
  [Parameter(Mandatory=$true)][string]$Action,
  [string]$Arg1,
  [string]$Arg2,
  [int]$Repeat = 1
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class SkyCtl {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);

  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT {
    public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo;
  }
  [StructLayout(LayoutKind.Explicit, Size=40)] public struct INPUT {
    [FieldOffset(0)] public uint type;
    [FieldOffset(8)] public KEYBDINPUT ki;
  }
  [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] p, int cb);

  // A few menus (e.g. the "content not present" Creations warning) render as
  // a dialog that only responds to a mouse click on Yes/No, not to a bound
  // key. `SetCursorPos` alone (tried first) silently fails here for the same
  // reason SendKeys fails for keyboard: it repositions the OS pointer without
  // ever posting a move event, so the game's own cursor sprite never budges
  // and the click lands wherever the game last thought the pointer was — not
  // where the screenshot shows it. `SendInput` with MOUSEEVENTF_MOVE|ABSOLUTE
  // (normalized to the virtual screen, same as a real relative-to-absolute
  // hardware report) is what actually moves the game's notion of the cursor,
  // mirroring why the header comment above insists on SendInput for keys.
  [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT {
    public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo;
  }
  [StructLayout(LayoutKind.Explicit, Size=40)] public struct MINPUT {
    [FieldOffset(0)] public uint type;
    [FieldOffset(8)] public MOUSEINPUT mi;
  }
  [DllImport("user32.dll", EntryPoint="SendInput")] public static extern uint SendMouseInput(uint n, MINPUT[] p, int cb);
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int nIndex);
  const int SM_XVIRTUALSCREEN = 76;
  const int SM_YVIRTUALSCREEN = 77;
  const int SM_CXVIRTUALSCREEN = 78;
  const int SM_CYVIRTUALSCREEN = 79;
  const uint MOUSEEVENTF_MOVE = 0x0001;
  const uint MOUSEEVENTF_ABSOLUTE = 0x8000;
  const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
  const uint MOUSEEVENTF_LEFTUP = 0x0004;
  public static void ClickAt(int screenX, int screenY) {
    int vx = GetSystemMetrics(SM_XVIRTUALSCREEN), vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    int vw = GetSystemMetrics(SM_CXVIRTUALSCREEN), vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    int nx = (int)(((long)(screenX - vx) * 65535) / vw);
    int ny = (int)(((long)(screenY - vy) * 65535) / vh);
    MINPUT[] mv = new MINPUT[1];
    mv[0].type = 0; mv[0].mi.dx = nx; mv[0].mi.dy = ny; mv[0].mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE;
    SendMouseInput(1, mv, Marshal.SizeOf(typeof(MINPUT)));
    System.Threading.Thread.Sleep(100);
    MINPUT[] down = new MINPUT[1]; down[0].type = 0; down[0].mi.dwFlags = MOUSEEVENTF_LEFTDOWN;
    SendMouseInput(1, down, Marshal.SizeOf(typeof(MINPUT)));
    System.Threading.Thread.Sleep(80);
    MINPUT[] up = new MINPUT[1]; up[0].type = 0; up[0].mi.dwFlags = MOUSEEVENTF_LEFTUP;
    SendMouseInput(1, up, Marshal.SizeOf(typeof(MINPUT)));
  }

  // Without this the capture runs DPI-virtualised: GetWindowRect hands back
  // logical coords, CopyFromScreen wants physical, and you silently screenshot
  // a magnified top-left crop of the window instead of the whole thing.
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
  public static void GoDpiAware() {
    try { SetThreadDpiAwarenessContext((IntPtr)(-4)); } catch {}   // PER_MONITOR_AWARE_V2
    try { SetProcessDPIAware(); } catch {}
  }

  [DllImport("user32.dll")] public static extern void SwitchToThisWindow(IntPtr h, bool alt);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint from, uint to, bool attach);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();

  // Windows refuses SetForegroundWindow from a process that does not already own
  // the foreground. Borrowing the current foreground thread's input queue lifts
  // that restriction, which is what makes SendInput land on the game.
  public static bool ForceForeground(IntPtr h) {
    uint me = GetCurrentThreadId();
    uint fgPid; uint fg = GetWindowThreadProcessId(GetForegroundWindow(), out fgPid);
    if (fg != 0 && fg != me) AttachThreadInput(me, fg, true);
    ShowWindow(h, 9);            // SW_RESTORE
    BringWindowToTop(h);
    SetForegroundWindow(h);
    SwitchToThisWindow(h, true);
    if (fg != 0 && fg != me) AttachThreadInput(me, fg, false);
    System.Threading.Thread.Sleep(250);
    return GetForegroundWindow() == h;
  }

  public static IntPtr FindGameWindow(int pid) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h,l) => {
      uint wp; GetWindowThreadProcessId(h, out wp);
      if ((uint)pid == wp && IsWindowVisible(h)) {
        RECT r; if (GetWindowRect(h, out r) && (r.R-r.L) > 100 && (r.B-r.T) > 100) { found = h; return false; }
      }
      return true;
    }, IntPtr.Zero);
    return found;
  }

  const uint KEYEVENTF_SCANCODE = 0x0008;
  const uint KEYEVENTF_KEYUP    = 0x0002;
  const uint KEYEVENTF_EXTENDED = 0x0001;

  public static void Tap(ushort scan, bool extended, int holdMs) {
    uint baseFlags = KEYEVENTF_SCANCODE | (extended ? KEYEVENTF_EXTENDED : 0);
    INPUT[] d = new INPUT[1];
    d[0].type = 1; d[0].ki.wScan = scan; d[0].ki.dwFlags = baseFlags;
    SendInput(1, d, Marshal.SizeOf(typeof(INPUT)));
    System.Threading.Thread.Sleep(holdMs);
    INPUT[] u = new INPUT[1];
    u[0].type = 1; u[0].ki.wScan = scan; u[0].ki.dwFlags = baseFlags | KEYEVENTF_KEYUP;
    SendInput(1, u, Marshal.SizeOf(typeof(INPUT)));
  }

  const ushort SCAN_LSHIFT = 0x2A;

  // Needed for console text with an underscore (Skyrim global-variable editor
  // IDs, e.g. Survival_PlayerHasBeenPrompted): underscore is Shift+Minus on a
  // US layout, and the existing Tap() has no way to hold a modifier down
  // across a second key. Holds left-shift, taps the given scan, releases
  // shift -- same SendInput/KEYEVENTF_SCANCODE mechanism the header comment
  // already mandates, just with the shift bracketing it.
  public static void TapShifted(ushort scan, int holdMs) {
    INPUT[] shiftDown = new INPUT[1];
    shiftDown[0].type = 1; shiftDown[0].ki.wScan = SCAN_LSHIFT; shiftDown[0].ki.dwFlags = KEYEVENTF_SCANCODE;
    SendInput(1, shiftDown, Marshal.SizeOf(typeof(INPUT)));
    System.Threading.Thread.Sleep(20);
    Tap(scan, false, holdMs);
    System.Threading.Thread.Sleep(20);
    INPUT[] shiftUp = new INPUT[1];
    shiftUp[0].type = 1; shiftUp[0].ki.wScan = SCAN_LSHIFT; shiftUp[0].ki.dwFlags = KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP;
    SendInput(1, shiftUp, Marshal.SizeOf(typeof(INPUT)));
  }
}
"@

[SkyCtl]::GoDpiAware()

# 'launch' starts the game (nothing to attach to yet) and 'stats' only reads
# log files already on disk — neither needs a running SkyrimSE process, so
# both are handled before the gate that every other action requires.
if ($Action.ToLower() -eq 'launch') {
  $repoRoot = Split-Path $PSScriptRoot -Parent
  $liveExe = Join-Path $repoRoot 'rust\target\release\skyrim-live.exe'
  if (-not (Test-Path $liveExe)) {
    "ERROR: $liveExe not found - build it first:`n  cargo build --release -p vfs-shim-dll`n  cargo build --release --manifest-path crates/vfs-payload/Cargo.toml --target-dir target`n  cargo build --release -p vfs-directord --bin skyrim-live"
    exit 1
  }
  $shimLog = if ($Arg1) { $Arg1 } else { 'C:\tmp\skyrim-data\perf\bypass-baseline-shim-stats.log' }
  $base = [System.IO.Path]::Combine([System.IO.Path]::GetDirectoryName($shimLog), [System.IO.Path]::GetFileNameWithoutExtension($shimLog))
  $outLog = "$base.live-out.log"
  $errLog = "$base.live-err.log"
  Remove-Item $shimLog, $outLog, $errLog -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force -Path (Split-Path $shimLog) | Out-Null
  # Inherited by the child (CreateProcessW with a null environment block
  # inherits the caller's env), and by SkyrimSE.exe/skse64_loader.exe in turn
  # -- the shim reads this from its own process environment once injected.
  $env:VFS_SHIM_STATS_LOG = $shimLog
  # Start-Process detaches fully: not a child job of this PowerShell process,
  # so it survives this script (and this tool call) exiting.
  $p = Start-Process -FilePath $liveExe -WorkingDirectory (Split-Path $liveExe) `
    -RedirectStandardOutput $outLog -RedirectStandardError $errLog -PassThru
  "launched skyrim-live pid=$($p.Id)`n  shim_log=$shimLog`n  live_out=$outLog`n  live_err=$errLog"
  exit 0
}
if ($Action.ToLower() -eq 'stats') {
  if (-not $Arg1) { "ERROR: need a shim-stats-log path"; exit 1 }
  if (-not (Test-Path $Arg1)) { "ERROR: shim stats log not found: $Arg1"; exit 1 }
  $text = Get-Content $Arg1 -Raw
  $idx = $text.IndexOf('under-root open outcomes:')
  "=== shim: under-root open outcomes ($Arg1) ==="
  if ($idx -ge 0) { $text.Substring($idx) } else { "(section absent - reporter never ticked, or nothing under-root was opened yet)" }
  if ($Arg2) {
    "=== director: open totals (from skyrim-live's own stderr, $Arg2) ==="
    if (Test-Path $Arg2) {
      $lines = Select-String -Path $Arg2 -Pattern 'vfs-io opens: ok=' | Select-Object -Last 1
      if ($lines) { $lines.Line } else { "(no 'vfs-io opens:' line yet - game may still be starting)" }
    } else {
      "ERROR: $Arg2 not found"
    }
  }
  exit 0
}

$proc = Get-Process -Name SkyrimSE -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $proc) { "ERROR: SkyrimSE not running"; exit 1 }
$hwnd = [SkyCtl]::FindGameWindow($proc.Id)
if ($hwnd -eq [IntPtr]::Zero) { "ERROR: no visible game window"; exit 1 }

# Scancode set 1. Extended keys need the extended flag or the game misreads them.
$scan = @{
  'UP'=@(0xC8,$true); 'DOWN'=@(0xD0,$true); 'LEFT'=@(0xCB,$true); 'RIGHT'=@(0xCD,$true)
  'ENTER'=@(0x1C,$false); 'ESC'=@(0x01,$false); 'SPACE'=@(0x39,$false)
  # GRAVE is an alias for TILDE — same physical key (scancode 0x29), the one
  # that toggles the Skyrim console. Both names accepted so `key GRAVE` works.
  'TILDE'=@(0x29,$false); 'GRAVE'=@(0x29,$false); 'E'=@(0x12,$false); 'R'=@(0x13,$false)
}

# Set-1 scancodes for console text. The game reads scancodes, not characters,
# so a console command has to be spelled out key by key.
$chars = @{
  'a'=0x1E;'b'=0x30;'c'=0x2E;'d'=0x20;'e'=0x12;'f'=0x21;'g'=0x22;'h'=0x23;'i'=0x17
  'j'=0x24;'k'=0x25;'l'=0x26;'m'=0x32;'n'=0x31;'o'=0x18;'p'=0x19;'q'=0x10;'r'=0x13
  's'=0x1F;'t'=0x14;'u'=0x16;'v'=0x2F;'w'=0x11;'x'=0x2D;'y'=0x15;'z'=0x2C
  '0'=0x0B;'1'=0x02;'2'=0x03;'3'=0x04;'4'=0x05;'5'=0x06;'6'=0x07;'7'=0x08;'8'=0x09;'9'=0x0A
  ' '=0x39;'.'=0x34;'-'=0x0C
}
# Underscore is Shift+Minus on a US layout -- handled via TapShifted, not a
# plain scancode, so it's kept out of $chars and special-cased in 'type'.
$shiftedChars = @{ '_'=0x0C }

switch ($Action.ToLower()) {
  'focus' {
    $ok = [SkyCtl]::ForceForeground($hwnd)
    "focused hwnd=0x{0:x} foreground={1}" -f [int64]$hwnd, $ok
  }
  'shot' {
    if (-not $Arg1) { "ERROR: need output path"; exit 1 }
    $r = New-Object SkyCtl+RECT
    [void][SkyCtl]::GetWindowRect($hwnd, [ref]$r)
    $w = $r.R - $r.L; $h = $r.B - $r.T
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $gfx = [System.Drawing.Graphics]::FromImage($bmp)
    $gfx.CopyFromScreen($r.L, $r.T, 0, 0, (New-Object System.Drawing.Size($w, $h)))
    $bmp.Save($Arg1, [System.Drawing.Imaging.ImageFormat]::Png)
    $gfx.Dispose(); $bmp.Dispose()
    "saved $Arg1 (${w}x${h}) from pid $($proc.Id)"
  }
  'key' {
    if (-not $scan.ContainsKey($Arg1.ToUpper())) { "ERROR: unknown key $Arg1"; exit 1 }
    if (-not [SkyCtl]::ForceForeground($hwnd)) { "WARN: window did not take foreground; keys may be lost" }
    $s = $scan[$Arg1.ToUpper()]
    for ($i = 0; $i -lt $Repeat; $i++) {
      [SkyCtl]::Tap([uint16]$s[0], [bool]$s[1], 60)
      Start-Sleep -Milliseconds 220
    }
    "sent $Arg1 x$Repeat to pid $($proc.Id)"
  }
  'click' {
    if (-not $Arg1 -or -not $Arg2) { "ERROR: need x and y (window-relative pixels, matching a `shot` screenshot)"; exit 1 }
    if (-not [SkyCtl]::ForceForeground($hwnd)) { "WARN: window did not take foreground; click may be lost" }
    $r = New-Object SkyCtl+RECT
    [void][SkyCtl]::GetWindowRect($hwnd, [ref]$r)
    $sx = $r.L + [int]$Arg1
    $sy = $r.T + [int]$Arg2
    [SkyCtl]::ClickAt($sx, $sy)
    "clicked window-relative ($Arg1,$Arg2) -> screen ($sx,$sy) on pid $($proc.Id)"
  }
  'type' {
    if (-not $Arg1) { "ERROR: need text"; exit 1 }
    if (-not [SkyCtl]::ForceForeground($hwnd)) { "WARN: window did not take foreground; keys may be lost" }
    foreach ($ch in $Arg1.ToLower().ToCharArray()) {
      $k = [string]$ch
      if ($shiftedChars.ContainsKey($k)) {
        [SkyCtl]::TapShifted([uint16]$shiftedChars[$k], 35)
        Start-Sleep -Milliseconds 45
        continue
      }
      if (-not $chars.ContainsKey($k)) { "WARN: no scancode for '$k'"; continue }
      [SkyCtl]::Tap([uint16]$chars[$k], $false, 35)
      Start-Sleep -Milliseconds 45
    }
    "typed '$Arg1' to pid $($proc.Id)"
  }
  default { "ERROR: unknown action $Action"; exit 1 }
}
