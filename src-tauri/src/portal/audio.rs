use super::*;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    id: String,
    label: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevices {
    system_outputs: Vec<AudioDevice>,
    microphones: Vec<AudioDevice>,
    default_system_output: Option<String>,
    default_microphone: Option<String>,
}

pub(super) fn spawn_audio_recorder(
    audio_path: &PathBuf,
    audio: AudioSettings,
) -> Result<Child, String> {
    let mut command = Command::new("ffmpeg");
    command.args(["-hide_banner", "-loglevel", "error", "-y"]);
    if audio.system_enabled {
        let system_source = audio
            .system_device
            .as_deref()
            .map(monitor_source_from_sink)
            .unwrap_or(default_system_audio_source()?);
        command.args([
            "-thread_queue_size",
            "1024",
            "-f",
            "pulse",
            "-i",
            &system_source,
        ]);
    }
    if audio.microphone_enabled {
        let microphone_source = audio
            .microphone_device
            .clone()
            .unwrap_or(pactl_default("source")?);
        command.args([
            "-thread_queue_size",
            "1024",
            "-f",
            "pulse",
            "-i",
            &microphone_source,
        ]);
    }
    command
        .args([
            "-filter_complex",
            &audio_mix_filter(&audio),
            "-map",
            "[mixed_audio]",
            "-ar",
            "48000",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            audio_path.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Could not start {} capture: {error}", audio.label()))
}

pub fn audio_devices() -> Result<AudioDevices, String> {
    let system_outputs = pactl_devices("sinks")?;
    let microphones = pactl_devices("sources")?
        .into_iter()
        .filter(|device| !device.id.ends_with(".monitor"))
        .collect();
    Ok(AudioDevices {
        system_outputs,
        microphones,
        default_system_output: pactl_default("sink").ok(),
        default_microphone: pactl_default("source").ok(),
    })
}

pub(super) fn pactl_devices(kind: &str) -> Result<Vec<AudioDevice>, String> {
    let output = Command::new("pactl")
        .args(["list", "short", kind])
        .output()
        .map_err(|error| format!("Could not list audio {kind}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not list audio {kind}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let _index = columns.next()?;
            let id = columns.next()?.to_owned();
            Some(AudioDevice {
                label: id.clone(),
                id,
            })
        })
        .collect())
}

pub(super) fn default_system_audio_source() -> Result<String, String> {
    Ok(monitor_source_from_sink(&pactl_default("sink")?))
}

pub(super) fn monitor_source_from_sink(sink: &str) -> String {
    format!("{sink}.monitor")
}

pub(super) fn pactl_default(kind: &str) -> Result<String, String> {
    let output = Command::new("pactl")
        .arg(format!("get-default-{kind}"))
        .output()
        .map_err(|error| format!("Could not run pactl to find the default {kind}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not find the default {kind}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() {
        return Err(format!("PulseAudio reported no default {kind}."));
    }
    Ok(value)
}

pub(super) fn audio_mix_filter(audio: &AudioSettings) -> String {
    let mut filters = Vec::new();
    let mut labels = Vec::new();
    let mut input_index = 0;
    if audio.system_enabled {
        filters.push(format!(
            "[{input_index}:a]volume={:.3}[system]",
            audio.system_volume
        ));
        labels.push("[system]");
        input_index += 1;
    }
    if audio.microphone_enabled {
        filters.push(format!(
            "[{input_index}:a]volume={:.3}[microphone]",
            audio.microphone_volume
        ));
        labels.push("[microphone]");
    }
    format!(
        "{}{}{}amix=inputs={}:normalize=0[mixed_audio]",
        filters.join(";"),
        if filters.is_empty() { "" } else { ";" },
        labels.join(""),
        labels.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_mix_filter_keeps_system_and_microphone_at_full_gain() {
        assert_eq!(
            audio_mix_filter(&AudioSettings {
                system_enabled: true,
                microphone_enabled: true,
                system_volume: 1.0,
                microphone_volume: 1.0,
                system_device: None,
                microphone_device: None,
            }),
            "[0:a]volume=1.000[system];[1:a]volume=1.000[microphone];[system][microphone]amix=inputs=2:normalize=0[mixed_audio]"
        );
    }

    #[test]
    fn audio_mix_filter_uses_only_the_enabled_source_at_its_selected_level() {
        let audio = AudioSettings {
            system_enabled: true,
            microphone_enabled: false,
            system_volume: 0.35,
            microphone_volume: 1.0,
            system_device: None,
            microphone_device: None,
        };
        assert_eq!(
            audio_mix_filter(&audio),
            "[0:a]volume=0.350[system];[system]amix=inputs=1:normalize=0[mixed_audio]"
        );
        assert_eq!(audio.label(), "system audio");
        assert!(audio.captures_audio());
    }

    #[test]
    fn system_monitor_source_is_derived_from_the_default_sink() {
        assert_eq!(
            monitor_source_from_sink("alsa_output.example"),
            "alsa_output.example.monitor"
        );
    }
}
