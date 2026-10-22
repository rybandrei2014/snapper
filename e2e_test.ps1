Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class Inj {
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int idx);
  public static void Key(byte vk, bool down){ keybd_event(vk,0,(uint)(down?0:2),UIntPtr.Zero); }
  public static void Hotkey(){ Key(0x5B,true); Key(0x10,true); Key(0x52,true); Key(0x52,false); Key(0x10,false); Key(0x5B,false); }
  public static void HotkeyP(){ Key(0x5B,true); Key(0x12,true); Key(0x50,true); Key(0x50,false); Key(0x12,false); Key(0x5B,false); }
  public static void Move(int x,int y){
    int vw=GetSystemMetrics(78), vh=GetSystemMetrics(79), vx=GetSystemMetrics(76), vy=GetSystemMetrics(77);
    uint ax=(uint)(((long)(x-vx)*65535)/(vw-1)); uint ay=(uint)(((long)(y-vy)*65535)/(vh-1));
    mouse_event(0x0001|0x8000, ax, ay, 0, UIntPtr.Zero);
  }
  public static void Down(){ mouse_event(0x0002,0,0,0,UIntPtr.Zero); }
  public static void Up(){ mouse_event(0x0004,0,0,0,UIntPtr.Zero); }
}
"@

Add-Type -TypeDefinition @"
using System;using System.Runtime.InteropServices;
public class W {
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string name);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr parent, IntPtr after, string cls, string name);
  [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
  public static void ClickButton(string name){
    IntPtr bar = FindWindowW("SnapperCtl", null);
    if(bar==IntPtr.Zero) bar = FindWindowW("SnapperAction", null);
    if(bar==IntPtr.Zero){ Console.WriteLine("    WARN: no button bar window"); return; }
    IntPtr btn = FindWindowExW(bar, IntPtr.Zero, "BUTTON", name);
    if(btn==IntPtr.Zero){ Console.WriteLine("    WARN: no button '" + name + "'"); return; }
    SendMessageW(btn, 0x00F5, IntPtr.Zero, IntPtr.Zero); // BM_CLICK
  }
}
"@

$outDir = Join-Path $env:USERPROFILE "Videos\Snapper"
$before = @(Get-ChildItem $outDir -Filter *.mp4 -ErrorAction SilentlyContinue)

$err = Join-Path $env:TEMP "snapper_err.txt"
$o = Join-Path $env:TEMP "snapper_out.txt"
Get-Process ffmpeg,snapper -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
$app = Start-Process -FilePath "target\debug\snapper.exe" -PassThru -WindowStyle Hidden -RedirectStandardError $err -RedirectStandardOutput $o
Start-Sleep 3

"[1] hotkey -> overlay"
[Inj]::Hotkey()
Start-Sleep 2
$hwnd = [W]::FindWindowW("SnapperOverlay", $null)
"    overlay hwnd: $hwnd"

"[2] drag selection (400,300) -> (1200,800)"
[Inj]::Move(400,300); Start-Sleep -Milliseconds 300
[Inj]::Down();   Start-Sleep -Milliseconds 300
[Inj]::Move(800,550); Start-Sleep -Milliseconds 300
[Inj]::Move(1200,800); Start-Sleep -Milliseconds 300
[Inj]::Up();     Start-Sleep 1

"[3] chooser -> click Record"
[W]::ClickButton("Record")
Start-Sleep 1

"[4] recording 3s (jiggle mouse so ddagrab produces frames)"
0..6 | ForEach-Object { [Inj]::Move(600 + ($_ % 2) * 40, 500); Start-Sleep -Milliseconds 400 }

"[5] ctl bar: Pause, wait 2s, Resume"
$ctl = [W]::FindWindowW("SnapperCtl", $null)
"    ctl bar hwnd: $ctl"
[W]::ClickButton("Pause")
Start-Sleep 2
[W]::ClickButton("Resume")
0..6 | ForEach-Object { [Inj]::Move(600 + ($_ % 2) * 40, 500); Start-Sleep -Milliseconds 400 }

"[6] ctl bar: Stop"
[W]::ClickButton("Stop")
Start-Sleep 3

Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
Get-Process ffmpeg -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

$after = @(Get-ChildItem $outDir -Filter *.mp4 -ErrorAction SilentlyContinue)
$new = $after | Where-Object { $before.Name -notcontains $_.Name }
if ($new) {
  "NEW FILE: $($new[0].FullName)  size=$($new[0].Length)"
  ffprobe -v error -show_entries format=duration,size -of default=nw=1 $new[0].FullName
} else {
  "NO NEW FILE PRODUCED"
}
"--- app stderr:"
Get-Content $err -ErrorAction SilentlyContinue
