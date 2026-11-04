# Salsa 代码异味复盘（剩余项）

> 状态：2026-11-05 已完成第一批清理；本文件只保留**尚未解决**的项。
> 范围：代码结构、重复、分层、可维护性。

---

## 0. 方法

扫描方式：

- 全仓 `*.rs` 行数统计
- 函数体长度统计（按 `fn` 与花括号配对）
- `grep` 重复模式：`enum Codec`、`CodecArg`、`allow(clippy::too_many_arguments)`、`Element::register`、`fn init()`
- 8 行窗口的重复块检测（排除生成表）
- 对照 `JustOneCacophony` 的代码结构，区分“移植带入”和“Rust 特有”

---

## 1. 优先级总览

| 优先级 | 项目 | 影响 |
|---|---|---|
| 高 | 两个 App 的 pipeline/插件注册/设备枚举重复；GUI 独立 workspace + 运行时 dylib | 构建、依赖、行为漂移 |
| 中 | GStreamer 插件样板重复 | 复制粘贴 |
| 中 | 参数爆炸 | 难测、难改 |
| 中 | 上帝函数/上帝对象 | 局部修改风险高 |
| 中 | 位流读写 4 套实现 | 边界行为不一致 |
| 中 | 错误策略混杂 | 库层 API 不稳定 |
| 中 | 超大模块、测试混排 | 导航/编译成本 |
| 中 | 设备枚举重复 | 两套行为 |
| 中 | 测试 harness 重复 | 维护性 |
| 低 | `CarrierInput` 三变体分发、`from_bed_name`、`as` 转换、stringly-typed 配置、残留采样率常量 | 可读性 |

---

## 2. 高优先级

### 2.3 两个 App 的 pipeline、插件注册、设备枚举重复

audiod 与 GUI 各有一套：

| 功能 | audiod | GUI |
|---|---|---|
| `element()` | `crates/apps/salsa-audiod/src/pipeline.rs:87` | `crates/apps/salsa-gui/src/pipeline.rs:321` |
| pipeline 构建 | `crates/apps/salsa-audiod/src/pipeline.rs:117+` | `crates/apps/salsa-gui/src/pipeline.rs:330` |
| 时长扫描 | — | `crates/apps/salsa-gui/src/pipeline.rs:451` |
| 设备枚举 | `crates/apps/salsa-audiod/src/devices.rs:60`、`:111` | `crates/apps/salsa-gui/src/pipeline.rs:176`、`:206`、`:223` |
| 元素注册 | `crates/apps/salsa-audiod/src/pipeline.rs:68-107` | 无显式注册，靠运行时 dylib |
| `Codec` 枚举 | `salsa-codec`（已共享） | `crates/apps/salsa-gui/src/pipeline.rs:15`（未复用） |

GUI 还维护一份运行时 `.dylib` 清单：

- `crates/apps/salsa-gui/src/main.rs:5`：`PLUGIN_LIBRARIES`
- `crates/apps/salsa-gui/src/main.rs:107`：文件系统搜索
- `crates/apps/salsa-gui/Cargo.toml:10`：自己的 `[workspace]`

这带来：

- `salsa-gui` 不在根 workspace，独立 `Cargo.lock`/`target`，依赖版本可能与主 workspace 漂移
- GUI 无法直接依赖 `gst-eac3joc`/`gst-truehd`/`gst-objectaudio` crate，只能加载 dylib
- GUI 的 `Codec` 无法复用 `salsa-codec`，元素名以字符串散落在多个文件
- 元素名（`salsaeac3jocparse` 等）仍以字符串散落在 tests 与日志中

建议方向：抽共享 `salsa-gst`（pipeline/注册/设备/元素名），并决策 GUI 是否进根 workspace。

### 2.4 GStreamer 插件样板重复

重复：

- `CapsInfo` + `push_src_caps`
  - `crates/gst-plugins/gst-eac3joc/src/parse.rs:26`、`:38`
  - `crates/gst-plugins/gst-truehd/src/parse.rs:21`、`:106`
- decoder 的 `ensure_output_caps` / `drain`
  - `crates/gst-plugins/gst-eac3joc/src/decoder.rs:54`、`:85`
  - `crates/gst-plugins/gst-truehd/src/decoder.rs:195`、`:329`
- `push_frames` vs `push_decoded` 结构相同
- `ElementMetadata` / `pad_templates` / `set_sink_caps` 在 6 个插件里同构

建议方向：

- 用宏/helper 统一 parser 的 metadata、pad template、caps 推流
- 用宏/helper 统一 decoder 的 caps 协商、flush 处理

---

## 3. 中优先级

### 3.1 参数爆炸

以下函数仍有过多参数（allow 仅保留在函数/模块级）：

