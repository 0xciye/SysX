import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, chmodSync, copyFileSync, readdirSync, rmSync } from "node:fs";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const hostExtension = process.platform === "win32" ? ".exe" : "";

function tool(name, envName) {
  if (process.env[envName]) return process.env[envName];
  const candidate = path.join(homedir(), ".cargo", "bin", `${name}${hostExtension}`);
  return existsSync(candidate) ? candidate : name;
}

const cargo = tool("cargo", "CARGO");
const rustc = tool("rustc", "RUSTC");
const target = process.env.TAURI_ENV_TARGET_TRIPLE
  ?? execFileSync(rustc, ["--print", "host-tuple"], { encoding: "utf8" }).trim();
const binaryExtension = target.includes("windows") ? ".exe" : "";
const profile = process.argv.includes("--debug") ? "debug" : "release";
const args = [
  "build",
  "--manifest-path",
  path.join(root, "rust-backend", "Cargo.toml"),
  "--bin",
  "receiver-backend",
  "--target",
  target,
];
if (profile === "release") args.push("--release");
const features = [];
if (
  (process.env.TENSORRT_PATH || process.env.TENSORRT_INCLUDE_DIR)
  && process.env.CUDA_PATH
) {
  features.push("tensorrt-shim");
}
// Object Detection GPU execution providers. DirectML runs on any DirectX 12
// GPU (AMD, Intel, NVIDIA); CoreML on macOS. CUDA and TensorRT through ONNX
// Runtime are opt-in: their provider DLLs are several hundred MB and need the
// NVIDIA CUDA/cuDNN/TensorRT runtimes on the user's machine.
// Windows loads ONNX Runtime from the onnxruntime.dll shipped next to
// SysX.exe (user request, 2026-10-31), so the backend links it dynamically.
if (target.includes("windows")) features.push("ort/load-dynamic", "directml");
if (target.includes("apple")) features.push("coreml");
if (process.env.SYSX_ORT_CUDA === "1") features.push("cuda", "ort-tensorrt");
if (features.length > 0) args.push("--features", features.join(","));

const build = spawnSync(cargo, args, { cwd: root, stdio: "inherit" });
if (build.error) throw build.error;
if (build.status !== 0) process.exit(build.status ?? 1);

const targetDir = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "rust-backend/target");
const binary = path.join(targetDir, target, profile, `receiver-backend${binaryExtension}`);
const sidecarDir = path.join(root, "src-tauri", "binaries");
const sidecar = path.join(sidecarDir, `receiver-backend-${target}${binaryExtension}`);
if (!existsSync(binary)) throw new Error(`Rust backend build did not produce ${binary}`);
mkdirSync(sidecarDir, { recursive: true });
copyFileSync(binary, sidecar);
// Runtime libraries ship next to SysX.exe, not inside it. They come from
// SYSX_ORT_DIR (an ONNX Runtime DirectML release: onnxruntime.dll,
// DirectML.dll, optional onnxruntime_providers_*.dll and OpenVINO DLLs) and
// from anything the backend build copied next to its binary.
const libraryPattern = /\.(dll|dylib|so)$/i;
for (const name of readdirSync(sidecarDir)) {
  if (libraryPattern.test(name)) rmSync(path.join(sidecarDir, name));
}
const librarySources = [path.dirname(binary)];
if (process.env.SYSX_ORT_DIR) librarySources.push(path.resolve(process.env.SYSX_ORT_DIR));
for (const dir of librarySources) {
  if (!existsSync(dir)) throw new Error(`SYSX_ORT_DIR does not exist: ${dir}`);
  for (const name of readdirSync(dir)) {
    if (libraryPattern.test(name)) copyFileSync(path.join(dir, name), path.join(sidecarDir, name));
  }
}
if (target.includes("windows") && !existsSync(path.join(sidecarDir, "onnxruntime.dll"))) {
  console.warn(
    "warning: no onnxruntime.dll for the Windows build. Set SYSX_ORT_DIR to an ONNX Runtime "
      + "DirectML release folder (1.22 or newer); Pixel Seek runs without it, Object Detection does not.",
  );
}
// Next to the release host too, so target/release/SysX.exe runs as shipped.
const hostDir = path.join(root, "src-tauri", "target", profile);
mkdirSync(hostDir, { recursive: true });
for (const name of readdirSync(sidecarDir)) {
  if (libraryPattern.test(name)) copyFileSync(path.join(sidecarDir, name), path.join(hostDir, name));
}
if (binaryExtension === "") chmodSync(sidecar, 0o755);
if (binaryExtension === "") {
  const debugSidecar = path.join(sidecarDir, "receiver-backend");
  copyFileSync(binary, debugSidecar);
  chmodSync(debugSidecar, 0o755);
}

const help = spawnSync(sidecar, ["--help"], { cwd: root, stdio: "inherit" });
if (help.error) throw help.error;
if (help.status !== 0) process.exit(help.status ?? 1);
