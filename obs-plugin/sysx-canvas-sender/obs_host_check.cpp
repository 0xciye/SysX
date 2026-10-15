// Headless libobs host for the SysX sender: loads the real obs.dll, starts
// D3D11 video without any window, loads the plugin exactly like OBS does
// (obs_open_module + obs_init_module), lets the render loop run, then shuts
// libobs down (which unloads the plugin).
//
//   cd <obs>/bin/64bit
//   sysx-obs-host-check.exe <path-to-sysx-canvas-sender.dll> <seconds>
//
// The plugin reads %APPDATA%\obs-studio\plugin_config\sysx-canvas-sender\sender.ini,
// so point APPDATA at a scratch folder to keep the real OBS profile untouched.
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>

#include <obs.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>

int main(int argc, char **argv) {
  if (argc < 3) {
    std::fprintf(stderr, "usage: sysx-obs-host-check <plugin.dll> <seconds>\n");
    return 1;
  }
  // libobs loads libobs-d3d11 with LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, which
  // skips the working directory; register OBS's bin folder explicitly.
  wchar_t bin[MAX_PATH];
  GetCurrentDirectoryW(MAX_PATH, bin);
  AddDllDirectory(bin);
  SetDllDirectoryW(bin);
  HMODULE obs = LoadLibraryA("obs.dll");
  if (!obs) {
    std::fprintf(stderr, "obs.dll not found; run from OBS bin\\64bit\n");
    return 1;
  }
  auto startup = reinterpret_cast<bool (*)(const char *, const char *, profiler_name_store_t *)>(
      GetProcAddress(obs, "obs_startup"));
  auto reset_video = reinterpret_cast<int (*)(obs_video_info *)>(GetProcAddress(obs, "obs_reset_video"));
  auto open_module = reinterpret_cast<int (*)(obs_module_t **, const char *, const char *)>(
      GetProcAddress(obs, "obs_open_module"));
  auto init_module = reinterpret_cast<bool (*)(obs_module_t *)>(GetProcAddress(obs, "obs_init_module"));
  auto shutdown = reinterpret_cast<void (*)()>(GetProcAddress(obs, "obs_shutdown"));
  auto version = reinterpret_cast<const char *(*)()>(GetProcAddress(obs, "obs_get_version_string"));
  if (!startup || !reset_video || !open_module || !init_module || !shutdown || !version) return 1;

  std::printf("{\"event\":\"libobs\",\"version\":\"%s\"}\n", version());
  std::fflush(stdout);
  if (!startup("en-US", nullptr, nullptr)) return 2;

  // Optional: argv[3] = render FPS (default 60), argv[4] = "hdr" for a
  // Rec.2100 PQ canvas (RGBA16F main texture; the plugin must skip it).
  const bool hdr = argc > 4 && std::strcmp(argv[4], "hdr") == 0;
  obs_video_info video{};
  video.graphics_module = "libobs-d3d11";
  video.fps_num = argc > 3 ? static_cast<uint32_t>(std::atoi(argv[3])) : 60;
  video.fps_den = 1;
  video.base_width = video.output_width = 1920;
  video.base_height = video.output_height = 1080;
  video.output_format = hdr ? VIDEO_FORMAT_P010 : VIDEO_FORMAT_NV12;
  video.gpu_conversion = true;
  video.colorspace = hdr ? VIDEO_CS_2100_PQ : VIDEO_CS_709;
  video.range = VIDEO_RANGE_PARTIAL;
  video.scale_type = OBS_SCALE_BICUBIC;
  if (const int result = reset_video(&video); result != OBS_VIDEO_SUCCESS) {
    std::printf("{\"event\":\"reset_video_failed\",\"code\":%d}\n", result);
    shutdown();
    return 3;
  }

  obs_module_t *module = nullptr;
  if (const int result = open_module(&module, argv[1], nullptr); result != MODULE_SUCCESS) {
    std::printf("{\"event\":\"open_module_failed\",\"code\":%d}\n", result);
    shutdown();
    return 4;
  }
  const bool loaded = init_module(module);
  std::printf("{\"event\":\"init_module\",\"loaded\":%s}\n", loaded ? "true" : "false");
  std::fflush(stdout);
  if (loaded) Sleep(static_cast<DWORD>(std::atoi(argv[2])) * 1000u);
  shutdown();
  std::printf("{\"event\":\"shutdown\"}\n");
  return loaded ? 0 : 5;
}