- `select_centroids_reconstruction`：`crates/libs/encoder/atmos-cluster/src/reconstruction.rs:368`，**16 个参数**
- `solve_gains`：`reconstruction.rs:197`，12 个参数
- `select_centroids`：`crates/libs/encoder/atmos-cluster/src/clustering.rs:24`，9 个参数
- `cluster_metadata`：`clustering.rs:302`，10 个参数
- `audio_reconstruction_gains`：`audio_assign.rs:9`
- `choose_payload`：`crates/libs/encoder/eac3joc-encoder/src/core.rs:407`，9 个参数
- `SpeakerRenderer::process` 也有一组 `#[allow(clippy::too_many_arguments)]`

建议方向：参数结构体（`ReconstructionParams`、`GainSolveRequest`、`PayloadRequest`），把剩余的函数级 allow 也消掉。

### 3.2 上帝函数/上帝对象

按当前行数：

| 位置 | 规模 |
|---|---|
| `crates/libs/encoder/atmos-cluster/src/pipeline.rs:379 run_cluster` | 355 行 |
| `crates/libs/encoder/atmos-cluster/src/renderer.rs:161 render` | 394 行 |
| `crates/apps/salsa-audiod/src/service.rs:494 render` | 337 行 |
| `crates/apps/salsa-audiod/src/main.rs:376 main` | 329 行 |
| `crates/libs/encoder/eac3joc-encoder/src/core.rs:88 encode` | 248 行 |
| `crates/apps/salsa-gui/src/app.rs:83-1286 impl SalsaApp` | 约 1200 行方法 |
| `crates/apps/salsa-gui/src/ui/stage.rs:78 draw` | 181 行 |

典型职责混合：I/O、状态机、校验、错误转换、统计输出。建议按阶段拆分，并把统计/日志移出核心路径。

### 3.3 位流读写 4 套实现

- `crates/libs/decoder/eac3joc/eac3joc/src/bit.rs:7`：手写 `BitReader`
- `crates/libs/encoder/eac3joc-encoder/src/bitwriter.rs:5`：手写 `BitWriter`，`:102` 又一套 `BitReader`
- `crates/libs/audio/objectaudio-oamd/src/bitstream.rs:15`：基于 `bitstream-io` 的 `SliceReader`
- `crates/libs/decoder/eac3joc/eac3joc/src/transport.rs:84`：零散 `read_bits`

风险：MSB-first、越界行为、可变长组语义各自实现，边界行为容易不一致。

建议方向：统一到 `bitstream-io`，或抽内部 `salsa-bitstream`，至少 ENC/DEC 共用。

### 3.4 错误策略混杂

- `thiserror` 枚举、`anyhow::Error` 包装、`String` 三种并存
  - `OamdError::Parse(anyhow::Error)`
  - `TrueHdError::Parse/Decode(anyhow::Error)`
  - `impl From<PipelineError> for String`：`crates/apps/salsa-gui/src/pipeline.rs:126`

建议方向：库边界统一 typed error；`anyhow` 只留在 App/CLI 层，库层不暴露 `String` 错误。

### 3.5 超大模块、测试混排

| 文件 | 行数 |
|---|---:|
| `crates/libs/audio/objectaudio-oamd/src/payload.rs` | 1949 |
| `crates/apps/salsa-gui/src/app.rs` | 1388 |
| `crates/apps/salsa-audiod/src/service.rs` | 1157 |
| `crates/libs/encoder/atmos-cluster/src/pipeline.rs` | 1084 |
| `crates/apps/salsa-gui/src/visual/timeline.rs` | 1003 |
| `crates/apps/salsa-audiod/src/output.rs` | 949 |

`payload.rs` 同时包含 parser 类型、wire 结构、`EXAMPLE_DATA` 和约 400 行测试。建议按 element 拆模块，测试移到 `tests/` 或独立 `#[cfg(test)]` 文件。

### 3.6 设备枚举重复

- `crates/apps/salsa-audiod/src/devices.rs:60 list_output_devices`、`:111 max_channels`
- `crates/apps/salsa-gui/src/pipeline.rs:176 enumerate_devices`、`:206 max_channels_for_device`、`:223 max_channels_from_caps`

两边都在 `DeviceMonitor` + caps channel 解析。建议共享设备查询层，App 只做展示模型转换。

### 3.7 测试 harness 重复

`crates/gst-plugins/*/tests/*.rs` 基本每篇都有：

```rust
fn init() {
    gst::init().unwrap();
    gst::Element::register(...);
    gst::Element::register(...);
}
```

位置包括：

- `gst-damf/tests/{sink,pipeline}.rs`
- `gst-eac3joc/tests/pipeline.rs`
- `gst-truehd/tests/pipeline.rs`
- `gst-objectaudio/tests/{pipeline,interop,adapt_roundtrip,hrtf,oar_stream}.rs`

建议抽 `test-support`（dev-only）crate，统一注册所有 `salsa*` 元素；注意与插件 crate 的 dev-dependency 循环。

---

## 4. 低优先级

### 4.1 `CarrierInput` 的三变体分发

`crates/apps/salsa-audiod/src/carrier.rs:106-137`：

