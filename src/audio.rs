use anyhow::{anyhow, Result};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, ERROR_PIPE_CONNECTED,
};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, eCapture, eConsole, eRender,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Storage::FileSystem::{PIPE_ACCESS_OUTBOUND, WriteFile};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 2;
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

#[derive(Debug, Clone, Copy)]
pub struct MixFormat {
    pub rate: u32,
    pub channels: u16,
    pub block_align: usize,
    pub ffmpeg_fmt: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub enum Source {
    Loopback,
    Mic,
}

fn device_for(source: Source) -> Result<IMMDevice> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let flow = match source {
            Source::Loopback => eRender,
            Source::Mic => eCapture,
        };
        let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole)?;
        Ok(device)
    }
}

fn parse_wf(wf: *const WAVEFORMATEX) -> MixFormat {
    unsafe {
        let w = &*wf;
        let mut fmt = "f32le";
        if w.wFormatTag == WAVE_FORMAT_PCM {
            fmt = "s16le";
        } else if w.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            let ext = std::ptr::read_unaligned(wf as *const WAVEFORMATEXTENSIBLE);
            // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT has data1 == 3
            if ext.SubFormat.data1 != 3 {
                fmt = "s16le";
            }
        }
        MixFormat {
            rate: w.nSamplesPerSec,
            channels: w.nChannels,
            block_align: w.nBlockAlign as usize,
            ffmpeg_fmt: fmt,
        }
    }
}

pub fn query_mix_format(source: Source) -> Result<MixFormat> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let device = device_for(source)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let wf = client.GetMixFormat()?;
        if wf.is_null() {
            return Err(anyhow!("null mix format"));
        }
        let mf = parse_wf(wf);
        CoTaskMemFree(Some(wf as *const c_void));
        Ok(mf)
    }
}

/// Create a server named pipe (server writes → ffmpeg reads). Returns the raw
/// HANDLE; the caller passes it to `start` and must CloseHandle it to unblock.
pub fn create_pipe(name: &str) -> Result<isize> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_OUTBOUND,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            4 * 1024 * 1024,
            0,
            0,
            None,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(anyhow!("CreateNamedPipeW failed for {name}"));
    }
    Ok(h.0 as isize)
}

/// Capture one source into a named pipe until `stop` is set or writes fail.
pub fn start(source: Source, pipe_raw: isize, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        if let Err(e) = run_capture(source, HANDLE(pipe_raw as *mut c_void), &stop) {
            eprintln!("[audio] {source:?} capture ended: {e}");
        }
    });
}

fn run_capture(source: Source, pipe: HANDLE, stop: &AtomicBool) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        // Accept the client (the audio recorder ffmpeg) immediately, BEFORE the
        // (sometimes slow) WASAPI Initialize below, and prime the pipe with
        // silence. This lets the recorder's raw-PCM demuxer establish the audio
        // stream and start at the same instant as the video grab, so audio and
        // the separately-recorded video line up instead of audio lagging by the
        // WASAPI init time.
        match ConnectNamedPipe(pipe, None) {
            Ok(()) => {}
            Err(_) if GetLastError() == ERROR_PIPE_CONNECTED => {}
            Err(e) => return Err(anyhow!("ConnectNamedPipe: {e}")),
        }
        let _ = write_pipe(&pipe, &vec![0u8; 8192]);

        let device = device_for(source)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let wf = client.GetMixFormat()?;
        if wf.is_null() {
            return Err(anyhow!("null mix format"));
        }
        let mf = parse_wf(wf);
        let block_align = mf.block_align;
        let flags = match source {
            Source::Loopback => AUDCLNT_STREAMFLAGS_LOOPBACK,
            Source::Mic => 0,
        };
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, wf, None)?;
        CoTaskMemFree(Some(wf as *const c_void));
        let capture: IAudioCaptureClient = client.GetService()?;

        client.Start()?;
        let mut last_write = std::time::Instant::now();
        // WASAPI loopback delivers nothing while the system is silent; feed
        // silence so sample-count timestamps keep tracking wall-clock time.
        let silence_chunk = vec![0u8; (mf.rate / 50) as usize * block_align];

        while !stop.load(Ordering::SeqCst) {
            let packets = match capture.GetNextPacketSize() {
                Ok(n) => n,
                Err(_) => break,
            };
            if packets == 0 {
                if last_write.elapsed() >= Duration::from_millis(20) {
                    if write_pipe(&pipe, &silence_chunk).is_err() {
                        break;
                    }
                    last_write = std::time::Instant::now();
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            for _ in 0..packets {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let len = frames as usize * block_align;
                let res = if len == 0 {
                    Ok(())
                } else if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 || data.is_null() {
                    write_pipe(&pipe, &vec![0u8; len])
                } else {
                    write_pipe(&pipe, std::slice::from_raw_parts(data, len))
                };
                let _ = capture.ReleaseBuffer(frames);
                if res.is_err() {
                    break;
                }
                last_write = std::time::Instant::now();
            }
        }
        client.Stop().ok();
    }
    Ok(())
}

fn write_pipe(pipe: &HANDLE, buf: &[u8]) -> Result<()> {
    let mut written = 0u32;
    unsafe {
        WriteFile(*pipe, Some(buf), Some(&mut written), None)
            .map_err(|_| anyhow!("pipe write failed"))
    }
}

pub fn close_pipe(pipe_raw: isize) {
    if pipe_raw != 0 {
        unsafe {
            let _ = CloseHandle(HANDLE(pipe_raw as *mut c_void));
        }
    }
}
