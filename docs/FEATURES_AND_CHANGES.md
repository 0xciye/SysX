# SysX — tính năng và những gì đã làm

Tài liệu này mô tả cách SysX hoạt động sau đợt làm việc 2026-10-15 → 2026-10-16, theo từng phần, kèm file code và mức độ đã kiểm chứng. Lịch sử commit đầy đủ: `git log` trên `main` (từ `33208e5` trở đi).

## 1. Tổng quan kiến trúc

```
OBS Studio ──(UDP RCVR v1, JPEG)──▶ receiver-backend ──(WebSocket)──▶ monitor /monitor.html
  sysx-canvas-sender.dll   ◀──SYSCFG──   (Rust, nhúng trong SysX.exe)  ──▶ config  /config.html
                                              ▲   │ movement_target (stdout)
                                              │   ▼
                                         SysX.exe (host native) ──▶ Medius / Ferrum (chuột)
```

| Thành phần | Thư mục | Vai trò |
|---|---|---|
| OBS plugin | `obs-plugin/sysx-canvas-sender` | Crop canvas OBS trên GPU, nén JPEG, gửi UDP |
| Backend | `rust-backend` | Nhận UDP, giải mã, Pixel Seek / Object Detection, chọn target, tính chuyển động, web server loopback |
| Web UI | `rust-ui` (Leptos/WASM) | Trang monitor (receiver) và trang config |
| Host native | `src-tauri` | Chạy backend, đọc thiết bị Medius/Ferrum, gửi chuyển động chuột, mở cửa sổ monitor |

## 2. OBS plugin (`sysx_canvas_sender.cpp`)

- **Capture:** trong main-rendered callback của OBS, copy vùng vuông giữa canvas (kích thước do receiver gửi qua `SYSCFG`) bằng `gs_copy_texture_region`, đọc về qua vòng 3 staging surface để luồng render không phải chờ GPU.
- **Nén và gửi:** luồng riêng nén JPEG bằng libjpeg-turbo (đầu vào BGRA trực tiếp, chất lượng 80, chroma 4:4:4 để màu Pixel Seek không nhoè), chia gói 1.170 byte + header 30 byte (≤1.200 byte/datagram), gửi UDP. Chỉ giữ frame mới nhất; encoder/mạng chậm thì frame cũ bị bỏ.
- **Cấu hình viết cứng trong code** (dòng đầu file, sửa xong build lại):
  `kReceiverHost = "127.0.0.1"`, `kReceiverPort = 4200`, `kJpegQuality = 80`, `kMaxFps = 0` (= mọi frame OBS), `kChroma444 = true`, `kDefaultCrop = 640`. Không có file cấu hình.
- **Không log, không thông báo:** không gọi `blog`, không ghi file, không hộp thoại. Chỉ OBS tự ghi tên module đã nạp vào log của OBS.
- **Không cần cài thêm gì:** libjpeg-turbo và VC++ runtime link tĩnh; DLL chỉ import `KERNEL32.dll` và `WS2_32.dll`; các hàm libobs lấy qua `GetProcAddress` từ `obs.dll` đang chạy.
- **Nhận crop size:** luồng `control_loop` chờ trên cùng socket UDP; chỉ chấp nhận gói `SYSCFG 1 CROP <n>` (1–8192) đến từ đúng `kReceiverHost:kReceiverPort`. Giá trị mới được áp dụng ở frame render kế tiếp (tạo lại texture crop).
- **Canvas HDR** (không phải BGRA 8-bit) bị bỏ qua, không crash.
- **Kiểm chứng:**
  - `sysx-sender-selftest` (không cần OBS): 3/3 frame (320×320 nhiều chunk, 200×120 có padding, 1×1), 0 lỗi, nhận `SYSCFG` 640.
  - `sysx-obs-host-check` (nạp plugin trong `obs.dll` OBS 32.2.2 thật, D3D11, không cửa sổ): 300/300 frame ở 60 FPS; render 240/360 FPS → nhận 234/357 FPS; HDR bị bỏ qua; unload sạch.
  - OBS Studio thật trên máy này (1920×1080, 240 FPS) đã nạp DLL (log OBS), backend nhận 217–240 FPS đầu vào.
  - **Chưa kiểm chứng:** chạy qua LAN giữa hai máy.

