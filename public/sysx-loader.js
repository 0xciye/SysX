const fallback = document.getElementById("sysx-boot-fallback");
if (fallback) {
  fallback.setAttribute("data-bootstrap", "loader-started");
}

async function start() {
  if (window.__TAURI__?.core?.invoke) {
    const encoded = await window.__TAURI__.core.invoke("ui_wasm_asset");
    const binary = Uint8Array.from(atob(encoded), (character) => character.charCodeAt(0));
    window.__SYSX_NATIVE_WASM_SOURCE__ = binary;
    const boot = await import("./sysx-boot.js");
    await boot.startRustUi(binary);
    return;
  }
  const boot = await import("./sysx-boot.js");
  await boot.startRustUi();
}

start().catch((error) => {
  if (fallback) fallback.textContent = `Rust UI bootstrap error: ${error?.message || error}`;
});
