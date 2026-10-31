//! Object Detection runtimes: ONNX Runtime on CPU or with the DirectML,
//! CUDA, TensorRT or CoreML execution provider (each compiled in by its
//! crate feature), OpenVINO, and the native TensorRT engine adapter. Unit
//! tests do not establish real-model or target-runtime readiness.

use crate::config::ModelDefinition;
use crate::results::DetectionObject;
use image::RgbImage;
use std::path::Path;
use std::time::Instant;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub struct YoloBox {
    pub class_id: u32,
    pub confidence: f32,
    pub bbox_xyxy: [f32; 4],
}

#[derive(Clone, Debug)]
pub struct InferenceOutput {
    pub shape: Vec<usize>,
    pub values: Vec<f32>,
    pub device: String,
    pub preprocess_ms: f64,
    pub inference_ms: f64,
}

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("model bundle: {0}")]
    Bundle(String),
    #[error("inference runtime: {0}")]
    Runtime(String),
    #[error("model output: {0}")]
    Output(String),
}

pub(crate) enum DetectionRuntime {
    Onnx(OnnxRuntime),
    OpenVino(OpenVinoRuntime),
    #[cfg(feature = "tensorrt")]
    TensorRt(TensorRtRuntime),
}

impl DetectionRuntime {
    pub(crate) fn load(
        model: &ModelDefinition,
        backend: &str,
        compute: &str,
    ) -> Result<Self, InferenceError> {
        let path = model
            .artifact_for_backend(backend)
            .map_err(|error| InferenceError::Bundle(error.into()))?;
        if !Path::new(path).is_file() {
            return Err(InferenceError::Bundle(format!(
                "the selected {backend} artifact is missing"
            )));
        }
        match backend {
            "onnxruntime" | "directml" | "cuda" | "coreml" => {
                OnnxRuntime::load(path, backend).map(Self::Onnx)
            }
            "openvino" => {
                // OpenVINO reads ONNX directly; its weights are then inside the file.
                let weights = if model.artifacts.openvino_xml.is_empty() {
                    ""
                } else {
                    model.artifacts.openvino_bin.as_str()
                };
                OpenVinoRuntime::load(path, weights, compute).map(Self::OpenVino)
            }
            "tensorrt" if !model.artifacts.tensorrt.is_empty() => {
                #[cfg(feature = "tensorrt")]
                {
                    TensorRtRuntime::load(path).map(Self::TensorRt)
                }
                #[cfg(not(feature = "tensorrt"))]
                {
                    Err(InferenceError::Runtime(
                        "SysX was built without the TensorRT engine adapter; remove the \
                         .engine file to run the ONNX model through TensorRT"
                            .into(),
                    ))
                }
            }
            "tensorrt" => OnnxRuntime::load(path, backend).map(Self::Onnx),
            _ => Err(InferenceError::Runtime(
                "unsupported inference backend".into(),
            )),
        }
    }

    pub(crate) fn infer(&mut self, input: &RgbImage) -> Result<InferenceOutput, InferenceError> {
        let width = input.width() as usize;
        let height = input.height() as usize;
        let preprocess_started = Instant::now();
        let input = rgb_chw(input);
        let preprocess_ms = preprocess_started.elapsed().as_secs_f64() * 1000.0;
        let inference_started = Instant::now();
        let mut output = match self {
            Self::Onnx(runtime) => runtime.infer(&input, width, height),
            Self::OpenVino(runtime) => runtime.infer(&input, width, height),
            #[cfg(feature = "tensorrt")]
            Self::TensorRt(runtime) => runtime.infer(&input, width, height),
        }?;
        output.preprocess_ms = preprocess_ms;
        output.inference_ms = inference_started.elapsed().as_secs_f64() * 1000.0;
        Ok(output)
    }
}

fn rgb_chw(image: &RgbImage) -> Vec<f32> {
    let pixels = image.as_raw();
    let width = image.width() as usize;
    let height = image.height() as usize;
    let plane = width * height;
    let mut input = vec![0.0; plane * 3];
    for (index, pixel) in pixels.chunks_exact(3).enumerate() {
        input[index] = f32::from(pixel[0]) / 255.0;
        input[plane + index] = f32::from(pixel[1]) / 255.0;
        input[plane * 2 + index] = f32::from(pixel[2]) / 255.0;
    }
    input
}

