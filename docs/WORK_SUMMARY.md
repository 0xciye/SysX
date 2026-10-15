# Tổng hợp công việc — 2026-10-15

## 1. Skill

- Đã cài 7 design skill từ `~/.codex/skills` vào `~/.claude/skills`: `impeccable`, `ui-ux-pro-max`, `design`, `design-system`, `ui-styling`, `no-ai-design-slop`, `design-first-ui-prompting`.
- `ponytail` không có trong thư mục codex; Claude có sẵn bản `anthropic-skills:ponytail` và đã dùng nó.

## 2. Receiver UI — bỏ egui, chuyển sang web monitor

- Xoá cửa sổ `eframe/egui`. Host native (`src-tauri`) giờ chỉ làm phần bắt buộc phải native: chạy backend nhúng, đọc Medius/Ferrum, gửi chuyển động chuột.
- Receiver là trang `/monitor.html` (Leptos/WASM), host mở bằng Edge `--app` (cửa sổ không thanh địa chỉ, profile riêng tại `%LOCALAPPDATA%\SysX\monitor-window`, cỡ mặc định 550 × 620). Không có Edge thì mở bằng trình duyệt mặc định.
- Đóng cửa sổ monitor → SysX tự tắt sau 3 giây (đã kiểm tra trên máy: backend tắt, UDP 4200 được giải phóng). Thêm retry khi xoá thư mục runtime vì Windows có lúc để lại thư mục rỗng.
- Trạng thái thiết bị và 5 nút chuột: host → backend (`device_status`, có kiểm tra hợp lệ) → `state.device` → trang monitor.
- Backend không gửi frame preview cũ hơn 1 giây, nên mở lại trang khi sender đã dừng sẽ không hiện ảnh cũ và báo nhầm "Receiving frames".
- Thiết kế (theo Impeccable / UI UX Pro Max / no-AI-slop):
  - Header mỏng: logo SysX bên trái, chỉ trạng thái thiết bị bên phải (đã bỏ "Waiting for sender" và "UDP 4200" theo yêu cầu).
  - Preview vuông tối đa 640px, HUD `độ phân giải · FPS · ms` nằm trong khung.
  - Khi chưa có frame: `NO FRAME` + dòng trạng thái; chỉ hiện dòng chi tiết khi có lỗi (đã bỏ "Listening on UDP 4200").
  - Hàng 5 nút chuột rộng bằng preview; giữ = chấm đặc xanh, nhả = vòng tròn, chưa biết = gạch ngang.
  - Monospace chỉ dùng cho số đo; không có animation trang trí.
- Ảnh kiểm tra (chụp bằng Edge headless, không mở cửa sổ): `design-review/receiver-monitor-web-{live,waiting,narrow}.png`. Hai thay đổi cuối (bỏ trạng thái/port ở header và dòng "Listening") đã build nhưng chưa chụp lại.

## 3. OBS plugin (`obs-plugin/sysx-canvas-sender`)

- Cấu hình nằm trong code (theo yêu cầu): sửa các hằng số đầu file `sysx_canvas_sender.cpp` rồi build lại.
  - `kReceiverHost = "127.0.0.1"`, `kReceiverPort = 4200`, `kJpegQuality = 80`, `kMaxFps = 0`, `kChroma444 = true`, `kDefaultCrop = 640`.
- Không ghi log vào OBS (theo yêu cầu).
- DLL không cần gì thêm: libjpeg-turbo và VC++ runtime link tĩnh; DLL chỉ phụ thuộc `KERNEL32.dll` và `WS2_32.dll` của Windows, các hàm OBS lấy từ `obs.dll` đang chạy.
- Crop trên GPU theo lệnh `SYSCFG 1 CROP <n>` từ receiver (chỉ nhận từ đúng receiver đã cấu hình), staging 3 surface để không chặn luồng render của OBS.
- Encode JPEG từ BGRA trực tiếp, chroma 4:4:4 để màu Pixel Seek không bị nhoè; buffer gửi UDP 8 MiB.
- Canvas HDR (không phải BGRA 8-bit) được bỏ qua, không crash.
- DLL đã build: `obs-plugin/sysx-canvas-sender/sysx-canvas-sender.dll`; đã cài vào `C:\Program Files\obs-studio\obs-plugins\64bit\`, DLL cũ đổi tên thành `sysx-canvas-sender-gpu-localhost.dll.bak`.

### Kết quả kiểm tra plugin

| Kiểm tra | Kết quả |
|---|---|
| `sysx-sender-selftest` → `udp-listen` | 3/3 frame (320×320 nhiều chunk, 200×120 có padding, 1×1), 0 lỗi/mất, nhận `SYSCFG` crop 640 |
| `sysx-obs-host-check`: plugin chạy trong `obs.dll` OBS 32.2.2 thật, D3D11, không cửa sổ, 60 FPS | 300/300 frame 640×640, unload sạch |
| Canvas HDR (Rec.2100 PQ) | không gửi frame, không lỗi |
| Render 240 FPS / 360 FPS | nhận 234,2 / 356,8 FPS (loopback, 4:4:4, quality 80) |
| Bản không log | 239 frame / 4 giây ở 60 FPS, không có dòng log nào của plugin |

## 4. Build và test

- `scripts/build-windows.ps1` build toàn bộ không cần Node: Trunk UI → backend → test 3 crate → `SysX.exe` → smoke UDP. `-AppSmoke` (mở cửa sổ) là tuỳ chọn.
- Sửa smoke script cho Windows PowerShell 5.1 (`ExitCode` rỗng nếu không giữ handle).
- Test: backend 37, UI 5, host 8 — đều pass. Smoke UDP 4200 pass.

## 5. Chưa làm

- Chạy plugin trong giao diện OBS Studio thật cùng SysX (bị dừng vì bạn đang chơi game).
- Chạy `scripts/build-windows.ps1 -AppSmoke`.
- Gửi qua LAN giữa hai máy (chỉ có một máy).
- Chụp lại monitor sau khi bỏ trạng thái/port ở header.
