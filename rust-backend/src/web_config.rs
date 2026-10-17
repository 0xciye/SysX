use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower_http::services::ServeDir;

const MAX_MODEL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_SETTINGS_BYTES: usize = 64 * 1024;

#[derive(Clone)]
struct ConfigState {
    ui_root: PathBuf,
    data_dir: PathBuf,
    token: String,
    backend_port: u16,
    config_port: u16,
}

pub async fn start(
    root: impl Into<PathBuf>,
    data_dir: impl Into<PathBuf>,
    token: String,
    backend_port: u16,
    requested_port: u16,
) -> Result<(u16, JoinHandle<()>), String> {
    let root = root.into();
    if !root.join("index.html").is_file() {
        return Err(format!("UI bundle is missing from {}", root.display()));
    }
    let data_dir = data_dir.into();
    let mut selected = None;
    for port in requested_port..requested_port.saturating_add(10) {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            selected = Some((port, listener));
            break;
        }
    }
    let (port, listener) =
        selected.ok_or_else(|| format!("No configuration port available from {requested_port}"))?;
    let state = ConfigState {
        ui_root: root.clone(),
        data_dir,
        token,
        backend_port,
        config_port: port,
    };
    let app = Router::new()
        .route("/config.html", get(config_page))
        .route(
            "/licenses/abcurves.txt",
            get(|| async { include_str!("../../third-party/abcurves/LICENSE") }),
        )
        .route("/monitor.html", get(|| async { StatusCode::NOT_FOUND }))
        .route("/api/session", get(session))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/upload-model", post(upload_model))
        .route("/api/commit-model-bundle", post(commit_model_bundle))
        .route(
            "/api/model-bundles/{id}",
            axum::routing::delete(delete_model_bundle),
        )
        .fallback_service(ServeDir::new(root))
        .layer(DefaultBodyLimit::max(MAX_MODEL_BYTES as usize))
        .with_state(state);
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            eprintln!("Local configuration server stopped: {error}");
        }
    });
    Ok((port, task))
}

async fn config_page(State(state): State<ConfigState>) -> Result<Html<String>, ApiError> {
    fs::read_to_string(state.ui_root.join("index.html"))
        .await
        .map(Html)
        .map_err(|error| ApiError::internal(error.to_string()))
}

async fn session(
    State(state): State<ConfigState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    verify_local(&headers, state.config_port, false)?;
    Ok(Json(
        json!({ "host": "127.0.0.1", "port": state.backend_port, "token": state.token }),
    ))
}

async fn get_settings(
    State(state): State<ConfigState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    verify_local(&headers, state.config_port, false)?;
    let path = state.data_dir.join("app-settings.json");
    let settings = crate::config::load_settings(&path);
    serde_json::to_value(settings)
        .map(Json)
        .map_err(|error| ApiError::bad_request(error.to_string()))
}

async fn put_settings(
    State(state): State<ConfigState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    verify_local(&headers, state.config_port, true)?;
    let bytes = collect_limited(body, MAX_SETTINGS_BYTES).await?;
    let path = state.data_dir.join("app-settings.json");
    let mut settings: crate::config::AppSettings =
        serde_json::from_slice(&bytes).map_err(|error| ApiError::bad_request(error.to_string()))?;
    // Older/config clients may not know about profiles. Keep the existing
    // slots in that case instead of replacing the other two profiles.
    if settings.profiles.is_empty() {
        let existing = crate::config::load_settings(&path);
        settings.profiles = existing.profiles;
    }
    settings
        .validate()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    crate::config::save_settings(path, &settings)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(json!({ "saved": true })))
}