pub(crate) struct OnnxRuntime {
    session: ort::session::Session,
    device: &'static str,
}

#[allow(dead_code)]
fn missing(
    name: &str,
    feature: &str,
) -> Result<Option<(ort::ep::ExecutionProviderDispatch, &'static str)>, InferenceError> {
    Err(InferenceError::Runtime(format!(
        "SysX was built without the {name} execution provider (feature `{feature}`)"
    )))
}

/// The execution provider for a `modelBackend` value, registered so that
/// a provider that fails to start is an error instead of a silent fallback
/// to CPU.
fn execution_provider(
    backend: &str,
) -> Result<Option<(ort::ep::ExecutionProviderDispatch, &'static str)>, InferenceError> {
    match backend {
        "onnxruntime" => Ok(None),
        "directml" => {
            #[cfg(feature = "directml")]
            {
                Ok(Some((
                    ort::ep::DirectML::default()
                        .with_performance_preference(
                            ort::ep::directml::PerformancePreference::HighPerformance,
                        )
                        .build()
                        .error_on_failure(),
                    "DirectML GPU",
                )))
            }
            #[cfg(not(feature = "directml"))]
            {
                missing("DirectML", "directml")
            }
        }
        "cuda" => {
            #[cfg(feature = "cuda")]
            {
                Ok(Some((
                    ort::ep::CUDA::default().build().error_on_failure(),
                    "CUDA GPU",
                )))
            }
            #[cfg(not(feature = "cuda"))]
            {
                missing("CUDA", "cuda")
            }
        }
        "tensorrt" => {
            #[cfg(feature = "ort-tensorrt")]
            {
                // Building an engine takes a while; keep it for the next launch.
                let cache = std::env::var_os("LOCALAPPDATA")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir)
                    .join("SysX")
                    .join("tensorrt-cache");
                let _ = std::fs::create_dir_all(&cache);
                let cache = cache.to_string_lossy().into_owned();
                Ok(Some((
                    ort::ep::TensorRT::default()
                        .with_fp16(true)
                        .with_engine_cache(true)
                        .with_engine_cache_path(&cache)
                        .with_timing_cache(true)
                        .with_timing_cache_path(&cache)
                        .build()
                        .error_on_failure(),
                    "TensorRT GPU",
                )))
            }
            #[cfg(not(feature = "ort-tensorrt"))]
            {
                missing("TensorRT", "ort-tensorrt")
            }
        }
        "coreml" => {
            #[cfg(feature = "coreml")]
            {
                Ok(Some((
                    ort::ep::CoreML::default().build().error_on_failure(),
                    "CoreML",
                )))
            }
            #[cfg(not(feature = "coreml"))]
            {
                missing("CoreML", "coreml")
            }
        }
        _ => Err(InferenceError::Runtime(
            "unsupported inference backend".into(),
        )),
    }
}

impl OnnxRuntime {
    fn load(path: &str, backend: &str) -> Result<Self, InferenceError> {
        let environment = onnx_environment()?;
        let mut builder = ort::session::Session::builder(&environment).map_err(runtime_error)?;
        let mut device = "CPU";
        if let Some((provider, name)) = execution_provider(backend)? {
            builder = builder
                .with_execution_providers([provider])
                .map_err(|error| InferenceError::Runtime(format!("{name}: {error}")))?;
            device = name;
        }
        let session = builder.commit_from_file(path).map_err(runtime_error)?;
        if session.inputs().len() != 1 || session.outputs().is_empty() {
            return Err(InferenceError::Runtime(
                "YOLO ONNX models must have one input and at least one output".into(),
            ));
        }
        Ok(Self { session, device })
    }

    fn infer(
        &mut self,
        input: &[f32],
        width: usize,
        height: usize,
    ) -> Result<InferenceOutput, InferenceError> {
        let tensor = ort::value::Tensor::from_array(([1usize, 3, height, width], input.to_vec()))
            .map_err(runtime_error)?;
        let outputs = self
            .session
            .run(ort::inputs![tensor])
            .map_err(runtime_error)?;
        let tensor = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(runtime_error)?;
        Ok(InferenceOutput {
            shape: tensor
                .0
                .iter()
                .map(|dimension| usize::try_from(*dimension).unwrap_or(0))
                .collect(),
            values: tensor.1.to_vec(),
            device: self.device.into(),
            preprocess_ms: 0.0,
            inference_ms: 0.0,
        })
    }
}

