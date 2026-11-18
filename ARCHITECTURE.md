# neo-log 架构设计

一个 Notepad++ 风格的日志分析工作台：多文件按时间合并、多级过滤子视图、书签、关键词高亮、可插拔的模块日志插件、会话持久化、tail 跟随、文本选择与自动换行。

## 1. 设计目标与非目标

| 目标 | 实现方式 |
| --- | --- |
| 大文件不卡顿 | mmap 零拷贝 + 行起始索引 + 虚拟化渲染，只处理可视行 |
| 多文件统一时间轴 | 插件负责时间解析，`k` 路稳定归并 |
| 多级过滤 | 不可变视图树，子视图物化父视图的行引用 |
| 书签跨视图稳定 | 书签以物理位置 `(DocId, line)` 为键，而非视图行号 |
| 模块差异可扩展 | `LogPlugin` trait + 注册表自动探测 + JSON 声明式插件 |
| 大结果集过滤省内存 | 过滤结果存为位图（1 bit/父行）+ 秩索引 |
| 工作现场可恢复 | 视图树/书签/规则/当前标签序列化为 JSON |
| 日志实时增长与轮转 | 轮询长度 / mtime / 创建时间 / 4 KB 前缀，区分追加与重写后重载并重建视图；F5 手动重载 |
| 纯 Rust、单进程 | eframe/egui 立即模式 GUI，无 Node/WebView 依赖 |

非目标（当前版本）：写入/编辑日志、跨进程二进制插件（dylib 热加载）、嵌入式脚本、全文索引与搜索替换。

## 2. 工作区划分

```
neo_log/
├── crates/core       neolog-core    纯逻辑库：文档、时间、合并、过滤、样式、会话、视图树、位图、快照
├── crates/plugins    neolog-plugins 内置插件：Generic / ROS 2 / Python logging / JSON Lines + JSON 声明式插件
├── crates/app        neolog-app     egui 桌面应用（bin: neo-log）
└── examples/plugins  动态插件 JSON 示例
```

依赖方向严格单向：`app → plugins → core`。`core` 不依赖 GUI，可单独用于 CLI 或测试。

```
┌─────────────────────────── crates/app (egui) ───────────────────────────┐
│ main.rs  窗口/DPI                                                       │
│ app.rs   ☰ 菜单、单行标签条、视图栈、重命名/备注、过滤/查找栏、侧栏抽屉 │
│ log_view.rs  虚拟化行渲染、软换行、列对齐、文本选择、双击取词、折叠       │
│ stats.rs  统计窗口（级别/模块/时间直方图/间隙）                          │
│ config.rs 配置目录/会话读写            theme.rs 字体（含 CJK）           │
│ demo.rs  示例日志                                                       │
└───────────────────────────────┬─────────────────────────────────────────┘
                                │ Session API
┌───────────────────────────────▼─────────────────────────────────────────┐
│ crates/plugins：GenericPlugin · Ros2Plugin · PythonLogPlugin             │
│                 JsonLinesPlugin · DeclarativePlugin（JSON，运行时加载）  │
└───────────────────────────────┬─────────────────────────────────────────┘
                                │ LogPlugin trait
┌───────────────────────────────▼─────────────────────────────────────────┐
│ crates/core                                                             │
│ source.rs  LogDoc / LineRef / DocId      time.rs   时间解析与格式化      │
│ merge.rs   k 路稳定归并                  matcher.rs 大小写不敏感匹配      │
│ filter.rs  查询 DSL → Filter             style.rs  HighlightRule/样式     │
│ plugin.rs  LogPlugin / PluginRegistry    text.rs   双击取词               │
│ bits.rs    位图过滤（rank/select）       persist.rs 会话快照/恢复         │
│ session.rs Session：文档、视图树、书签、规则的总协调者                    │
└─────────────────────────────────────────────────────────────────────────┘
```

## 3. 核心数据模型

### 3.1 物理层：`LogDoc`（[source.rs](crates/core/src/source.rs)）

- `LogDoc::open`（source.rs:50）用 `memmap2` 映射文件；若整个文件不是合法 UTF-8，退化为一次性有损转码 `String`，避免逐行校验的不可预测性。
- 打开时构建 `line_starts: Vec<usize>`：`line_text(line)`（source.rs:97）用二分定位切片，`O(log n)` 取行且零拷贝。
- 每行的时间戳在打开/切换插件时预计算为 `Vec<i64>`（纳秒），无时间戳的行向前填充（多行堆栈/异常归入同一条事件）；文件头无时间戳的行向后填充。
- `LineRef { doc: DocId, line: u32 }`（source.rs:11）是全局稳定的物理行引用。

### 3.2 视图层：`ViewNode` 与 `Rows`（[session.rs](crates/core/src/session.rs)）

```rust
pub enum Base { Doc(DocId), Merged(Vec<DocId>), View(ViewId) }
pub enum Rows {
    Doc(DocId),                        // 整篇文档，零物化
    List(Arc<Vec<LineRef>>),           // 合并结果
    Bits(BitRows),                     // 过滤结果：父行位图
}
pub struct BitRows { source: BitsSource, bits: Arc<Bitmap> }
pub enum BitsSource { Doc(DocId), List(Arc<Vec<LineRef>>), Parent(Arc<Rows>) }
pub struct ViewNode {
    id, title, query,
    base: Base,
    filters: Vec<Filter>,
    rows: Rows,
    parent: Option<ViewId>,
    children: Vec<ViewId>,
}
```

- 顶层视图有两种：文档视图 `Base::Doc`、合并视图 `Base::Merged`。
- 任意视图都可以 `fork_view` 派生 `Base::View(parent)` 的子视图，层级不限。
- `Rows::Doc` 指向整篇文档，完全不物化；合并视图物化为 `Arc<Vec<LineRef>>`。
- 过滤结果统一为 `Rows::Bits`（session.rs:742）：对父视图的每个候选行置 1 bit，再 `finish()` 建立累计秩表；`select(k)`（bits.rs:62）二分秩表 + 清除低位即得第 k 个命中行，`count_before`（bits.rs:78）用于渲染时定位。
- `Rows::len` 对位图返回 `count_ones()`（session.rs:78），而不是候选行数。
- 子视图的空过滤直接克隆父视图的 `Rows`（含 `Arc`），零分配。
- `Session::revision`（session.rs:150）在行集合、插件、规则、重载等任何可能改变渲染的变化时自增，UI 缓存（换行布局、折叠分组、列宽采样）以 `(ViewId, revision …)` 为键失效。

