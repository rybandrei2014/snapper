Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @"
using System;using System.Runtime.InteropServices;
public class W {
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string name);
  [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int left, top, right, bottom; }
  public static void Click(int id){
    IntPtr bar = FindWindowW("SnapperCtl", null);
    if(bar==IntPtr.Zero) bar = FindWindowW("SnapperAction", null);
    if(bar==IntPtr.Zero){ Console.WriteLine("    WARN: no bar window"); return; }
    SendMessageW(bar, 0x8010, (IntPtr)id, IntPtr.Zero);
  }
}
"@
Add-Type -TypeDefinition @"
using System;using System.Runtime.InteropServices;
public class Inj {
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int idx);
  public static void Key(byte vk, bool down){ keybd_event(vk,0,(uint)(down?0:2),UIntPtr.Zero); }
  public static void Hotkey(){ Key(0x5B,true); Key(0x10,true); Key(0x52,true); Key(0x52,false); Key(0x10,false); Key(0x5B,false); }
  public static void Move(int x,int y){
    int vw=GetSystemMetrics(78), vh=GetSystemMetrics(79), vx=GetSystemMetrics(76), vy=GetSystemMetrics(77);
    uint ax=(uint)(((long)(x-vx)*65535)/(vw-1)); uint ay=(uint)(((long)(y-vy)*65535)/(vh-1));
    mouse_event(0x0001|0x8000, ax, ay, 0, UIntPtr.Zero);
  }
  public static void Down(){ mouse_event(0x0002,0,0,0,UIntPtr.Zero); }
  public static void Up(){ mouse_event(0x0004,0,0,0,UIntPtr.Zero); }
}
"@

function Shot($cls, $path) {
  $h = [W]::FindWindowW($cls, $null)
  if ($h -eq [IntPtr]::Zero) { "  MISSING window $cls"; return }
  $r = New-Object W+RECT
  [void][W]::GetWindowRect($h, [ref]$r)
  $w = $r.right - $r.left; $ht = $r.bottom - $r.top
  $bmp = New-Object System.Drawing.Bitmap($w, $ht)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($r.left, $r.top, 0, 0, $bmp.Size)
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
  "  saved $path ($($r.right-$r.left)x$($r.bottom-$r.top))"
}

$err = Join-Path $env:TEMP "snapper_err.txt"
$o = Join-Path $env:TEMP "snapper_out.txt"
Get-Process ffmpeg,snapper -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
$app = Start-Process -FilePath "target\debug\snapper.exe" -PassThru -WindowStyle Hidden -RedirectStandardError $err -RedirectStandardOutput $o
Start-Sleep 3

"[1] hotkey -> overlay"
[Inj]::Hotkey(); Start-Sleep 2
"[2] drag selection"
[Inj]::Move(400,300); Start-Sleep -Milliseconds 300
[Inj]::Down(); Start-Sleep -Milliseconds 300
[Inj]::Move(1200,800); Start-Sleep -Milliseconds 300
[Inj]::Up(); Start-Sleep 1

"[3] chooser shot"
Shot "SnapperAction" (Join-Path $env:TEMP "v2_chooser.png")

"[4] record"
[W]::Click(1); Start-Sleep 1
0..4 | ForEach-Object { [Inj]::Move(600 + ($_ % 2) * 40, 500); Start-Sleep -Milliseconds 400 }
Shot "SnapperCtl" (Join-Path $env:TEMP "v2_rec.png")

"[5] pause"
[W]::Click(1); Start-Sleep 1
Shot "SnapperCtl" (Join-Path $env:TEMP "v2_pause.png")

"[6] stop"
[W]::Click(2); Start-Sleep 2
Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
Get-Process ffmpeg -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
"--- stderr:"
Get-Content $err -ErrorAction SilentlyContinue