fn onnx_environment() -> Result<&'static ort::environment::Environment, InferenceError> {
    static ENVIRONMENT: std::sync::OnceLock<Result<ort::environment::Environment, String>> =
        std::sync::OnceLock::new();
    match ENVIRONMENT.get_or_init(|| {
        ort::init()
            .with_telemetry(false)
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(environment) => Ok(environment),
        Err(message) => Err(InferenceError::Runtime(message.clone())),
    }
}

pub(crate) struct OpenVinoRuntime {
    compiled: openvino::CompiledModel,
    request: openvino::InferRequest,
    device: String,
}

impl OpenVinoRuntime {
    fn load(path: &str, weights: &str, compute: &str) -> Result<Self, InferenceError> {
        let device = match compute {
            "cpu" => "CPU",
            "gpu" => "GPU",
            "npu" => "NPU",
            _ => {
                return Err(InferenceError::Runtime(
                    "OpenVINO compute target must be CPU, GPU, or NPU".into(),
                ));
            }
        };
        let mut core = openvino::Core::new().map_err(runtime_error)?;
        let model = core
            .read_model_from_file(path, weights)
            .map_err(runtime_error)?;
        let compiled = core
            .compile_model(&model, openvino::DeviceType::from(device))
            .map_err(runtime_error)?;
        if compiled.get_input_size().map_err(runtime_error)? != 1
            || compiled.get_output_size().map_err(runtime_error)? == 0
        {
            return Err(InferenceError::Runtime(
                "YOLO OpenVINO models must have one input and at least one output".into(),
            ));
        }
        let mut compiled = compiled;
        let request = compiled.create_infer_request().map_err(runtime_error)?;
        Ok(Self {
            compiled,
            request,
            device: format!("OpenVINO {device}"),
        })
    }

    fn infer(
        &mut self,
        input: &[f32],
        width: usize,
        height: usize,
    ) -> Result<InferenceOutput, InferenceError> {
        let shape =
            openvino::Shape::new(&[1, 3, height as i64, width as i64]).map_err(runtime_error)?;
        let mut tensor =
            openvino::Tensor::new(openvino::ElementType::F32, &shape).map_err(runtime_error)?;
        tensor
            .get_data_mut::<f32>()
            .map_err(runtime_error)?
            .copy_from_slice(input);
        self.request
            .set_input_tensor(&tensor)
            .map_err(runtime_error)?;
        self.request.infer().map_err(runtime_error)?;
        let output = self
            .request
            .get_output_tensor_by_index(0)
            .map_err(runtime_error)?;
        let shape = output
            .get_shape()
            .map_err(runtime_error)?
            .get_dimensions()
            .iter()
            .map(|dimension| usize::try_from(*dimension).unwrap_or(0))
            .collect();
        let values = output.get_data::<f32>().map_err(runtime_error)?.to_vec();
        let _ = &self.compiled;
        Ok(InferenceOutput {
            shape,
            values,
            device: self.device.clone(),
            preprocess_ms: 0.0,
            inference_ms: 0.0,
        })
    }
}

#[cfg(feature = "tensorrt")]
pub(crate) struct TensorRtRuntime {
    // The wrappers contain stable native handles. Keep their C++ owners alive
    // and explicitly drop dependents before owners in Drop.
    context: std::mem::ManuallyDrop<baracuda_tensorrt::ExecutionContext<'static>>,
    engine: std::mem::ManuallyDrop<baracuda_tensorrt::Engine<'static>>,
    runtime: std::mem::ManuallyDrop<baracuda_tensorrt::Runtime>,
    cuda_context: baracuda_driver::Context,
    stream: baracuda_driver::Stream,
    input_name: String,
    output_name: String,
    device: String,
}

#[cfg(feature = "tensorrt")]
impl Drop for TensorRtRuntime {
    fn drop(&mut self) {
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.context);
            std::mem::ManuallyDrop::drop(&mut self.engine);
            std::mem::ManuallyDrop::drop(&mut self.runtime);
        }
    }
}