每个标签页对应一个根视图，派生过滤只在标签页内部形成层级：

```
标签页：合并视图 (root)            标签页：文档 A (root)
└── level>=warn (child)            └── module:planner
    ├── re:timeout (grandchild)
    └── bm: (grandchild)
```

应用侧用两份状态描述导航：`active: Vec<ViewId>` 是“活动标签页的根 → 当前叶”路径，`leaves: HashMap<ViewId, ViewId>` 记住每个根视图上次停留的叶子。切换标签页时 `activate_root` 通过 `resolve_leaf` 取回叶子（叶子已被删除或换了根就回退到根本身），再用 `view_path()`（session.rs:186）重建 `active`。因此标签页本身不需要额外的绘制层级，`active` 仍然是所有“当前视图”逻辑（过滤栏、查找、统计、复制、书签）的唯一入口。

### 3.3 装饰层

| 机制 | 数据 | 作用范围 | 备注 |
| --- | --- | --- | --- |
| 插件行样式 | `RowStyle` | 文档（随插件） | 级别着色、错误行背景、粗体 |
| 高亮规则 | `HighlightRule` | Global / Doc / Plugin | 关键词行背景、文字着色/加粗 |
| 书签 | `BTreeMap<DocId, BTreeSet<u32>>` | 文档 | 不随过滤/合并改变 |
| 选中行 | `Option<LineRef>` | 应用 | 状态栏显示文件/行号/时间 |
| 文本选区 | `Selection { anchor_row, anchor_byte, focus_row, focus_byte }` | 应用 + 当前视图 | 跨行时为多个字节区间；视图/过滤切换即清空 |

`RuleScope`（style.rs:50）决定规则命中哪些文档；`Session::rules_for`（session.rs:632）在渲染时按 `(DocId, plugin_id)` 过滤。规则保存匹配器（字面量或正则）与 `TextStyle`（前景色/粗体）以及可选的整行背景色。

## 4. 关键流程

### 4.1 打开文件

```
rfd 选择 / 拖拽 / Ctrl+O
  → Session::open_path
      → LogDoc::open (mmap + line_starts)
      → PluginRegistry::detect(path, sample)   // sniff 分数最高且 > 0
      → plugin.timestamp 逐行解析，forward-fill
  → Session::create_doc_view (Rows::Doc，零物化)
  → App 切换到该视图
```

### 4.2 合并

```
勾选文档 → 合并所选
  → Session::create_merged_view
      → merge_docs：BinaryHeap<Reverse<(time_ns, doc_order, line)>>
      → Rows::List(merged)
```

归并是稳定的：时间相同先按文档传入顺序，再按行号，因此每个文件内部顺序不会被打乱（merge.rs:8）。

### 4.3 派生 / 更新子视图（过滤）

```
过滤框输入 / 右键筛选 / 标签条 ＋ → 派生子视图
  → parse_query → Vec<Filter>
  → compute_rows(Base::View(parent), filters)
      → 从 parent.rows 逐行构造 MatchCtx { text, plugin, time, bookmarked }
      → filters.iter().all(matches)（AND 语义）
      → 命中的候选下标写入 Bitmap，finish() 建秩表
      → Rows::Bits { source: Parent(父行), bits }
```

`update_view`（session.rs:407）原地替换过滤条件后调用 `recompute_subtree`（session.rs:660），递归重算该节点及其所有后代。文档插件切换也会触发 `rebuild_all`，因为时间/级别语义可能改变。合并视图的过滤使用 `BitsSource::List`，文档视图的过滤使用 `BitsSource::Doc`，位图长度始终等于候选行数。

标签条（app.rs:2267）只渲染根视图，并位于唯一一行外壳里：左侧是 `☰` 菜单（app.rs:1942）与侧栏开关 `◧`，中间标签区放在横向 `ScrollArea` 中，标签再多也只横向滚动、不换行；每个标签显示标题、根类型标记（`▤` 文档 / `⇄` 合并）与关闭 `×`，活动标签额外显示派生层级的路径 chip。点击标签调用 `activate_root`（app.rs:1544）恢复该标签上次停留的叶子；关闭标签调用 `close_tab`（app.rs:1565），`Session::remove_view` 会一并删除它的整棵子树。

路径 chip 点开后才渲染视图栈面板（app.rs:2454）：每一级显示标题（悬停可见查询与备注），点击调用 `set_active_path` 切到该级；右键可打开重命名/备注对话框。每级的 `×` 调用 `remove_view_level`（session.rs:462）：把被删节点的子节点原位插回父节点的 `children`，改写 `child.parent` 与 `Base::View(parent)`，再 `recompute_subtree`，因此后代过滤会在新基座上立即重算；书签与高亮规则锚定 `(DocId, line)`，不受影响。根视图不可这样移除，因为其子节点会失去文档基座。视图栈与派生面板都挂在顶栏内，`Esc` 或“收起”关闭；侧栏不再重复渲染视图树。

### 4.4 渲染（每帧）

```
App::update
  → top panel（☰ + 侧栏开关 + 横向滚动标签条）→ bottom status → central 日志
  →（浮层）侧栏抽屉（Order::Middle，覆盖日志左侧，不参与布局）
  →（浮动窗口）统计窗口（可拖动/缩放，不参与布局）
  → log_view::show
      → 非换行：ScrollArea::show_rows(row_height)，只构建可视行
      → 换行：ScrollArea::show_viewport + WrapCache 前缀和，按可视窗口反查逻辑行
      → 折叠：ScrollArea::show_rows 作用于 FoldCache.entries（折叠后的条目）
      → 列对齐：表头 + ScrollArea::show_rows，每行按 ColumnLayout 分栏绘制
      → 对每个可视行：
          RowCache 命中 → 绘制缓存 galley
          →（首次出现）插件 RowStyle + 规则 + 查找 span → LayoutJob → galley 入缓存
          → 选中覆盖 → 文档色条 → 书签圆点 → 行号 → 查找命中底色
      → 点击/双击/拖拽命中 → Response 回传 App
```

