//! Object Detection models built into the backend (user request,
//! 2026-10-31: models are part of the software, not chosen by the user).
//!
//! `build.rs` embeds every `.onnx`, `.engine`/`.plan` and `.xml`/`.bin` file
//! of `rust-backend/models/` (or `SYSX_EMBEDDED_MODELS`) and its `onnx/`,
//! `tensorrt/`, `openvino/` subfolders. Files are grouped by model name and
//! precision: `<name>_fp16.*` and `<name>_int8.*` (a name without a suffix
//! is FP16). Settings show one entry per model ("<Name> (W×H)"); Apply picks
//! the variant for the chosen Compute precision and the artifact for the
//! chosen Runtime, and reports what is missing instead of substituting.

use crate::config::{AppSettings, ModelArtifacts, ModelDefinition, ModelRegistry};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// One embedded file.
#[derive(Clone, Copy, Debug)]
pub struct EmbeddedArtifact {
    pub stem: &'static str,
    /// Lowercase: onnx, engine, plan, xml or bin.
    pub extension: &'static str,
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/embedded_models.rs"));

pub const PRECISIONS: [&str; 2] = ["fp16", "int8"];

/// The files this build carries. Unit tests see none, so they do not depend
/// on what is in `rust-backend/models` (they install their own artifacts).
pub fn embedded() -> &'static [EmbeddedArtifact] {
    if cfg!(test) {
        &[]
    } else {
        EMBEDDED_ARTIFACTS
    }
}

/// Model name and precision of a file stem.
fn split_stem(stem: &str) -> (&str, &'static str) {
    for precision in PRECISIONS {
        if let Some(name) = stem
            .strip_suffix(precision)
            .and_then(|name| name.strip_suffix(['_', '-', '.']))
        {
            return (name, precision);
        }
    }
    (stem, "fp16")
}

/// One precision of a model and which runtimes it has its own files for.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Variant {
    pub precision: String,
    /// Registry entry: `<model>@<precision>`.
    pub id: String,
    pub onnx: bool,
    pub tensorrt: bool,
    pub openvino: bool,
}

/// A built-in model as Settings shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BuiltinModel {
    pub id: String,
    pub label: String,
    pub variants: Vec<Variant>,
}

/// "game_light_256" at 256×256 → "Game Light (256×256)".
fn label(name: &str, width: u32, height: u32) -> String {
    let mut words: Vec<&str> = name
        .split(['_', '-', ' '])
        .filter(|word| !word.is_empty())
        .collect();
    if words.len() > 1
        && words
            .last()
            .is_some_and(|last| last.parse::<u32>().ok() == Some(width))
    {
        words.pop();
    }
    let title = words
        .iter()
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect::<String>()
            })
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("{title} ({width}×{height})")
}

/// Which variant a Runtime + Compute choice uses.
pub fn resolve(
    models: &[BuiltinModel],
    model: &str,
    precision: &str,
    backend: &str,
) -> Result<String, String> {
    let entry = models
        .iter()
        .find(|entry| entry.id == model)
        .ok_or_else(|| format!("Built-in model {model} does not exist"))?;
    let usable = |variant: &Variant| match backend {
        "openvino" => variant.openvino || variant.onnx,
        "tensorrt" => variant.tensorrt || variant.onnx,
        _ => variant.onnx,
    };
    if let Some(variant) = entry
        .variants
        .iter()
        .find(|variant| variant.precision == precision && usable(variant))
    {
        return Ok(variant.id.clone());
    }
    let available: Vec<String> = entry
        .variants
        .iter()
        .filter(|variant| usable(variant))
        .map(|variant| variant.precision.to_uppercase())
        .collect();
    let runtime = match backend {
        "openvino" => "OpenVINO",
        "tensorrt" => "TensorRT",
        _ => "ONNX Runtime",
    };
    Err(if available.is_empty() {
        format!("{} has no model for {runtime}", entry.label)
    } else {
        format!(
            "{} has no {} model for {runtime}; choose {}",
            entry.label,
            precision.to_uppercase(),
            available.join(" or ")
        )
    })
}

/// `trusted`: the folder's stamp matches this build, so a file of the right
/// length is current and is not read back (models are large; startup was
/// slow reading them on every launch).
fn write_if_changed(path: &Path, bytes: &[u8], trusted: bool) -> Result<(), String> {
    let current = if trusted {
        std::fs::metadata(path).is_ok_and(|meta| meta.len() == bytes.len() as u64)
    } else {
        std::fs::read(path).ok().as_deref() == Some(bytes)
    };
    if !current {
        std::fs::write(path, bytes)
            .map_err(|error| format!("Cannot write {}: {error}", path.display()))?;
    }
    Ok(())
}