#[cfg(feature = "tensorrt")]
impl TensorRtRuntime {
    fn load(path: &str) -> Result<Self, InferenceError> {
        if !baracuda_tensorrt::shim_built() {
            return Err(InferenceError::Runtime(
                "TensorRT support needs a build with TensorRT headers and CUDA_PATH set".into(),
            ));
        }
        let device = baracuda_driver::Device::get(0).map_err(runtime_error)?;
        let cuda_context = baracuda_driver::Context::new(&device).map_err(runtime_error)?;
        cuda_context.set_current().map_err(runtime_error)?;
        let stream = baracuda_driver::Stream::new(&cuda_context).map_err(runtime_error)?;
        let runtime = baracuda_tensorrt::Runtime::with_null_logger().map_err(runtime_error)?;
        let engine_bytes = std::fs::read(path).map_err(runtime_error)?;
        let engine = runtime
            .deserialize_engine(&engine_bytes)
            .map_err(runtime_error)?;
        let mut input_name = None;
        let mut output_name = None;
        for index in 0..engine.num_io_tensors().map_err(runtime_error)? {
            let name = engine.io_tensor_name(index).map_err(runtime_error)?;
            match engine.tensor_io_mode(&name).map_err(runtime_error)? {
                baracuda_tensorrt::IoMode::Input if input_name.is_none() => {
                    input_name = Some(name);
                }
                baracuda_tensorrt::IoMode::Output if output_name.is_none() => {
                    output_name = Some(name);
                }
                _ => {}
            }
        }
        let input_name = input_name
            .ok_or_else(|| InferenceError::Runtime("TensorRT engine has no input tensor".into()))?;
        let output_name = output_name.ok_or_else(|| {
            InferenceError::Runtime("TensorRT engine has no output tensor".into())
        })?;
        let engine: baracuda_tensorrt::Engine<'static> = unsafe { std::mem::transmute(engine) };
        let context = engine.create_execution_context().map_err(runtime_error)?;
        let context: baracuda_tensorrt::ExecutionContext<'static> =
            unsafe { std::mem::transmute(context) };
        Ok(Self {
            context: std::mem::ManuallyDrop::new(context),
            engine: std::mem::ManuallyDrop::new(engine),
            runtime: std::mem::ManuallyDrop::new(runtime),
            cuda_context,
            stream,
            input_name,
            output_name,
            device: device.name().map_err(runtime_error)?,
        })
    }

