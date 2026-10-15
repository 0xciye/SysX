const fallback = document.getElementById("sysx-boot-fallback");
if (fallback) {
  fallback.setAttribute("data-bootstrap", "loader-started");
}

// sysx-boot.js initializes the Rust/WASM UI when imported.
import("./sysx-boot.js").catch((error) => {
  if (fallback) fallback.textContent = `Rust UI bootstrap error: ${error?.message || error}`;
});
