//! The OpenVINO runtime ovfetch installed, and whether it can use the NPU.

use anyhow::{Context, Result, bail};
use openvino::{Core, DeviceType, ElementType, Shape, Tensor};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub fn default_prefix(flag: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p);
    }
    let home = std::env::var_os("HOME").context("no HOME; pass --openvino-dir")?;
    Ok(PathBuf::from(home).join(".local/lib/vinosearch/openvino"))
}

/// The fields of `ovfetch.lock.json` this program reads.
#[derive(Deserialize)]
pub struct Lock {
    pub openvino: String,
    pub artifact: Artifact,
}

#[derive(Deserialize)]
pub struct Artifact {
    pub id: String,
}

pub fn read_lock(prefix: &Path) -> Result<Lock> {
    let path = prefix.join("ovfetch.lock.json");
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "{} not found; run scripts/install-runtime.sh",
            path.display()
        )
    })?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Load `libopenvino_c` from the prefix. Its own `$ORIGIN` RUNPATH finds the
/// rest of OpenVINO and its plugins next to it. Then the NPU prefix, if
/// `ovfetch npu install` made one: it has to be in place before OpenVINO
/// first opens the NPU, which `Core::new` may do.
pub fn load(prefix: &Path) -> Result<()> {
    let lib = prefix.join("libopenvino_c.so");
    if !lib.is_file() {
        bail!(
            "{} not found; run scripts/install-runtime.sh",
            lib.display()
        );
    }
    openvino_sys::library::load_from(lib).map_err(anyhow::Error::msg)?;
    preload_npu(ovfetch::detect::default_npu_prefix().as_deref())?;
    Ok(())
}

/// Load the libraries `ovfetch npu install` put in `npu_prefix`: the NPU
/// compiler a distro's driver package lacks (Fedora's 1.32.0 rpm), or a
/// whole driver. They are not on the linker path, so the app loads them
/// itself, in the order `ovfetch-preload.txt` lists and with `RTLD_GLOBAL`;
/// when the driver later asks for them by soname, the dynamic linker hands
/// back the copies already loaded. After OpenVINO (the compiler needs its
/// TBB) and before anything opens the NPU. Returns what was loaded; nothing
/// when there is no prefix or no preload list.
pub fn preload_npu(npu_prefix: Option<&Path>) -> Result<Vec<PathBuf>> {
    use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW};

    let Some(prefix) = npu_prefix else {
        return Ok(Vec::new());
    };
    let list = prefix.join(ovfetch::detect::NPU_PRELOAD);
    let text = match fs::read_to_string(&list) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", list.display())),
    };
    let mut loaded = Vec::new();
    for name in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        // SAFETY: loading a library runs its initialisers. These are the
        // files ovfetch hash-checked into the user's own prefix, or the
        // host's Level Zero loader by soname; the same trust as the OpenVINO
        // prefix loaded above.
        let lib = unsafe { Library::open(Some(name), RTLD_NOW | RTLD_GLOBAL) }
            .map_err(|e| anyhow::anyhow!("loading {name} (from {}): {e}", list.display()))?;
        // Stays loaded for the life of the process, like OpenVINO itself.
        std::mem::forget(lib);
        loaded.push(PathBuf::from(name));
    }
    Ok(loaded)
}

const PROBE_LEN: usize = 64;

/// Relu over a static `[1, PROBE_LEN]` f32 input, from scripts/gen-probe.py.
/// ONNX rather than OpenVINO IR: ovfetch's prefix ships the ONNX frontend only.
const PROBE_ONNX: &[u8] = include_bytes!("probe.onnx");

pub struct ProbeTiming {
    pub compile_ms: u128,
    pub infer_us: u128,
}

/// Compile the probe on `device`, run it once, and check the output is ReLU of
/// the input. Values are exact in f16, which the NPU computes in.
pub fn probe(core: &mut Core, device: &str) -> Result<ProbeTiming> {
    let model = core.read_model_from_buffer(PROBE_ONNX, None)?;
    let t0 = Instant::now();
    let mut compiled = core.compile_model(&model, DeviceType::from(device))?;
    let compile_ms = t0.elapsed().as_millis();
    let mut request = compiled.create_infer_request()?;

    let input: Vec<f32> = (0..PROBE_LEN)
        .map(|i| i as f32 - (PROBE_LEN / 2) as f32)
        .collect();
    let mut tensor = Tensor::new(ElementType::F32, &Shape::new(&[1, PROBE_LEN as i64])?)?;
    tensor.get_data_mut::<f32>()?.copy_from_slice(&input);
    request.set_tensor("input", &tensor)?;
    let t1 = Instant::now();
    request.infer()?;
    let infer_us = t1.elapsed().as_micros();

    let output = request.get_tensor("output")?;
    let got = output.get_data::<f32>()?;
    for (i, (x, y)) in input.iter().zip(got).enumerate() {
        if x.max(0.0) != *y {
            bail!("probe output[{i}] is {y}, expected {}", x.max(0.0));
        }
    }
    Ok(ProbeTiming {
        compile_ms,
        infer_us,
    })
}