    fn infer(
        &mut self,
        input: &[f32],
        width: usize,
        height: usize,
    ) -> Result<InferenceOutput, InferenceError> {
        use baracuda_driver::memory::DeviceBuffer;
        use baracuda_tensorrt::DataType;
        use std::ffi::c_void;

        self.cuda_context.set_current().map_err(runtime_error)?;
        let dims = baracuda_tensorrt::Dims::new(&[1, 3, height as i64, width as i64]);
        self.context
            .set_input_shape(&self.input_name, dims)
            .map_err(runtime_error)?;
        let output_shape = self
            .context
            .tensor_shape(&self.output_name)
            .map_err(runtime_error)?
            .as_slice()
            .iter()
            .map(|dimension| usize::try_from(*dimension).unwrap_or(0))
            .collect::<Vec<_>>();
        let output_len = checked_shape_size(&output_shape)?;
        let input_type = self
            .engine
            .tensor_data_type(&self.input_name)
            .map_err(runtime_error)?;
        let output_type = self
            .engine
            .tensor_data_type(&self.output_name)
            .map_err(runtime_error)?;
        let input_storage = if input_type == DataType::Float {
            let buffer = DeviceBuffer::<f32>::from_slice(&self.cuda_context, input)
                .map_err(runtime_error)?;
            unsafe {
                self.context
                    .set_tensor_address(&self.input_name, buffer.as_raw().0 as usize as *mut c_void)
                    .map_err(runtime_error)?;
            }
            InputBuffer::Float(buffer)
        } else if input_type == DataType::Half {
            let half_input = input
                .iter()
                .map(|value| half::f16::from_f32(*value).to_bits())
                .collect::<Vec<_>>();
            let buffer = DeviceBuffer::<u16>::from_slice(&self.cuda_context, &half_input)
                .map_err(runtime_error)?;
            unsafe {
                self.context
                    .set_tensor_address(&self.input_name, buffer.as_raw().0 as usize as *mut c_void)
                    .map_err(runtime_error)?;
            }
            InputBuffer::Half(buffer)
        } else {
            return Err(InferenceError::Runtime(
                "TensorRT model input must use float32 or float16".into(),
            ));
        };
        let mut values = if output_type == DataType::Float {
            let buffer =
                DeviceBuffer::<f32>::new(&self.cuda_context, output_len).map_err(runtime_error)?;
            unsafe {
                self.context
                    .set_tensor_address(
                        &self.output_name,
                        buffer.as_raw().0 as usize as *mut c_void,
                    )
                    .map_err(runtime_error)?;
            }
            OutputBuffer::Float(buffer)
        } else if output_type == DataType::Half {
            let buffer =
                DeviceBuffer::<u16>::new(&self.cuda_context, output_len).map_err(runtime_error)?;
            unsafe {
                self.context
                    .set_tensor_address(
                        &self.output_name,
                        buffer.as_raw().0 as usize as *mut c_void,
                    )
                    .map_err(runtime_error)?;
            }
            OutputBuffer::Half(buffer)
        } else {
            return Err(InferenceError::Runtime(
                "TensorRT model output must use float32 or float16".into(),
            ));
        };
        unsafe {
            self.context
                .enqueue_v3(&self.stream)
                .map_err(runtime_error)?;
        }
        self.stream.synchronize().map_err(runtime_error)?;
        let values = values.read_f32(output_len).map_err(runtime_error)?;
        drop(input_storage);
        Ok(InferenceOutput {
            shape: output_shape,
            values,
            device: self.device.clone(),
            preprocess_ms: 0.0,
            inference_ms: 0.0,
        })
    }
}

#[cfg(feature = "tensorrt")]
#[allow(dead_code)] // Keep input allocations alive until the CUDA stream has completed.
enum InputBuffer {
    Float(baracuda_driver::memory::DeviceBuffer<f32>),
    Half(baracuda_driver::memory::DeviceBuffer<u16>),
}

#[cfg(feature = "tensorrt")]
enum OutputBuffer {
    Float(baracuda_driver::memory::DeviceBuffer<f32>),
    Half(baracuda_driver::memory::DeviceBuffer<u16>),
}

#[cfg(feature = "tensorrt")]
impl OutputBuffer {
    fn read_f32(&mut self, len: usize) -> Result<Vec<f32>, InferenceError> {
        match self {
            Self::Float(buffer) => {
                let mut values = vec![0.0; len];
                buffer.copy_to_host(&mut values).map_err(runtime_error)?;
                Ok(values)
            }
            Self::Half(buffer) => {
                let mut values = vec![0u16; len];
                buffer.copy_to_host(&mut values).map_err(runtime_error)?;
                Ok(values
                    .into_iter()
                    .map(|value| half::f16::from_bits(value).to_f32())
                    .collect())
            }
        }
    }
}

