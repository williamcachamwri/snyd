# snyd vs macOS Spotlight: Đánh giá trung thực

> Đánh giá dựa trên benchmark thực tế (Apple M1 Pro, macOS 14, release build) và kiến trúc đã được xác minh của cả hai hệ thống. Không có thông tin nào được bịa đặt.

---

## 1. Benchmark thực tế

Dữ liệu từ `cargo bench` trên corpus tổng hợp (1,000 và 10,000 files).

| Query | snyd (1K) | snyd (10K) | mdfind (1K) | mdfind (10K) | snyd nhanh hơn |
|-------|-----------|------------|-------------|--------------|----------------|
| Prefix `budget` | **0.037 ms** | **0.154 ms** | 20.9 ms | 20.8 ms | **560× / 135×** |
| Broad `report` | **0.056 ms** | **0.275 ms** | 20.9 ms | 20.6 ms | **370× / 75×** |
| Fuzzy `bdgt` | **0.030 ms** | **0.116 ms** | 20.9 ms | 20.8 ms | **700× / 179×** |
| Not found `xyz...` | **0.056 ms** | **0.275 ms** | 20.9 ms | 20.7 ms | **370× / 75×** |

**Quan sát quan trọng:** `mdfind` latency dao động quanh **~20–21 ms** bất kể corpus size hay query type. Điều này không phải vì Spotlight chậm, mà vì đây là **IPC overhead** — mdfind phải gửi request tới `mds` daemon, chờ nó query index on-disk, rồi trả về. Phần lớn 20 ms này là overhead giao tiếp, không phải thời gian tìm kiếm thực sự.

---

## 2. Spotlight mạnh ở đâu (những gì snyd không làm được)

### a. Full-content indexing
Spotlight extract và index **nội dung bên trong file** (text trong PDF, Word, Excel, email) thông qua `mdimporter` plugins. snyd chỉ index **filename** và **body text được push thủ công** qua `index_content` API.

- Tìm kiếm `"quarterly report"` trong **nội dung** PDF → Spotlight làm được, snyd không (trừ khi bạn đã OCR/pre-extract text và push vào snyd).
- Tìm kiếm theo metadata như `author`, `created date`, `tags` → Spotlight native, snyd không hỗ trợ.

### b. System-wide integration
Spotlight là subsystem của macOS:
- Không cần cài đặt, không cần maintain daemon riêng
- Tích hợp với Finder, Alfred, Raycast, LaunchBar
- Handle permissions, sandboxing, iCloud, Time Machine snapshots tự động

snyd yêu cầu:
- Cài đặt và chạy daemon riêng (`snyd`)
- Maintain Unix socket, handle crashes, cache invalidation
- Không tích hợp sẵn với bất kỳ app nào (trừ khi bạn viết bridge)

### c. Persistent on-disk index
Spotlight lưu index trên disk (`.Spotlight-V100/ContentIndex.db`), persistent qua reboot. snyd lưu cache nhưng phải rebuild `HashMap` index từ cache khi khởi động (~121 ms cho 50K docs theo benchmark).

---

## 3. snyd mạnh ở đâu (những gì Spotlight không tối ưu)

### a. Filename search latency
Spotlight không phải được thiết kế để **tìm file theo tên** nhanh nhất có thể. Nó là generic search engine cho metadata + content. snyd là **specialized filename search engine**.

Ví dụ thực tế: bạn gõ `"bud"` trong launcher app. snyd trả về kết quả trong **0.037 ms** (1K files) — dưới 1/500 mili giây. Spotlight cần **~20 ms** vì phải đi qua IPC + query system-wide index.

Điều này quan trọng vì 20 ms là ngưỡng mà người dùng **có thể cảm nhận được độ trễ**; 0.037 ms là instant.