async fn upload_model(
    State(state): State<ConfigState>,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    verify_local(&headers, state.config_port, true)?;
    let bundle_id = headers
        .get("x-bundle-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !valid_bundle_id(bundle_id) {
        return Err(ApiError::bad_request("Invalid model bundle ID"));
    }
    let filename = headers
        .get("x-filename")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let extension = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = artifact_kind(&extension).ok_or_else(|| {
        ApiError::bad_request("Upload ONNX, TensorRT engine, or OpenVINO IR files")
    })?;
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if length == 0 || length > MAX_MODEL_BYTES {
        return Err(ApiError::bad_request(
            "Model upload must be non-empty and smaller than 2 GiB",
        ));
    }
    let staging_dir = state
        .data_dir
        .join("models")
        .join(".uploads")
        .join(bundle_id);
    fs::create_dir_all(&staging_dir)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let filename = staged_filename(kind, &extension);
    let final_path = staging_dir.join(filename);
    if final_path.exists()
        || (kind == ArtifactKind::TensorRt
            && (staging_dir.join("tensorrt.engine").exists()
                || staging_dir.join("tensorrt.plan").exists()))
    {
        return Err(ApiError::bad_request(
            "A model bundle can contain only one file for each format",
        ));
    }
    let temporary_path = staging_dir.join(format!(".upload-{}.tmp", kind.key()));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let mut stream = body.into_data_stream();
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                drop(output);
                let _ = fs::remove_file(&temporary_path).await;
                return Err(ApiError::bad_request(error.to_string()));
            }
        };
        written = written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| ApiError::bad_request("Model upload is too large"))?;
        if written > MAX_MODEL_BYTES || written > length {
            drop(output);
            let _ = fs::remove_file(&temporary_path).await;
            return Err(ApiError::bad_request(
                "Model upload exceeded its declared size",
            ));
        }
        if let Err(error) = output.write_all(&chunk).await {
            drop(output);
            let _ = fs::remove_file(&temporary_path).await;
            return Err(ApiError::internal(error.to_string()));
        }
    }
    if written != length {
        drop(output);
        let _ = fs::remove_file(&temporary_path).await;
        return Err(ApiError::bad_request(
            "Model upload ended before all bytes were received",
        ));
    }
    if let Err(error) = output.flush().await {
        drop(output);
        let _ = fs::remove_file(&temporary_path).await;
        return Err(ApiError::internal(error.to_string()));
    }
    drop(output);
    fs::rename(&temporary_path, &final_path)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok((StatusCode::CREATED, Json(json!({ "uploaded": true }))))
}

async fn commit_model_bundle(
    State(state): State<ConfigState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    verify_local(&headers, state.config_port, true)?;
    let bundle_id = headers
        .get("x-bundle-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !valid_bundle_id(bundle_id) {
        return Err(ApiError::bad_request("Invalid model bundle ID"));
    }
    let model_dir = state.data_dir.join("models");
    let staging_dir = model_dir.join(".uploads").join(bundle_id);
    let onnx = staging_dir.join("onnx.onnx");
    let openvino_xml = staging_dir.join("openvino.xml");
    let openvino_bin = staging_dir.join("openvino.bin");
    let tensorrt_engine = staging_dir.join("tensorrt.engine");
    let tensorrt_plan = staging_dir.join("tensorrt.plan");
    let tensorrt = match (tensorrt_engine.is_file(), tensorrt_plan.is_file()) {
        (true, false) => tensorrt_engine,
        (false, true) => tensorrt_plan,
        _ => {
            return Err(ApiError::bad_request(
                "Bundle requires exactly one TensorRT .engine or .plan file",
            ))
        }
    };
    for path in [&onnx, &openvino_xml, &openvino_bin, &tensorrt] {
        let metadata = fs::metadata(path).await.map_err(|_| {
            ApiError::bad_request("Bundle requires ONNX, TensorRT, and OpenVINO XML/BIN files")
        })?;
        if metadata.len() == 0 {
            return Err(ApiError::bad_request(
                "Model bundle files must not be empty",
            ));
        }
    }
    let final_dir = model_dir.join(bundle_id);
    if final_dir.exists() {
        return Err(ApiError::bad_request("Model bundle already exists"));
    }
    fs::rename(&staging_dir, &final_dir)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let artifacts = crate::config::ModelArtifacts {
        onnx: final_dir.join("onnx.onnx").to_string_lossy().into_owned(),
        tensorrt: final_dir
            .join(tensorrt.file_name().unwrap_or_default())
            .to_string_lossy()
            .into_owned(),
        openvino_xml: final_dir
            .join("openvino.xml")
            .to_string_lossy()
            .into_owned(),
        openvino_bin: final_dir
            .join("openvino.bin")
            .to_string_lossy()
            .into_owned(),
    };
    Ok(Json(json!({ "artifacts": artifacts })))
}