pub fn decode_yolo_output(
    shape: &[usize],
    values: &[f32],
    confidence_threshold: f32,
    iou_threshold: f32,
) -> Result<Vec<YoloBox>, InferenceError> {
    if !(0.0..=1.0).contains(&confidence_threshold)
        || !(0.0..=1.0).contains(&iou_threshold)
        || shape.len() != 3
        || shape[0] != 1
    {
        return Err(InferenceError::Output(
            "expected a batch-1 YOLO output and thresholds from 0 to 1".into(),
        ));
    }
    let (channels, anchors, channel_major) = match (shape[1] >= 5, shape[2] >= 5) {
        (true, false) => (shape[1], shape[2], true),
        (false, true) => (shape[2], shape[1], false),
        (true, true) if shape[1] <= shape[2] => (shape[1], shape[2], true),
        (true, true) => (shape[2], shape[1], false),
        (false, false) => {
            return Err(InferenceError::Output(
                "expected YOLOv8 output shaped [1, 4+classes, anchors] or [1, anchors, 4+classes]"
                    .into(),
            ));
        }
    };
    if channels.checked_mul(anchors) != Some(values.len()) {
        return Err(InferenceError::Output(
            "YOLO output dimensions do not match its values".into(),
        ));
    }
    let at = |channel: usize, anchor: usize| {
        if channel_major {
            values[channel * anchors + anchor]
        } else {
            values[anchor * channels + channel]
        }
    };
    let mut by_class = std::collections::HashMap::<u32, Vec<YoloBox>>::new();
    for anchor in 0..anchors {
        let mut class_id = 0u32;
        let mut score = f32::NEG_INFINITY;
        for class_index in 4..channels {
            let candidate = at(class_index, anchor);
            if candidate > score {
                score = candidate;
                class_id = (class_index - 4) as u32;
            }
        }
        if !score.is_finite() || score < confidence_threshold {
            continue;
        }
        let center_x = at(0, anchor);
        let center_y = at(1, anchor);
        let width = at(2, anchor);
        let height = at(3, anchor);
        let bbox_xyxy = [
            center_x - width * 0.5,
            center_y - height * 0.5,
            center_x + width * 0.5,
            center_y + height * 0.5,
        ];
        if !bbox_xyxy.iter().all(|value| value.is_finite())
            || bbox_xyxy[2] <= bbox_xyxy[0]
            || bbox_xyxy[3] <= bbox_xyxy[1]
        {
            continue;
        }
        by_class.entry(class_id).or_default().push(YoloBox {
            class_id,
            confidence: score,
            bbox_xyxy,
        });
    }
    let mut result = Vec::new();
    for boxes in by_class.values_mut() {
        boxes.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        let mut kept: Vec<YoloBox> = Vec::new();
        for candidate in boxes.drain(..) {
            if kept.iter().all(|prior| {
                intersection_over_union(candidate.bbox_xyxy, prior.bbox_xyxy) <= iou_threshold
            }) {
                kept.push(candidate);
                if kept.len() >= 300 {
                    break;
                }
            }
        }
        result.extend(kept);
    }
    result.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    result.truncate(300);
    Ok(result)
}

/// Already decoded, NMS-free output (Ultralytics end-to-end / YOLOv10 / NMS
/// exports): `[1, N, 6]` rows, or `[1, 6, N]`, of `x1, y1, x2, y2, score,
/// class` in input pixels. Keeps rows at or above `confidence_threshold`
/// without a second NMS.
pub fn decode_end2end_output(
    shape: &[usize],
    values: &[f32],
    confidence_threshold: f32,
) -> Result<Vec<YoloBox>, InferenceError> {
    if !(0.0..=1.0).contains(&confidence_threshold) || shape.len() != 3 || shape[0] != 1 {
        return Err(InferenceError::Output(
            "expected a batch-1 output and a threshold from 0 to 1".into(),
        ));
    }
    let (rows, row_major) = match (shape[1], shape[2]) {
        (rows, 6) => (rows, true),
        (6, rows) => (rows, false),
        _ => {
            return Err(InferenceError::Output(
                "expected NMS-free output shaped [1, N, 6] (x1, y1, x2, y2, score, class)".into(),
            ));
        }
    };
    if rows.checked_mul(6) != Some(values.len()) {
        return Err(InferenceError::Output(
            "output dimensions do not match its values".into(),
        ));
    }
    let at = |row: usize, field: usize| {
        if row_major {
            values[row * 6 + field]
        } else {
            values[field * rows + row]
        }
    };
    let mut result: Vec<YoloBox> = (0..rows)
        .filter_map(|row| {
            let bbox_xyxy = [at(row, 0), at(row, 1), at(row, 2), at(row, 3)];
            let confidence = at(row, 4);
            let class = at(row, 5).round();
            let valid = confidence.is_finite()
                && confidence >= confidence_threshold
                && class.is_finite()
                && (0.0..=u32::MAX as f32).contains(&class)
                && bbox_xyxy.iter().all(|value| value.is_finite())
                && bbox_xyxy[2] > bbox_xyxy[0]
                && bbox_xyxy[3] > bbox_xyxy[1];
            valid.then_some(YoloBox {
                class_id: class as u32,
                confidence,
                bbox_xyxy,
            })
        })
        .collect();
    result.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    result.truncate(300);
    Ok(result)
}

