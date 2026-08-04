use std::collections::HashMap;
use std::io;
use std::mem::{align_of, size_of};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W,
    PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

mod framing;

use framing::read_json_record;

const PROTOCOL_VERSION: u32 = 5;

#[derive(Deserialize)]
#[serde(tag = "type")]
enum HostFrame {
    #[serde(rename = "init")]
    Init {
        v: u32,
        #[serde(rename = "layerSettings")]
        _layer_settings: Value,
        #[serde(rename = "deviceSettings")]
        device_settings: Value,
    },
    #[serde(rename = "settings")]
    Settings {
        v: u32,
        #[serde(rename = "layerSettings")]
        _layer_settings: Value,
        #[serde(rename = "deviceSettings")]
        device_settings: Value,
    },
    #[serde(rename = "message")]
    Message {
        v: u32,
        #[serde(rename = "surface")]
        _surface: RendererSurface,
        #[serde(rename = "payload")]
        _payload: Value,
    },
    #[serde(rename = "shutdown")]
    Shutdown { v: u32 },
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum CompanionFrame<T: Serialize> {
    #[serde(rename = "ready")]
    Ready { v: u32 },
    #[serde(rename = "message")]
    Message {
        v: u32,
        target: MessageTarget,
        payload: T,
    },
    #[serde(rename = "error")]
    Error {
        v: u32,
        message: String,
        code: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RendererSurface {
    Interface,
    Wallpaper,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
enum MessageTarget {
    Broadcast,
}

#[derive(Serialize)]
#[serde(tag = "kind")]
enum Payload {
    #[serde(rename = "system.sample")]
    Sample(SystemSample),
    #[serde(rename = "system.error")]
    Error { message: String },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SystemSample {
    captured_at_unix_ms: u128,
    cpu: CpuSample,
    memory: MemorySample,
    gpu: Option<GpuSample>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CpuSample {
    usage_percent: f64,
    logical_processors: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MemorySample {
    used_bytes: u64,
    total_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GpuSample {
    name: String,
    usage_percent: Option<f64>,
    dedicated_total_bytes: u64,
}

struct Control {
    running: bool,
    interval: Duration,
}

struct Sampler {
    previous_cpu: Option<CpuTimes>,
    gpu: Option<GpuAdapter>,
    gpu_utilization: Option<GpuUtilization>,
}

#[derive(Clone, Copy)]
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

struct GpuAdapter {
    name: String,
    dedicated_total_bytes: u64,
    luid: AdapterLuid,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct AdapterLuid {
    high: u32,
    low: u32,
}

struct GpuUtilization {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    adapter_luid: AdapterLuid,
}

fn main() -> Result<(), String> {
    if std::env::var("MYWALLPAPER_PROTOCOL").as_deref() != Ok("process-v2") {
        return Err("MYWALLPAPER_PROTOCOL must be process-v2".to_owned());
    }

    let output = Arc::new(Mutex::new(io::stdout()));
    let control = Arc::new((
        Mutex::new(Control {
            running: true,
            interval: Duration::from_secs(1),
        }),
        Condvar::new(),
    ));
    let mut initialized = false;
    let mut sampler_thread = None;
    let mut input = io::stdin();

    while let Some(frame) =
        read_json_record::<HostFrame>(&mut input).map_err(|error| error.to_string())?
    {
        let version = match &frame {
            HostFrame::Init { v, .. }
            | HostFrame::Settings { v, .. }
            | HostFrame::Message { v, .. }
            | HostFrame::Shutdown { v } => *v,
        };
        if version != PROTOCOL_VERSION {
            write_companion_error(
                &output,
                "protocol-version",
                format!("unsupported protocol version {version}"),
            )?;
            break;
        }
        match frame {
            HostFrame::Init {
                device_settings, ..
            } if !initialized => {
                if let Err(message) = set_interval(&control, &device_settings) {
                    write_companion_error(&output, "settings-invalid", message)?;
                    break;
                }
                write_frame(
                    &output,
                    &CompanionFrame::<Value>::Ready {
                        v: PROTOCOL_VERSION,
                    },
                )?;
                initialized = true;
                let thread_output = output.clone();
                let thread_control = control.clone();
                sampler_thread = Some(thread::spawn(move || {
                    run_sampler(thread_control, thread_output)
                }));
            }
            HostFrame::Settings {
                device_settings, ..
            } if initialized => {
                if let Err(message) = set_interval(&control, &device_settings) {
                    write_companion_error(&output, "settings-invalid", message)?;
                    break;
                }
            }
            HostFrame::Message { .. } if initialized => {}
            HostFrame::Shutdown { .. } => break,
            _ => {
                write_companion_error(
                    &output,
                    "protocol-state",
                    "invalid companion lifecycle frame".to_owned(),
                )?;
                break;
            }
        }
    }

    {
        let (lock, wake) = &*control;
        let mut state = lock
            .lock()
            .map_err(|_| "control lock poisoned".to_owned())?;
        state.running = false;
        wake.notify_all();
    }
    if let Some(handle) = sampler_thread {
        handle
            .join()
            .map_err(|_| "sampler thread panicked".to_owned())?;
    }
    Ok(())
}

fn run_sampler(control: Arc<(Mutex<Control>, Condvar)>, output: Arc<Mutex<io::Stdout>>) {
    let mut sampler = Sampler::new();
    loop {
        let payload = match sampler.sample() {
            Ok(sample) => Payload::Sample(sample),
            Err(message) => Payload::Error { message },
        };
        if let Err(error) = write_frame(
            &output,
            &CompanionFrame::Message {
                v: PROTOCOL_VERSION,
                target: MessageTarget::Broadcast,
                payload,
            },
        ) {
            eprintln!("native companion output failed: {error}");
            return;
        }

        let (lock, wake) = &*control;
        let state = match lock.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if !state.running {
            return;
        }
        let interval = state.interval;
        let (state, _) = match wake.wait_timeout(state, interval) {
            Ok(result) => result,
            Err(_) => return,
        };
        if !state.running {
            return;
        }
    }
}

impl Sampler {
    fn new() -> Self {
        let gpu = GpuAdapter::discover();
        let gpu_utilization = gpu
            .as_ref()
            .and_then(|adapter| GpuUtilization::open(adapter.luid));
        Self {
            previous_cpu: None,
            gpu,
            gpu_utilization,
        }
    }

    fn sample(&mut self) -> Result<SystemSample, String> {
        let current_cpu = cpu_times()?;
        let usage_percent = self
            .previous_cpu
            .map(|previous| cpu_usage(previous, current_cpu))
            .unwrap_or(0.0);
        self.previous_cpu = Some(current_cpu);
        let memory = memory_sample()?;
        let gpu_usage = self
            .gpu_utilization
            .as_ref()
            .and_then(GpuUtilization::sample);
        let gpu = self.gpu.as_ref().map(|adapter| GpuSample {
            name: adapter.name.clone(),
            usage_percent: gpu_usage,
            dedicated_total_bytes: adapter.dedicated_total_bytes,
        });
        Ok(SystemSample {
            captured_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "system clock is before the Unix epoch".to_owned())?
                .as_millis(),
            cpu: CpuSample {
                usage_percent,
                logical_processors: thread::available_parallelism()
                    .map(|count| count.get())
                    .unwrap_or(1),
            },
            memory,
            gpu,
        })
    }
}

impl GpuAdapter {
    fn discover() -> Option<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            for index in 0..32 {
                let Ok(adapter) = factory.EnumAdapters1(index) else {
                    break;
                };
                let Ok(description) = adapter.GetDesc1() else {
                    continue;
                };
                if description.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let end = description
                    .Description
                    .iter()
                    .position(|character| *character == 0)
                    .unwrap_or(description.Description.len());
                return Some(Self {
                    name: String::from_utf16_lossy(&description.Description[..end]),
                    dedicated_total_bytes: description.DedicatedVideoMemory as u64,
                    luid: AdapterLuid {
                        high: description.AdapterLuid.HighPart as u32,
                        low: description.AdapterLuid.LowPart,
                    },
                });
            }
        }
        None
    }
}

impl GpuUtilization {
    fn open(adapter_luid: AdapterLuid) -> Option<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(
                query,
                w!(r"\GPU Engine(*)\Utilization Percentage"),
                0,
                &mut counter,
            ) != 0
            {
                PdhCloseQuery(query);
                return None;
            }
            if PdhCollectQueryData(query) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            Some(Self {
                query,
                counter,
                adapter_luid,
            })
        }
    }

    fn sample(&self) -> Option<f64> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let mut buffer_size = 0_u32;
            let mut item_count = 0_u32;
            let status = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut buffer_size,
                &mut item_count,
                None,
            );
            if status != PDH_MORE_DATA || buffer_size == 0 || item_count == 0 {
                return None;
            }
            let words = (buffer_size as usize + align_of::<usize>() - 1) / align_of::<usize>();
            let mut buffer = vec![0_usize; words];
            let items = buffer.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            if PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut buffer_size,
                &mut item_count,
                Some(items),
            ) != 0
            {
                return None;
            }
            if item_count as usize * size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() > buffer_size as usize
            {
                return None;
            }
            let mut engines = HashMap::<String, f64>::new();
            for item in std::slice::from_raw_parts(items, item_count as usize) {
                if item.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA
                    && item.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA
                {
                    continue;
                }
                let value = item.FmtValue.Anonymous.doubleValue;
                if !value.is_finite() || value < 0.0 {
                    continue;
                }
                let name = item.szName.to_string().ok()?;
                let Some(engine) = gpu_engine_key(&name, self.adapter_luid) else {
                    continue;
                };
                *engines.entry(engine).or_default() += value;
            }
            engines
                .into_values()
                .reduce(f64::max)
                .map(|value| value.clamp(0.0, 100.0))
        }
    }
}

