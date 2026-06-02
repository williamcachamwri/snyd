# snyd vs Alfred vs Raycast: Đánh giá trung thực

> **Lưu ý quan trọng:** snyd là **search engine/backend** (Rust daemon). Alfred và Raycast là **end-user products** (launcher app đầy đủ UI, workflow, ecosystem). So sánh trực tiếp giống như so sánh động cơ ô tô với chiếc xe hoàn chỉnh. Bài này so sánh **khía cạnh search engine** và phân tích nếu xây app trên snyd thì sẽ như thế nào.

---

## 1. Benchmark search latency (khía cạnh duy nhất so sánh trực tiếp được)

| Engine/App | Search Latency | Ghi chú |
|------------|---------------|---------|
| **snyd** (trigram index, in-memory) | **0.03–3 ms** | Đo từ lúc gửi JSON request đến nhận batch results. Không có UI rendering. |
| **Alfred 5.7** | **~34 ms** | Theo survey trên M2. Bao gồm cả search + UI rendering. |
| **Raycast 1.104** | **~18 ms** | Theo survey trên M2. Bao gồm cả search + UI rendering. |
| **Raycast 2.0 beta** (Rust indexer) | **sub-10 ms** | Beta tester report. Chưa stable. Viết lại indexer bằng Rust. |
| **Spotlight** | **~120 ms** | Cold search. Progressive loading.

**Phân tích:**
- snyd nhanh hơn Alfred/Raycast **10–1,000×** ở tầng engine. Nhưng con số này không công bằng vì Alfred/Raycast còn phải render UI, icons, actions.
- Raycast 2.0 đang viết lại indexer bằng Rust (giống snyd) — điều này cho thấy họ cũng nhận ra native indexer bằng Rust là hướng đi đúng cho performance.
- Trong thực tế, **người dùng không cảm nhận được** sự khác biệt giữa 18ms (Raycast) và 34ms (Alfred). Cả hai đều "instant" với mắt thường.

---

## 2. Alfred/Raycast làm gì mà snyd không làm (và không thể làm)

### UI/UX đầy đủ
- **Launcher bar:** Command palette, search bar, rich results với icons, previews
- **Actions:** Right-arrow để rename/move/copy/open in Terminal (Alfred), inline actions (Raycast)
- **Rendering:** Electron (Raycast) / native Cocoa (Alfred) — cả hai đều tốn thời gian render

snyd chỉ là một Rust daemon trả về JSON qua Unix socket. Không có UI, không có icons, không có actions.

### Ecosystem
- **Alfred Workflows:** Hàng trăm workflow do cộng đồng viết, tích hợp với mọi thứ (GitHub, Jira, Spotify...)
- **Raycast Extensions:** 1,300+ extensions, store có review, API chính thức
- **AI Integration:** Raycast Pro ($10/tháng) có built-in GPT-4/Claude/Perplexity

snyd không có ecosystem. Bạn phải tự viết mọi thứ.

### Tính năng ngoài search
- **Clipboard history:** Alfred (Powerpack), Raycast (built-in)
- **Window management:** Raycast (built-in), Alfred (workflow)
- **Calculator, dictionary, system commands:** Cả hai đều có
- **Cloud sync:** Raycast sync qua server, Alfred không cần cloud

snyd chỉ search file. Không có clipboard, window management, calculator.

---

## 3. snyd có thể vượt trội ở đâu nếu xây app trên nó

### a. Tiered index control
snyd có **DocTier system** — exclude `node_modules`, `.cargo`, `DerivedData` mặc định, opt-in hidden files. Alfred/Raycast dùng Spotlight index hoặc scan tất cả, không có khái niệm "cache tier".

Nếu bạn là developer, Alfred tìm kiếm sẽ flood kết quả với `node_modules` files. snyd sẽ loại bỏ chúng mặc định.

### b. Fuzzy matching thực sự
- **snyd:** Damerau-Levenshtein với penalty scoring. `"bdgt"` match `"budget"`.
- **Alfred:** Fuzzy search nhẹ, chủ yếu prefix + substring. Không typo-tolerant.
- **Raycast:** Tương tự Alfred, substring match + scoring đơn giản.

### c. Extension fast-path
- **snyd:** `".pdf"` scan `extension` field trực tiếp (~0.05ms).
- **Alfred/Raycast:** Tìm `"pdf"` trong tên file hoặc Spotlight metadata, không có fast-path riêng cho extension.

### d. Custom ranking
- **snyd:** access-frequency boost, extension scoring, depth penalty, acronym match — bạn kiểm soát thuật toán.
- **Alfred/Raycast:** Black-box scoring, không expose ranking algorithm.

