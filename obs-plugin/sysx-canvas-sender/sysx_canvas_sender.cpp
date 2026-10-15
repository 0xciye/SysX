// SysX OBS canvas sender: captures the OBS main texture, crops the
// receiver-requested centered square on the GPU, JPEG-encodes it off the render
// thread and sends it to SysX using the RCVR v1 UDP protocol
// (docs/OBS_RECEIVER_PROTOCOL.md).
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>

#include <obs-module.h>
#include <graphics/graphics.h>

#include <csetjmp>
#include <cstdio>
#include <jpeglib.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

OBS_DECLARE_MODULE()

MODULE_EXPORT const char *obs_module_name(void) { return "SysX Canvas Sender"; }
MODULE_EXPORT const char *obs_module_description(void) {
  return "Streams the centered OBS canvas crop to a SysX receiver over UDP.";
}

namespace {

constexpr size_t kMaxJpegBytes = 64u * 1024u * 1024u;
constexpr size_t kPayloadBytes = 1170;
constexpr size_t kHeaderBytes = 30;
constexpr char kControlPrefix[] = "SYSCFG 1 CROP ";
constexpr int kSendBufferBytes = 8 * 1024 * 1024;

// Runtime configuration, read from
// %APPDATA%\obs-studio\plugin_config\sysx-canvas-sender\sender.ini.
// The file is created with these defaults on first load.
struct Config {
  std::string host = "127.0.0.1";
  uint16_t port = 4200;
  int jpeg_quality = 80;
  // 0 = every OBS render frame.
  uint32_t max_fps = 0;
  // 4:4:4 keeps small colored targets intact for Pixel Seek; 0 uses 4:2:0.
  bool chroma_444 = true;
  // Startup crop until the receiver's first SYSCFG control packet arrives.
  uint32_t default_crop = 640;
};

// libobs functions are resolved from the already-loaded obs.dll so the module
// builds against headers plus a minimal import library.
struct ObsApi {
  using AddRendered = void (*)(void (*)(void *), void *);
  using RemoveRendered = void (*)(void (*)(void *), void *);
  using GetMainTexture = gs_texture_t *(*)();
  using EnterGraphics = void (*)();
  using LeaveGraphics = void (*)();
  using TextureSize = uint32_t (*)(const gs_texture_t *);
  using TextureFormat = enum gs_color_format (*)(const gs_texture_t *);
  using TextureCreate = gs_texture_t *(*)(uint32_t, uint32_t, enum gs_color_format,
                                           uint32_t, const uint8_t **, uint32_t);
  using TextureDestroy = void (*)(gs_texture_t *);
  using CopyTextureRegion = void (*)(gs_texture_t *, uint32_t, uint32_t, gs_texture_t *,
                                     uint32_t, uint32_t, uint32_t, uint32_t);
  using StageTexture = void (*)(gs_stagesurf_t *, gs_texture_t *);
  using StageCreate = gs_stagesurf_t *(*)(uint32_t, uint32_t, enum gs_color_format);
  using StageDestroy = void (*)(gs_stagesurf_t *);
  using StageMap = bool (*)(gs_stagesurf_t *, uint8_t **, uint32_t *);
  using StageUnmap = void (*)(gs_stagesurf_t *);

  AddRendered add_rendered = nullptr;
  RemoveRendered remove_rendered = nullptr;
  GetMainTexture get_main_texture = nullptr;
  EnterGraphics enter_graphics = nullptr;
  LeaveGraphics leave_graphics = nullptr;
  TextureSize texture_width = nullptr;
  TextureSize texture_height = nullptr;
  TextureFormat texture_format = nullptr;
  TextureCreate texture_create = nullptr;
  TextureDestroy texture_destroy = nullptr;
  CopyTextureRegion copy_texture_region = nullptr;
  StageTexture stage_texture = nullptr;
  StageCreate stage_create = nullptr;
  StageDestroy stage_destroy = nullptr;
  StageMap stage_map = nullptr;
  StageUnmap stage_unmap = nullptr;

  template <typename T>
  static void resolve(HMODULE module, const char *name, T &out) {
    out = reinterpret_cast<T>(GetProcAddress(module, name));
  }