```rust
pub enum CarrierInput {
    File(FileInput),
    Shared(SharedInput),
    Scripted(ScriptedInput),
}
```

`next_chunk` 是同样的三路 match。如果引入 trait，可与 codec access unit 一起处理。

### 4.4 `from_bed_name` 把 DAMF 名词放进 IR crate

- `crates/libs/audio/objectaudio-ir/src/speaker.rs:75`

`objectaudio-ir` 自称 codec-independent IR，但 `from_bed_name` 解析的是 DAMF/OAMD 的名称/别名（`LSS`、`TFL`、`Lfh`、`Ltm` 等）。

如果这是有意的共享语义表，可以保留；如果 IR 要保持独立，建议把名称解析留在 `objectaudio-damf`/`objectaudio-oamd`，IR 只接受枚举。

### 4.6 `as` 数值转换与 stringly-typed 配置

- 全仓约 500+ 处 `as` 转换
- `salsa-audiod/src/config.rs` 的 `codec`/`render` 是字符串（`codec` 已在边界用 `Codec::from_label` 解析一次，但字段仍是字符串）
- GUI 的 `Layout`/`DeviceSelection` 也有字符串路径

建议关键路径用 `try_from`/显式 Q15 类型；配置在边界处解析一次，内部只传枚举。

### 4.8 残留的采样率/帧长常量

`salsa-codec` 已集中 `EAC3_DECODER_DELAY`、`EAC3_FRAME_SAMPLES`、`SAMPLE_RATE`、`CARRIER_SAMPLE_RATE`，但仍有字面量散落：

- GStreamer 插件的 `48_000`/`192_000`/`1536`（如 `gst-objectaudio`、`gst-truehd`、`gst-damf`）
- tests 中的 `48_000`/`1536`
- `eac3joc/src/transport.rs` 的 `FRAME_SAMPLES`/`1536`

建议继续把这些引用收敛到 `salsa-codec`，特别是延迟相关值。

---

## 5. 建议清理顺序

1. **抽 `salsa-gst`（pipeline/注册/设备）**
   - 处理 2.3、3.6
   - GUI 是否进根 workspace 单独决策
2. **GStreamer 宏/helper**
   - 处理 2.4、3.7
3. **参数结构体 + 拆分上帝函数**
   - 处理 3.1、3.2
4. **统一位流 + 错误处理**
   - 处理 3.3、3.4
5. **模块拆分、测试搬迁、命名清理**
   - 处理 3.5、4.x

---

## 6. 明确不建议动的部分

以下不是异味，或属于有意设计，不要为了“统一”而改：

- `iec61937/src/carrier.rs` 的 burst/word-swap 逻辑：协议层，保持独立
- `eac3joc/src/bed.rs` 的 `position_bit` / `expand_chanmap` / `acmod_positions`：E-AC-3 wire 表，和 OAMD bed bit 不是一回事
- `objectaudio-oamd/src/payload.rs` 的 `STD_BED_LIST` / `ISF_COUNT_LIST`：wire 格式常量
- `objectaudio-render/src/layouts.rs`：生成表，若要改应改生成器，不要手改
- `crates/libs/render/hrtf/src/tables_public.rs`、`tables_rosella.rs`、`eac3joc-dsp/src/qmf_tables.rs`、`huffman_tables.rs`：生成/参考数据
- `salsa-gui` 的 eframe 依赖和 UI 生命周期：属于 App 层，不是重复的 codec 逻辑

---

## 7. 复现扫描命令

```sh
cd /Users/a1/ddp_research/salsa

# 行数
find crates -path '*/target/*' -prune -o -type f -name '*.rs' -print \
  | xargs wc -l | sort -n | tail -40

# Codec 枚举
grep -rn "pub enum Codec\|enum CodecArg" --include='*.rs' --exclude-dir=target crates

# 参数爆炸
grep -rn "allow(clippy::too_many_arguments" --include='*.rs' --exclude-dir=target crates

# 位流实现
grep -rn "struct BitReader\|struct BitWriter\|struct SliceReader\|fn read_bits" \
  --include='*.rs' --exclude-dir=target crates

# 插件注册重复
grep -rn "fn init()\|Element::register" --include='*.rs' \
  --exclude-dir=target crates/gst-plugins/*/tests crates/apps
```

---

## 8. 交接摘要

**最值得先做**：抽 `salsa-gst`（pipeline/注册/设备），并决策 GUI 是否进根 workspace；这两步能消掉 2.3、3.6 以及 GUI 的 `Codec`/dylib 重复。

**最容易被忽略**：GUI 的独立 workspace + 运行时 dylib 加载，是“同一功能两套集成方式”的根源；不解决它，pipeline/设备/插件注册的重复会反复出现。

**最需要谨慎的剩余风险**：库边界错误类型混杂（3.4）和参数爆炸（3.1），改动面广，且目前缺少结构体/trait 约束。

**不要误改**：wire 表、生成表、E-AC-3 chanmap、HRTF/QMF 常量。
