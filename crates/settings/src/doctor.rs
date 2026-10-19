//! What `vinosearch doctor` reports: the NPU, its driver, the OpenVINO
//! runtime and a probe model run on the configured device. Run as a separate
//! process, as the CLI does it, so this window never loads OpenVINO.

use std::process::Command;
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::integration::vinosearch;

#[derive(Clone, Debug, Default)]
pub struct State {
    pub running: bool,
    pub report: Option<Report>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// Every check passed, the probe included.
    pub ok: bool,
    /// (label, value) per line doctor printed.
    pub lines: Vec<(String, String)>,
    /// Why it stopped early (runtime missing, ...), from stderr.
    pub error: Option<String>,
}

pub fn run(state: Arc<Mutex<State>>, ctx: &egui::Context) {
    state.lock().unwrap().running = true;
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let report = match Command::new(vinosearch()).arg("doctor").output() {
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let error = stderr
                    .lines()
                    .map(|l| l.strip_prefix("error: ").unwrap_or(l))
                    .collect::<Vec<_>>()
                    .join("\n");
                Report {
                    ok: out.status.success(),
                    lines: parse(&String::from_utf8_lossy(&out.stdout)),
                    error: (!error.trim().is_empty()).then(|| error.trim().to_owned()),
                }
            }
            Err(e) => Report {
                ok: false,
                lines: Vec::new(),
                error: Some(format!("Couldn't run vinosearch: {e}")),
            },
        };
        *state.lock().unwrap() = State {
            running: false,
            report: Some(report),
        };
        ctx.request_repaint();
    });
}

/// `key:   value` lines, with doctor's keys as the window's labels.
fn parse(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let label = match key.trim() {
                "npu" => "NPU",
                "driver" => "Driver",
                "compiler" => "NPU compiler",
                "ceiling" => "Newest supported",
                "warning" => "Warning",
                "prefix" => "Runtime",
                "installed" => "Installed",
                "loaded" => "Loaded",
                "devices" => "Devices",
                "probe" => "Probe",
                other => other,
            };
            let mut value = value.trim();
            if key.trim() == "installed" {
                // "OpenVINO 2026.4.1 (pypi/openvino/<wheel>)": the version says it.
                value = value.split(" (").next().unwrap_or(value);
            }
            Some((format!("{label}:"), value.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_lines_become_labelled_rows() {
        let out = "npu:       none\ndriver:    none\nprefix:    /h/.local/lib/vinosearch/openvino\n\
                   probe:     ok on CPU (compile 27 ms, infer 456 us)\n\
                   installed: OpenVINO 2026.4.1 (pypi/openvino/openvino-2026.4.1.whl)\n";
        let lines = parse(out);
        assert_eq!(lines[0], ("NPU:".into(), "none".into()));
        assert_eq!(
            lines[2],
            (
                "Runtime:".into(),
                "/h/.local/lib/vinosearch/openvino".into()
            )
        );
        assert_eq!(
            lines[3],
            (
                "Probe:".into(),
                "ok on CPU (compile 27 ms, infer 456 us)".into()
            )
        );
        assert_eq!(lines[4], ("Installed:".into(), "OpenVINO 2026.4.1".into()));
    }
}