## 3. Giao thức receiver ↔ sender

- **UDP RCVR v1** (`rust-backend/src/udp.rs`): header 30 byte big-endian (magic `RCVR`, version 1, frame_id, chunk index/total, width/height, timestamp). Kiểm tra mọi trường, JPEG phải có SOI ở chunk đầu và EOI ở chunk cuối, giới hạn bộ nhớ (64 MiB/frame, 128 MiB tổng, 32 frame dở dang, hết hạn 100 ms).
- **SYSCFG** (`rust-backend/src/receiver.rs`): receiver gửi `SYSCFG 1 CROP <Crop Resolution>`
  - khi thấy sender mới,
  - khi đổi Crop Resolution,
  - **và lặp lại mỗi 1 giây** khi sender còn gửi (bù gói bị router làm rơi trên LAN).
  Chỉ địa chỉ có gói bắt đầu bằng `RCVR` mới được coi là sender; gói rác trong mạng không giành được lệnh crop.
- **Bộ đệm nhận UDP 8 MiB** để một loạt ~100 gói của một frame không bị rơi trong lúc đang giải mã frame trước.
- Test: `crop_config_is_repeated_to_the_rcvr_sender_and_never_to_stray_hosts`, `decoded_frame_keeps_the_sender_jpeg_for_preview`.

## 4. Pixel Seek (`rust-backend/src/pixel_seek.rs`, `targeting.rs`)

- **Mask toàn frame** theo ngưỡng HSV cố định trong code: đỏ H 0–10/S 50–255/V 180–255, vàng H 20–40/S 157–255/V 252–255, tím H 140–160/S 60–255/V 240–255 (H thang 0–179). Mỗi lúc chỉ chọn một màu.
- **Xử lý màu bằng bảng tra (giảm latency):** thay vì đổi RGB→HSV bằng phép chia số thực cho từng pixel, mỗi màu có một bảng 2 MiB (1 bit cho mỗi màu trong 16,7 triệu màu RGB), tính sẵn một lần bằng chính hàm HSV cũ trong luồng nền khi backend khởi động. Mỗi pixel giờ chỉ là 1 phép tra bảng; kết quả giống hệt (test `lookup_tables_match_the_hsv_profiles_exactly`).
- **Một mục tiêu duy nhất mỗi frame:** pixel cao nhất nằm trong vòng FOV (cùng hàng thì lấy pixel gần tâm ngang nhất). Chỉ vùng màu (blob) chứa pixel đó được flood-fill và báo về, nên Preview chỉ có một box và một đường nối.
- **Liên tục leo lên pixel cao hơn:** mỗi frame tìm lại; nếu đang nhắm hàng 4 mà hàng 3, 2, 1, 0 xuất hiện thì target chuyển lên tương ứng.
- **Khoá mục tiêu (chống nhảy qua lại giữa 2 mục tiêu trong FOV):**
  - Sau mỗi frame, pipeline nhớ box của blob đang nhắm trong 100 ms (`pipeline.rs`).
  - Frame sau chỉ tìm pixel cao nhất trong box cũ nới rộng `max(12 px, 50% cạnh lớn của box)`, giao với FOV (`lock_region`, `highest_pixel_in_fov_within`).
  - Pixel cao hơn mọc ra sát mục tiêu nằm trong vùng này → vẫn leo lên. Mục tiêu khác ở chỗ khác trong FOV (kể cả cao hơn) bị bỏ qua.
  - Mục tiêu cũ biến mất / ra khỏi FOV / khoá quá 100 ms → chọn lại pixel cao nhất trong toàn FOV.
- Test: `only_the_topmost_blob_inside_fov_becomes_the_single_target`, `blobs_outside_the_fov_are_masked_but_never_targeted`, `a_locked_target_is_kept_when_another_higher_target_is_in_the_fov`, `a_locked_target_still_climbs_to_higher_pixels_next_to_it`, `a_lost_lock_falls_back_to_the_topmost_target_in_the_fov`, `pixel_seek_keeps_climbing_to_each_newly_visible_higher_pixel`.

## 5. Object Detection (`inference.rs`, `targeting.rs`)