- 行高 = `fonts.row_height(monospace(font_size)) + 2`；字号来自 `App::font_size`（`Ctrl+滚轮`），行高、行号栏宽度与换行列宽都随之变化。
- 查找命中由 `Needle::find_all` 生成 `FIND_BG` 背景 `StyledSpan`，叠加在插件装饰与高亮规则之上；与过滤互不影响。
- 折叠模式下 `Scroll::Row` 先经 `entry_of_row` 映射为折叠条目下标，保证查找/跳转仍能定位到目标行所在组。
- 列对齐模式下每个单元格单独布局 galley 并按列裁剪；选区、装饰与查找 span 都先映射到单元格局部字节再绘制，命中后再换算回整行字节。
- `LayoutJob` 按 `StyledSpan` 边界切分，粗体使用独立字体族 `mono-bold`（theme.rs），因此插件/规则可以混合颜色与字重。
- `RowCache`（log_view.rs）按 `(LineRef, revision, 字号, 查找键)` 缓存每行的 `RowStyle`、装饰/规则/查找 span 与整行 `Arc<Galley>`：滚动时同一行只做一次哈希查找，选中的行不重新装饰。`SearchMatcher::key()` 由查询与选项哈希得到，保证查找条件变化时失效；`revision` 覆盖行文本、规则、书签与视图重算。换行/列对齐模式复用 span 与样式，各自按分栏重新布局；缓存按帧淘汰，上限 2048 行 / 1024 个 galley。
- 双击用 `galley.cursor_from_pos(local).ccursor.index` 得到字符索引，再由 `word_at_char`（text.rs）取出 token；对 CJK 按字符而非字节处理。
- 行号栏左侧 22px 用于点击切换书签；双击不会误触书签。

### 4.5 软换行

`WrapCache`（log_view.rs:92）缓存两列数据：每逻辑行的视觉行数 `counts`、前缀和 `prefix`（u64）。列宽由 `(可用宽度 - 行号栏 - 24) / 'M' 字符宽` 估算，行内宽度用 `unicode-width` 逐字符累计，CJK 记 2 列。

```
revision/视图/列宽变化
  → ensure：逐行 wrap_count 填 counts，累加 prefix
滚动
  → total_visual = prefix[rows]
  → 可视 visual 起点 → row_at_visual 二分反查逻辑行
  → 每个逻辑行按 wrap_segments 切成多个 galley 段，段间绘制选区与书签
```

复杂度 `O(N)` 只在键变化时重建；滚动帧为 `O(可视段)`。代价是列宽是估算值，组合字符/emoji 可能与实际排版相差 1 列。

### 4.6 文本选择与复制

- 每个可视行以 `Sense::click_and_drag` 交互；`drag_started` 时记录拖拽模式（文本 / 整行）并建立锚点，之后的每一帧由 `extend_drag` 按全局指针位置更新焦点行与字节；`Shift+点击` 以原锚点扩选。拖拽模式放在 egui 临时数据里（键为 `ui.id().with("log-drag-mode")`），因此跨行、跨帧都能延续，松开主键即清除。
- 拖拽起点在文本区是文本选择，起点在行号栏是整行选择：`line_selection` 用 `usize::MAX` 表示“行尾”，向上拖时把锚点字节翻到行尾，保证正反两个方向都能覆盖整行。`三击` 或单击行号栏等同整行选择。
- 文本/列对齐/软换行三套渲染器共用同一套语义：字节偏移来自 galley 的 `cursor_from_pos`，软换行先定位段再取段内字符索引；列对齐从命中的单元格映射回整行字节。
- 选区拖到视口上下 24px 内时 `auto_scroll_drag` 通过 `ui.scroll_with_delta` 持续滚动（速度随越界距离线性增大），可一次选过整屏。
- `Ctrl+A` 选择当前视图全部可视行（`Selection` 覆盖 `0..rows-1`，末行到行尾），`Esc` 清除选区；`Ctrl+C` 交给 `App::copy_selection`（app.rs:501）：按 `bounds()` 排序后逐行切片（首行从选区字节起、末行到选区字节止），`floor_char` 规避切在多字节字符中间；无选区时复制状态栏当前选中行。
- 选区绘制为半透明 `SELECTION` 覆盖矩形，按每行/每段的文本宽度计算；整行选区铺满文本区右侧。

### 4.7 tail 跟随、轮转检测与手动重载

```
勾选“跟随” → follow = true，WatchFile 记录 FileStamp{len, modified, created, head}
每 600ms（F5 / 文件菜单“重新加载全部”走同一套分类）：
  → file_stamp：metadata + 读取前 4 KB 做 DefaultHasher 指纹
  → 与上次比较：
      文件消失             → 只提示一次“已消失”，保留内容；重新出现按重写处理
      created 变化 / 变短 / 同长但前缀变化 → ChangeKind::Rewrite
      变长且旧前缀不变     → ChangeKind::Append
      完全相同             → 跳过（手动重载仍会强制重读）
  → Session::reload_doc(doc, mode)（session.rs:246）
      → 重新 mmap / 重建行索引 / 重算时间轴
      → 重建全部视图（位图按新行数重建）
      → Append：裁剪越界书签；Rewrite：丢弃该文档全部书签
      → revision += 1
  → Rewrite 时同时清空指向该文档的选中行；跟随模式滚到底部
```

轮询而非平台 inotify/ReadDirectoryChangesW，是为了零额外依赖、跨平台一致；代价是 600ms 延迟。`created` 提供 rename+新建式轮转的身份判据（文件系统不支持创建时间时为 `None`），4 KB 前缀负责发现等长的就地重写。重载失败（文件被占用）只更新状态栏，不中断跟随。

书签策略：追加不移动行号，保留；轮转/截断后物理行号指向不同内容，必须丢弃并在状态栏报告数量。注意 Windows 上映射中的文件仍可被其他进程追加，但截断会被拒绝（`ERROR_USER_MAPPED_FILE`），因此 `copytruncate` 式轮转需要先关闭 neo-log 中的该文件。

### 4.8 会话持久化