/// Writes `artifacts` under `data_dir`, registers one entry per model and
/// precision, drops every other registry entry (their files stay on disk),
/// and activates the variant for the current settings. Without built-in
/// models Object Detection falls back to Pixel Seek.
pub fn install(
    data_dir: &Path,
    registry: &mut ModelRegistry,
    settings: &mut AppSettings,
    artifacts: &[EmbeddedArtifact],
) -> Result<Vec<BuiltinModel>, String> {
    install_stamped(data_dir, registry, settings, artifacts, None)
}

/// `install` for this build's embedded models, skipping the file check when
/// the folder's stamp says they were written by the same build.
pub fn install_embedded(
    data_dir: &Path,
    registry: &mut ModelRegistry,
    settings: &mut AppSettings,
) -> Result<Vec<BuiltinModel>, String> {
    let stamp = (!embedded().is_empty()).then_some(EMBEDDED_MODELS_HASH);
    install_stamped(data_dir, registry, settings, embedded(), stamp)
}

fn install_stamped(
    data_dir: &Path,
    registry: &mut ModelRegistry,
    settings: &mut AppSettings,
    artifacts: &[EmbeddedArtifact],
    stamp: Option<u64>,
) -> Result<Vec<BuiltinModel>, String> {
    let folder = data_dir.join("models").join("builtin");
    let stamp_path = folder.join(".stamp");
    let stamp_text = stamp.map(|hash| format!("{hash:x}"));
    let trusted = stamp_text.is_some()
        && std::fs::read_to_string(&stamp_path).ok() == stamp_text;
    if stamp_text.is_some() && !trusted {
        // Files may change below; the stamp is written again once they are.
        let _ = std::fs::remove_file(&stamp_path);
    }
    // model name → precision → extension → bytes
    let mut grouped: BTreeMap<&str, BTreeMap<&str, BTreeMap<&str, &[u8]>>> = BTreeMap::new();
    for artifact in artifacts {
        let (name, precision) = split_stem(artifact.stem);
        grouped
            .entry(name)
            .or_default()
            .entry(precision)
            .or_default()
            .insert(artifact.extension, artifact.bytes);
    }
    if !grouped.is_empty() {
        std::fs::create_dir_all(&folder)
            .map_err(|error| format!("Cannot create the model folder: {error}"))?;
    }
    let mut models = Vec::new();
    let mut ids = Vec::new();
    for (name, precisions) in &grouped {
        // The input size and class names come from an ONNX file, FP16 first.
        let Some(reference) = PRECISIONS
            .iter()
            .filter_map(|precision| precisions.get(precision)?.get("onnx").copied())
            .next()
        else {
            continue; // no ONNX at all: the model cannot be described
        };
        let info = crate::onnx_info::read(reference).map_err(|error| format!("{name}: {error}"))?;
        let (width, height) = info
            .input_size
            .unwrap_or((settings.crop_resolution, settings.crop_resolution));
        let reference_path = folder.join(format!("{name}.onnx"));
        write_if_changed(&reference_path, reference, trusted)?;
        let mut variants = Vec::new();
        for precision in PRECISIONS {
            let Some(files) = precisions.get(precision) else {
                continue;
            };
            let file = |extension: &str| folder.join(format!("{name}_{precision}.{extension}"));
            let mut paths = ModelArtifacts {
                // Registry entries need an ONNX path; a variant without its
                // own ONNX keeps the reference, which `resolve` never picks
                // for ONNX Runtime.
                onnx: reference_path.to_string_lossy().into_owned(),
                tensorrt: String::new(),
                openvino_xml: String::new(),
                openvino_bin: String::new(),
            };
            if let Some(bytes) = files.get("onnx") {
                let path = file("onnx");
                write_if_changed(&path, bytes, trusted)?;
                paths.onnx = path.to_string_lossy().into_owned();
            }
            for extension in ["engine", "plan"] {
                if let Some(bytes) = files.get(extension) {
                    let path = file(extension);
                    write_if_changed(&path, bytes, trusted)?;
                    paths.tensorrt = path.to_string_lossy().into_owned();
                    break;
                }
            }
            if let (Some(xml), Some(bin)) = (files.get("xml"), files.get("bin")) {
                let (xml_path, bin_path) = (file("xml"), file("bin"));
                write_if_changed(&xml_path, xml, trusted)?;
                write_if_changed(&bin_path, bin, trusted)?;
                paths.openvino_xml = xml_path.to_string_lossy().into_owned();
                paths.openvino_bin = bin_path.to_string_lossy().into_owned();
            }
            let variant = Variant {
                precision: precision.into(),
                id: format!("{name}@{precision}"),
                onnx: files.contains_key("onnx"),
                tensorrt: !paths.tensorrt.is_empty(),
                openvino: !paths.openvino_xml.is_empty(),
            };
            register(registry, &variant.id, name, paths, (width, height), &info)?;
            ids.push(variant.id.clone());
            variants.push(variant);
        }
        models.push(BuiltinModel {
            id: (*name).to_owned(),
            label: label(name, width, height),
            variants,
        });
    }
    let others: Vec<String> = registry
        .models
        .keys()
        .filter(|id| !ids.contains(id))
        .cloned()
        .collect();
    for id in others {
        registry.remove(&id).map_err(|error| error.to_string())?;
    }
    let model = settings
        .active_model_id
        .clone()
        .filter(|id| models.iter().any(|model| model.id == *id))
        .or_else(|| models.first().map(|model| model.id.clone()));
    // The variant for the saved Runtime + Compute; failing that, any variant
    // of the model until Apply reports what is missing.
    let active = model.as_ref().and_then(|model| {
        resolve(
            &models,
            model,
            &settings.model_precision,
            &settings.model_backend,
        )
        .ok()
        .or_else(|| {
            models
                .iter()
                .find(|entry| entry.id == *model)
                .and_then(|entry| entry.variants.first())
                .map(|variant| variant.id.clone())
        })
    });
    registry
        .activate(active.as_deref())
        .map_err(|error| error.to_string())?;
    if model.is_none() && settings.strategy == "object_detection" {
        settings.strategy = "pixel_seek".into();
    }
    settings.active_model_id = model;
    if let (Some(text), false) = (stamp_text, trusted) {
        if !grouped.is_empty() {
            std::fs::write(&stamp_path, text)
                .map_err(|error| format!("Cannot write {}: {error}", stamp_path.display()))?;
        }
    }
    Ok(models)
}

