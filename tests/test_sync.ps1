# A/V sync test: play a clip whose white flash and 1 kHz tone happen at the SAME
# instant, record it with Snapper, then measure the offset between the detected
# video flash and audio tone in the recording. Offset ~0 means in sync.
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
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
public class W {
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string name);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr parent, IntPtr after, string cls, string name);
  [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
  public static void ClickButton(string name){
    IntPtr bar = FindWindowW("SnapperCtl", null);
    if(bar==IntPtr.Zero) bar = FindWindowW("SnapperAction", null);
    if(bar==IntPtr.Zero){ Console.WriteLine("    WARN: no button bar window"); return; }
    int id = name=="Record" ? 1 : name=="Screenshot" ? 2 : name=="Stop" ? 2 : 1; // Pause/Resume=1
    SendMessageW(bar, 0x8010, (IntPtr)id, IntPtr.Zero); // WM_SNAP_CLICK
  }
}
"@

$clip = Join-Path $env:TEMP "syncclip.mp4"
# 6 s clip: full-screen white flash at t=3.00 (0.12 s) + 1 kHz tone at t=3.00 (0.12 s)
ffmpeg -v error -y -f lavfi -i 'color=c=black:s=1280x720:r=30:d=6,geq=lum=255*between(T\,3\,3.12):cb=128:cr=128' -f lavfi -i 'sine=frequency=1000:duration=0.12' -filter_complex '[1:a]adelay=3000|3000[a]' -map 0:v -map '[a]' -c:v libx264 -pix_fmt yuv420p -c:a aac $clip
if (-not (Test-Path $clip)) { "clip gen failed"; exit 1 }

$outDir = Join-Path $env:USERPROFILE "Videos\Snapper"
$before = @(Get-ChildItem $outDir -Filter *.mp4 -ErrorAction SilentlyContinue)
$err = Join-Path $env:TEMP "snapper_err.txt"
$o = Join-Path $env:TEMP "snapper_out.txt"
Get-Process ffmpeg,snapper,ffplay -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
$app = Start-Process -FilePath "target\debug\snapper.exe" -PassThru -WindowStyle Hidden -RedirectStandardError $err -RedirectStandardOutput $o
Start-Sleep 3

"[1] hotkey -> overlay"
[Inj]::Hotkey(); Start-Sleep 2
"[2] drag selection (400,300) -> (1200,800)"
[Inj]::Move(400,300); Start-Sleep -Milliseconds 300
[Inj]::Down();   Start-Sleep -Milliseconds 300
[Inj]::Move(1200,800); Start-Sleep -Milliseconds 300
[Inj]::Up();     Start-Sleep 1
"[3] chooser -> Record"
[W]::ClickButton("Record"); Start-Sleep 1
"[4] play clip fullscreen (tone+flash at t=3)"
$play = Start-Process -FilePath "ffplay" -ArgumentList "-v","error","-fs","-autoexit",$clip -PassThru
Start-Sleep 6
"[5] ctl Stop"
[W]::ClickButton("Stop"); Start-Sleep 3
Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
Get-Process ffmpeg,ffplay -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

$after = @(Get-ChildItem $outDir -Filter *.mp4 -ErrorAction SilentlyContinue)
$rec = $after | Where-Object { $before.Name -notcontains $_.Name } | Select-Object -First 1
if (-not $rec) { "NO NEW FILE"; Get-Content $err; exit 1 }
"REC: $($rec.FullName)"

# audio tone start = end of the leading silence
$ad = ffmpeg -i $rec.FullName -af "silencedetect=n=-35dB:d=0.25" -f null - 2>&1 | Select-String "silence_end" | Select-Object -First 1
# video flash start = first big scene change
$vd = ffmpeg -i $rec.FullName -vf "select=gt(scene\,0.3),showinfo" -vsync vfr -f null - 2>&1 | Select-String "pts_time:" | Select-Object -First 1
"audio silence_end : $($ad.Line.Trim())"
"video scene change: $($vd.Line.Trim())"
"--- app stderr:"
Get-Content $err -ErrorAction SilentlyContinue