```
关闭窗口 / 点击“保存会话”
  → App::snapshot：Session::snapshot + active 标签路径
  → config::save_session → %APPDATA%/neo-log/session.json（pretty JSON）
启动
  → config::load_session → Session::restore
      → 按 id 重建文档（open_path，恢复 plugin_id/day_base）
      → 按 id 升序重建视图（解析 query、compute_rows、挂回父子关系）
      → 恢复书签（过滤越界行）、规则（重建 Matcher，保留 id/next_rule）
      → 全部成功才替换原 Session；任一文件缺失则整体回退示例
  → active 取快照中最后一个仍存在的视图路径，否则最后一个根视图
```

快照带 `version`；读到更高版本直接报错，避免新旧字段误解。示例/内存文档不落盘，避免下次启动指向虚构路径；打开真实文件时若当前全是示例文件，会先丢弃示例工作区，保证“示例 → 打开文件 → 关闭”这条常见路径可以正常保存。写文件为直接覆盖（非临时文件 rename），崩溃窗口极小但未做原子写。

### 4.9 非破坏性查找（Ctrl+F / F3）与全局搜索

查找是与过滤正交的「视图内导航」，不创建子视图、不改 `ViewNode.rows`。查询统一编译为 `SearchMatcher`（[matcher.rs](crates/core/src/matcher.rs)），渲染高亮、命中列表与全局结果上下文共用同一份语义：

```
SearchOptions { regex, case_sensitive, whole_word }
  → SearchMatcher::build(query, opts)
      → Plain { Needle, whole_word } 或 Regex { case_insensitive(!case), whole_word }
      → 全词：命中两侧不能是字母/数字/下划线
输入框 / 选项 / revision / 切换视图变化
  → ensure_find：key = (view, revision, query, options)
      → matcher.is_match 逐行扫描 visible_len(view)
      → 命中行写下标到 Vec<usize>（存可见行号，不存 LineRef）
Enter / F3 → cursor 前进（越界回绕），Shift+F3 → 后退
  → selected = line_ref(row)，pending_scroll = Scroll::Row(row)
  → 渲染层用同一 matcher 给命中文本加 FIND_BG 背景 span
```

扫描 `O(N_v)` 只在 key 变化时发生；滚动帧零成本。把命中保存为可见行号使 bitmap 过滤视图下无需再反查父行。全词匹配走 `Needle::any_match`（带谓词、找到即停、无临时分配），整词搜索不会因每行构造 Vec 而变慢。非法正则在查找栏内联报错，不写入命中缓存。

全局搜索（`Ctrl+Shift+F`）复用同一 `find_input` 与选项，但扫描对象是**物理文档**而非视图：

```
Enter / 点击“搜索”
  → SearchMatcher::build → 扫描 session.docs()（文件顺序，非时间合并）
  → 每条命中存 GlobalHit { doc, line }，上限 5000 条，超出置 truncated
  → 结果面板 ScrollArea::show_rows 虚拟化：文件名 + 行号 + 时间 + 命中附近片段
点击结果
  → 优先选该文档未过滤的根视图（Base::Doc 且 filters 为空，row == line）
  → 否则在根视图中按 LineRef 反查行号；都没有则新建文档视图
  → set_active_path + selected + Scroll::Row，状态栏提示
```

结果缓存记录扫描时的 `revision`；文档重载/收藏变化后标为“基于旧版本”，按 Enter 重扫。单次扫描 `O(N_全部)` 只在显式搜索时发生，因此逐键输入不会触发全库扫描。

### 4.10 统计窗口（级别/模块/直方图/间隙）

`StatsCache`（[stats.rs](crates/app/src/stats.rs)）以 `(ViewId, revision)` 为键缓存一次完整扫描结果：

```
compute：单次遍历可见行
  → plugin.severity → 6 级计数（未知单列）
  → plugin.module   → HashMap 计数，排序取 Top 10
  → line_time > 0   → (时间, 行号) 序列
      → histogram_data：跨度/80 取「整齐」桶宽（1ms…1h），桶数上限 240
      → gap_data：相邻时间差 ≥ 1s，按长度降序取前 8
```

面板交互都转成既有能力：级别/模块点击生成 `level=…` / `module:…` 查询并 `fork_view`；直方图柱子点击生成 `time:HH:MM:SS..HH:MM:SS`（按时刻取模，见 5 节）；间隙点击回传 `Action::Jump`，直接定位到断档后的第一行。面板关闭时不计算，`revision` 变化时整表失效。

### 4.11 重复行折叠

`FoldCache`（log_view.rs）把「相邻且模板相同」的行合并为一个条目：

```
模板化：ASCII 数字串 → '#'，其余字符 ASCII 小写
逐行比较模板 → 相邻相同者成组 FoldGroup{start, len}
条目展开状态 BTreeSet<group_start>
  → 折叠：每组建一个 entry（显示组首行 + ×N 徽标）
  → 展开：组内每行一个 entry（首行徽标显示 - N）
点击徽标 → Response.toggle_fold(group) → 重建 entries
```

折叠条目是渲染层概念：`Rows` 与书签锚点仍是物理行，`Scroll::Row` 经 `entry_of_row` 二分映射。该缓存仅在 `(view, revision)` 变化时重建，滚动 `O(可视条目)`。只在连续行间折叠，且与软换行互斥（两者都需要接管垂直布局）。

### 4.12 视图重命名与备注

每个 `ViewNode` 带可编辑的 `title` 与 `note`。`Session::set_view_meta`（session.rs:424）裁剪首尾空白后写回；名称为空时回退到 `default_view_title`（有查询用查询，否则文件名 / 合并数量 / “父标题 · 全部”）。

```
标签页 / 视图栈层级右键“重命名 / 备注…”
  → App::open_rename(id)：载入当前标题与备注，请求输入焦点
  → rename_window：单行名称 + 多行备注（Enter 或 Ctrl+Enter 保存，Esc 取消）
  → Session::set_view_meta → ViewNode.title / note
```

备注是纯展示元数据：标签页与视图栈层级的悬停提示追加“备注: …”，视图栈面板中该级标题下方显示备注。它不参与过滤/查找，也不增加 `revision`，因此改名写字不会重建位图或让换行缓存失效。持久化时 `ViewSnapshot.note` 带 `#[serde(default)]`，旧会话文件可直接读取；恢复后标题与备注原样回到标签页与视图栈。

### 4.13 列对齐视图

