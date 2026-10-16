import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const ui = path.join(root, "rust-ui");
const executable = process.platform === "win32" ? "trunk.exe" : "trunk";
const bundledTrunk = path.join(homedir(), ".cargo", "bin", executable);
const trunk = existsSync(bundledTrunk) ? bundledTrunk : "trunk";
const rustcName = process.platform === "win32" ? "rustc.exe" : "rustc";
const bundledRustc = path.join(homedir(), ".cargo", "bin", rustcName);
const rustc = process.env.RUSTC ?? (existsSync(bundledRustc) ? bundledRustc : rustcName);
const env = { ...process.env };
delete env.NO_COLOR;
env.PATH = [path.join(homedir(), ".cargo", "bin"), env.PATH]
  .filter(Boolean)
  .join(path.delimiter);

const mode = process.argv[2] ?? "build";
if (mode === "serve") {
  const server = spawn(trunk, ["serve"], { cwd: ui, env, stdio: "inherit" });
  for (const signal of ["SIGINT", "SIGTERM"]) {
    process.on(signal, () => server.kill(signal));
  }
  server.on("error", (error) => {
    console.error(`Could not start Trunk: ${error.message}`);
    process.exitCode = 1;
  });
  server.on("exit", (code, signal) => {
    process.exitCode = code ?? (signal ? 1 : 0);
  });
} else if (mode === "build") {
  const result = spawnSync(trunk, ["build", "--release"], {
    cwd: ui,
    env,
    stdio: "inherit",
  });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
  const targetDirectory = path.join(root, "target");
  mkdirSync(targetDirectory, { recursive: true });
  const stripper = path.join(
    targetDirectory,
    process.platform === "win32" ? "strip-wasm-debug.exe" : "strip-wasm-debug",
  );
  const compile = spawnSync(
    rustc,
    [
      path.join(root, "scripts", "strip-wasm-debug.rs"),
      "--edition",
      "2021",
      "-O",
      "-o",
      stripper,
    ],
    { cwd: root, env, stdio: "inherit" },
  );
  if (compile.error) throw compile.error;
  if (compile.status !== 0) process.exit(compile.status ?? 1);
  const strip = spawnSync(stripper, [path.join(root, "dist")], {
    cwd: root,
    env,
    stdio: "inherit",
  });
  if (strip.error) throw strip.error;
  if (strip.status !== 0) process.exit(strip.status ?? 1);
} else {
  throw new Error(`Unknown Rust UI mode: ${mode}`);
}