  bool load() {
    HMODULE module = GetModuleHandleA("obs.dll");
    if (!module) return false;
    resolve(module, "obs_add_main_rendered_callback", add_rendered);
    resolve(module, "obs_remove_main_rendered_callback", remove_rendered);
    resolve(module, "obs_get_main_texture", get_main_texture);
    resolve(module, "obs_enter_graphics", enter_graphics);
    resolve(module, "obs_leave_graphics", leave_graphics);
    resolve(module, "gs_texture_get_width", texture_width);
    resolve(module, "gs_texture_get_height", texture_height);
    resolve(module, "gs_texture_get_color_format", texture_format);
    resolve(module, "gs_texture_create", texture_create);
    resolve(module, "gs_texture_destroy", texture_destroy);
    resolve(module, "gs_copy_texture_region", copy_texture_region);
    resolve(module, "gs_stage_texture", stage_texture);
    resolve(module, "gs_stagesurface_create", stage_create);
    resolve(module, "gs_stagesurface_destroy", stage_destroy);
    resolve(module, "gs_stagesurface_map", stage_map);
    resolve(module, "gs_stagesurface_unmap", stage_unmap);
    return add_rendered &&remove_rendered && get_main_texture && enter_graphics &&
           leave_graphics && texture_width && texture_height && texture_format &&
           texture_create && texture_destroy && copy_texture_region && stage_texture &&
           stage_create && stage_destroy && stage_map && stage_unmap;
  }
};

ObsApi obs_api;


uint64_t monotonic_ns() {
  return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(
      std::chrono::steady_clock::now().time_since_epoch()).count());
}

std::string trim(const std::string &value) {
  const auto first = value.find_first_not_of(" \t\r\n");
  if (first == std::string::npos) return {};
  const auto last = value.find_last_not_of(" \t\r\n");
  return value.substr(first, last - first + 1);
}

std::filesystem::path config_path() {
  const char *appdata = std::getenv("APPDATA");
  std::filesystem::path root = appdata ? appdata : ".";
  return root / "obs-studio" / "plugin_config" / "sysx-canvas-sender" / "sender.ini";
}

void write_default_config(const std::filesystem::path &path, const Config &config) {
  std::error_code error;
  std::filesystem::create_directories(path.parent_path(), error);
  std::ofstream file(path);
  if (!file) return;
  file << "; SysX OBS canvas sender. Restart OBS after editing.\n"
       << "; host: SysX receiver IPv4 address or hostname. port: receiver UDP port.\n"
       << "host = " << config.host << "\n"
       << "port = " << config.port << "\n"
       << "; JPEG quality 1-100.\n"
       << "jpeg_quality = " << config.jpeg_quality << "\n"
       << "; Frames per second cap; 0 sends every OBS render frame.\n"
       << "max_fps = " << config.max_fps << "\n"
       << "; 1 = 4:4:4 chroma (best for Pixel Seek colors), 0 = 4:2:0 (smaller).\n"
       << "chroma_444 = " << (config.chroma_444 ? 1 : 0) << "\n"
       << "; Crop used until the receiver reports its Crop Resolution.\n"
       << "default_crop = " << config.default_crop << "\n";
}

Config load_config() {
  Config config;
  const auto path = config_path();
  std::ifstream file(path);
  if (!file) {
    write_default_config(path, config);
    return config;
  }
  std::string line;
  while (std::getline(file, line)) {
    line = trim(line);
    if (line.empty() || line[0] == ';' || line[0] == '#' || line[0] == '[') continue;
    const auto equals = line.find('=');
    if (equals == std::string::npos) continue;
    const std::string key = trim(line.substr(0, equals));
    const std::string value = trim(line.substr(equals + 1));
    const long number = std::strtol(value.c_str(), nullptr, 10);
    if (key == "host" && !value.empty()) config.host = value;
    else if (key == "port" && number >= 1 && number <= 65535) config.port = static_cast<uint16_t>(number);
    else if (key == "jpeg_quality" && number >= 1 && number <= 100) config.jpeg_quality = static_cast<int>(number);
    else if (key == "max_fps" && number >= 0 && number <= 1000) config.max_fps = static_cast<uint32_t>(number);
    else if (key == "chroma_444") config.chroma_444 = number != 0;
    else if (key == "default_crop" && number >= 1 && number <= 8192) config.default_crop = static_cast<uint32_t>(number);
  }
  return config;
}

struct Frame {
  uint32_t width = 0;
  uint32_t height = 0;
  uint64_t timestamp = 0;
  std::vector<uint8_t> bgra; // tightly packed, width * 4 bytes per row
};

