use anyhow::{anyhow, Result};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, eConsole, eRender, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_OUTBOUND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};
use windows::Win32::Storage::FileSystem::WriteFile;

const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 2;
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

#[derive(Debug, Clone, Copy)]
pub struct MixFormat {
    pub rate: u32,
    pub channels: u16,
    pub block_align: usize,
    pub ffmpeg_fmt: &'static str,
}

fn render_device() -> Result<IMMDevice> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
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
            // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT has Data1 == 3
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

pub fn query_mix_format() -> Result<MixFormat> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED);
        let device = render_device()?;
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

pub struct AudioCapture {
    stop: Arc<AtomicBool>,
}

impl AudioCapture {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub fn create_pipe(name: &str) -> Result<HANDLE> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let handle = CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_OUTBOUND,
            windows::Win32::System::Pipes::PIPE_TYPE_BYTE
                | windows::Win32::System::Pipes::PIPE_READMODE_BYTE
                | windows::Win32::System::Pipes::PIPE_WAIT,
            1,
            0,
            4 * 1024 * 1024,
            0,
            None,
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(anyhow!("CreateNamedPipeW failed"));
        }
        Ok(handle)
    }
}

pub fn start(pipe: HANDLE, stop: Arc<AtomicBool>) {
    let pipe_raw = pipe.0 as isize;
    std::thread::spawn(move || {
        let pipe = HANDLE(pipe_raw as *mut c_void);
        if let Err(e) = run_loopback(pipe, &stop) {
            eprintln!("[audio] loopback capture ended: {e}");
        }
        unsafe {
            let _ = CloseHandle(pipe);
        }
    });
}

fn run_loopback(pipe: HANDLE, stop: &AtomicBool) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED);
        let device = render_device()?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let wf = client.GetMixFormat()?;
        if wf.is_null() {
            return Err(anyhow!("null mix format"));
        }
        let block_align = (*wf).nBlockAlign as usize;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            0,
            0,
            wf,
            None,
        )?;
        CoTaskMemFree(Some(wf as *const c_void));
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;

        if ConnectNamedPipe(pipe, None).is_err() {
            client.Stop().ok();
            return Err(anyhow!("ffmpeg never connected to audio pipe"));
        }

        while !stop.load(Ordering::SeqCst) {
            let packets = match capture.GetNextPacketSize() {
                Ok(n) => n,
                Err(_) => break,
            };
            if packets == 0 {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            for _ in 0..packets {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let len = frames as usize * block_align;
                let mut written = 0u32;
                let res = if len == 0 {
                    Ok(())
                } else if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 || data.is_null() {
                    WriteFile(pipe, Some(&vec![0u8; len]), Some(&mut written), None)
                } else {
                    WriteFile(
                        pipe,
                        Some(std::slice::from_raw_parts(data, len)),
                        Some(&mut written),
                        None,
                    )
                };
                let _ = capture.ReleaseBuffer(frames);
                if res.is_err() {
                    client.Stop().ok();
                    return Ok(()); // ffmpeg closed the pipe: recording finished
                }
            }
        }
        client.Stop().ok();
    }
    Ok(())
}