pub fn to_detection_objects(
    boxes: Vec<YoloBox>,
    classes: Option<&std::collections::HashMap<u32, String>>,
    crop_x: u32,
    crop_y: u32,
) -> Vec<DetectionObject> {
    boxes
        .into_iter()
        .map(|detection| DetectionObject {
            class_id: detection.class_id,
            class_name: classes
                .and_then(|names| names.get(&detection.class_id))
                .cloned()
                .unwrap_or_else(|| detection.class_id.to_string()),
            bbox_xyxy: [
                detection.bbox_xyxy[0] + crop_x as f32,
                detection.bbox_xyxy[1] + crop_y as f32,
                detection.bbox_xyxy[2] + crop_x as f32,
                detection.bbox_xyxy[3] + crop_y as f32,
            ],
            confidence: Some(detection.confidence),
            pixel_count: None,
        })
        .collect()
}

fn intersection_over_union(left: [f32; 4], right: [f32; 4]) -> f32 {
    let intersection_width = (left[2].min(right[2]) - left[0].max(right[0])).max(0.0);
    let intersection_height = (left[3].min(right[3]) - left[1].max(right[1])).max(0.0);
    let intersection = intersection_width * intersection_height;
    let left_area = (left[2] - left[0]).max(0.0) * (left[3] - left[1]).max(0.0);
    let right_area = (right[2] - right[0]).max(0.0) * (right[3] - right[1]).max(0.0);
    intersection / (left_area + right_area - intersection).max(f32::EPSILON)
}

#[cfg(feature = "tensorrt")]
fn checked_shape_size(shape: &[usize]) -> Result<usize, InferenceError> {
    shape.iter().try_fold(1usize, |size, dimension| {
        size.checked_mul(*dimension)
            .ok_or_else(|| InferenceError::Output("output tensor is too large".into()))
    })
}