fn gpu_engine_key(instance_name: &str, adapter_luid: AdapterLuid) -> Option<String> {
    let normalized = instance_name.to_ascii_lowercase();
    let marker_index = normalized.find("_luid_")?;
    // GPU Engine instances encode the DXGI LUID as high and low hexadecimal tokens.
    let mut luid_parts = normalized[marker_index + "_luid_".len()..].split('_');
    let high = u32::from_str_radix(luid_parts.next()?.strip_prefix("0x")?, 16).ok()?;
    let low = u32::from_str_radix(luid_parts.next()?.strip_prefix("0x")?, 16).ok()?;
    if (AdapterLuid { high, low }) != adapter_luid {
        return None;
    }
    Some(normalized[marker_index..].to_owned())
}

impl Drop for GpuUtilization {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

fn cpu_times() -> Result<CpuTimes, String> {
    unsafe {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user))
            .map_err(|error| format!("GetSystemTimes failed: {error}"))?;
        Ok(CpuTimes {
            idle: filetime_value(idle),
            kernel: filetime_value(kernel),
            user: filetime_value(user),
        })
    }
}

fn cpu_usage(previous: CpuTimes, current: CpuTimes) -> f64 {
    let idle = current.idle.saturating_sub(previous.idle);
    let kernel = current.kernel.saturating_sub(previous.kernel);
    let user = current.user.saturating_sub(previous.user);
    let total = kernel.saturating_add(user);
    if total == 0 {
        0.0
    } else {
        ((total.saturating_sub(idle)) as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

fn filetime_value(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

fn memory_sample() -> Result<MemorySample, String> {
    unsafe {
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        GlobalMemoryStatusEx(&mut status)
            .map_err(|error| format!("GlobalMemoryStatusEx failed: {error}"))?;
        Ok(MemorySample {
            used_bytes: status.ullTotalPhys.saturating_sub(status.ullAvailPhys),
            total_bytes: status.ullTotalPhys,
        })
    }
}

fn set_interval(control: &Arc<(Mutex<Control>, Condvar)>, settings: &Value) -> Result<(), String> {
    let interval = match settings.get("refreshInterval").and_then(Value::as_str) {
        Some("1s") => Duration::from_secs(1),
        Some("5s") => Duration::from_secs(5),
        Some("2s") => Duration::from_secs(2),
        _ => {
            return Err(
                "refreshInterval must be one of the manifest values: 1s, 2s, or 5s".to_owned(),
            )
        }
    };
    let (lock, wake) = &**control;
    let mut state = lock
        .lock()
        .map_err(|_| "control lock poisoned while applying settings".to_owned())?;
    state.interval = interval;
    wake.notify_all();
    Ok(())
}

fn write_frame<T: Serialize>(output: &Arc<Mutex<io::Stdout>>, value: &T) -> Result<(), String> {
    let mut output = output
        .lock()
        .map_err(|_| "output lock poisoned".to_owned())?;
    framing::write_json_record(&mut *output, value).map_err(|error| error.to_string())
}

fn write_companion_error(
    output: &Arc<Mutex<io::Stdout>>,
    code: &str,
    message: String,
) -> Result<(), String> {
    write_frame(
        output,
        &CompanionFrame::<Value>::Error {
            v: PROTOCOL_VERSION,
            message,
            code: code.to_owned(),
        },
    )
}
