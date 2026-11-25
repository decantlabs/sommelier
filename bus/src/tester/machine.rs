//! The machine class of a test machine.
//!
//! The operating system, its build and the memory come from `sysinfo`. The GPU comes through the
//! [`Probe`] trait: the real probe ([`CommandProbe`]) runs one command per operating system, and the
//! parsers of the command output are pure functions that tests feed with fixtures. A tester can state the
//! card by hand when the probe fails.

use sysinfo::System;

use super::cmd::CommandRunner;
use crate::report::types::MachineClass;

/// Which command produced a GPU text, so the right parser reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuSource {
    /// `Get-CimInstance Win32_VideoController | Format-List Name,AdapterRAM,DriverVersion` on Windows.
    WindowsCim,
    /// `nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv,noheader,nounits`.
    NvidiaSmi,
    /// `lspci -mm`.
    LspciMm,
}

/// The raw text of a GPU command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuOutput {
    pub source: GpuSource,
    pub text: String,
}

/// What a GPU parser found. The name is already normalised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuInfo {
    pub name: String,
    pub memory_mb: u32,
    pub driver: String,
}

/// Asks the system for the GPU. Tests use a fake.
pub trait Probe {
    /// The raw output of the GPU command, or why there is none.
    fn gpu_output(&self) -> Result<GpuOutput, String>;
}

/// The real probe: runs the GPU command of this operating system through a [`CommandRunner`].
pub struct CommandProbe<'a> {
    pub runner: &'a dyn CommandRunner,
}

impl Probe for CommandProbe<'_> {
    fn gpu_output(&self) -> Result<GpuOutput, String> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        if cfg!(windows) {
            let args = s(&[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-CimInstance Win32_VideoController | Format-List Name,AdapterRAM,DriverVersion",
            ]);
            let out = self
                .runner
                .run("powershell", &args)
                .map_err(|_| "the GPU query with powershell failed".to_string())?;
            if out.code == Some(0) && !out.stdout.trim().is_empty() {
                return Ok(GpuOutput {
                    source: GpuSource::WindowsCim,
                    text: out.stdout,
                });
            }
            return Err("the GPU query with powershell returned nothing".into());
        }
        let smi = s(&[
            "--query-gpu=name,memory.total,driver_version",
            "--format=csv,noheader,nounits",
        ]);
        if let Ok(out) = self.runner.run("nvidia-smi", &smi)
            && out.code == Some(0)
            && !out.stdout.trim().is_empty()
        {
            return Ok(GpuOutput {
                source: GpuSource::NvidiaSmi,
                text: out.stdout,
            });
        }
        match self.runner.run("lspci", &s(&["-mm"])) {
            Ok(out) if out.code == Some(0) && !out.stdout.trim().is_empty() => Ok(GpuOutput {
                source: GpuSource::LspciMm,
                text: out.stdout,
            }),
            _ => Err("neither nvidia-smi nor lspci gave a GPU".into()),
        }
    }
}

/// Lower case, single spaces, no trademark marks, no vendor prefix such as `nvidia geforce`.
pub fn normalize_gpu(raw: &str) -> String {
    let lower = raw
        .to_lowercase()
        .replace("(r)", "")
        .replace("(tm)", "")
        .replace(['\u{ae}', '\u{2122}'], "");
    let mut s = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    for prefix in ["nvidia ", "geforce "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
        }
    }
    s.chars().filter(|c| !c.is_control()).take(100).collect()
}

fn mb_from_bytes(b: u64) -> u32 {
    u32::try_from(b / (1024 * 1024)).unwrap_or(u32::MAX)
}

fn lower_clean(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect()
}

/// Reads the output of the Windows command. It picks the adapter with the most memory and skips
/// software adapters. `AdapterRAM` is a 32 bit value in Windows, so a card above 4 GiB shows at most
/// 4095 MB. That is a known limit of the source.
pub fn parse_gpu_windows(text: &str) -> Option<GpuInfo> {
    let mut found: Vec<(String, u64, String)> = Vec::new();
    let mut cur: (String, u64, String) = (String::new(), 0, String::new());
    let mut flush = |cur: &mut (String, u64, String)| {
        if !cur.0.is_empty() {
            found.push(std::mem::take(cur));
        }
        *cur = (String::new(), 0, String::new());
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            flush(&mut cur);
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_lowercase(), v.trim());
        match k.as_str() {
            "name" => {
                if !cur.0.is_empty() {
                    flush(&mut cur);
                }
                cur.0 = v.to_string();
            }
            "adapterram" => cur.1 = v.parse().unwrap_or(0),
            "driverversion" => cur.2 = v.to_string(),
            _ => {}
        }
    }
    flush(&mut cur);
    let soft = |n: &str| {
        let n = n.to_lowercase();
        [
            "basic render",
            "basic display",
            "remote",
            "virtual",
            "parsec",
        ]
        .iter()
        .any(|w| n.contains(w))
    };
    let pick = found
        .iter()
        .filter(|(n, _, _)| !soft(n))
        .max_by_key(|(_, ram, _)| *ram)
        .or_else(|| found.first())?;
    Some(GpuInfo {
        name: normalize_gpu(&pick.0),
        memory_mb: mb_from_bytes(pick.1),
        driver: lower_clean(&pick.2),
    })
}