class Sender {
 public:
  bool start(const Config &config) {
    config_ = config;
    crop_size_.store(config.default_crop);
    WSADATA data{};
    if (WSAStartup(MAKEWORD(2, 2), &data) != 0) return false;
    winsock_ready_ = true;

    addrinfo hints{};
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_DGRAM;
    addrinfo *resolved = nullptr;
    const std::string port = std::to_string(config.port);
    if (getaddrinfo(config.host.c_str(), port.c_str(), &hints, &resolved) != 0 || !resolved) {
      close_socket();
      return false;
    }
    std::memcpy(&destination_, resolved->ai_addr, sizeof(destination_));
    freeaddrinfo(resolved);

    socket_ = ::socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    if (socket_ == INVALID_SOCKET) {
      close_socket();
      return false;
    }
    // A 640 x 640 frame is ~100 datagrams; the default 64 KiB buffer drops
    // chunks of nearly every frame at high FPS.
    const int send_buffer = kSendBufferBytes;
    setsockopt(socket_, SOL_SOCKET, SO_SNDBUF, reinterpret_cast<const char *>(&send_buffer),
               sizeof(send_buffer));
    u_long nonblocking = 1;
    if (ioctlsocket(socket_, FIONBIO, &nonblocking) != 0) {
      close_socket();
      return false;
    }
    stopping_ = false;
    worker_ = std::thread([this] { run(); });
    control_worker_ = std::thread([this] { control_loop(); });
    return true;
  }

  void stop() {
    {
      std::lock_guard<std::mutex> lock(mutex_);
      stopping_ = true;
      pending_.reset();
    }
    condition_.notify_one();
    if (worker_.joinable()) worker_.join();
    if (control_worker_.joinable()) control_worker_.join();
    close_socket();
  }

  uint32_t crop_size() const { return crop_size_.load(); }
  uint32_t max_fps() const { return config_.max_fps; }

  void submit_mapped(const uint8_t *data, uint32_t width, uint32_t height, uint32_t stride,
                     uint64_t timestamp) {
    if (!data || width == 0 || height == 0 || stride < width * 4u) return;
    Frame next;
    {
      // Reuse the previous frame's allocation when the worker has handed it back.
      std::lock_guard<std::mutex> lock(mutex_);
      next.bgra.swap(spare_);
    }
    next.width = width;
    next.height = height;
    next.timestamp = timestamp;
    const size_t row = static_cast<size_t>(width) * 4u;
    next.bgra.resize(row * height);
    for (uint32_t y = 0; y < height; ++y)
      std::memcpy(next.bgra.data() + row * y, data + static_cast<size_t>(stride) * y, row);
    {
      std::lock_guard<std::mutex> lock(mutex_);
      if (stopping_) return;
      pending_ = std::move(next); // latest-frame semantics
    }
    condition_.notify_one();
  }

 private:
  void control_loop() {
    std::array<char, 128> message{};
    for (;;) {
      {
        std::lock_guard<std::mutex> lock(mutex_);
        if (stopping_) return;
      }
      fd_set readable;
      FD_ZERO(&readable);
      FD_SET(socket_, &readable);
      timeval timeout{0, 100000};
      if (::select(0, &readable, nullptr, nullptr, &timeout) <= 0) continue;
      sockaddr_in from{};
      int from_size = sizeof(from);
      const int received = ::recvfrom(socket_, message.data(), static_cast<int>(message.size() - 1),
                                      0, reinterpret_cast<sockaddr *>(&from), &from_size);
      if (received <= 0) continue;
      // Only the configured receiver may change the crop.
      if (from.sin_addr.s_addr != destination_.sin_addr.s_addr || from.sin_port != destination_.sin_port)
        continue;
      message[static_cast<size_t>(received)] = '\0';
      if (std::strncmp(message.data(), kControlPrefix, sizeof(kControlPrefix) - 1) != 0) continue;
      const char *digits = message.data() + sizeof(kControlPrefix) - 1;
      char *end = nullptr;
      const auto value = std::strtoul(digits, &end, 10);
      if (end != digits && value >= 1 && value <= 8192) crop_size_.store(static_cast<uint32_t>(value));
    }
  }

  void run() {
    for (;;) {
      Frame frame;
      {
        std::unique_lock<std::mutex> lock(mutex_);
        condition_.wait(lock, [this] { return stopping_ || pending_.has_value(); });
        if (stopping_) return;
        frame = std::move(*pending_);
        pending_.reset();
      }
      try {
        send_frame(frame);
      } catch (...) {
        // A bad allocation or encoder failure must never terminate OBS's worker.
      }
      std::lock_guard<std::mutex> lock(mutex_);
      if (spare_.empty()) spare_.swap(frame.bgra);
    }
  }