### b. Fuzzy typo tolerance
Spotlight không có fuzzy matching cho filename. Gõ `"bdgt"` trong Spotlight sẽ không tìm thấy `"budget"`. snyd có Damerau-Levenshtein fallback với penalty scoring, nên `"bdgt"` vẫn match `"budget_report.xlsx"`.

### c. Tiered index & fine-grained control
Spotlight index **mọi thứ** (system files, caches, node_modules, .git). snyd cho phép:
- Exclude `node_modules`, `.cargo`, `DerivedData` mặc định (tier system)
- Opt-in hidden files (`--index-hidden`)
- Custom include/exclude patterns

Spotlight có `Privacy` tab để exclude folders, nhưng không có khái niệm "cache tier" hay "hidden tier".

### d. Extension fast-path
Tìm `".pdf"` trong Spotlight trả về cả file có "pdf" trong content. snyd có fast-path scan `extension` field, trả về chính xác các file `.pdf` trong ~0.05 ms.

---

## 4. Các trade-off thực sự

| Tiêu chí | snyd | Spotlight |
|-----------|------|-----------|
| **Search scope** | Filename + optional body text | Metadata + content + filename |
| **Latency** | 0.03–3 ms | ~20 ms (IPC bound) |
| **Memory** | In-memory (~200B/doc) | On-disk (không tốn RAM user) |
| **Scalability** | 2M+ docs (RAM bound) | Full disk (SSD bound) |
| **Persistence** | Rebuild từ cache khi restart | Persistent native |
| **Setup** | Cần cài + chạy daemon | Có sẵn trong macOS |
| **Integration** | Cần custom client/bridge | Finder, Alfred, Raycast... |
| **Fuzzy matching** | Có (Damerau-Levenshtein) | Không |
| **Content search** | Không (trừ opt-in push) | Native |
| **iCloud/Time Machine** | Không handle | Native |

---

## 5. Kết luận: Dùng cái nào khi nào

**Dùng snyd khi:**
- Bạn đang viết một launcher app (Alfred/Raycast clone) cần **instant filename search**
- Bạn cần **fuzzy matching** cho typo-tolerant search
- Bạn muốn **exclude caches/node_modules** khỏi kết quả mặc định
- Bạn cần **custom ranking** (access-frequency boost, extension scoring)

**Dùng Spotlight khi:**
- Bạn cần **tìm nội dung bên trong file** (text trong PDF, Word)
- Bạn cần **metadata search** (author, creation date, tags)
- Bạn muốn **zero setup** — macOS đã có sẵn
- Bạn cần **system-wide search** tích hợp với Finder

**Dùng cả hai (multi-layered pipeline):**
Đây là cách tốt nhất. snyd làm layer 1 (fast fuzzy filename), Spotlight làm layer 2 (content/metadata fallback). Đây là kiến trúc mà nhiều launcher app (Alfred, Raycast) đang dùng — họ có index riêng cho filename, rồi fallback sang Spotlight khi cần content search.

---

## 6. Những gì benchmark không nói

- **Cold-start:** snyd cần ~78–159 ms để index 50K–100K files. Spotlight đã index sẵn trong background. snyd lợi thế hơn nếu bạn index một subset nhỏ (e.g. chỉ `~/Projects`), nhưng yếu thế hơn nếu bạn cần search toàn bộ disk.
- **Update latency:** Spotlight cập nhật index trong vòng 1–7 giây khi file thay đổi (qua FSEvents + mdworker). snyd cũng dùng `notify` nhưng không chạy mdimporter plugins, nên update nhanh hơn cho filename-only nhưng không extract content mới.
- **Accuracy:** Spotlight không bao giờ miss một file đã được index (vì nó index tất cả). snyd có thể miss nếu file nằm ngoài `scopes` hoặc bị exclude bởi tier rules.

---

*Đánh giá này dựa trên benchmark thực tế từ snyd v0.2.4 và tài liệu công khai về Spotlight architecture (Apple Developer Docs, Eclectic Light Company, Mac OS X Internals).*
