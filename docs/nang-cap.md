# Nâng cấp ZenForge: kế hoạch dựa trên số đo

Tài liệu này ghi lại kết quả đo thật trên bản v0.2.0 (24 test pass, clippy sạch, aislop 100/100) và các hướng nâng cấp xếp theo mức thiệt hại. Mọi con số đều đo được, không ước lượng.

Ngày đo: 17/11/2026. Máy đo: container `hermes-workspace`, binary `target/release/zenforge` sha256 `279dbfa7…`.

## 1. Hiện trạng đo được

### Tốc độ: không phải nút thắt

| Phép đo | Kết quả |
|---|---|
| `changeset create` 2.000 file | 58 ms |
| `changeset commit` 2.000 file | 85 ms |
| `status` quét 2.000 file | 47 ms |
| `changeset create` khi chỉ 1/2.000 file đổi | 64 ms |
| `blame` file 2.000 dòng, lịch sử 1 phiên bản | 22 ms |
| `blame` cùng file, lịch sử 20 phiên bản | 14 ms |

Blame không chậm đi khi lịch sử dài, vì nó chỉ đọc bảng `blame` đã tính sẵn chứ không duyệt lại toàn bộ version. Tốc độ không phải vấn đề cần ưu tiên.

### Dung lượng: đây là vấn đề lớn

Cùng một workload (file 500 dòng 23 KB, sửa đúng 1 dòng 30 lần):

| Hệ | Dung lượng |
|---|---|
| ZenForge | 907 KB |
| git | 102 KB |
| Tỷ lệ | **8,9 lần** |

Nguyên nhân: mỗi lần commit một file, ZenForge ghi **toàn bộ nội dung file** vào bảng `objects` dưới dạng TEXT. git nén theo delta và pack lại. Với agent sửa file lớn hàng nghìn lần mỗi ngày, đây là chi phí không thể bỏ qua.

Đo riêng phần tăng: 30 lần sửa 1 dòng trong file 23 KB làm DB tăng 732 KB, tức 24 KB mỗi lần sửa 1 dòng. Đúng bằng kích thước file.

## 2. Chín lỗi tìm được, xếp theo thiệt hại

### P0: mất dữ liệu, phải sửa trước mọi thứ khác

**Lỗi 1: merge không cập nhật working copy và không phát hiện xung đột**

Kịch bản đo: nhánh `dev` sửa `shared.txt` thành "DEV VERSION", nhánh `main` sửa cùng file thành "MAIN VERSION". Chạy `branch merge dev --target main`:

```
merged dev into main: 1 changeset(s) applied
```

Báo thành công. Nhưng:
- File trên đĩa vẫn là "MAIN VERSION", không phải "DEV VERSION".
- HEAD của `main` đã trỏ sang changeset của dev.
- `status` sau đó báo `M shared.txt` (file bị coi là đã sửa so với head).

Kết quả: head và working copy lệch nhau, người dùng không được cảnh báo. Nếu agent tin theo head mà commit tiếp, bản "DEV VERSION" sẽ đè lên hoặc biến mất khỏi lịch sử. Đây là mất dữ liệu im lặng, đúng loại lỗi nguy hiểm nhất trong một hệ source control.

Hai lỗi con trong cùng một chỗ:
- `merge_branch` chỉ đổi `parent` và `head`, không ghi file ra đĩa.
- Không có kiểm tra xung đột: hai nhánh cùng sửa một file thì fast-forward vẫn được coi là hợp lệ.

**Lỗi 2: blame gán sai chủ khi nội dung dòng trùng nhau**

Kịch bản: agentA viết file có 2 dòng `return None`. agentB thêm dòng thứ ba cũng `return None`. Blame:

```
cs77dfe4bb52d7 agentA      1 | return None
cs77dfe4bb52d7 agentA      2 | other
cs77dfe4bb52d7 agentA      3 | return None
cs77dfe4bb52d7 agentA      4 | return None   <- dòng này agentB viết
```

Dòng 4 do agentB tạo nhưng bị gán cho agentA. Với code thật, `return None`, `}`, `import os`, dòng trống xuất hiện rất nhiều, nên tỷ lệ gán sai cao chứ không phải hiếm.

Nguyên nhân: `recompute_blame` so khớp **nội dung** dòng qua `HashSet`, không theo vị trí hay danh tính. Đây là hệ quả trực tiếp của việc chọn cách so khớp rẻ tiền.

### P1: sai kết quả nhưng không mất dữ liệu