fn register(
    registry: &mut ModelRegistry,
    id: &str,
    name: &str,
    artifacts: ModelArtifacts,
    (width, height): (u32, u32),
    info: &crate::onnx_info::OnnxInfo,
) -> Result<(), String> {
    match registry.models.get(id).cloned() {
        Some(mut model) => {
            model.artifacts = artifacts;
            model.input_width = width;
            model.input_height = height;
            model.class_names = info.class_names.clone();
            model.end2end = info.end2end;
            model.enabled = true;
            registry.update(model).map_err(|error| error.to_string())
        }
        None => registry
            .add(ModelDefinition {
                id: id.to_owned(),
                name: name.to_owned(),
                artifacts,
                input_width: width,
                input_height: height,
                confidence: 0.5,
                iou: 0.45,
                device: "auto".into(),
                enabled: true,
                class_names: info.class_names.clone(),
                end2end: info.end2end,
            })
            .map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Graph input "images" [1, 3, side, side] and Ultralytics class names.
    pub(crate) fn onnx(side: u8) -> &'static [u8] {
        fn tag(field: u8, payload: &[u8]) -> Vec<u8> {
            [
                vec![(field << 3) | 2, payload.len() as u8],
                payload.to_vec(),
            ]
            .concat()
        }
        let dims = [1u8, 3, side, side]
            .iter()
            .flat_map(|size| tag(1, &[1 << 3, *size]))
            .collect::<Vec<_>>();
        let tensor = tag(1, &[vec![1 << 3, 1], tag(2, &dims)].concat());
        let input = tag(11, &[tag(1, b"images"), tag(2, &tensor)].concat());
        let names = tag(14, &[tag(1, b"names"), tag(2, b"{0: 'Enemy'}")].concat());
        [tag(7, &input), names].concat().leak()
    }

    fn artifact(
        stem: &'static str,
        extension: &'static str,
        bytes: &'static [u8],
    ) -> EmbeddedArtifact {
        EmbeddedArtifact {
            stem,
            extension,
            bytes,
        }
    }

    #[test]
    fn a_matching_stamp_skips_reading_the_files_and_a_new_build_rewrites_them() {
        let dir = std::env::temp_dir().join(format!(
            "sysx-builtin-stamp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut registry = ModelRegistry::load(dir.join("models.yaml")).unwrap();
        let mut settings = AppSettings::default();
        let artifacts = [artifact("m_fp16", "engine", b"engine-1"), artifact("m_fp16", "onnx", onnx(64))];
        let install = |registry: &mut ModelRegistry, settings: &mut AppSettings, stamp| {
            install_stamped(&dir, registry, settings, &artifacts, Some(stamp)).unwrap();
        };
        let folder = dir.join("models").join("builtin");
        install(&mut registry, &mut settings, 1);
        assert_eq!(std::fs::read_to_string(folder.join(".stamp")).unwrap(), "1");
        // Same build: a same-length file is trusted, not compared.
        std::fs::write(folder.join("m_fp16.engine"), b"engine-X").unwrap();
        install(&mut registry, &mut settings, 1);
        assert_eq!(std::fs::read(folder.join("m_fp16.engine")).unwrap(), b"engine-X");
        // Another build checks every file again and rewrites what differs.
        install(&mut registry, &mut settings, 2);
        assert_eq!(std::fs::read(folder.join("m_fp16.engine")).unwrap(), b"engine-1");
        assert_eq!(std::fs::read_to_string(folder.join(".stamp")).unwrap(), "2");
        // A missing file is written even with a matching stamp.
        std::fs::remove_file(folder.join("m_fp16.engine")).unwrap();
        install(&mut registry, &mut settings, 2);
        assert!(folder.join("m_fp16.engine").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stems_split_into_name_and_precision_and_labels_read_well() {
        assert_eq!(
            split_stem("game_light_256_fp16"),
            ("game_light_256", "fp16")
        );
        assert_eq!(
            split_stem("game_light_256_int8"),
            ("game_light_256", "int8")
        );
        assert_eq!(split_stem("plain"), ("plain", "fp16"));
        assert_eq!(label("game_light_256", 256, 256), "Game Light (256×256)");
        assert_eq!(label("all_games", 256, 256), "All Games (256×256)");
    }

    #[test]
    fn runtime_and_compute_pick_the_matching_variant_and_report_gaps() {
        let dir = std::env::temp_dir().join(format!(
            "sysx-builtin-variants-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut registry = ModelRegistry::load(dir.join("models.yaml")).unwrap();
        let mut settings = AppSettings {
            strategy: "object_detection".into(),
            ..AppSettings::default()
        };
        let artifacts = [
            artifact("light_64_fp16", "onnx", onnx(64)),
            artifact("light_64_fp16", "engine", b"trt"),
            artifact("light_64_fp16", "xml", b"<xml/>"),
            artifact("light_64_fp16", "bin", b"w16"),
            artifact("light_64_int8", "xml", b"<xml8/>"),
            artifact("light_64_int8", "bin", b"w8"),
            artifact("heavy", "onnx", onnx(96)),
        ];
        let models = install(&dir, &mut registry, &mut settings, &artifacts).unwrap();
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["heavy", "light_64"]);
        assert_eq!(models[1].label, "Light (64×64)");

        let pick = |precision, backend| resolve(&models, "light_64", precision, backend);
        assert_eq!(pick("fp16", "onnxruntime").unwrap(), "light_64@fp16");
        assert_eq!(pick("int8", "openvino").unwrap(), "light_64@int8");
        assert_eq!(pick("fp16", "tensorrt").unwrap(), "light_64@fp16");
        let error = pick("int8", "directml").unwrap_err();
        assert!(
            error.contains("no INT8 model for ONNX Runtime; choose FP16"),
            "{error}"
        );
        let int8 = &registry.models["light_64@int8"].artifacts;
        assert_eq!(std::fs::read(&int8.openvino_bin).unwrap(), b"w8");
        assert!(registry.models["light_64@fp16"]
            .artifacts
            .tensorrt
            .ends_with("light_64_fp16.engine"));
        // A file without a precision suffix is FP16.
        assert_eq!(
            resolve(&models, "heavy", "fp16", "cuda").unwrap(),
            "heavy@fp16"
        );

        assert_eq!(settings.active_model_id.as_deref(), Some("heavy"));
        assert_eq!(registry.active_model_id.as_deref(), Some("heavy@fp16"));
        settings.active_model_id = Some("light_64".into());
        settings.model_precision = "int8".into();
        settings.model_backend = "openvino".into();
        install(&dir, &mut registry, &mut settings, &artifacts).unwrap();
        assert_eq!(registry.active_model_id.as_deref(), Some("light_64@int8"));

        install(&dir, &mut registry, &mut settings, &[]).unwrap();
        assert!(registry.models.is_empty());
        assert_eq!(settings.active_model_id, None);
        assert_eq!(settings.strategy, "pixel_seek");
        let _ = std::fs::remove_dir_all(dir);
    }
}