`LogPlugin::columns` 把一行拆成 `LineColumns { level, module, message }`，每个 `ColumnCell` 带 `text` 与可选的源字节 `range`；时间列不来自插件，而由 UI 用 `Session::line_time` 格式化（UTC，跨天视图补 `MM-DD`）。

```
ColumnsCache::ensure(view, revision)
  → 按 ≤1500 个采样行取级别/模块最大显示宽度（级别 5..9，模块 6..18 列）
  → time_range 首尾日期不同 → 时间列用 MM-DD HH:MM:SS.mmm，否则 HH:MM:SS.mmm
每行
  → plugin.columns(text) → 三格；None（续行/堆栈）整行落到消息列
  → 每格：映射装饰/规则/查找 span 到局部字节 → LayoutJob + 按列裁剪
  → 选区矩形同理按格内前缀宽度绘制；命中用 galley.cursor_from_pos 得格内字节 + range.0
```

内置插件各自负责切分：ROS 2 取 `[LEVEL] [time] [node]` 前缀，Python 取 `time - module - LEVEL - `，JSON Lines 的级别来自解析后的 `Severity::label()`、消息保留原始行，声明式插件用「前缀链」启发式——只接受起点依次向前、且间隔仅有分隔符的字段，消息中的同名级别词不会误判成列。列视图与软换行、折叠互斥，因为三者都要接管行的垂直布局；列宽缓存只在 `(view, revision)` 变化时重算。

### 4.14 单行外壳、快捷键与帮助

顶栏只有一行：`☰` 菜单、`◧` 侧栏开关、垂直分隔线与横向滚动的标签条共用 `tab_strip`（app.rs:2267）。七个菜单收进 `☰` 的弹出层（`menu_button`，app.rs:1942）并作为子菜单展开，子菜单关闭会沿 `MenuResponse::Close` 级联收起整个 `☰`；品牌标识移到弹出层顶部。菜单项只负责派发动作，不持有额外状态；原先的按钮工具栏与独立菜单行都已删除，打开/重载/示例/合并等动作在文件菜单里：

| 菜单 | 内容 |
| --- | --- |
| 文件 | 打开、重载、合并勾选文件、保存会话、示例、退出 |
| 编辑 | 复制、查找/上一个/下一个、全局搜索、跳转行、切换书签、仅书签视图 |
| 显示 | 侧栏显隐、过滤栏显隐、跟随、换行、折叠重复、列对齐、统计窗口、字号缩放、顶部/底部 |
| 主题 | 深色 / 浅色 / 高对比单选，立即生效并写入 `ui.conf` |
| 视图 | 派生过滤视图、编辑过滤条件、应用到当前、展开视图栈、重命名/备注、移除过滤级、关闭当前标签页 |
| 高亮 | 关键词输入框与颜色选择器（菜单内编辑）、整行/文字高亮关键词、清除全部规则 |
| 帮助 | 快捷键与提示（`F1`）、关于（版本、会话文件、动态插件目录） |

菜单项统一经 `menu_item` 构造：右侧显示快捷键，不可用时 `add_enabled(false)` 置灰。动作复用过滤栏的同一组方法（`set_follow` / `set_wrap` / `set_fold` / `set_columns` / `set_stats`、`copy_to_clipboard`、`toggle_selected_bookmark`、`close_active_view`、`remove_active_level`、`zoom_font` 等），过滤栏复选框也调用这些 setter，避免菜单、复选框、快捷键多条入口出现行为分叉。

过滤栏本身也可收起：`top_bar` 只在 `filter_bar_open` 为真时挂载过滤卡片，隐藏时 `filter_input`、活动视图、过滤结果与书签都不受影响；显示菜单的“过滤栏”复选框调用 `set_filter_bar`，状态提示写入状态栏。隐藏后跟随、换行等开关仍可从显示菜单操作，窗口变窄时收起过滤栏即可消除顶部横向溢出。高亮关键词与颜色选择器随高亮菜单展开，因此顶部不再需要常驻输入行。

工作区侧栏不再是 `SidePanel`，而是 `sidebar_drawer`（app.rs:2834）渲染的 `Order::Middle` 浮层：固定贴在日志区左上角、宽度 318px，覆盖日志而不参与布局，因此收起时日志立即获得全部宽度。开关来自 `F9`、标签行 `◧` 或显示菜单；`sidebar_guard` 保证打开那一帧不会被同一次点击立即关闭，之后点击日志区（背景层）、按 `Esc` 或点 `×` 都会收起，点击浮动窗口/菜单不会误收。统计同样是浮动的 `egui::Window`（`stats_window`，app.rs:2876），可拖动、可缩放、带标题栏关闭按钮，不再占用右侧固定宽度。

状态栏压缩为单行：`TopBottomPanel::bottom` 上下内边距 2px、文字 10px，右侧“文件 · 标签页 · 规则”改为普通小字而非 chip，整条约 20px。界面开关与主题一起写入 `ui.conf`（`config::UiPrefs`，键为 `theme` / `sidebar` / `stats` / `filter_bar`），启动时在构建界面之前读回；旧文件只有 `theme=` 时其余项取默认值（侧栏关闭、统计关闭、过滤栏展开）。

主题不是编译期常量：`theme::Palette` 保存手绘 UI 与日志行需要的全部颜色，`PALETTES` 中预置深色 / 浅色 / 高对比三套，`CURRENT` 只是一个 `AtomicUsize` 索引，`theme::p()` 无锁返回当前调色板。切换主题时 `apply_theme` 先 `set_theme`，再用 `apply_style` 以当前调色板重建 egui `Visuals`（浅色走 `Visuals::light()` 并同步窗口标题栏），最后丢弃 `RowCache`——它缓存了上一套调色板烘焙出的 base color、查找高亮与 galley。插件给出的级别/装饰色经 `theme::ink` 转换：浅色主题下向文字色混合，避免黄色 WARN、浅蓝 INFO 在白底上失去对比度。选择结果写入配置目录的 `ui.conf`（`theme=<id>`，同一文件还保存侧栏/统计/过滤栏开关），启动时在 `apply_style` 之前读回，因此首帧就是正确配色。