fn runtime_error(error: impl std::fmt::Display) -> InferenceError {
    InferenceError::Runtime(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_class_major_yolo_and_keeps_overlapping_classes() {
        let values = vec![
            10.0, 11.0, 20.0, 20.0, // cx
            10.0, 11.0, 20.0, 20.0, // cy
            8.0, 8.0, 10.0, 10.0, // width
            8.0, 8.0, 10.0, 10.0, // height
            0.9, 0.8, 0.1, 0.1, // class 0
            0.1, 0.1, 0.95, 0.4, // class 1
        ];
        let detections = decode_yolo_output(&[1, 6, 4], &values, 0.5, 0.5).unwrap();
        assert_eq!(detections.len(), 2);
        assert_eq!(detections[0].class_id, 1);
        assert_eq!(detections[1].class_id, 0);
        assert_eq!(detections[0].bbox_xyxy, [15.0, 15.0, 25.0, 25.0]);
    }

    #[test]
    fn decodes_nms_free_rows_without_a_second_nms() {
        let rows = vec![
            10.0, 10.0, 20.0, 30.0, 0.30, 1.0, // below the threshold
            40.0, 40.0, 60.0, 60.0, 0.80, 0.0, // body
            42.0, 41.0, 58.0, 59.0, 0.90, 1.0, // overlapping head: kept
            41.0, 41.0, 59.0, 59.0, 0.70, 0.0, // overlapping body: kept (no NMS)
            0.0, 0.0, 0.0, 0.0, 0.00, 0.0, // padding row
            5.0, 5.0, 4.0, 9.0, 0.95, 2.0, // invalid box
        ];
        let detections = decode_end2end_output(&[1, 6, 6], &rows, 0.5).unwrap();
        let summary: Vec<(u32, f32)> = detections
            .iter()
            .map(|detection| (detection.class_id, detection.confidence))
            .collect();
        assert_eq!(summary, [(1, 0.9), (0, 0.8), (0, 0.7)]);
        assert_eq!(detections[0].bbox_xyxy, [42.0, 41.0, 58.0, 59.0]);
        // Field-major [1, 6, N] (N != 6) gives the same boxes.
        let wide: Vec<f32> = rows[6..12]
            .iter()
            .zip(&rows[12..18])
            .flat_map(|(a, b)| [*a, *b])
            .collect();
        let detections = decode_end2end_output(&[1, 6, 2], &wide, 0.5).unwrap();
        assert_eq!(detections.len(), 2);
        assert_eq!(detections[0].bbox_xyxy, [42.0, 41.0, 58.0, 59.0]);
        assert!(decode_end2end_output(&[1, 300, 9], &vec![0.0; 2700], 0.5).is_err());
        assert!(decode_end2end_output(&[1, 2, 6], &[0.0; 6], 0.5).is_err());
    }

    #[test]
    fn supports_anchor_major_output_and_class_aware_nms() {
        let values = vec![
            20.0, 20.0, 10.0, 10.0, 0.4, 0.95, // candidate 0
            20.5, 20.5, 10.0, 10.0, 0.9, 0.1, // candidate 1
        ];
        let detections = decode_yolo_output(&[1, 2, 6], &values, 0.5, 0.45).unwrap();
        assert_eq!(detections.len(), 2);
        assert_eq!(detections[0].class_id, 1);
        assert_eq!(detections[1].class_id, 0);
    }

    #[test]
    fn confidence_threshold_includes_equal_scores_and_rejects_lower_scores() {
        let at_threshold =
            decode_yolo_output(&[1, 5, 1], &[10.0, 10.0, 4.0, 4.0, 0.5], 0.5, 0.45).unwrap();
        let below_threshold =
            decode_yolo_output(&[1, 5, 1], &[10.0, 10.0, 4.0, 4.0, 0.49], 0.5, 0.45).unwrap();

        assert_eq!(at_threshold.len(), 1);
        assert!(below_threshold.is_empty());
    }

    #[test]
    fn rejects_malformed_output_shapes_and_thresholds() {
        assert!(decode_yolo_output(&[1, 5], &[0.0; 5], 0.5, 0.45).is_err());
        assert!(decode_yolo_output(&[1, 5, 10], &[0.0; 50], 1.1, 0.45).is_err());
    }

    #[test]
    fn maps_yolo_boxes_to_original_frame_coordinates() {
        let objects = to_detection_objects(
            vec![YoloBox {
                class_id: 4,
                confidence: 0.75,
                bbox_xyxy: [1.0, 2.0, 3.0, 4.0],
            }],
            None,
            100,
            200,
        );
        assert_eq!(objects[0].bbox_xyxy, [101.0, 202.0, 103.0, 204.0]);
    }

    /// Loads every runtime the shipped DLLs provide and runs a tiny model
    /// through it. Needs `SYSX_RUNTIME_SMOKE_MODEL` (an ONNX that doubles a
    /// `[1,3,H,W]` input) and the DLL folder on `PATH`/`ORT_DYLIB_PATH`; a
    /// device this machine lacks is reported, not failed.
    #[test]
    #[ignore]
    fn runtime_smoke() {
        let Ok(model) = std::env::var("SYSX_RUNTIME_SMOKE_MODEL") else {
            return;
        };
        let devices = openvino::Core::new()
            .map_err(|error| error.to_string())
            .and_then(|core| {
                core.available_devices()
                    .map(|devices| format!("{devices:?}"))
                    .map_err(|error| error.to_string())
            });
        println!("OpenVINO devices: {devices:?}");
        let image = RgbImage::from_pixel(8, 8, image::Rgb([51, 102, 255]));
        let mut ran = 0;
        for (backend, compute) in [
            ("onnxruntime", "cpu"),
            ("directml", "gpu"),
            ("cuda", "gpu"),
            ("openvino", "cpu"),
            ("openvino", "gpu"),
            ("openvino", "npu"),
        ] {
            let runtime = match backend {
                "openvino" => OpenVinoRuntime::load(&model, "", compute).map(DetectionRuntime::OpenVino),
                _ => OnnxRuntime::load(&model, backend).map(DetectionRuntime::Onnx),
            };
            match runtime.and_then(|mut runtime| runtime.infer(&image)) {
                Ok(output) => {
                    assert_eq!(output.shape, [1, 3, 8, 8]);
                    assert!((output.values[0] - 0.4).abs() < 1e-3, "{}", output.values[0]);
                    println!("{backend}/{compute}: ok on {}", output.device);
                    ran += 1;
                }
                Err(error) => println!("{backend}/{compute}: {error}"),
            }
        }
        assert!(ran > 0, "no runtime ran");
    }
}
