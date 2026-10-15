import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, chmodSync, copyFileSync } from "node:fs";
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
if (
  (process.env.TENSORRT_PATH || process.env.TENSORRT_INCLUDE_DIR)
  && process.env.CUDA_PATH
) {
  args.push("--features", "tensorrt-shim");
}

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
if (binaryExtension === "") chmodSync(sidecar, 0o755);

if (process.platform === "darwin") {
  const signature = spawnSync("codesign", ["--force", "--deep", "--sign", "-", sidecar], {
    cwd: root,
    stdio: "inherit",
  });
  if (signature.error) throw signature.error;
  if (signature.status !== 0) process.exit(signature.status ?? 1);
}

const help = spawnSync(sidecar, ["--help"], { cwd: root, stdio: "inherit" });
if (help.error) throw help.error;
if (help.status !== 0) process.exit(help.status ?? 1);