`handle_shortcuts` 集中处理 `F1` / `F2` / `F5` / `F9`、`Ctrl+S` / `O` / `W` / `Q`、`Ctrl+B` / `C` / `F` / `G`、`Ctrl+Shift+F`、`Ctrl++` / `-` / `0`、`Home` / `End` / `PgUp` / `PgDn` 与 `Esc`。对话框打开时 `Esc` 按「重命名 → 跳转 → 全局搜索 → 帮助 → 关于 → 查找栏」的栈序关闭，之后依次清除选区、收起视图栈/派生面板与侧栏；帮助与关于是普通 `egui::Window`，不写入会话快照。

### 4.15 日志行右键菜单

三种行渲染路径（普通/折叠、软换行、列对齐）都调用同一个 `row_context_menu`：右键时把指针下的单词按 widget id 存入 egui 临时数据，菜单在后续帧继续读取它，避免菜单打开后指针移入菜单项时丢失上下文。菜单只把用户意图写进 `log_view::Response.action`，由 app 的 `handle_row_action` 执行，视图层不接触剪贴板、过滤视图或高亮规则：

| 菜单项 | 执行 |
| --- | --- |
| 复制整行 / 复制选中内容 / 复制单词 | `ctx.copy_text`，选区按 `Selection::row_range` 截取并 floor 到字符边界 |
| 添加 / 移除书签 | `session.toggle_bookmark`，与 `Ctrl+B`、行号圆点同一入口 |
| 高亮单词（文字）/ 以该词高亮整行 | 新增文档级高亮规则，颜色取高亮菜单中的选择器 |
| 筛选包含该词 | 把词转成 `re:<escaped>` 后派生子视图；`literal_query` 转义正则元字符，避免 `level=warn`、`a.b` 被重新解释成级别过滤或正则 |
| 只显示 X 级 | `level=warn` 等精确级别查询；级别来自插件解析 |
| 只显示模块 X | `module:<name>`，含空格时给模块名加引号 |
| 在全部文件中搜索该词 | 填入查找框并打开全局搜索窗口 |
| 展开 / 收起这 N 行 | 折叠模式下调用 `fold_cache.toggle`，组号与折叠徽标一致 |

## 5. 过滤 DSL

`parse_query`（filter.rs:81）按空白切词，双引号保留空格，非法引号/正则返回中文错误并显示在状态栏。所有词项 AND。

| 语法 | 含义 |
| --- | --- |
| `timeout reset` | 同时包含两个子串（ASCII 大小写不敏感） |
| `"connection reset"` | 含空格的短语 |
| `!error` | 取反 |
| `re:time.?out` | 正则（始终大小写不敏感） |
| `module:planner` | 插件解析出的模块名包含该子串 |
| `level>=warn` | 级别至少 WARN（`>` 为严格高于，`<=`/`<` 同理） |
| `field:key=value` | 结构化字段值包含 value；默认 logfmt，JSON Lines 支持嵌套 `a.b` |
| `time:08:00:00..09:00:00` | 时刻区间 `[起, 止)`，按 `time mod 一天` 匹配，不支持跨午夜 |
| `bm:` / `bookmark` | 仅书签行（物化时的快照） |

## 6. 插件契约

```rust
pub trait LogPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn sniff(&self, path: &Path, sample: &[&str]) -> f32 { 0.0 }
    fn timestamp(&self, line: &str, day_base: i64) -> Option<i64>;
    fn severity(&self, line: &str) -> Option<Severity>;
    fn module<'a>(&self, line: &'a str) -> Option<&'a str>;
    fn columns<'a>(&self, line: &'a str) -> Option<LineColumns<'a>> { None }
    fn field(&self, line: &str, key: &str) -> Option<String> { /* logfmt 默认实现 */ }
    fn decorate(&self, line: &str, out: &mut Vec<StyledSpan>);
    fn row_style(&self, line: &str) -> RowStyle;
}
```

`id`/`name` 返回 `&str` 而非 `&'static str`，因为声明式插件在运行时持有 `String`。

- 自动探测：`PluginRegistry::detect` 取 `sniff > 0` 中的最高分，全部为 0 时回退 `PlainPlugin`。
- 时间戳必须由插件给出；默认实现调用通用解析器（ISO、方括号、纯时间、Python logging 等）。
- `field` 用于 `field:key=value` 过滤：默认实现按 logfmt 解析（引号内可含空格、key 大小写不敏感、值去引号与尾部标点）；结构化插件可覆盖，JSON Lines 默认支持 `a.b` 嵌套路径。
- `decorate` 输出字节区间样式，渲染层负责与高亮规则叠加。
- `columns` 供列对齐视图使用，默认 `None`（整行按普通文本渲染）；返回 `ColumnCell` 时可携带源行字节区间，使选区、装饰与查找仍以原始字节为准。
- `field_contains` 与 `field` 语义一致但无字符串分配，供 `field:` 过滤逐行调用；覆盖了 `field` 的插件必须同时覆盖它（JSON Lines 已覆盖）。
- `detect_severity` / `decorate_severity` 用字节 token 扫描实现原 `\b` 语义，不再逐行跑正则；ROS 2 插件的 `[级别] [秒.纳秒] [模块]` 前缀由一次手动解析同时供 sniff / 时间戳 / 级别 / 模块 / 列视图 / 装饰使用。
- 用户可在侧栏为单个文档改选插件，触发 `Session::set_doc_plugin` 重算该文档时间轴与所有视图。
- 内置插件位于 [crates/plugins/src](crates/plugins/src)：ROS 2（`[INFO] [sec.nanos] [node]`）、Python logging（`asctime - name - LEVEL - msg`）、JSON Lines（`timestamp`/`level`/`module` 等字段，epoch 自动换算秒/毫秒/微秒/纳秒）、通用文本。

### 6.1 声明式 JSON 插件