async fn delete_model_bundle(
    State(state): State<ConfigState>,
    headers: HeaderMap,
    AxumPath(bundle_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    verify_local(&headers, state.config_port, true)?;
    if !valid_bundle_id(&bundle_id) {
        return Err(ApiError::bad_request("Invalid model bundle ID"));
    }
    let staging_dir = state
        .data_dir
        .join("models")
        .join(".uploads")
        .join(bundle_id);
    match fs::remove_dir_all(staging_dir).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(StatusCode::NO_CONTENT),
        Err(error) => Err(ApiError::internal(error.to_string())),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArtifactKind {
    Onnx,
    TensorRt,
    OpenVinoXml,
    OpenVinoBin,
}

impl ArtifactKind {
    fn key(self) -> &'static str {
        match self {
            Self::Onnx => "onnx",
            Self::TensorRt => "tensorrt",
            Self::OpenVinoXml => "openvino-xml",
            Self::OpenVinoBin => "openvino-bin",
        }
    }
}

fn artifact_kind(extension: &str) -> Option<ArtifactKind> {
    match extension.to_ascii_lowercase().as_str() {
        "onnx" => Some(ArtifactKind::Onnx),
        "engine" | "plan" => Some(ArtifactKind::TensorRt),
        "xml" => Some(ArtifactKind::OpenVinoXml),
        "bin" => Some(ArtifactKind::OpenVinoBin),
        _ => None,
    }
}

fn staged_filename(kind: ArtifactKind, extension: &str) -> String {
    match kind {
        ArtifactKind::Onnx => "onnx.onnx".into(),
        ArtifactKind::TensorRt => format!("tensorrt.{}", extension.to_ascii_lowercase()),
        ArtifactKind::OpenVinoXml => "openvino.xml".into(),
        ArtifactKind::OpenVinoBin => "openvino.bin".into(),
    }
}

fn valid_bundle_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

async fn collect_limited(body: Body, limit: usize) -> Result<Vec<u8>, ApiError> {
    let mut stream = body.into_data_stream();
    let mut result = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ApiError::bad_request(error.to_string()))?;
        if result.len().saturating_add(chunk.len()) > limit {
            return Err(ApiError::bad_request("Settings payload exceeds 64 KiB"));
        }
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

fn verify_local(headers: &HeaderMap, port: u16, require_origin: bool) -> Result<(), ApiError> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let host_only = host.split(':').next().unwrap_or("");
    if !["127.0.0.1", "localhost"].contains(&host_only) {
        return Err(ApiError::forbidden());
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let expected = format!("http://127.0.0.1:{port}");
    if (require_origin && origin != Some(expected.as_str()))
        || origin
            .is_some_and(|value| value != expected && value != format!("http://localhost:{port}"))
    {
        return Err(ApiError::forbidden());
    }
    Ok(())
}

struct ApiError {
    status: StatusCode,
    message: String,
}
impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
    fn forbidden() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: "Local request rejected".into(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_headers(port: u16, include_origin: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, format!("127.0.0.1:{port}").parse().unwrap());
        if include_origin {
            headers.insert(
                header::ORIGIN,
                format!("http://127.0.0.1:{port}").parse().unwrap(),
            );
        }
        headers
    }

    #[tokio::test]
    async fn settings_api_loads_and_updates_the_app_data_file() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir =
            std::env::temp_dir().join(format!("sysx-settings-api-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&data_dir).await.unwrap();
        let state = ConfigState {
            ui_root: data_dir.clone(),
            data_dir: data_dir.clone(),
            token: "a".repeat(64),
            backend_port: 43128,
            config_port: 43127,
        };

        let mut settings = crate::config::AppSettings::default();
        settings.sens = 1.25;
        let response = match put_settings(
            State(state.clone()),
            local_headers(state.config_port, true),
            Body::from(serde_json::to_vec(&settings).unwrap()),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => panic!("settings update failed: {}", error.message),
        };
        assert_eq!(response.0["saved"], true);
        assert!(data_dir.join("app-settings.json").is_file());

        let response = match get_settings(State(state), local_headers(43127, false)).await {
            Ok(response) => response,
            Err(error) => panic!("settings load failed: {}", error.message),
        };
        assert_eq!(response.0["sens"], 1.25);
        let _ = fs::remove_dir_all(data_dir).await;
    }

    #[test]
    fn model_bundle_upload_accepts_only_expected_artifact_formats() {
        assert_eq!(artifact_kind("ONNX"), Some(ArtifactKind::Onnx));
        assert_eq!(artifact_kind("engine"), Some(ArtifactKind::TensorRt));
        assert_eq!(artifact_kind("PLAN"), Some(ArtifactKind::TensorRt));
        assert_eq!(artifact_kind("xml"), Some(ArtifactKind::OpenVinoXml));
        assert_eq!(artifact_kind("bin"), Some(ArtifactKind::OpenVinoBin));
        assert_eq!(artifact_kind("pt"), None);
        assert_eq!(
            staged_filename(ArtifactKind::TensorRt, "PLAN"),
            "tensorrt.plan"
        );
    }

    #[test]
    fn bundle_ids_are_uuid_shaped_and_cannot_select_a_path() {
        assert!(valid_bundle_id("2af3e8c1-7341-4c97-a191-61b4bd29f6d2"));
        assert!(!valid_bundle_id("../../models"));
        assert!(!valid_bundle_id("2af3e8c173414c97a19161b4bd29f6d2"));
    }

    #[tokio::test]
    async fn model_bundle_upload_commits_all_three_runtime_formats() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir =
            std::env::temp_dir().join(format!("sysx-model-bundle-{}-{suffix}", std::process::id()));
        let config_port = 43127;
        let state = ConfigState {
            ui_root: data_dir.clone(),
            data_dir: data_dir.clone(),
            token: "a".repeat(64),
            backend_port: 43128,
            config_port,
        };
        let bundle_id = "e1b2c3d4-5678-4abc-a123-0123456789ab";
        let files = [
            ("detector.onnx", b"onnx".as_slice()),
            ("detector.engine", b"tensorrt".as_slice()),
            ("detector.xml", b"openvino xml".as_slice()),
            ("detector.bin", b"openvino weights".as_slice()),
        ];

        for (filename, bytes) in files {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::HOST,
                format!("127.0.0.1:{config_port}").parse().unwrap(),
            );
            headers.insert(
                header::ORIGIN,
                format!("http://127.0.0.1:{config_port}").parse().unwrap(),
            );
            headers.insert("x-bundle-id", bundle_id.parse().unwrap());
            headers.insert("x-filename", filename.parse().unwrap());
            headers.insert(
                header::CONTENT_LENGTH,
                bytes.len().to_string().parse().unwrap(),
            );
            let response =
                upload_model(State(state.clone()), headers, Body::from(bytes.to_vec())).await;
            let (status, _) = match response {
                Ok(value) => value,
                Err(error) => panic!("upload {filename} failed: {}", error.message),
            };
            assert_eq!(status, StatusCode::CREATED);
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            header::HOST,
            format!("127.0.0.1:{config_port}").parse().unwrap(),
        );
        headers.insert(
            header::ORIGIN,
            format!("http://127.0.0.1:{config_port}").parse().unwrap(),
        );
        headers.insert("x-bundle-id", bundle_id.parse().unwrap());
        let response = match commit_model_bundle(State(state), headers).await {
            Ok(value) => value,
            Err(error) => panic!("model bundle commit failed: {}", error.message),
        };
        let artifacts: crate::config::ModelArtifacts =
            serde_json::from_value(response.0["artifacts"].clone()).unwrap();
        artifacts.validate().unwrap();
        for artifact in artifacts.paths() {
            assert!(Path::new(artifact).is_file(), "missing artifact {artifact}");
        }

        let _ = fs::remove_dir_all(data_dir).await;
    }
}