/// Print what ovfetch knows about the machine, then load the runtime and run
/// the probe. Ok(false) means a check failed and was reported.
pub fn doctor(prefix: &Path, device: &str) -> Result<bool> {
    let data = ovfetch::data::Data::load(None)?;
    let machine = ovfetch::detect::machine();
    let status = ovfetch::status::status(&machine, &data);
    let npu = machine.devices.iter().find(|d| d.kind == "npu");
    println!(
        "npu:       {}",
        npu.map_or("none".into(), |d| format!("{} ({})", d.pci_id, d.slot))
    );
    println!(
        "driver:    {}",
        machine.npu_driver.as_deref().unwrap_or("none")
    );
    println!(
        "compiler:  {}",
        if machine.npu_compiler {
            "present"
        } else {
            "missing (`ovfetch npu install` fetches Intel's)"
        }
    );
    if let Some(p) = &machine.npu_prefix {
        println!("npu prefix: {p} (from `ovfetch npu install`, preloaded)");
    }
    if let Some(c) = &status.openvino_ceiling {
        println!("ceiling:   OpenVINO {c}");
    }
    for w in &status.warnings {
        println!("warning:   {w}");
    }

    let lock = read_lock(prefix)?;
    println!("prefix:    {}", prefix.display());
    println!(
        "installed: OpenVINO {} ({})",
        lock.openvino, lock.artifact.id
    );

    load(prefix)?;
    let mut core = Core::new()?;
    let version = openvino::version();
    println!("loaded:    {}", version.build_number);
    let devices: Vec<String> = core
        .available_devices()?
        .iter()
        .map(ToString::to_string)
        .collect();
    println!("devices:   {}", devices.join(", "));
    if !devices.iter().any(|d| d == device) {
        println!("probe:     {device} not available");
        return Ok(false);
    }

    match probe(&mut core, device) {
        Ok(t) => {
            println!(
                "probe:     ok on {device} (compile {} ms, infer {} us)",
                t.compile_ms, t.infer_us
            );
            Ok(true)
        }
        Err(e) => {
            println!("probe:     failed on {device}: {e:#}");
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(name: &str, lock: Option<&str>) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vinosearch-runtime-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        if let Some(text) = lock {
            fs::write(dir.join("ovfetch.lock.json"), text).unwrap();
        }
        dir
    }

    #[test]
    fn a_lock_file_gives_the_version_and_artifact() {
        let dir = prefix(
            "ok",
            Some(
                r#"{"openvino": "2026.4.1", "artifact": {"id": "ubuntu24-x86_64", "sha256": "ab"}, "extra": 1}"#,
            ),
        );
        let lock = read_lock(&dir).unwrap();
        assert_eq!(lock.openvino, "2026.4.1");
        assert_eq!(lock.artifact.id, "ubuntu24-x86_64");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_or_malformed_lock_file_is_an_error_not_a_panic() {
        let dir = prefix("missing", None);
        let err = read_lock(&dir).err().unwrap();
        assert!(err.to_string().contains("install-runtime.sh"), "{err:#}");
        for (name, text) in [
            ("empty", ""),
            ("truncated", r#"{"openvino": "2026"#),
            ("no-artifact", r#"{"openvino": "2026.4.1"}"#),
            (
                "wrong-type",
                r#"{"openvino": 2026, "artifact": {"id": "x"}}"#,
            ),
        ] {
            let dir = prefix(name, Some(text));
            let err = read_lock(&dir).err().unwrap();
            assert!(err.to_string().starts_with("parsing "), "{name}: {err:#}");
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_npu_prefix_or_preload_list_loads_nothing() {
        assert!(preload_npu(None).unwrap().is_empty());
        let dir = prefix("npu", None);
        assert!(preload_npu(Some(&dir)).unwrap().is_empty());
        // An empty list loads nothing either.
        fs::write(dir.join(ovfetch::detect::NPU_PRELOAD), "\n  \n").unwrap();
        assert!(preload_npu(Some(&dir)).unwrap().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_library_that_wont_load_names_the_list() {
        let dir = prefix("npu-bad", None);
        fs::write(
            dir.join(ovfetch::detect::NPU_PRELOAD),
            "/nonexistent/libvinosearch-test.so\n",
        )
        .unwrap();
        let err = preload_npu(Some(&dir)).err().unwrap().to_string();
        assert!(err.contains("libvinosearch-test.so"), "{err}");
        assert!(err.contains(ovfetch::detect::NPU_PRELOAD), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_default_prefix_is_under_home_unless_given() {
        let given = PathBuf::from("/opt/ov");
        assert_eq!(default_prefix(Some(given.clone())).unwrap(), given);
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                default_prefix(None).unwrap(),
                Path::new(&home).join(".local/lib/vinosearch/openvino")
            );
        }
    }
}