  struct JpegError {
    jpeg_error_mgr manager;
    jmp_buf jump;
  };

  // Returns a malloc'd JPEG buffer (caller frees) or nullptr.
  unsigned char *encode_jpeg(const Frame &frame, unsigned long &size) {
    if (frame.width > 65535 || frame.height > 65535 ||
        static_cast<uint64_t>(frame.width) * frame.height > 33554432ull)
      return nullptr;
#ifndef JCS_EXTENSIONS
    rgb_.resize(static_cast<size_t>(frame.width) * 3u);
#endif
    jpeg_compress_struct compressor{};
    JpegError error{};
    compressor.err = jpeg_std_error(&error.manager);
    // libjpeg's default handler calls exit(); jump back here instead.
    error.manager.error_exit = [](j_common_ptr info) {
      longjmp(reinterpret_cast<JpegError *>(info->err)->jump, 1);
    };
    unsigned char *encoded = nullptr;
    size = 0;
    if (setjmp(error.jump)) {
      jpeg_destroy_compress(&compressor);
      free(encoded);
      return nullptr;
    }
    jpeg_create_compress(&compressor);
    jpeg_mem_dest(&compressor, &encoded, &size);
    compressor.image_width = frame.width;
    compressor.image_height = frame.height;
#ifdef JCS_EXTENSIONS
    compressor.input_components = 4;
    compressor.in_color_space = JCS_EXT_BGRA;
#else
    compressor.input_components = 3;
    compressor.in_color_space = JCS_RGB;
#endif
    jpeg_set_defaults(&compressor);
    jpeg_set_quality(&compressor, config_.jpeg_quality, TRUE);
    compressor.dct_method = JDCT_IFAST;
    if (config_.chroma_444) {
      compressor.comp_info[0].h_samp_factor = 1;
      compressor.comp_info[0].v_samp_factor = 1;
    }
    jpeg_start_compress(&compressor, TRUE);
    const size_t row = static_cast<size_t>(frame.width) * 4u;
    while (compressor.next_scanline < compressor.image_height) {
      const uint8_t *src = frame.bgra.data() + row * compressor.next_scanline;
#ifdef JCS_EXTENSIONS
      JSAMPROW line = const_cast<JSAMPROW>(src);
#else
      for (uint32_t x = 0; x < frame.width; ++x) {
        rgb_[x * 3u] = src[x * 4u + 2u];
        rgb_[x * 3u + 1u] = src[x * 4u + 1u];
        rgb_[x * 3u + 2u] = src[x * 4u];
      }
      JSAMPROW line = rgb_.data();
#endif
      jpeg_write_scanlines(&compressor, &line, 1);
    }
    jpeg_finish_compress(&compressor);
    jpeg_destroy_compress(&compressor);
    if (size == 0 || size > kMaxJpegBytes) {
      free(encoded);
      return nullptr;
    }
    return encoded;
  }

  void send_frame(const Frame &frame) {
    unsigned long size = 0;
    unsigned char *jpeg = encode_jpeg(frame, size);
    if (!jpeg) return;
    const uint64_t chunks = (size + kPayloadBytes - 1u) / kPayloadBytes;
    if (chunks == 0 || chunks > 65535) {
      free(jpeg);
      return;
    }
    const uint64_t frame_id = next_frame_id_++;
    std::array<uint8_t, kHeaderBytes + kPayloadBytes> packet{};
    std::memcpy(packet.data(), "RCVR", 4);
    packet[4] = 1; // version
    packet[5] = 0; // flags
    put_u64(packet.data() + 6, frame_id);
    put_u16(packet.data() + 16, static_cast<uint16_t>(chunks));
    put_u16(packet.data() + 18, static_cast<uint16_t>(frame.width));
    put_u16(packet.data() + 20, static_cast<uint16_t>(frame.height));
    put_u64(packet.data() + 22, frame.timestamp);
    for (uint64_t index = 0; index < chunks; ++index) {
      const size_t offset = static_cast<size_t>(index) * kPayloadBytes;
      const size_t bytes = std::min<size_t>(kPayloadBytes, size - offset);
      put_u16(packet.data() + 14, static_cast<uint16_t>(index));
      std::memcpy(packet.data() + kHeaderBytes, jpeg + offset, bytes);
      ::sendto(socket_, reinterpret_cast<const char *>(packet.data()),
               static_cast<int>(kHeaderBytes + bytes), 0,
               reinterpret_cast<const sockaddr *>(&destination_), sizeof(destination_));
    }
    free(jpeg);
  }