- Một bundle model gồm ONNX, TensorRT `.engine/.plan` và OpenVINO `.xml/.bin`; Backend chọn artifact, Compute chọn thiết bị (ONNX Runtime chỉ CPU; OpenVINO CPU/GPU/NPU; TensorRT cần GPU và bản build có TensorRT).
- Giải mã output YOLOv8 thô `[1, 4+classes, anchors]` hoặc `[1, anchors, 4+classes]`, lọc theo Min confidence, NMS theo từng class. Crop phải đúng kích thước input của model (không resize).
- Chọn target: box trong FOV gần tâm nhất (box rộng >100 px bị loại), điểm neo 5% từ đỉnh box (class 0: giữa box).
- **Khoá có sẵn:** giữ mục tiêu cũ nếu box mới cùng class và chồng >50%, chỉ đổi khi box khác gần tâm hơn khoảng 3 lần.
- **Chưa kiểm chứng:** không có model thật trong repo; chưa chạy inference thật trên Windows/GPU.

## 6. Làm mượt và chuyển động chuột

- **Làm mượt X/Y** (`ExponentialTargetSmoother`): alpha 0.01–1.0 cho từng trục; 1.0 = không làm mượt. Reset khi mất target hoặc đổi cấu hình.
- **Backend** (`rust-backend/src/movement.rs`): bộ điều khiển NCAF ba vùng (snap 150 / near 50), dự đoán vận tốc, đường cong Bézier, cộng dồn phần lẻ, thỉnh thoảng micro-correction; công thức đổi đơn vị `360·2.54·sens / (54.54·DPI)` (mặc định sens 0.235, DPI 800); offset X/Y cộng vào target. Ticker riêng chạy mỗi 4 ms với target mới nhất.
- **Host** (`src-tauri/src/movement.rs`): cộng dồn delta còn tươi (≤100 ms), chỉ di chuyển khi nút trigger (Right/Middle/M4/M5) đang giữ, cộng recoil vào trục Y, kẹp ±32 mỗi lệnh; gửi qua Medius `move_rel` hoặc Ferrum `km.move`.
- **Chưa kiểm chứng:** chưa có phần cứng Medius/Ferrum để thử.

## 7. Receiver UI (`rust-ui`)

- **Không dùng egui:** trang `/monitor.html` (Leptos/WASM) mở trong cửa sổ Edge `--app` không viền, profile riêng tại `%LOCALAPPDATA%\SysX\monitor-window`, cỡ mặc định 550×620 (Edge nhớ cỡ sau khi bạn chỉnh).
- **Thiết kế** (áp dụng các skill impeccable / ui-ux-pro-max / no-ai-design-slop; spec: `design-system/sysx-native-receiver/MASTER.md`):
  - Header mỏng: logo SysX + trạng thái thiết bị (đã bỏ "Waiting for sender" và "UDP 4200" theo yêu cầu).
  - Preview vuông tối đa 640px; HUD trong khung: `Res: 640 · <FPS> FPS · <ms> ms` (ms = thời gian detect mỗi frame: crop → detect → chọn target).
  - Khi chưa có frame: `NO FRAME` + trạng thái; dòng chi tiết chỉ hiện khi có lỗi.
  - Target vẽ kiểu **"Hairline reticle"**: đường 1.25 px mờ dần từ tâm, vòng 8 px có 4 vạch, chấm nhỏ ở giữa, có viền tối để nổi trên nền sáng.
  - Mouse State: một thanh liền 30 px, 5 nút bằng nhau; giữ = nền xanh + chữ đậm + vạch dưới; chưa có thiết bị = mờ.