### e. No subscription / fully open-source
- **snyd:** MIT license, miễn phí, self-hosted, zero telemetry.
- **Alfred:** Powerpack $43 một lần (không phải subscription).
- **Raycast:** Free tier giới hạn, Pro $10/tháng.

---

## 4. Bảng so sánh toàn diện

| Tiêu chí | snyd | Alfred 5.7 | Raycast 1.104 |
|----------|------|------------|---------------|
| **Loại** | Search engine (daemon) | End-user launcher app | End-user launcher app |
| **Search latency (engine)** | **0.03–3 ms** | ~34 ms (bao gồm UI) | ~18 ms (bao gồm UI) |
| **UI** | Không có | Native Cocoa | Electron + native layer |
| **Fuzzy typo-tolerant** | **Có** (D-L distance) | Hạn chế | Hạn chế |
| **Extension ecosystem** | Không | 1000+ workflows | 1300+ extensions |
| **AI built-in** | Không | Không | Có (Pro) |
| **Clipboard history** | Không | Có (Powerpack) | Có (built-in) |
| **Window management** | Không | Workflow | Built-in |
| **Tiered index** | **Có** (Normal/Hidden/Cache) | Không | Không |
| **Custom ranking** | **Có** (code-level) | Không | Không |
| **Open source** | **Có** (MIT) | Không | Không |
| **Giá** | **Miễn phí** | $43 một lần | Free / $10/tháng |
| **Platform** | macOS/Linux | macOS only | macOS (+ Windows beta) |
| **Setup** | Cần cài + chạy daemon | Cài app là xong | Cài app là xong |

---

## 5. Kết luận: snyd "hơn" ở đâu, thua ở đâu

### snyd HƠN khi:
- Bạn đang **xây một launcher app mới** và cần engine nhanh, fuzzy, có thể customize ranking
- Bạn cần **tiered index** để hide caches/node_modules khỏi kết quả
- Bạn cần **typo-tolerant search** (Damerau-Levenshtein)
- Bạn muốn **self-hosted, zero telemetry, MIT license**
- Bạn cần **extension fast-path** (`.pdf`, `.rs` scan field trực tiếp)

### snyd THUA khi:
- Bạn là **end-user** muốn mở app và dùng ngay → Alfred/Raycast hoàn chỉnh hơn
- Bạn cần **ecosystem** (workflows, extensions, AI)
- Bạn cần **UI/UX** đẹp, rich results, inline actions
- Bạn cần **clipboard, window management, calculator** — snyd không có
- Bạn không muốn **maintain daemon** (crash, socket, cache invalidation)

### Câu trả lời ngắn gọn:
**snyd không "hơn" Alfred hay Raycast vì chúng không cùng tầng.** snyd là engine, họ là xe hoàn chỉnh.

Nhưng nếu bạn đang xây một chiếc xe mới (ví dụ: một launcher app cho developer, hoặc một file manager tùy chỉnh), thì **snyd là engine mạnh hơn Spotlight/Alfred's default indexer** ở khía cạnh:
- Speed: 0.03ms vs ~20ms IPC
- Fuzzy: D-L distance vs substring match
- Control: tiered index + custom ranking

Raycast 2.0 cũng đang nhận ra điều này — họ đang viết lại indexer bằng Rust (giống snyd) để đạt sub-10ms.

---

## 6. Nếu bạn muốn "hơn" Alfred/Raycast

Để thực sự cạnh tranh với Alfred/Raycast, bạn cần xây **Snything app** (SwiftUI/Tauri/Electron) với:

| Yếu tố | Mức độ cần thiết | Khó khăn |
|--------|-----------------|----------|
| snyd engine | Có sẵn | ✅ Done |
| UI/UX đẹp | Rất cao | Phải viết từ đầu |
| Fuzzy + fast | Có sẵn | ✅ Done |
| Extensions ecosystem | Rất cao | Cần API + store |
| Clipboard history | Cao | macOS API restrictions |
| Window management | Trung bình | Cần accessibility permissions |
| AI integration | Trung bình | OpenAI API là đủ |
| macOS integration | Rất cao | Secure input, sandboxing |

**Raycast mất ~4 năm để đạt 1,300 extensions. Alfred mất ~15 năm.** Một engine nhanh là cần thiết nhưng không đủ. Ecosystem và UI/UX mới là moat thực sự.

---

*Đánh giá dựa trên benchmark snyd v0.2.4, survey data từ DEV Community và Toolchew (300+ developers, Q1 2026), và public architecture information về Alfred/Raycast/Spotlight.*
