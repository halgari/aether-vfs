# Drive the Skyrim window: screenshot it, and send scancode-level key input.
#
# Games read input via DirectInput / raw input, which ignores SendKeys and
# WM_KEYDOWN posts. SendInput with KEYEVENTF_SCANCODE is what actually
# registers, so that is what this uses.
#
#   gamectl.ps1 shot  <out.png>
#   gamectl.ps1 key   <NAME> [repeat]
#   gamectl.ps1 focus
param(
  [Parameter(Mandatory=$true)][string]$Action,
  [string]$Arg1,
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
}
"@

[SkyCtl]::GoDpiAware()

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
  'type' {
    if (-not $Arg1) { "ERROR: need text"; exit 1 }
    if (-not [SkyCtl]::ForceForeground($hwnd)) { "WARN: window did not take foreground; keys may be lost" }
    foreach ($ch in $Arg1.ToLower().ToCharArray()) {
      $k = [string]$ch
      if (-not $chars.ContainsKey($k)) { "WARN: no scancode for '$k'"; continue }
      [SkyCtl]::Tap([uint16]$chars[$k], $false, 35)
      Start-Sleep -Milliseconds 45
    }
    "typed '$Arg1' to pid $($proc.Id)"
  }
  default { "ERROR: unknown action $Action"; exit 1 }
}