  static void put_u16(uint8_t *out, uint16_t value) {
    out[0] = static_cast<uint8_t>(value >> 8);
    out[1] = static_cast<uint8_t>(value);
  }
  static void put_u64(uint8_t *out, uint64_t value) {
    for (int i = 7; i >= 0; --i) out[7 - i] = static_cast<uint8_t>(value >> (i * 8));
  }

  void close_socket() {
    if (socket_ != INVALID_SOCKET) {
      closesocket(socket_);
      socket_ = INVALID_SOCKET;
    }
    if (winsock_ready_) {
      WSACleanup();
      winsock_ready_ = false;
    }
  }

  Config config_;
  std::mutex mutex_;
  std::condition_variable condition_;
  std::optional<Frame> pending_;
  std::vector<uint8_t> spare_;
  std::vector<uint8_t> rgb_; // only used without libjpeg-turbo extensions
  bool stopping_ = false;
  bool winsock_ready_ = false;
  std::thread worker_;
  std::thread control_worker_;
  std::atomic<uint32_t> crop_size_{640};
  uint64_t next_frame_id_ = 1;
  SOCKET socket_ = INVALID_SOCKET;
  sockaddr_in destination_{};
};

Sender sender;

// GPU path: copy the centered crop of the main texture, stage it through a
// three-surface ring so the render thread never waits on the readback it just
// queued, and hand only the crop to the sender's worker thread.
class GpuCapture {
 public:
  void render() {
    gs_texture_t *main_texture = obs_api.get_main_texture();
    if (!main_texture) return;
    // HDR canvases render to RGBA16F; copy_texture_region needs matching
    // formats, so they are silently not sent.
    if (obs_api.texture_format(main_texture) != GS_BGRA) return;
    const uint64_t now = monotonic_ns();
    if (const uint32_t fps = sender.max_fps(); fps && now - last_capture_ns_ < 1000000000ull / fps)
      return;

    const uint32_t source_width = obs_api.texture_width(main_texture);
    const uint32_t source_height = obs_api.texture_height(main_texture);
    const uint32_t crop_size = std::min({sender.crop_size(), source_width, source_height});
    if (crop_size == 0) return;
    ensure_resources(crop_size);
    if (!crop_texture_) return;

    const uint32_t left = (source_width - crop_size) / 2u;
    const uint32_t top = (source_height - crop_size) / 2u;
    obs_api.copy_texture_region(crop_texture_, 0, 0, main_texture, left, top, crop_size, crop_size);
    obs_api.stage_texture(stages_[write_index_], crop_texture_);
    written_[write_index_] = true;
    captured_ns_[write_index_] = now;
    last_capture_ns_ = now;

    const size_t read_index = (write_index_ + 1) % kStageCount;
    if (written_[read_index]) {
      uint8_t *mapped = nullptr;
      uint32_t linesize = 0;
      if (obs_api.stage_map(stages_[read_index], &mapped, &linesize)) {
        try {
          sender.submit_mapped(mapped, crop_size, crop_size, linesize, captured_ns_[read_index]);
        } catch (...) {
          // A dropped frame must never propagate through OBS's render callback.
        }
        obs_api.stage_unmap(stages_[read_index]);
      }
      written_[read_index] = false;
    }
    write_index_ = read_index;
  }

  void destroy() {
    for (auto *&stage : stages_) {
      if (stage) obs_api.stage_destroy(stage);
      stage = nullptr;
    }
    if (crop_texture_) obs_api.texture_destroy(crop_texture_);
    crop_texture_ = nullptr;
    written_.fill(false);
    crop_size_ = 0;
    write_index_ = 0;
  }

 private:
  static constexpr size_t kStageCount = 3;

  void ensure_resources(uint32_t crop_size) {
    if (crop_size_ == crop_size && crop_texture_) return;
    destroy();
    crop_texture_ = obs_api.texture_create(crop_size, crop_size, GS_BGRA, 1, nullptr, GS_RENDER_TARGET);
    if (!crop_texture_) return;
    for (auto *&stage : stages_) stage = obs_api.stage_create(crop_size, crop_size, GS_BGRA);
    if (!stages_[0] || !stages_[1] || !stages_[2]) {
      destroy();
      return;
    }
    crop_size_ = crop_size;
  }

