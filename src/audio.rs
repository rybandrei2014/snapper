use anyhow::{anyhow, Result};
use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, eCapture, eConsole, eRender,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

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

/// Capture one source into a raw PCM file until `stop` is set or writes fail.
pub fn start(source: Source, path: PathBuf, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        if let Err(e) = run_capture(source, &path, &stop) {
            eprintln!("[audio] {source:?} capture ended: {e}");
        }
    });
}

fn run_capture(source: Source, path: &Path, stop: &AtomicBool) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
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

        let mut file = File::create(path)?;
        let mut last_write = std::time::Instant::now();
        // WASAPI loopback delivers nothing while the system is silent;
        // synthesize silence so the stream stays continuous in real time.
        let silence_chunk = vec![0u8; (mf.rate / 50) as usize * block_align];

        while !stop.load(Ordering::SeqCst) {
            let packets = match capture.GetNextPacketSize() {
                Ok(n) => n,
                Err(_) => break,
            };
            if packets == 0 {
                if last_write.elapsed() >= Duration::from_millis(20) {
                    if file.write_all(&silence_chunk).is_err() {
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
                    file.write_all(&vec![0u8; len])
                } else {
                    file.write_all(std::slice::from_raw_parts(data, len))
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