**Lỗi 3: không có lệnh hoàn tác**

`zenforge --help` chỉ có: init, status, log, changeset, blame, history, checkout, branch. Không có `undo`, `revert`, `restore`, `reset`, `gc`, `fsck`.

Với agent sửa file hàng nghìn lần mỗi ngày, không có undo nghĩa là một lần ghi sai phải sửa tay hoặc dựng lại repo. Jujutsu giải bài này bằng **operation log**: mỗi thao tác ghi lại một snapshot trạng thái repo, cho phép `jj undo`, `jj op revert`, `jj op restore` ([jj operation log](https://docs.jj-vcs.dev/latest/operation-log/)). ZenForge không có tầng này.

**Lỗi 4: không có `fsck`, repo hỏng thì không biết**

Không có lệnh kiểm tra tính toàn vẹn: object thiếu cho một `changeset_files`, `parent` trỏ tới changeset không tồn tại, changeset `committed` nhưng không có `file_versions`. Những trạng thái này sinh ra từ chính các lỗi transaction đã sửa, nghĩa là repo cũ có thể đang hỏng mà không có cách phát hiện.

**Lỗi 5: không có xung đột ba chiều, merge chỉ fast-forward**

Không tính merge base. Hai nhánh phân kỳ cùng sửa một file thì không có khái niệm xung đột, chỉ có "áp changeset". Người dùng không được hỏi, không được xem diff ba chiều, không có cách giải quyết.

### P2: chi phí vận hành

**Lỗi 6: object không bao giờ bị thu hồi**

Changeset tạo ra rồi bỏ không commit vẫn để lại object trong bảng `objects`. Đo: 5 changeset pending bỏ không commit sinh 5 object, chiếm chỗ vĩnh viễn. Không có `gc`. Với agent tạo snapshot liên tục rồi huỷ, rác tích luỹ không giới hạn.

**Lỗi 7: lưu toàn bộ nội dung, không nén, không delta**

Xem mục 1. Đây là nguyên nhân của tỷ lệ 8,9 lần so với git.

**Lỗi 8: không có giao diện lập trình**

Chỉ có CLI. Không có HTTP API, không có MCP server. Anthropic nêu rõ một trong các nguyên tắc của Scion là **mọi truy vấn client dùng đều mở ra thành API**, nhờ đó job CI không cần giữ bản sao repo ([Scion](https://www.anthropic.com/research/redesigning-source-control)). Với ZenForge, công cụ ngoài muốn hỏi "dòng này ai sửa" phải shell ra CLI và parse text.

**Lỗi 9: so khớp đường dẫn tuyệt đối, không có rename detection**

Đổi tên file được ghi thành xoá cũ và thêm mới, mất toàn bộ lịch sử của file. git giải bằng similar-object detection khi diff.

## 3. Bốn hướng nâng cấp, cân nhắc theo chi phí và lợi ích

### Hướng A: sửa lỗi P0 ngay (bắt buộc)

Phạm vi: merge ghi file ra đĩa, phát hiện xung đột, blame theo vị trí.

Blame đúng cần thuật toán so khớp dòng theo dãy, không theo tập hợp. Cách rẻ và đủ tốt: duyệt từng version từ cũ tới mới, chạy LCS (longest common subsequence) giữa version trước và version sau, dòng nào khớp trong LCS thì giữ chủ cũ, dòng mới thì gán cho changeset hiện tại. Chi phí O(n·m) mỗi cặp version, với file vài nghìn dòng là vài triệu phép so sánh, chấp nhận được.

Xung đột: tính merge base bằng BFS hai chiều trên `parent`, so ba bản (base, ours, theirs). Nếu cả hai bên sửa cùng một vùng thì báo xung đột và dừng, không ghi.

Ước tính: 400 đến 600 dòng Rust, 8 đến 10 test mới. Đây là việc phải làm trước khi ZenForge được dùng cho bất cứ repo thật nào.

### Hướng B: giảm dung lượng bằng delta (lợi ích lớn nhất trên mỗi dòng code)

Thay vì lưu toàn bộ nội dung, lưu chuỗi thao tác so với version trước:

- Bảng `objects` giữ nguyên cho object đầy đủ.
- Thêm bảng `deltas(hash, base_hash, ops)` lưu danh sách (vị trí, xoá, chèn).
- Khi đọc object, nếu không có trong `objects` thì lần ngược chuỗi delta tới object gốc rồi dựng lại.
- Định kỳ gộp chuỗi delta dài thành object đầy đủ (giống git pack).

Hoặc cách đơn giản hơn nhiều: **nén từng object bằng zstd hoặc flate2**. File text nén được 3 đến 5 lần. Một dòng `flate2::write::ZlibEncoder` khi ghi, `ZlibDecoder` khi đọc, đổi cột `content TEXT` thành `content BLOB`. Ước tính giảm 3 đến 4 lần dung lượng với khoảng 40 dòng code, không đổi schema logic.

Đề xuất: làm nén trước (40 dòng, giảm 3 đến 4 lần), delta sau nếu vẫn chưa đủ.

### Hướng C: operation log để có undo

Học theo jj. Thêm bảng `operations(id, parent_op, kind, target_id, created_at, snapshot_json)`. Mỗi lệnh ghi ghi thêm một dòng operation, `snapshot_json` chứa trạng thái các con trỏ (head từng nhánh, current_branch).

Có nó thì `undo` chỉ là: đọc operation trước, khôi phục con trỏ. Không cần đụng tới object (vốn bất biến). Đây là lý do jj làm được undo rẻ: object không đổi, chỉ con trỏ đổi.

Ước tính: 250 dòng, 5 test. Giá trị cao vì agent ghi sai liên tục.

### Hướng D: MCP server

Bọc các lệnh sẵn có thành MCP tool: `zenforge_status`, `zenforge_changeset_create`, `zenforge_blame`, `zenforge_history`, `zenforge_diff`, `zenforge_log`. Dùng crate `rmcp` hoặc viết JSON-RPC trực tiếp trên stdio.

Lợi ích: agent gọi trực tiếp không cần shell, không phải parse text, có schema rõ cho tham số. Đây là điểm khác biệt so với git: git MCP server phải bọc một hệ thống nặng, ZenForge bọc thẳng vào SQLite.

Ước tính: 300 dòng. Nên làm sau khi P0 xong, vì bọc một hệ thống còn mất dữ liệu thì chỉ phát tán lỗi nhanh hơn.

## 4. Thứ tự đề xuất

| Bước | Việc | Lý do | Ước tính |
|---|---|---|---|
| 1 | Merge an toàn: ghi file, phát hiện xung đột, tính merge base | Đang mất dữ liệu im lặng | 400 đến 600 dòng |
| 2 | Blame theo dãy (LCS) thay vì theo tập nội dung | Đang gán sai chủ | 150 đến 200 dòng |
| 3 | Nén object bằng flate2 | 8,9 lần xuống còn khoảng 2 đến 3 lần so với git | 40 dòng |
| 4 | `fsck` kiểm tra toàn vẹn | Phát hiện repo hỏng do bản cũ | 120 dòng |
| 5 | Operation log và `undo` | Agent ghi sai cần hoàn tác | 250 dòng |
| 6 | `gc` thu hồi object mồ côi | Rác tích luỹ vô hạn | 80 dòng |
| 7 | MCP server | Tích hợp agent | 300 dòng |

Bước 1 đến 3 là điều kiện để coi ZenForge là dùng được. Bước 4 đến 7 là để dùng tốt.

## 5. Điều chưa kiểm chứng

Những điểm dưới đây là suy luận, chưa có phép đo hoặc nguồn xác nhận:

- Tốc độ sẽ ra sao khi repo có 100.000 changeset. Mới đo tới 3 changeset và 2.000 file.
- Nén flate2 giảm chính xác bao nhiêu lần trên code Rust thật. Mới suy từ tỷ lệ nén chung của text.
- LCS có đủ nhanh cho file 50.000 dòng không. Mới đo tới 2.000 dòng.
- `rusqlite` với cột BLOB có phát sinh vấn đề bộ nhớ khi object vài chục MB không.
- Ảnh hưởng của WAL mode khi nhiều agent ghi đồng thời trên cùng một repo. Mới thử 8 tiến trình, chưa thử 100.

## 6. Nguồn

- Anthropic, "Redesigning source control at Anthropic" (Scion), https://www.anthropic.com/research/redesigning-source-control
- Jujutsu, "Operation log", https://docs.jj-vcs.dev/latest/operation-log/
- Jujutsu, "Revset language", https://docs.jj-vcs.dev/latest/revsets/
- Pijul, "Theory" (đồ thị dòng, CRDT), https://pijul.org/manual/theory
- git, "git-merge-base", https://git.github.io/htmldocs/git-merge-base.html
- SQLite, "Recursive common table expressions", https://www.sqlite.org/lang_with.html
