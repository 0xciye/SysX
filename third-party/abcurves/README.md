# ABCurves C Renderer subset

Source: https://github.com/optima-manent/ABCurves
Commit: e197441f8d343d3b4a4069c7cae20764d0953d15
Author: Optima Manent. License: MIT; full notice in LICENSE.

`include/` and `src/` are unchanged copies of upstream `runtime/c/`.
`renderer_global_h80.bin` is the unchanged upstream frozen model from `models/`.
UPSTREAM.json records their SHA-256 digests. SysX's ownership/axis-conversion shim
is outside this directory in `rust-backend/src/movement/abcurves_bridge.c`.

This is the Renderer only, not the Continuous/Static Planner, Python package,
prebuilt DLL, raw movement datasets, or any shipped user profile. See
`docs/ABCURVES_INTEGRATION.md` for the count, timing and lifecycle boundaries.