/// Reads the output of `nvidia-smi` in CSV form. The first line is taken.
pub fn parse_gpu_nvidia_smi(text: &str) -> Option<GpuInfo> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    if parts.len() < 3 || parts[0].is_empty() {
        return None;
    }
    Some(GpuInfo {
        name: normalize_gpu(parts[0]),
        memory_mb: parts[1].parse().unwrap_or(0),
        driver: lower_clean(parts[2]),
    })
}

/// Reads the output of `lspci -mm`. It gives no memory size and no driver version, so those stay
/// empty. A GPU that is not from Intel wins over one that is.
pub fn parse_gpu_lspci(text: &str) -> Option<GpuInfo> {
    let mut cards: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let fields = quoted_fields(line);
        if fields.len() < 3 {
            continue;
        }
        let class = fields[0].to_lowercase();
        if !(class.contains("vga") || class.contains("3d controller") || class.contains("display"))
        {
            continue;
        }
        let device = fields[2];
        let name = match (device.find('['), device.rfind(']')) {
            (Some(a), Some(b)) if b > a => &device[a + 1..b],
            _ => device,
        };
        cards.push((fields[1].to_lowercase(), name.to_string()));
    }
    let pick = cards
        .iter()
        .find(|(vendor, _)| !vendor.contains("intel"))
        .or_else(|| cards.first())?;
    Some(GpuInfo {
        name: normalize_gpu(&pick.1),
        memory_mb: 0,
        driver: "unknown".into(),
    })
}

/// The double quoted fields of an `lspci -mm` line.
fn quoted_fields(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(a) = rest.find('"') {
        let after = &rest[a + 1..];
        let Some(b) = after.find('"') else { break };
        out.push(&after[..b]);
        rest = &after[b + 1..];
    }
    out
}

/// Parses a GPU command output by its source.
pub fn parse_gpu(out: &GpuOutput) -> Option<GpuInfo> {
    match out.source {
        GpuSource::WindowsCim => parse_gpu_windows(&out.text),
        GpuSource::NvidiaSmi => parse_gpu_nvidia_smi(&out.text),
        GpuSource::LspciMm => parse_gpu_lspci(&out.text),
    }
}

/// The machine class of this machine. The GPU text is `unknown` when the probe fails.
pub fn machine_class(probe: &dyn Probe) -> MachineClass {
    machine_class_with(probe, None)
}

/// Like [`machine_class`], with the GPU stated by the tester (`--gpu "<text>"`). The stated text wins
/// over the probe for the GPU name. Memory size and driver still come from the probe when it works.
pub fn machine_class_with(probe: &dyn Probe, gpu_override: Option<&str>) -> MachineClass {
    let memory_mb = match System::new() {
        Ok(mut sys) => {
            sys.refresh_memory();
            mb_from_bytes(sys.total_memory())
        }
        Err(_) => 0,
    };
    let os = if cfg!(windows) { "windows" } else { "linux" };
    let build = System::os_version()
        .or_else(|_| System::kernel_version())
        .unwrap_or_else(|_| "unknown".into());
    let info = probe
        .gpu_output()
        .ok()
        .and_then(|o| parse_gpu(&o))
        .unwrap_or(GpuInfo {
            name: "unknown".into(),
            memory_mb: 0,
            driver: "unknown".into(),
        });
    let stated = gpu_override.map(normalize_gpu).filter(|g| !g.is_empty());
    MachineClass {
        os: os.into(),
        os_build: lower_clean(&build),
        gpu: stated.unwrap_or(info.name),
        gpu_memory_mb: info.memory_mb,
        driver: info.driver,
        memory_mb,
    }
}