  gs_texture_t *crop_texture_ = nullptr;
  std::array<gs_stagesurf_t *, kStageCount> stages_{};
  std::array<bool, kStageCount> written_{};
  std::array<uint64_t, kStageCount> captured_ns_{};
  uint32_t crop_size_ = 0;
  size_t write_index_ = 0;
  uint64_t last_capture_ns_ = 0;
};

GpuCapture gpu_capture;

void main_rendered_callback(void *) {
  try {
    gpu_capture.render();
  } catch (...) {
    // Never let an allocation/graphics failure escape into OBS.
  }
}

bool loaded = false;

} // namespace

bool obs_module_load(void) {
  if (!obs_api.load()) return false;
  const Config config = load_config();
  if (!sender.start(config)) return false;
  obs_api.add_rendered(main_rendered_callback, nullptr);
  loaded = true;
  return true;
}

void obs_module_unload(void) {
  if (!loaded) return;
  obs_api.remove_rendered(main_rendered_callback, nullptr);
  obs_api.enter_graphics();
  gpu_capture.destroy();
  obs_api.leave_graphics();
  sender.stop();
  loaded = false;
}

#ifdef SYSX_SENDER_SELFTEST
// Headless check of the encode/packetize/control path without OBS:
//   sysx-sender-selftest.exe <port>
// against `receiver-backend udp-listen --host 127.0.0.1 --port <port> --count 3`.
int main(int argc, char **argv) {
  Config config;
  config.port = static_cast<uint16_t>(argc > 1 ? std::atoi(argv[1]) : 4299);
  config.default_crop = 320; // the receiver must replace this with its 640
  if (!sender.start(config)) return 1;
  struct Size { uint32_t width, height, padding; };
  for (const Size size : {Size{320, 320, 0}, Size{200, 120, 64}, Size{1, 1, 0}}) {
    const uint32_t stride = size.width * 4u + size.padding;
    std::vector<uint8_t> image(static_cast<size_t>(stride) * size.height);
    for (uint32_t y = 0; y < size.height; ++y)
      for (uint32_t x = 0; x < size.width; ++x) {
        uint8_t *pixel = image.data() + static_cast<size_t>(stride) * y + x * 4u;
        pixel[0] = static_cast<uint8_t>(x); // B
        pixel[1] = static_cast<uint8_t>(y); // G
        pixel[2] = 255;                     // R
        pixel[3] = 255;
      }
    sender.submit_mapped(image.data(), size.width, size.height, stride, monotonic_ns());
    std::this_thread::sleep_for(std::chrono::milliseconds(150));
  }
  for (int i = 0; i < 20 && sender.crop_size() != 640; ++i)
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
  const uint32_t crop = sender.crop_size();
  // Optional: `<port> <seconds>` keeps streaming a 640x640 dark frame with a
  // moving red square (a Pixel Seek "red" target) at ~60 FPS for UI checks.
  const int seconds = argc > 2 ? std::atoi(argv[2]) : 0;
  const uint64_t end = monotonic_ns() + static_cast<uint64_t>(seconds) * 1000000000ull;
  std::vector<uint8_t> frame(640u * 640u * 4u);
  for (uint32_t tick = 0; monotonic_ns() < end; ++tick) {
    const uint32_t left = 360 + (tick % 120) / 2, top = 230;
    for (uint32_t y = 0; y < 640; ++y)
      for (uint32_t x = 0; x < 640; ++x) {
        uint8_t *pixel = frame.data() + (static_cast<size_t>(y) * 640u + x) * 4u;
        const bool target = x >= left && x < left + 40 && y >= top && y < top + 40;
        pixel[0] = target ? 0 : static_cast<uint8_t>(18 + y / 16);
        pixel[1] = target ? 0 : static_cast<uint8_t>(22 + x / 20);
        pixel[2] = target ? 255 : 16;
        pixel[3] = 255;
      }
    sender.submit_mapped(frame.data(), 640, 640, 640u * 4u, monotonic_ns());
    std::this_thread::sleep_for(std::chrono::milliseconds(16));
  }
  sender.stop();
  std::printf("{\"event\":\"selftest\",\"receiver_crop\":%u}\n", crop);
  return crop == 640 ? 0 : 2;
}
#endif