`DeclarativePlugin`（[declarative.rs:12](crates/plugins/src/declarative.rs#L12)）把同一套 trait 能力数据化：

| JSON 字段 | 映射到 trait |
| --- | --- |
| `sniff` / `sniff_score` | `sniff`：任一样例行含子串即返回得分 |
| `timestamp` | `timestamp`：`common` 交给通用解析器；`epoch` 用 `sec_group`+`nanos_group` |
| `severity` / `module` | 各自一个正则 + 捕获组 |
| `decorate` | 正则命中文本的 `fg`/`bg`/`bold` |
| `severity_styles` | 按级别覆盖 `row_style` |

`columns` 由同一组正则派生：按匹配起点排序后逐段接受，只有两段之间全是分隔符（空白、`:-|;,[]()`）才把字段放进对应列，遇到消息正文立即停止，因此消息里恰好出现的级别词不会变成一列。

`load_dir`（declarative.rs:322）在启动时读取配置目录 `plugins/*.json`，按文件名排序；单个文件失败只记录到 `LoadReport.errors`，不影响其他插件与启动。JSON 示例见 [examples/plugins/service-log.json](examples/plugins/service-log.json)。不做任意代码执行的取舍见第 8 节。

新增 Rust 插件三件事：实现 trait → 加入 `builtin_plugins()` → 写 `sniff` 与至少一个时间格式测试。新增日志格式也可以完全不写 Rust，直接放 JSON。

## 7. 复杂度与性能

| 操作 | 复杂度 | 说明 |
| --- | --- | --- |
| 打开文件 | `O(n)` | 建行索引 + 逐行时间解析；mmap 无整文件拷贝，行索引用 `memchr` 扫换行 |
| 逐行级别 / 模块解析 | `O(行长)` | 字节 token 扫描，无正则、无分配；ROS 前缀一次解析供多个查询复用 |
| 时间戳解析 | `O(行长)` | 数字扫描快速路径（含小数位），非标准空白/格式回落正则 |
| 大小写不敏感匹配 | `O(行长)` | ASCII 查询用 `memchr2` 定位首字节，再比较剩余字节 |
| 正则过滤 / 正则查找 | `O(N)` | 从 HIR 提取“必须出现”的字面量先做 `memchr` 预筛，仅候选行进入正则引擎；交替与可选段不预筛 |
| 取一行文本 | `O(log n)` | 行索引二分 + 切片 |
| 合并 k 个文件 | `O(N log k)` | 堆大小 k；输出 N 个 `LineRef` |
| 过滤一层 | `O(N_p)` | N_p 为父视图行数；结果位图为 `N_p/8` 字节 + 秩表 `N_p/64×4` 字节 |
| 位图取第 k 行 | `O(log(N_p/64))` | 二分秩表 + 64 位内清低位 |
| 重算子树 | `O(Σ N_i)` | 自顶向下；未变化的父行不重新匹配 |
| 每帧渲染 | `O(可视行)` | `show_rows` 只构建约 50 行；样式/装饰/查找 span 与 galley 命中 `RowCache`，首帧后才解析 |
| 查找扫描 | `O(N_v)` | 仅 `(视图, revision, 查询, 选项)` 变化时；滚动与导航为 `O(1)` |
| 全局搜索 | `O(N_全部)` | 仅显式搜索时扫描全部文档，单个结果集上限 5000 条 |
| 折叠分组 | `O(N_v)` | 仅 `(视图, revision)` 变化时；滚动为 `O(可视条目)` |
| 列宽采样 | `O(min(N_v, 1500))` | 仅 `(视图, revision)` 变化时；滚动为 `O(可视行 × 4 格)` |
| 统计计算 | `O(N_v)` | 仅在面板打开且 key 变化时；直方图桶数 ≤ 240 |
| 换行布局重建 | `O(N)` | 仅在 `(视图, revision, 列宽)` 变化时；滚动为 `O(可视段)` |
| tail 检查 | `O(文档数)` | 每 600ms 一次 `metadata` + 每文件 ≤4 KB 前缀读取；有变化才 `O(n)` 重载 |
| 手动重载 | `O(Σ N)` | 强制重读全部磁盘文件；书签按 Append/Rewrite 保留或清空 |

内存对比：100 万候选行过滤后，`Vec<LineRef>` 约 8 MB；位图约 124 KB + 秩表约 62 KB。代价是随机取行多一次 `select`。

百万行 ROS 2 基准（`cargo run --release --example perf -p neolog-plugins`，Windows 本机；数值随机器浮动）：

| 操作 | 优化前 | 优化后 |
| --- | --- | --- |
| 打开 1M 行（建索引 + 时间轴） | 237 ms | 85 ms |
| 过滤 `level>=warn` | 174 ms | 53 ms |
| 过滤 `level=error` | 182 ms | 51 ms |
| 过滤 `module:lidar` | 178 ms | 61 ms |
| 过滤 `timeout`（纯文本） | 82 ms | 26 ms |
| 过滤 `re:latency 49[0-9]ms`（需加引号作为单个词项） | — | 37 ms |
| 正则扫描 1M 行（同一表达式，含取行） | 56 ms | 27 ms |
| 查找扫描 1M 行（纯文本） | 85 ms | 44 ms |
| 查找扫描 1M 行（正则） | — | 40 ms |
| 50 个可视行的逐行解析 | 44 µs | 21 µs |
| 50 个可视行的样式 + span + LayoutJob 构建（每帧，临时进程内基准） | 40 µs | <1 µs |
| 统计窗口全量扫描 | — | 121 ms |
| `field:level=INFO` 全量扫描（行内无 `=`） | 156 ms | 13 ms |
| `field:level=INFO` 全量扫描（logfmt 行，80 万命中） | — | 65 ms |
| `field:frame=999999` 全量扫描（logfmt 行，1 命中） | — | 71 ms |

优化手段：级别关键字改为字节 token 扫描（保留 `\b` 语义）；ROS 2 前缀由多次正则捕获改为一次手写解析；时间戳增加纯数字快路径并保留正则兜底；ASCII 大小写不敏感查找用 `memchr2` 找首字节；正则过滤与正则查找先用 `regex-syntax` HIR 提取“必须出现”的字面量做预筛（交替与可选段放弃预筛，非 ASCII 行直接交给引擎，避免破坏 Unicode 大小写折叠语义）；`plugin_for` / `rules_for` 改为借用返回、`field:` 走 `field_contains`、书签仅在过滤器需要时查询，去掉每行的临时字符串、`Arc` 克隆与哈希查找；行索引用 `memchr_iter` 扫换行；渲染层用 `RowCache` 缓存每行的样式、span 与 galley，滚动帧不再重复装饰与排版；`field:key=value` 先用 `memchr` 找 `=`（没有 `=` 的行直接判否），纯 ASCII 且无引号的行只在 `=` 之间跳跃并取名字/值，引号或非 ASCII 行回退到原分词器，并在分词前用 ASCII 大小写不敏感查找确认 key 出现。剩余的单项成本主要是统计面板的首帧全量扫描（约 120 ms），只在显式打开面板时触发。

进一步优化空间：把 `line_text` 所需的时间解析按需惰性化、用临时文件 + rename 做原子会话写入、为统计窗口缓存上次结果。

## 8. 设计取舍

1. **不可变视图树 vs 可变管线**：子视图引用父结果，天然支持多层过滤与一致的书签；代价是父视图更新需重算整棵子树（当前规模可接受）。
2. **位图 vs 物化行引用**：过滤结果保留父行位图，内存约 1/64，且父行下标本就是稳定锚点；代价是取行需 `select` 二分，合并视图仍需物化 `Vec<LineRef>`。
3. **物理行键 vs 视图行号**：书签、选中、规则都锚定 `(DocId, line)`，切换标签/过滤后仍然有效。
4. **egui vs Web 前端**：无 Node 依赖、单可执行文件、虚拟化简单；文本排版能力有限，软换行采用等宽列估算而非完整字形测量以保持百万行可滚动。
5. **插件在进程内 vs 动态库**：进程内零序列化开销、可直接返回借用切片；动态加载选择 JSON 声明式而非 dylib，因为 Rust 无稳定 ABI、且不希望在日志分析器里执行第三方代码。表达力不足的部分仍可编译内置插件。
6. **轮询跟随 vs 平台事件**：轮询零依赖、跨平台一致，600ms 延迟可接受；代价是仅凭长度、mtime、创建时间和 4 KB 前缀判断变化，等长且只改前缀之后内容的就地改写会漏检。
7. **会话恢复全有或全无**：任一文件缺失就回退示例，避免“半个工作区”造成误解；代价是移动/删除一个文件就会丢弃整次恢复（快照本身仍在磁盘上）。
8. **查找与折叠是渲染层能力**：命中行与折叠组都不修改视图树，因此不改变书签/规则锚点，也不产生多余标签；代价是查找/折叠缓存需要监听 `revision` 并重建，且折叠只合并相邻同模板行。
9. **`time:` 按时刻取模**：直方图点击与 `time:` 过滤共用同一语义，跨天日志可直接筛出「每天 08:00–09:00」；代价是无法表达跨午夜区间或指定日期。
10. **全局搜索扫文档而非视图**：结果是物理文件中的命中，不受当前过滤影响，点击后切到该文件的未过滤视图；代价是结果按文件顺序而非时间排列，且单次上限 5000 条。
11. **列视图由插件切分而非固定正则**：每种格式的前缀规则本就写在插件里，列视图复用同一份解析，避免 UI 与插件对“模块从哪开始”给出两种答案；代价是无前缀的续行只能落在消息列，声明式插件对字段顺序有要求。

## 9. 验证

- `cargo test --workspace`：core 65 项、plugins 17 项、app 15 项（直方图桶覆盖、间隙排序、追加/同长重写/轮转分类、无变化判定、行缓存按 revision/字号/查找键失效、整行选择在两个方向上的行范围、主题 id 往返与三套调色板差异、右键菜单查询转义与模块名引号、标签页记忆叶节点与回退/文档根查找、`ui.conf` 已知键解析与未知键忽略）全部通过，覆盖时间格式（含快速路径与正则兜底的一致性、`time:` 时刻解析与时间戳前缀剥离）、归并稳定性、过滤 DSL（含 `field:` / `time:`）、查找匹配器（正则/大小写/全词边界/字面量预筛不误杀、Unicode 折叠与非 ASCII 行回退、缓存键随查询与选项变化）、级别 token 扫描的边界语义、位图 rank/select、书签跨视图、重载时 Append/Rewrite 书签策略、移除过滤级后子视图上移重算、视图名称/备注的编辑与空名回退、子视图重算、插件探测、JSON Lines 解析、ROS 前缀手动解析、四种内置/声明式插件的列切分、会话快照往返（含备注）、声明式插件解析（含示例 JSON）、行缓存的 span 复用与失效、`field:` 快速扫描与原分词器的逐例一致性。
- 百万行基准 `cargo run --release --example perf -p neolog-plugins` 覆盖打开、五类过滤、无字段与 logfmt 两种 `field:` 扫描、查找扫描、可视行逐行解析与统计全量扫描，用于回归性能；行缓存的收益另用临时进程内基准测得（50 行 × 100 帧样式/span/LayoutJob 构建 3.97 ms → 94 µs），未作为常驻测试入库。
- 运行时已通过截图确认：合并时间轴、级别背景色、粗体、CJK 字体、文档色条、多行标签条与过滤栏、规则统计；这项截图验证早于两次外壳改造（先是标签条 + 视图栈 + 派生面板替换多行嵌套标签与侧栏视图树，之后又把菜单收进 `☰`、侧栏改为浮层抽屉、统计改为浮动窗口并压缩状态栏），当前单行外壳同样未做截图回归；列对齐视图下时间/级别/模块/消息四列对齐、WARNING/CRITICAL 完整显示、续行落入消息列、预置选区跨单元格正确绘制。
- 持久化、跟随、位图、动态插件、选择/换行/列对齐、`☰` 菜单与快捷键入口（菜单与过滤栏共用同一组 setter，含侧栏/过滤栏收起与展开；顶部工具栏与独立菜单行已合并进单行外壳）与 P0/P1（查找与查找选项、全局搜索、跳转/缩放、统计窗口、间隙检测、折叠、结构化字段）均通过编译与核心单测；GUI 交互（`☰` 子菜单展开与置灰、`F9` 侧栏抽屉与点击日志区收起、快捷键、跟随、F5 重载、拖选、软换行、列对齐切换与格内拖选、查找栏、全局搜索结果、折叠徽标、统计窗口拖动/缩放、直方图点击、重命名/备注与帮助/关于窗口）未做自动化回归，因为当前环境禁止模拟鼠标/键盘输入，仅由代码路径覆盖，建议手动验证一次。
- 已知限制：文本选择不跨视图保留；`bm:` 是派发时快照；换行列宽为估算值。