- **Preview đủ FPS đầu vào:** backend báo socket ngay khi có frame mới (kênh `watch`, không dùng timer vì timer Windows ~15,6 ms từng giới hạn ~60 FPS), gửi thẳng JPEG gốc không nén lại, chia sẻ frame bằng `Arc`, `state()` không còn khoá chờ luồng UDP; trang giải mã bằng `createImageBitmap` ngoài main thread. Đo với nguồn 240 FPS: trang nhận 230,3 và vẽ 229,7 frame/s (trước đó ~60). Mắt chỉ thấy tối đa bằng tần số quét màn hình.
- **Giảm lag (2026-10-16):** mỗi frame chỉ giải mã + vẽ đúng 1 lần (trước đó 3 tín hiệu cùng báo nên mỗi frame bị giải mã ~3 lần), tại một thời điểm chỉ có 1 lượt giải mã (frame mới đến thì vẽ frame mới nhất kế tiếp), canvas chỉ cấp phát lại khi đổi kích thước, state 100 ms chỉ cập nhật khi giá trị thay đổi. Đo headless: frame = decode = draw, 0 lần resize canvas/s.
- **Trang config chỉ để lưu cấu hình:** backend không gửi frame preview cho trang config (chỉ cửa sổ monitor nhận), nên Chrome không còn phải nhận 240 JPEG/s; đã bỏ ô "Actual inference device".
- **Màu FPS và latency trên HUD** (so với FPS đầu vào, nên sender 60 FPS chạy đủ vẫn xanh): FPS xử lý ≥ 95 % FPS đầu vào = xanh, ≥ 80 % = vàng, thấp hơn = đỏ; latency ≤ nửa thời gian 1 frame (1000/FPS đầu vào) = xanh, ≤ 1 frame = vàng, dài hơn 1 frame = đỏ (không theo kịp). Hàm `fps_tone`/`latency_tone` trong `rust-ui/src/layout.rs` có test.
- Ảnh kiểm tra: `design-review/receiver-monitor-web-*.png`.

## 8. Host native và vòng đời (`src-tauri`)

- **Một SysX duy nhất:** mutex `Local\SysX.Receiver.SingleInstance` (`single_instance.rs`). Mở lần hai → đưa cửa sổ monitor đang có lên trước (hoặc mở lại từ `%LOCALAPPDATA%\SysX\instance.json`) rồi thoát, không tạo backend thứ hai (trước đây gây lỗi 10048 trên UDP 4200).
- **Đóng cửa sổ monitor → SysX tắt** sau 3 giây (60 giây nếu monitor không bao giờ kết nối).
- **Backend tự tắt khi SysX chết** (kể cả End task): backend thoát khi kết nối `role=host` đóng, nên không còn backend mồ côi giữ cổng 4200.
- **Thư mục runtime cố định** `%LOCALAPPDATA%\SysX\runtime` (dọn bản cũ khi khởi động) để quyền Windows Firewall cho LAN chỉ cần cấp một lần.
- Trạng thái thiết bị/nút chuột: host → backend (`device_status`, có kiểm tra hợp lệ) → `state.device` → trang monitor.

## 9. Chạy qua router LAN

1. Máy OBS: sửa `kReceiverHost` thành IP LAN của máy SysX, build lại DLL, cài vào OBS.
2. Máy SysX: mở UDP 4200 một lần (PowerShell quyền admin):
   `netsh advfirewall firewall add rule name="SysX UDP 4200" dir=in action=allow protocol=UDP localport=4200 profile=private`
3. Dùng dây LAN gigabit: crop 640 ≈ 75–97 KiB/frame ≈ 150–190 Mbit/s ở 240 FPS. Wi-Fi/100 Mbit: giảm `kMaxFps`, đặt `kChroma444 = false` hoặc hạ `kJpegQuality`.

Chi tiết: `docs/OBS_RECEIVER_PROTOCOL.md` mục 7. **Chưa thử trên hai máy thật.**

## 10. Build và test

- `scripts/build-windows.ps1` (không cần Node): Trunk UI → backend → test cả 3 crate → `src-tauri/target/release/SysX.exe` → smoke UDP 4200. `-AppSmoke` (mở cửa sổ) là tuỳ chọn.
- Test hiện tại: backend 45, UI 6, host 10 — đều pass.
- Plugin: build bằng CMake + MSVC (xem `obs-plugin/sysx-canvas-sender/README.md`), kèm `sysx-sender-selftest` và `sysx-obs-host-check`.

## 11. Chưa làm / chưa kiểm chứng

- LAN giữa hai máy thật.
- Model Object Detection thật, TensorRT/OpenVINO trên GPU/NPU.
- Phần cứng Medius/Ferrum (chuyển động chuột và trạng thái nút).
- `scripts/build-windows.ps1 -AppSmoke` (mở cửa sổ) chưa chạy trọn vẹn.
