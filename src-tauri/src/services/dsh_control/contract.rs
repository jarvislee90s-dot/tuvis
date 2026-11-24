//! 逐操作契约识别（计划 §2.3 固定入口 `contract::read`；Spec §6.2–6.5）。
//!
//! **做什么**：对**当前安装的官方 DSH Desktop 包**做只读、有预算的契约识别——
//! 解析 `app.asar` 目录，取出官方生成的 typert 描述符（`lib/typert.host.js`）与
//! 少量协议声明文件（上传路由、审批取值、转发事件白名单、事件结果信封），按
//! 有版本的结构/语义规则逐 Operation 产出 [`OperationContractMatch`]。
//!
//! **不做什么**（纪律）：
//! * 不执行包内 JS（没有 JS 引擎，也不为识别启动任何子进程）；
//! * 不发任何写帧、不连接 Desktop（纯文件读取；[`ContractFileSource`] 只提供
//!   `open` → [`ContractFileReader`] 的读面——验收⑥的编译期边界）；
//! * 不把「方法名出现」当契约匹配：每条规则要求**目标方法自己的 descriptor 块**
//!   内出现结构形状**与**关键语义片段（B-1：兄弟方法的同名字段不得替目标方法
//!   命中——按 `'ns/method':{` 切块匹配，见 [`Rule::windows`]）；
//! * 不把预算受限误标为「永久不支持」：预算超限只把**受影响操作**记入
//!   [`ContractReadReport::inconclusive`]（「暂不可识别」词），无关操作照常；
//! * 不把合成夹具称为官方新版实测：本文件的规则锚点全部来自本机 0.2.1-rc.3 包内
//!   真实描述符（逐条注释），但测试输入是 synthetic 拼装。
//!
//! # 两个生产入口（申报）
//!
//! * [`read_report`] —— **能力求值必须走这个**：返回 [`ContractReadReport`]，把
//!   「预算受限/来源暂不可读」的逐操作事实一起交给 `capability::evaluate`，
//!   否则验收⑤（预算超限只限相关项）在真实装配上不成立。
//! * [`read`] —— 计划 §2.3 的固定签名，**只投影可持久化证据**（`ContractEvidence`），
//!   **丢弃**预算/不确诊事实。它用于「把证据落账」，**不得**用于能力求值。
//!
//! # 阻塞 IO（调用方纪律）
//!
//! 两个入口都是**同步阻塞 IO**（读 100+ MiB 的 asar 并整包哈希）：调用方必须放进
//! `spawn_blocking`（或等价线程池），**不得**在 Tauri 主线程、3 秒轮询路径或
//! 事件循环临界区内直接调用。
//!
//! # `rules` 参数语义（申报）
//!
//! [`read`] 的 `rules: u32` = **调用方要求的规则表代际**，必须与当前二进制内的
//! [`CONTRACT_RULE_VERSION`] 相等；不等时返回 `DSH_CONTRACT_RULES_MISMATCH`，而不是
//! 「用新规则算一遍再冒充旧代际证据」。语义闭环：
//! * 证据自带 `rule_version`（= 实际用于匹配的规则表版本，恒等于
//!   [`CONTRACT_RULE_VERSION`]）；
//! * [`super::capability::OperationContext::contract_rule_version`] 与证据版本不一致
//!   → 证据整份失效（`operation_rules_stale`），迫使重新识别；
//! * 这样「规则换代后旧 rule_version 证据不可复用」（验收③）在**生产入口**与
//!   **求值侧**各有一道锁。
//!
//! # `package_sha256` 语义（申报）
//!
//! 证据的包指纹 = 对**整个 `app.asar` 字节流**实算的 SHA-256（经同一个已打开
//! 句柄分块顺序读，不整包载入内存、不每块重开）。同 build 号重打包只要字节变化，
//! 指纹必变 → 旧证据不归因到新包（验收②）。
//!
//! **这是唯一权威的「当前包身份」**：`LocalHost.package_sha256` 是 identity 的
//! **描述符级快速身份**（Info.plist / app-update.yml 哈希），与内容指纹不是一层，
//! 不得用来填 `capability::OperationContext::current_package_sha256`。
//!
//! 夹具 README 申报的「真实包级 SHA-256 未采」指**未人工登记**某版本的期望值；
//! 本模块不依赖任何登记表，指纹由当前包现算，因此对未知 build 同样成立。
//! 读取顺序：**先校验目录头（预算/合法性），再算整包哈希**——畸形/超大头的包
//! 不会先付出整包读取代价（P2-2）。

use super::identity::{join_lexical, lexical_parent};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::error::{codes, DshError};
use super::model::{ContractEvidence, LocalHost, Operation, OperationContractMatch};

/// 契约规则版本。**规则一变（新增/收紧结构或语义要求）就 +1**：同一份包内容在
/// 不同规则版本下的结论不得互相当证据复用（`ContractEvidence.rule_version` 与
/// `capability::OperationContext::contract_rule_version` 两处比对）。
///
/// **版本政策**（C-4，T09 接线前）：仅**扫描器自身修正**且对官方包的识别结论
/// 不变时**不**升版（轮 3/轮 4 的 methods 区定位、注释感知、短读拒绝等修正即
/// 此类——版本保持 2）；**识别语义或规则内容**变化（片段增删/收紧/窗口形态
/// 调整）才 +1。
///
/// 版本史：1 = T08 首版；2 = 复审轮 3 Mi-1 收紧——CommandsList/CommandExecute
/// 补结果/参数形状锚，ModelSelect 的 `required` 逐面（additionalProperties 尾）
/// 锚定，SessionRename/SessionCreate 的结果面形状限定（多表面副本收口）；
/// 3 = P1-1 收紧——session/prompt 请求核心输入补类型级判据（requestId:string /
/// sessionId:string / content:array，均锚 prompt 方法窗），关闭「`required` 只锁
/// 字段名不锁类型」的核心输入形状漏检；核心输入漂移只关 Send（FileUpload/
/// ImageUpload 锚点与请求形状独立，不连坐）。
/// 4 = P1-1 复审 B-1——补文本载荷判据（`"text":{"type":"string"}` +
/// text part 的 `"required":["type","text"]`），关闭「`"const":"text"` 只锁判别值、
/// 不锁文本字段」的残留漏检。
/// 5 = P1-1 复审 Minor-1——再补 text part 判别属性自身的声明类型
/// （`"type":{"type":"string","const":"text"`），关闭「判别值在但判别属性类型
/// 漂移」的最后一处文本字段面漏检。
/// 6 = P1-1 复审（2026-11-17 用户裁决）——Send 规则收窄为**唯一实证契约**：
/// wire 参数是单一 `request` 对象（块内 `name: 'request'` + `mode: 'strict'`
/// 实锚）+ 明确受理结果（`"required":["accepted"]`）；无实证的 ACP-only 放行
/// 窗口组删除（真机 asar 双锚缺席，而发送器恒发 request 包装）——杜绝「能力
/// 门放行发送器不支持的契约」。每次发送的形状探测整链随之移除。
/// 7 = 销项复审 P2——新增缺席窗 `MethodNot`：Send 规则加负锚 `},{name:'`
/// （归一化后仅参数表第二参数项会出现该连接串），把「保留全部旧片段、
/// 增加第二个必填参数」的外扩漂移钉成失配（fail-closed）。
/// 8 = 销项复审二轮 P2——缺席/计数锚都有绕过面（连接串非唯一、占位符打断、
/// 固定阈值撞多面渲染：真机 prompt 块 3 面各一份参数表，`name:'` 总数=3）；
/// 改用**结构窗** `MethodParamTableSingleEntry`（字符串/注释感知，每份
/// parameters 数组恰一个条目，面数无关），参数表「恰一条目」成为硬判据。
/// 9 = 销项复审三轮 P2（**分面绑定**）——Send 规则的锚此前在整方法块内浮动，
/// 区域错位漏检（`accepted` 锚可被 request schema 里的诱饵满足、`mode:'strict'`
/// 可被块内别处满足）；新增分面区域窗 `Window::MethodRegion`，锚按面重排：
/// 参数表头面 [parameters:[..jsonSchema:)（name/strict）、请求 schema 面
/// [jsonSchema:..result:)（四字段 required + 类型级判据九锚）、受理结果面
/// [result:..块尾)（accepted 两锚）；参数表「恰一条目」结构窗保留。
pub const CONTRACT_RULE_VERSION: u32 = 10;

/// 契约读取预算（计划全局约束建议值：目录头 64 MiB、候选单文件 48 MiB、候选数 128）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractBudget {
    /// asar 目录 JSON 的最大字节数（真实包约 4.7 MiB；64 MiB 是畸形包防御上限）。
    pub package_header_bytes: u64,
    /// 单个候选描述符文件的最大字节数（真实最大 365 KB；48 MiB 是防御上限）。
    pub candidate_file_bytes: u64,
    /// 候选描述符文件数量上限的**防御值**。
    ///
    /// N-4 申报：候选来源按 [`Source`] 去重后只有 9 类，正常包不会触达 128；
    /// 只有当官方把同一后缀复制出大量副本（或目录被人为塞满同名文件）时才会
    /// 触发。保留 128 是「畸形包不得把读取变成全包扫描」的兜底，不是常规路径。
    pub candidate_files: usize,
}

impl ContractBudget {
    /// 计划建议预算（2026-10-29 TDD 计划「全局约束」）。
    pub const DEFAULT: Self = Self {
        package_header_bytes: 64 * 1024 * 1024,
        candidate_file_bytes: 48 * 1024 * 1024,
        candidate_files: 128,
    };
}

/// asar 目录树的最大嵌套深度（畸形包防御；真实包目录深度 < 10）。
pub const MAX_DIRECTORY_DEPTH: usize = 32;

/// 已打开的只读包句柄：`len` + `read_range`，**同一句柄顺序读**（不每块重开）。
///
/// `len` 故意不配 `is_empty`：包文件不可能合法为空（`read_header` 先按
/// `len < 16` 拒畸形），公开一个永不成立的伴随方法只是死代码。
#[allow(clippy::len_without_is_empty)]
pub trait ContractFileReader {
    /// 文件字节长度。
    fn len(&self) -> u64;
    /// 读取 `[offset, offset+len)`：**必须返回恰好 `len` 字节，或返回 Err**。
    ///
    /// 读契约（轮 4 N3 收紧）：短读 = 句柄契约被破坏，消费方不做「尽力而为」
    /// 拼凑——头区域按畸形头拒绝（`FRAME_INVALID`），整包哈希按 IO 错误拒绝
    /// （`STATE_IO`），候选文件记 `unresolved`。理由：静默短读会产出伪指纹 /
    /// 「更短但合法」的目录 JSON，让失败看起来像一份干净的全零匹配报告。
    fn read_range(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, DshError>;
}

/// 只读文件源（契约识别的唯一 IO 面）。**刻意没有写方法**——验收⑥的编译期边界：
/// 本模块拿不到「写」的能力，只读取证不可能产生业务写入。
///
/// 唯一能力是「打开一个只读句柄」：生产实现打开一次真实文件句柄并顺序读
/// （P2-2：不每块重开）；测试实现是内存字节 + 调用记录的假句柄。**没有**任何写/删除/
/// 重命名方法。
pub trait ContractFileSource: Send + Sync {
    /// 打开一个包文件（生产 = `std::fs::File`；测试 = 内存字节）。
    fn open(&self, path: &Path) -> Result<Box<dyn ContractFileReader + '_>, DshError>;
}

/// 生产文件源：`std::fs` 只读投影（单句柄 seek + read，不写任何文件）。
#[derive(Debug, Default, Clone, Copy)]
pub struct StdContractFiles;

impl ContractFileSource for StdContractFiles {
    /// 生产路径：**打开一次**，后续 header/哈希/候选全走同一句柄（P2-2）。
    fn open(&self, path: &Path) -> Result<Box<dyn ContractFileReader + '_>, DshError> {
        let file = std::fs::File::open(path).map_err(|e| {
            DshError::new(
                codes::STATE_IO,
                format!("contract: open {}: {e}", path.display()),
            )
        })?;
        let len = file
            .metadata()
            .map_err(|e| {
                DshError::new(
                    codes::STATE_IO,
                    format!("contract: stat {}: {e}", path.display()),
                )
            })?
            .len();
        Ok(Box::new(StdFileReader { file, len }))
    }
}

/// `std::fs::File` 的只读句柄包装。
struct StdFileReader {
    file: std::fs::File,
    len: u64,
}

impl ContractFileReader for StdFileReader {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_range(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, DshError> {
        use std::io::{Read, Seek, SeekFrom};
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| DshError::new(codes::STATE_IO, format!("contract: seek: {e}")))?;
        let mut buffer = vec![0u8; usize::try_from(len).unwrap_or(usize::MAX)];
        self.file
            .read_exact(&mut buffer)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("contract: read: {e}")))?;
        Ok(buffer)
    }
}

/// 契约读取报告：证据 + 「暂不可识别」的逐操作事实。
///
/// 两类「暂不可识别」**分开记账**（P2-3）：`inconclusive` = 预算受限（计划⑤），
/// `unresolved` = 来源读取失败或多义副本（IO/歧义）。二者在能力求值里都映射为
/// `operation_inconclusive`（不误标永久不支持），但报告保留区别供诊断与退避策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractReadReport {
    /// 逐操作匹配（未命中的操作不在此表）。
    pub evidence: ContractEvidence,
    /// 因**预算受限**而暂不可识别的操作。
    pub inconclusive: BTreeSet<Operation>,
    /// 因**来源读取失败或副本多义**而暂不可识别的操作。
    pub unresolved: BTreeSet<Operation>,
    /// 本次读取是否触达任一层预算上限（诊断用；`unresolved` 不计入）。
    pub budget_limited: bool,
}

impl ContractReadReport {
    /// 两类「暂不可识别」的并集（能力求值的输入）。
    pub fn undecided(&self) -> BTreeSet<Operation> {
        self.inconclusive.union(&self.unresolved).copied().collect()
    }
}

// ============================================================
// 生产入口（§2.3 签名固定 + 能力求值入口）
// ============================================================

/// 计划 §2.3 的固定签名：只投影**可持久化证据**。
///
/// **警告（P1-1 裁决）**：本入口**丢弃**预算受限/来源不可读的逐操作事实——
/// 它只适合「把证据落账」，**不得**用于能力求值。能力求值必须调 [`read_report`]
/// （或 [`read_report_with`]），否则验收⑤在真实装配上不成立（目录头超预算会得到
/// 空匹配表 → 全部误标「契约未匹配」）。
///
/// **阻塞 IO**：调用方必须 `spawn_blocking`，不得在 Tauri 主线程/3 秒轮询路径调用。
pub fn read(target: &LocalHost, rules: u32) -> Result<ContractEvidence, DshError> {
    read_with(target, rules, &StdContractFiles)
}

/// 注入文件源的伴随入口（测试专用；[`read`] 委托它）。
pub fn read_with(
    target: &LocalHost,
    rules: u32,
    files: &dyn ContractFileSource,
) -> Result<ContractEvidence, DshError> {
    read_report_with(target, rules, files, &ContractBudget::DEFAULT).map(|report| report.evidence)
}

/// **能力求值的生产入口**（P1-1）：保留预算/不确诊事实，交给 `capability::evaluate`。
///
/// **阻塞 IO**：调用方必须 `spawn_blocking`，不得在 Tauri 主线程/3 秒轮询路径调用。
pub fn read_report(target: &LocalHost, rules: u32) -> Result<ContractReadReport, DshError> {
    read_report_with(target, rules, &StdContractFiles, &ContractBudget::DEFAULT)
}

/// 带预算/报告的注入入口（生产 [`read_report`] 与预算验收共用同一路径）。
pub fn read_report_with(
    target: &LocalHost,
    rules: u32,
    files: &dyn ContractFileSource,
    budget: &ContractBudget,
) -> Result<ContractReadReport, DshError> {
    if rules != CONTRACT_RULE_VERSION {
        return Err(DshError::new(
            codes::CONTRACT_RULES_MISMATCH,
            format!(
                "contract: requested rules {rules}, current rule table {}",
                CONTRACT_RULE_VERSION
            ),
        ));
    }
    let package = package_path(target)?;
    let mut reader = files.open(&package)?;

    // 1) 目录头：**先于整包哈希**校验预算/合法性（P2-2）。
    let header = match read_header(&mut *reader, budget) {
        Ok(header) => header,
        Err(HeaderOutcome::OverBudget) => {
            // 未算哈希：证据指纹留空（此报告只允许用于 inconclusive 判定）。
            return Ok(ContractReadReport {
                evidence: ContractEvidence {
                    package_sha256: String::new(),
                    rule_version: CONTRACT_RULE_VERSION,
                    matches: BTreeMap::new(),
                },
                inconclusive: Operation::ALL.into_iter().collect(),
                unresolved: BTreeSet::new(),
                budget_limited: true,
            });
        }
        Err(HeaderOutcome::Invalid(detail)) => {
            return Err(DshError::new(codes::FRAME_INVALID, detail));
        }
        Err(HeaderOutcome::Io(error)) => return Err(error),
    };

    // 2) 整包内容指纹（同一句柄顺序读）。
    let package_sha256 = hash_package(&mut *reader)?;

    // 3) 候选来源：按已知后缀收集（可能多副本）；超预算/读失败/副本多义分别记账。
    let mut candidates: BTreeMap<Source, Vec<PathBuf>> = BTreeMap::new();
    for entry in &header.entries {
        if let Some(kind) = Source::from_entry_path(entry) {
            candidates.entry(kind).or_default().push(entry.clone());
        }
    }
    let mut available: BTreeMap<Source, String> = BTreeMap::new();
    // N6（轮 4）：方法块扫描需要**原始文本**（换行/注释原文）。归一化文本自
    // C-1（T09 接线前）起只剩 N-10 副本一致性比对一个用途——窗口匹配走
    // `code_sources`（剥注释文本）。
    let mut raw_sources: BTreeMap<Source, String> = BTreeMap::new();
    let mut over_budget: BTreeSet<Source> = BTreeSet::new();
    let mut broken: BTreeSet<Source> = BTreeSet::new();
    let mut budget_limited = false;
    let mut examined = 0usize;
    for (kind, paths) in &candidates {
        for path in paths {
            examined += 1;
            if examined > budget.candidate_files {
                over_budget.insert(*kind);
                budget_limited = true;
                break;
            }
            let Some((offset, size)) = header.locate(path) else {
                broken.insert(*kind);
                continue;
            };
            if size > budget.candidate_file_bytes {
                over_budget.insert(*kind);
                budget_limited = true;
                continue;
            }
            match reader.read_range(offset, size) {
                Ok(bytes) if bytes.len() as u64 == size => {
                    let raw = String::from_utf8_lossy(&bytes).into_owned();
                    let text = normalize(&raw);
                    match available.get(kind) {
                        // N-10：同后缀多副本必须内容一致，否则该来源多义。
                        Some(existing) if *existing != text => {
                            broken.insert(*kind);
                        }
                        // 销项复审 P2-5：normalized 相等但 **raw 不等** → raw 结构
                        // 分歧被藏（双引号串 vs 模板串边界把真假第二参数抹平）——
                        // 与块级歧义（MethodIndex 的「归一化+raw 都相等才算等价」）
                        // 同向：该来源多义记 unresolved，不取首文件命中。
                        Some(_) if raw_sources.get(kind).is_some_and(|stored| *stored != raw) => {
                            broken.insert(*kind);
                        }
                        Some(_) => {}
                        None => {
                            available.insert(*kind, text);
                            raw_sources.insert(*kind, raw);
                        }
                    }
                }
                // Mi-4（轮 3）：短读 = 句柄契约被破坏——把截断文本当来源内容会让
                // 「读到了什么」变成薛定谔的。与读失败同账（broken → unresolved）。
                Ok(_) => {
                    broken.insert(*kind);
                }
                // P2-3：读取失败 ≠ 预算受限（记 unresolved，不置 budget_limited）。
                Err(_) => {
                    broken.insert(*kind);
                }
            }
        }
    }

    // 4) 逐操作匹配：片段必须落在**目标方法自己的块**（或声明的辅助窗口）内。
    let (index, ambiguous_sources) = MethodIndex::new(&raw_sources);
    // Major-2（轮 3）：methods 区内同键分歧副本 → 来源多义，与 N-10 文件副本
    // 歧义同账（broken → unresolved）：宁可整源暂不可识别，不猜哪块是真。
    broken.extend(ambiguous_sources.iter().copied());
    // C-1（T09 接线前）：Whole/Between 窗口的匹配文本 = 原始文本**剥注释**后归一化
    // （注释里的旧片段不承载契约；字符串内容保留）。
    let code_sources: BTreeMap<Source, String> = raw_sources
        .iter()
        .map(|(source, raw)| (*source, normalize(&strip_comments(raw))))
        .collect();
    let mut matches = BTreeMap::new();
    let mut inconclusive = BTreeSet::new();
    let mut unresolved = BTreeSet::new();
    for rule in RULES {
        if rule
            .windows
            .iter()
            .any(|(window, _)| over_budget.contains(&window.source()))
        {
            inconclusive.insert(rule.operation);
            continue;
        }
        if rule
            .windows
            .iter()
            .any(|(window, _)| broken.contains(&window.source()))
        {
            unresolved.insert(rule.operation);
            continue;
        }
        if !rule_matches(rule, &code_sources, &index) {
            continue;
        }
        matches.insert(
            rule.operation,
            OperationContractMatch {
                method: rule.method.to_string(),
                structural_rule: rule.structural_rule.to_string(),
                semantic_rule: rule.semantic_rule.to_string(),
            },
        );
    }

    Ok(ContractReadReport {
        evidence: ContractEvidence {
            package_sha256,
            rule_version: CONTRACT_RULE_VERSION,
            matches,
        },
        inconclusive,
        unresolved,
        budget_limited,
    })
}

// ============================================================
// 包定位与 asar 读取
// ============================================================

/// 官方安装包内 `app.asar` 的绝对路径（由**已核验的**可执行文件词法推导）。
///
/// * macos：`<bundle>/Contents/MacOS/<name>` → `<bundle>/Contents/Resources/app.asar`；
/// * windows：`<安装目录>/DeepSeek Harness.exe` → `<安装目录>/resources/app.asar`。
///
/// 路径形态来自 E2/E3 与 Mac 报告实测；不做任何盘符/端口常量假设。
/// Windows 分支是**纯词法**推导（`windows_package_path_is_lexically_derived` 单测），
/// 不代表 Windows 已实机验证（T08 不启用 Windows 平台）。
pub fn package_path(target: &LocalHost) -> Result<PathBuf, DshError> {
    let executable = target.executable.to_string_lossy().to_string();
    let platform = target.platform.to_ascii_lowercase();
    match platform.as_str() {
        "macos" => {
            let macos_dir = lexical_parent(&executable).ok_or_else(|| {
                DshError::new(
                    codes::UNSUPPORTED,
                    "contract: macos executable has no Contents/MacOS parent",
                )
            })?;
            let contents = lexical_parent(&macos_dir).ok_or_else(|| {
                DshError::new(
                    codes::UNSUPPORTED,
                    "contract: macos executable has no Contents parent",
                )
            })?;
            Ok(join_lexical(&contents, &["Resources", "app.asar"]))
        }
        "windows" => {
            let root = lexical_parent(&executable).ok_or_else(|| {
                DshError::new(
                    codes::UNSUPPORTED,
                    "contract: windows executable has no install root",
                )
            })?;
            Ok(join_lexical(&root, &["resources", "app.asar"]))
        }
        other => Err(DshError::new(
            codes::UNSUPPORTED,
            format!("contract: unsupported platform {other}"),
        )),
    }
}

/// asar 目录解析产物：条目名 → (绝对 offset, size)。
struct AsarHeader {
    entries: Vec<PathBuf>,
    table: BTreeMap<PathBuf, (u64, u64)>,
}

impl AsarHeader {
    fn locate(&self, path: &Path) -> Option<(u64, u64)> {
        self.table.get(path).copied()
    }
}

enum HeaderOutcome {
    OverBudget,
    Invalid(String),
    /// 读取/定位失败（保留原始 `STATE_IO` 码，不被改写成帧错误——P2-3）。
    Io(DshError),
}

const ASAR_PREFIX_BYTES: u64 = 16;

fn read_header(
    reader: &mut dyn ContractFileReader,
    budget: &ContractBudget,
) -> Result<AsarHeader, HeaderOutcome> {
    let len = reader.len();
    if len < ASAR_PREFIX_BYTES {
        return Err(HeaderOutcome::Invalid(
            "asar too short for prefix".to_string(),
        ));
    }
    let prefix = reader
        .read_range(0, ASAR_PREFIX_BYTES)
        .map_err(HeaderOutcome::Io)?;
    // N-C（轮 4 修订）：trait 契约 = 「恰好 len 字节或 Err」；短读按畸形头拒绝
    // （不 panic、不对短切片做偏移索引），头区域内不做尽力拼凑。
    if prefix.len() < ASAR_PREFIX_BYTES as usize {
        return Err(HeaderOutcome::Invalid(format!(
            "asar prefix truncated: {} of {} bytes",
            prefix.len(),
            ASAR_PREFIX_BYTES
        )));
    }
    let word = |at: usize| -> u64 {
        u32::from_le_bytes([prefix[at], prefix[at + 1], prefix[at + 2], prefix[at + 3]]) as u64
    };
    if word(0) != 4 {
        return Err(HeaderOutcome::Invalid(format!(
            "unexpected asar header prefix: {}",
            word(0)
        )));
    }
    let header_size = word(4);
    let json_size = word(12);
    if header_size < 8 || json_size == 0 {
        return Err(HeaderOutcome::Invalid(
            "asar header sizes out of range".to_string(),
        ));
    }
    if json_size > budget.package_header_bytes {
        return Err(HeaderOutcome::OverBudget);
    }
    let data_start = 8 + header_size;
    if ASAR_PREFIX_BYTES + json_size > data_start || data_start > len {
        return Err(HeaderOutcome::Invalid(
            "asar header overruns package".to_string(),
        ));
    }
    let json = reader
        .read_range(ASAR_PREFIX_BYTES, json_size)
        .map_err(HeaderOutcome::Io)?;
    // N3（轮 4）：头 JSON 短读 = 句柄契约被破坏——注入 reader 可以返回**更短但
    // 合法**的 JSON（如 `{"files":{}}`），产出「干净全零匹配报告 + 非空指纹」，
    // 与漂移解除逻辑组合会清空全部漂移结论。按畸形头拒绝（与 16 字节前缀短读
    // 同账：头区域内的契约破坏归 Invalid，数据区走 Err/broken——申报）。
    if json.len() as u64 != json_size {
        return Err(HeaderOutcome::Invalid(format!(
            "asar header json truncated: {} of {json_size} bytes",
            json.len()
        )));
    }
    let directory: serde_json::Value = serde_json::from_slice(&json)
        .map_err(|e| HeaderOutcome::Invalid(format!("asar directory is not JSON: {e}")))?;
    let mut entries = Vec::new();
    let mut table = BTreeMap::new();
    collect_entries(&directory, "", data_start, 0, &mut entries, &mut table)
        .map_err(HeaderOutcome::Invalid)?;
    Ok(AsarHeader { entries, table })
}

/// 只核对桌面包内 CLI 的结构；这不是发行方签名验证。
pub(crate) fn bundled_cli_archive_fingerprint(path: &Path) -> Option<String> {
    let before = std::fs::metadata(path).ok()?;
    let mut reader = StdContractFiles.open(path).ok()?;
    if !bundled_cli_archive_matches(reader.as_mut()) {
        return None;
    }
    let digest = hash_package(reader.as_mut()).ok()?;
    let after = std::fs::metadata(path).ok()?;
    if before.len() != after.len() || before.modified().ok()? != after.modified().ok()? {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return None;
        }
    }
    Some(digest)
}

fn bundled_cli_archive_matches(reader: &mut dyn ContractFileReader) -> bool {
    let Ok(header) = read_header(reader, &ContractBudget::DEFAULT) else {
        return false;
    };
    let cli_range = header.locate(Path::new(
        "/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/cli.js",
    ));
    let bin_range = header.locate(Path::new("/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js"));
    match (cli_range, bin_range) {
        (Some((a, alen)), Some((b, blen)))
            if a.checked_add(alen).is_some_and(|end| end <= b)
                || b.checked_add(blen).is_some_and(|end| end <= a) => {}
        _ => return false,
    }
    let mut read_entry = |name: &str| -> Option<Vec<u8>> {
        let (offset, size) = header.locate(Path::new(name))?;
        if size == 0 || size > 2 * 1024 * 1024 || offset.checked_add(size)? > reader.len() {
            return None;
        }
        let bytes = reader.read_range(offset, size).ok()?;
        (bytes.len() as u64 == size).then_some(bytes)
    };
    let Some(desktop) = read_entry("/dsh/node_modules/@deepseek-ai/dsh-desktop-host/package.json")
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        return false;
    };
    let Some(dsh) = read_entry("/dsh/node_modules/@deepseek-ai/dsh/package.json")
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        return false;
    };
    if desktop.get("name").and_then(|v| v.as_str()) != Some("@deepseek-ai/dsh-desktop-host")
        || dsh.get("name").and_then(|v| v.as_str()) != Some("@deepseek-ai/dsh")
        || dsh
            .get("bin")
            .and_then(|v| v.get("dsh"))
            .and_then(|v| v.as_str())
            != Some("lib/bin.js")
    {
        return false;
    }
    let Some(cli) = read_entry("/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/cli.js") else {
        return false;
    };
    let Some(bin) = read_entry("/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js") else {
        return false;
    };
    !bin.is_empty()
        && String::from_utf8(cli).is_ok_and(|text| text.contains("@deepseek-ai/dsh/lib/bin.js"))
}

/// 递归收集普通文件条目；**深度上限** [`MAX_DIRECTORY_DEPTH`]（N-5：畸形包防御）。
fn collect_entries(
    node: &serde_json::Value,
    prefix: &str,
    data_start: u64,
    depth: usize,
    entries: &mut Vec<PathBuf>,
    table: &mut BTreeMap<PathBuf, (u64, u64)>,
) -> Result<(), String> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err(format!(
            "asar directory nesting exceeds {MAX_DIRECTORY_DEPTH} levels"
        ));
    }
    let Some(files) = node.get("files").and_then(|value| value.as_object()) else {
        return Ok(());
    };
    for (name, meta) in files {
        let path = format!("{prefix}/{name}");
        if meta.get("files").is_some() {
            collect_entries(meta, &path, data_start, depth + 1, entries, table)?;
            continue;
        }
        let size = meta
            .get("size")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        // 真实 asar 的 offset 是字符串（如 "0"）；兼容数字形态。
        let offset = meta
            .get("offset")
            .and_then(|value| {
                value
                    .as_str()
                    .and_then(|text| text.parse::<u64>().ok())
                    .or_else(|| value.as_u64())
            })
            .unwrap_or(0);
        let unpacked = meta
            .get("unpacked")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        // unpacked 条目不在 asar 数据区（另有伴随目录），本模块不读它。
        if unpacked {
            continue;
        }
        let absolute = PathBuf::from(&path);
        entries.push(absolute.clone());
        table.insert(absolute, (data_start.saturating_add(offset), size));
    }
    Ok(())
}

/// 整包内容 SHA-256（同一句柄分块顺序读；只经只读接口）。
fn hash_package(reader: &mut dyn ContractFileReader) -> Result<String, DshError> {
    const CHUNK: u64 = 1024 * 1024;
    let len = reader.len();
    let mut hasher = Sha256::new();
    let mut offset = 0;
    while offset < len {
        let take = CHUNK.min(len - offset);
        let bytes = reader.read_range(offset, take)?;
        // Mi-4（轮 3）：短读 = 句柄契约被破坏。静默截断会产出「伪指纹」——一份
        // 看起来成功的整包哈希，实际没有覆盖全包（验收②的重打包归因会被它骗过）。
        if bytes.len() as u64 != take {
            return Err(DshError::new(
                codes::STATE_IO,
                format!(
                    "contract: short read during package hash at offset {offset}: {} of {take} bytes",
                    bytes.len()
                ),
            ));
        }
        hasher.update(&bytes);
        offset += take;
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

// ============================================================
// 候选来源（官方包内真实路径；仅这 9 个文件参与识别）
// ============================================================

/// 契约来源（官方包内生成描述符/协议声明文件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// `@deepseek-ai/dsh-api-gateway/lib/types/stream-protocol.js`（`$events/result` 信封）。
    StreamProtocol,
    /// `@deepseek-ai/dsh-api-remotes/lib/types/remote-events.js`（waterfall 转发白名单）。
    RemoteEvents,
    /// `@deepseek-ai/dsh-api-session-controller/lib/typert.host.js`（会话域方法描述符）。
    SessionController,
    /// `@deepseek-ai/dsh-api-workspace-controller/lib/typert.host.js`（工作区/归档/置顶）。
    WorkspaceController,
    /// `@deepseek-ai/dsh-client-file-upload/lib/types/http-route.js`（原始字节上传路由）。
    UploadRoute,
    /// `@deepseek-ai/dsh-client-file-upload/lib/types/protocol.js`（上传路径常量）。
    UploadProtocol,
    /// `@deepseek-ai/dsh-commands/lib/typert.host.js`（slash 命令）。
    Commands,
    /// `@deepseek-ai/dsh-user-approval/lib/types/index.js`（审批取值词表）。
    ApprovalTypes,
    /// `@deepseek-ai/dsh-user-questions/lib/typert.host.js`（问答）。
    UserQuestions,
}

impl Source {
    /// 包内路径后缀（2026-10-30 在本机 0.2.1-rc.3 `app.asar` 实读存在）。
    fn suffix(self) -> &'static str {
        match self {
            Source::StreamProtocol => "dsh-api-gateway/lib/types/stream-protocol.js",
            Source::RemoteEvents => "dsh-api-remotes/lib/types/remote-events.js",
            Source::SessionController => "dsh-api-session-controller/lib/typert.host.js",
            Source::WorkspaceController => "dsh-api-workspace-controller/lib/typert.host.js",
            Source::UploadRoute => "dsh-client-file-upload/lib/types/http-route.js",
            Source::UploadProtocol => "dsh-client-file-upload/lib/types/protocol.js",
            Source::Commands => "dsh-commands/lib/typert.host.js",
            Source::ApprovalTypes => "dsh-user-approval/lib/types/index.js",
            Source::UserQuestions => "dsh-user-questions/lib/typert.host.js",
        }
    }

    fn from_entry_path(path: &Path) -> Option<Self> {
        let text = path.to_string_lossy();
        Source::ALL
            .into_iter()
            .find(|source| text.ends_with(source.suffix()))
    }

    /// 全部来源（稳定顺序）。
    pub const ALL: [Source; 9] = [
        Source::StreamProtocol,
        Source::RemoteEvents,
        Source::SessionController,
        Source::WorkspaceController,
        Source::UploadRoute,
        Source::UploadProtocol,
        Source::Commands,
        Source::ApprovalTypes,
        Source::UserQuestions,
    ];
}

// ============================================================
// 方法切块与匹配窗口（B-1 的核心）
// ============================================================

/// 匹配窗口：规则片段必须落在窗口内，**不得跨窗口借用**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// 目标方法自己的 descriptor 块（从 `'ns/method':{` 锚点到其配对 `}`；锚点
    /// 只在 `methods:` 对象体内取，见 [`method_blocks`]）。
    Method(Source, &'static str),
    /// 方法块内的**分面区域窗**（销项复审三轮 P2「分面绑定」）：片段必须落在
    /// 块内 [start 锚..end 锚) 区间（normalize 后文本上查找，与 [`Window::Method`]
    /// 同一匹配面）。锚在整方法块内浮动时，「request 区的诱饵」能冒充结果面、
    /// 「块内别处的散锚」能冒充参数面——区域错位漏检；分面绑定把每类锚钉死在
    /// 它自己的面里。匹配语义见 [`rule_matches`] 的分组判定：
    /// * start 锚出现点依次为**互不重叠的连续段界**（[s1..s2)、[s2..s3)、…、
    ///   [sk..end)）——真机包多面渲染（codec/strictCodec 各一份参数表+result）
    ///   各面各占一段，任一段满足即算该窗命中（与
    ///   [`Window::MethodParamTableSingleEntry`] 的逐表口径一致）；
    /// * **同窗全部片段必须落在同一段内**（跨段拼接不算——P2-4：旧「每个
    ///   start 划到 end 锚」的区间互相重叠，第一面的区间吞掉第二面，拼接锚
    ///   能在重叠区凑数命中）；
    /// * start 锚一处都没有 → fail-closed；段右界 = min(下一 start 锚, 首个
    ///   end 锚)；末段 end 锚(Some)找不到 → 该段划不出来；end=None → 块尾。
    MethodRegion {
        source: Source,
        method: &'static str,
        start: &'static str,
        end: Option<&'static str>,
    },
    /// 目标方法块的**参数表结构窗**：块内每个 `parameters:[…]` 数组都必须
    /// **恰好一个条目**（字符串/注释感知地数深度一层的 `{` 组）。真机包是
    /// 多面渲染（codec/strictCodec/…每面一份参数表），面数随上游版本可变，
    /// 故判据是「每表恰一条目」而非固定总数——上游给参数表增加第二个必填
    /// 参数（任何字段顺序/任何拼写/参数间有注释）即失配（销项复审 P2 及二
    /// 轮 P2：外扩漂移的结构化钉法，正锚对外扩天然免疫）。块内没有任何
    /// parameters 数组 → fail-closed（正锚 `name:'request'` 也会失配，双重
    /// 兜底）。
    MethodParamTableSingleEntry(Source, &'static str),
    /// 两字面量之间的声明区（用于 schema 声明段，如 plan-review intent）。
    Between(Source, &'static str, &'static str),
    /// 整个来源文件（单用途协议文件：上传路由/审批取值/事件白名单/事件结果信封）。
    Whole(Source),
}

impl Window {
    fn source(&self) -> Source {
        match self {
            Window::Method(source, _)
            | Window::MethodParamTableSingleEntry(source, _)
            | Window::MethodRegion { source, .. }
            | Window::Between(source, _, _)
            | Window::Whole(source) => *source,
        }
    }
}

/// 方法块索引：来源 → (方法名 → 块文本)。
///
/// 切块规则（复审轮 3 Major-2）：只在 `methods:` 对象体内取 `'ns/method':{`
/// 锚点（见 [`method_blocks`]）。同一方法键在区内出现多块时：内容一致取其一
/// （等价副本）；**内容分歧 → 该来源多义**（照 N-10 副本歧义纪律：宁可整源
/// 暂不可识别，不猜哪块是真 descriptor），由调用方记入 `unresolved`。
struct MethodIndex {
    /// 归一化块（片段匹配面）。
    blocks: BTreeMap<Source, BTreeMap<String, String>>,
    /// 原始块（结构扫描面——保留真实引号/模板串边界，见
    /// `method_block_param_tables_single_entry`）。
    raw_blocks: BTreeMap<Source, BTreeMap<String, String>>,
}

impl MethodIndex {
    /// 建索引；返回「同键多块且内容分歧」的来源集合（多义 → 不参与匹配）。
    ///
    /// 输入 = **原始文本**表（轮 4 N6：块扫描需要换行与注释原文；块文本由
    /// [`method_blocks`] 内部剥注释 + 归一化）。
    fn new(raw_sources: &BTreeMap<Source, String>) -> (Self, BTreeSet<Source>) {
        let mut blocks = BTreeMap::new();
        let mut raw_blocks = BTreeMap::new();
        let mut ambiguous = BTreeSet::new();
        for (source, text) in raw_sources {
            let mut per_method: BTreeMap<String, String> = BTreeMap::new();
            let mut per_method_raw: BTreeMap<String, String> = BTreeMap::new();
            for (name, raw, body) in method_blocks(text) {
                match per_method.get(&name) {
                    None => {
                        per_method.insert(name.clone(), body);
                        per_method_raw.insert(name, raw);
                    }
                    // 等价副本取其一：归一化与 **raw 都相等**才算等价——归一化
                    // 相等不证明 raw 结构一致（反引号折叠可把模板串边界伪造成
                    // 双引号串），而结构窗吃 raw——取错副本会漏检（三轮 P2-2）。
                    // 任一不等 → 该来源多义（整源暂不可识别进 unresolved）。
                    Some(existing)
                        if *existing == body && per_method_raw.get(&name) == Some(&raw) => {}
                    Some(_) => {
                        ambiguous.insert(*source);
                    }
                }
            }
            blocks.insert(*source, per_method);
            raw_blocks.insert(*source, per_method_raw);
        }
        (Self { blocks, raw_blocks }, ambiguous)
    }

    fn get(&self, source: Source, method: &str) -> Option<&str> {
        self.blocks
            .get(&source)
            .and_then(|per_method| per_method.get(method))
            .map(String::as_str)
    }

    /// 原始块（未归一化；结构扫描专用——真实引号/模板串边界保留）。
    fn get_raw(&self, source: Source, method: &str) -> Option<&str> {
        self.raw_blocks
            .get(&source)
            .and_then(|per_method| per_method.get(method))
            .map(String::as_str)
    }
}

/// 定位**原始文本**（未归一化）里 `methods:{...}` 对象体（`{` 之后到配对 `}`
/// 之前的半开区间）。
///
/// **为什么在原始文本上定位**（轮 4 N6）：`//` 行注释依赖换行符定界，归一化把
/// 换行删掉了——在归一化文本上定位会把 `//` 之后吞到文件尾（合法包被误杀）。
/// 原始文本保留换行，行注释语义成立。
///
/// 定位扫描自身是**字符串/注释感知**的（`'…'`/`"…"`/`` `…` `` 字符串含 `\` 转义、
/// `//` 行注释、`/* */` 块注释都被跳过）：注释/字符串里出现的 `methods:` 不是
/// 区域起点。保守边界：定位途中遇到**未闭合**字符串/注释 → 返回 `None`（后续
/// 字节无法可靠定位）。区域的配对 `}` 由同一状态机求出；不配对 → `None`。
///
/// **为什么限定 methods 区**（轮 3 Major-2，理由经轮 4 N7 更正）：真实包尾区
/// 存在**双引号字符串内含反引号**的构造（如
/// "Host service backing the generated \`ctx.remote.session\` namespace."）——
/// 归一化把反引号统一映射成双引号后，该串内部出现「未转义引号」，在归一化文本
/// 上做全文件前向词法必然在此失配（轮 3 曾误述为「模板串内含未转义双引号」，
/// 实测构造见上，如实更正）。轮 4 起定位/块抽取改在**原始文本**上做（反引号按
/// 字符串处理，该构造不再致失配），区域限定仍保留：把 methods 区之外的伪锚点
/// （JSDoc/声明串/尾区模型区副本）整体排除出锚点面。
///
/// **属性键前界**（轮 4 N2）：`methods` 必须处于属性键位置（文件首/`{`/`,`/`(` /
/// `=` 之后，跳过空白；无 `return` 回退，理由见 `is_property_key_position`）——
/// `submethods:` 等更长标识符里的同形子串不是区域起点，前置伪区因此无法遮蔽真
/// methods 区。
fn methods_region(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                let mut closed = false;
                while index + 1 < bytes.len() {
                    if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                        closed = true;
                        index += 2;
                        break;
                    }
                    index += 1;
                }
                if !closed {
                    return None;
                }
            }
            b'\'' | b'"' | b'`' => {
                let Some(close) = string_close(bytes, index) else {
                    return None; // 未闭合字符串：后续定位不可靠
                };
                index = close + 1;
            }
            _ => {
                // 字节比较（不用 str 切片：index 可能落在多字节字符中间）。
                if bytes[index..].starts_with(b"methods") && is_property_key_position(bytes, index)
                {
                    if let Some(colon) = skip_ws_then_expect(bytes, index + "methods".len(), b':') {
                        if let Some(brace) = skip_ws_then_expect(bytes, colon + 1, b'{') {
                            let body_start = brace + 1;
                            let body_end = matching_brace_end(text, body_start)?;
                            return Some((body_start, body_end));
                        }
                    }
                }
                index += 1;
            }
        }
    }
    None
}

/// `index` 是否处于属性键位置：文件首，或前一个非空白字节是 `{`/`,`/`(`/`=`（闭集）。
/// `submethods` 里 `methods` 的前一字节是标识符字符 → 否。
fn is_property_key_position(bytes: &[u8], index: usize) -> bool {
    let mut cursor = index;
    while cursor > 0 && bytes[cursor - 1].is_ascii_whitespace() {
        cursor -= 1;
    }
    if cursor == 0 {
        return true;
    }
    // C-2（T09 接线前）：不设 `return` 回退分支——合法 JS 的 `return { methods: … }`
    // 已被 `{` 分支覆盖；紧邻比对 `return` 会放行 `returnmethods:` 这类**合法键**
    // （其 `methods:` 子串的前一字节是标识符字符）。
    matches!(bytes[cursor - 1], b'{' | b',' | b'(' | b'=')
}

/// 从 `at` 起跳过空白；恰好遇到 `expected` → 返回其下标。
fn skip_ws_then_expect(bytes: &[u8], mut at: usize, expected: u8) -> Option<usize> {
    while at < bytes.len() && bytes[at].is_ascii_whitespace() {
        at += 1;
    }
    if at < bytes.len() && bytes[at] == expected {
        Some(at)
    } else {
        None
    }
}

/// 从 `open`（处于代码态的引号字节）起找配对闭引号的下标；含 `\` 转义。
fn string_close(bytes: &[u8], open: usize) -> Option<usize> {
    let quote = bytes[open];
    let mut close = open + 1;
    while close < bytes.len() {
        if bytes[close] == b'\\' {
            close += 2;
            continue;
        }
        if bytes[close] == quote {
            return Some(close);
        }
        close += 1;
    }
    None
}

/// `'name'` 闭引号之后（跳空白）是否为 `:{`；返回 `{` 之后的下标。
fn anchor_body_start(bytes: &[u8], after_close: usize) -> Option<usize> {
    let colon = skip_ws_then_expect(bytes, after_close, b':')?;
    let brace = skip_ws_then_expect(bytes, colon + 1, b'{')?;
    Some(brace + 1)
}

/// 剥离代码态注释（字符串感知；轮 4 N4）：`//` 到行尾、`/* */` 到闭合，各以
/// `\u{1}` 占位（归一化不消掉的不可见字节，杜绝跨注释边界拼出假片段）；
/// 字符串字面量（`'…'`/`"…"`/`` `…` ``，含 `\` 转义）内容**原样保留**——字符串
/// 承载片段仍可达，为已申报边界。未闭合字符串/块注释：余下内容按注释处理到尾。
fn strip_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                out.push(0x01);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() {
                    if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
                out.push(0x01);
            }
            b'\'' | b'"' | b'`' => match string_close(bytes, index) {
                Some(close) => {
                    out.extend_from_slice(&bytes[index..=close]);
                    index = close + 1;
                }
                None => {
                    out.push(0x01);
                    index = bytes.len();
                }
            },
            _ => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 在 `methods:` 对象体内按 `'ns/method':{` 切块（返回 (方法名, 块文本)）。
///
/// **输入是原始文本**（未归一化；轮 4 N6——见 [`methods_region`]）。块文本 =
/// [`strip_comments`] 之后 [`normalize`]（去空白、反引号→双引号），供片段匹配。
///
/// **锚点只在 `methods:` 对象体内、代码态出现**（复审轮 3 Major-2，与配对 `}`
/// 扫描同一套字符串/注释感知状态机）：方法名出现在 methods 区之前（前置文档
/// 注释、声明串、字符串字面量）或之后（`export const TYPERT` 尾区/模型区）都
/// 不构成锚点——伪锚点因此无法遮蔽真 descriptor（B-1 同级假阳性的根源）。
/// 锚点形态 `'ns/method'` 后**跳空白**允许 `: {`（原始文本含空白）。
///
/// **块文本剥离注释**（轮 4 N4）：descriptor 块内的注释（含旧版本残留片段）不
/// 承载契约片段；字符串字面量内容保留（字符串承载片段仍可达 = 已申报边界）。
///
/// 块体 = descriptor 对象的**配对 `}`**（[`matching_brace_end`]：字符串/注释内
/// 的花括号不参与配对）。块内括号不配对（畸形）→ 停止扫描本区（不吞尾），已
/// 收块的锚点保留，交由调用方判未匹配。
fn method_blocks(text: &str) -> Vec<(String, String, String)> {
    let Some((body_start, body_end)) = methods_region(text) else {
        return Vec::new();
    };
    let region = &text[body_start..body_end];
    let bytes = region.as_bytes();
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                let mut closed = false;
                while index + 1 < bytes.len() {
                    if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                        closed = true;
                        index += 2;
                        break;
                    }
                    index += 1;
                }
                if !closed {
                    return out;
                }
            }
            b'\'' | b'"' | b'`' => {
                let open = index;
                let Some(close) = string_close(bytes, index) else {
                    // 未闭合字符串：余下内容不可靠，停止扫描（不吞尾）。
                    return out;
                };
                // 代码态单引号字符串字面量后（跳空白）紧跟 `:{` → 方法锚点。
                if bytes[open] == b'\'' {
                    if let Some(body) = anchor_body_start(bytes, close + 1) {
                        let name = &region[open + 1..close];
                        if is_method_name(name) {
                            let Some(body_end) = matching_brace_end(region, body) else {
                                // 块括号不配对（畸形）：本方法无块，停止扫描。
                                return out;
                            };
                            let raw_block = &region[open..=body_end];
                            out.push((
                                name.to_string(),
                                raw_block.to_string(),
                                normalize(&strip_comments(raw_block)),
                            ));
                            index = body_end + 1;
                            continue;
                        }
                    }
                }
                index = close + 1;
            }
            _ => index += 1,
        }
    }
    out
}

/// 从 `start`（位于 `{` 之后）起找与它配对的 `}` 的下标；**字符串与注释感知**。
///
/// 真实描述符的 schema 串（`jsonSchema` / `declaration`）里含大量 `{`/`}`，且
/// 可能带转义引号；不跳过字符串字面量会很快把深度算错。注释（`//`、`/* */`）
/// 内的花括号同样不参与配对（与锚点扫描同一套状态机，复审轮 3 Major-2）。
///
/// 返回 `None` = 括号不配对（畸形文件）——调用方不得凭空吞到文件尾。
fn matching_brace_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 1usize;
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                let mut closed = false;
                while index + 1 < bytes.len() {
                    if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                        closed = true;
                        index += 2;
                        break;
                    }
                    index += 1;
                }
                if !closed {
                    return None; // 未闭合块注释：余下配对不可靠
                }
            }
            b'\'' | b'"' | b'`' => {
                // 字符串字面量整体跳过（含转义）；未闭合 → 视为不配对。
                let close = string_close(bytes, index)?;
                index = close + 1;
            }
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

/// 方法名形态：含且只含一个 `/`，**两段都非空**且都是标识符字符（N8：排除
/// `'a/b/c'`、`'/a'`、`'a/'` 与普通键）。
fn is_method_name(name: &str) -> bool {
    let Some((namespace, method)) = name.split_once('/') else {
        return false;
    };
    if namespace.is_empty() || method.is_empty() || method.contains('/') {
        return false;
    }
    namespace
        .chars()
        .chain(method.chars())
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '$' | '.'))
}

/// 单窗口的文本（不存在 → None）。
///
/// C-1（T09 接线前）：Whole/Between 的片段匹配基于**剥注释**文本——注释里的旧
/// 片段不得替代码命中（复审实证：审批文件代码删词、注释保留 → ApproveOnce 仍
/// 命中）。剥离在**原始文本**上进行（`//` 行注释依赖换行定界），再归一化；字符
/// 串字面量内容保留。**残留边界（如实申报）**：部分窗口片段本就由字符串承载
/// （`Whole(UploadRoute)` 的 MIME 串 `application/octet-stream`、MultiAnswer/
/// PlanReview 的 Between `declaration` JSON 串）——字符串里的旧片段仍可替漂移的
/// 代码命中；`strip_comments` 不识别 JS 正则字面量，未来包若在正则里出现 `//`/
/// `/*` 会吞代码（方向 fail-closed 失配；本机九源代码态斜杠为 0）。
fn window_text(
    window: &Window,
    code_sources: &BTreeMap<Source, String>,
    index: &MethodIndex,
) -> Option<String> {
    Some(match window {
        Window::Whole(source) => code_sources.get(source)?.clone(),
        Window::Method(source, method) => index.get(*source, method)?.to_string(),
        // 结构窗不走窗口文本（rule_matches 特判先于本函数）。
        Window::MethodParamTableSingleEntry(..) => return None,
        // 分面区域窗不走窗口文本（rule_matches 分组判定先于本函数）。
        Window::MethodRegion { .. } => return None,
        Window::Between(source, start, end) => {
            let text = code_sources.get(source)?;
            let begin = text.find(start)?;
            let finish = text[begin..].find(end).map(|offset| begin + offset)?;
            text[begin..finish].to_string()
        }
    })
}

/// 规则匹配（P2-A：**逐窗口归属**）。
///
/// 每个 `(window, fragment)` 对都要求：该窗口存在，且该片段出现在**这个窗口
/// 自己的文本**里。任一不满足 → 未匹配。因此「第二窗口的同名副本」不能替
/// 第一窗口的漂移买单。
/// 参数表结构检查：方法块内每个 `parameters:[…]` 数组的**深度一层条目数
/// 恰为 1**（字符串/注释感知——字符串内容里的同形串不计，注释已由
/// strip_comments 变占位符不影响括号配对）。没有任何 parameters 数组 →
/// false（fail-closed）。多面渲染（每面一份参数表）天然兼容：每份各查各的。
fn method_block_param_tables_single_entry(block: &str) -> bool {
    let b = block.as_bytes();
    let needle = b"parameters:";
    let mut i = 0usize;
    let mut arrays_checked = 0usize;
    while i < b.len() {
        // 注释跳过（raw 块上有真注释；注释内容里的括号/parameters 不计）。
        if b[i] == b'/' && i + 1 < b.len() && (b[i + 1] == b'/' || b[i + 1] == b'*') {
            if b[i + 1] == b'/' {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            } else {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            continue;
        }
        // 字符串跳过（含转义）——字符串内容里的 `parameters:[` 不计。
        if b[i] == b'\'' || b[i] == b'"' || b[i] == b'`' {
            let quote = b[i];
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == quote {
                    break;
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if b[i..].starts_with(needle) {
            // raw 块保留空白：`parameters:` 与 `[` 之间可能隔任意空白/换行。
            let mut k = i + needle.len();
            while k < b.len() && (b[k] == b' ' || b[k] == b'\t' || b[k] == b'\n' || b[k] == b'\r') {
                k += 1;
            }
            if k >= b.len() || b[k] != b'[' {
                i += 1;
                continue;
            }
            let arr_open = k; // 指向 `[`
                              // 元素=深度一层的逗号分段（字符串/注释/嵌套括号都算段内内容）。
                              // 「恰一个分段且以 `{` 开头」= 唯一支持的参数对象形态；标识符、
                              // 展开（`...x`）、第二个对象、空表全部失配（销项复审二轮 P2-2：
                              // 数对象字面量会把标识符/展开条目漏成零）。
            let mut depth_sq = 0usize;
            let mut depth_brace = 0usize;
            let mut elements = 0usize;
            let mut current_has_content = false;
            let mut current_starts_with_object = true;
            let mut j = arr_open;
            let mut closed = false;
            while j < b.len() {
                let c = b[j];
                if c == b'\'' || c == b'"' || c == b'`' {
                    // 字符串开头的顶层段 = 非参数对象形态 → 失配（三轮 P2-1：
                    // 段首判定必须在字符串分支前，不得让字符串绕过）。
                    if depth_sq == 1 && !current_has_content {
                        return false;
                    }
                    if depth_sq == 1 {
                        current_has_content = true;
                    }
                    let quote = c;
                    let mut k = j + 1;
                    while k < b.len() {
                        if b[k] == b'\\' {
                            k += 2;
                            continue;
                        }
                        if b[k] == quote {
                            break;
                        }
                        k += 1;
                    }
                    j = k + 1;
                    continue;
                }
                if c == b'/' && j + 1 < b.len() && (b[j + 1] == b'/' || b[j + 1] == b'*') {
                    // raw 块上有真注释：跳过（注释内容不算元素内容）。
                    if b[j + 1] == b'/' {
                        while j < b.len() && b[j] != b'\n' {
                            j += 1;
                        }
                    } else {
                        j += 2;
                        while j + 1 < b.len() && !(b[j] == b'*' && b[j + 1] == b'/') {
                            j += 1;
                        }
                        j += 2;
                    }
                    continue;
                }
                if c == b'[' {
                    // `[` 开头的顶层段（嵌套数组条目）→ 失配（三轮 P2-1：
                    // 数对象字面量会把 `[]`/嵌套数组条目漏计成零）。
                    if depth_sq == 1 && !current_has_content {
                        return false;
                    }
                    depth_sq += 1;
                } else if c == b']' {
                    depth_sq -= 1;
                    if depth_sq == 0 {
                        closed = true;
                        break;
                    }
                } else if c == b',' && depth_sq == 1 && depth_brace == 0 {
                    // 只有深度一（对象外）的逗号才是元素分隔；对象内部键值
                    // 逗号（depth_brace>0）不算。
                    // 空段（hole/前导逗号）→ 失配；尾逗号后接 `]` 走闭合分支。
                    if !current_has_content {
                        return false;
                    }
                    if current_has_content {
                        elements += 1;
                        if elements == 1 && !current_starts_with_object {
                            // 首元素不是参数对象（标识符/展开等）→ 直接失配。
                            return false;
                        }
                    }
                    current_has_content = false;
                    current_starts_with_object = true;
                } else if depth_sq == 1 {
                    if !b[j].is_ascii_whitespace() {
                        if !current_has_content {
                            current_starts_with_object = c == b'{';
                        }
                        current_has_content = true;
                    }
                    if c == b'{' {
                        depth_brace += 1;
                    } else if c == b'}' {
                        depth_brace -= 1;
                    }
                }
                j += 1;
            }
            if !closed {
                return false;
            }
            if current_has_content {
                elements += 1;
                if !current_starts_with_object {
                    return false;
                }
            }
            if elements != 1 {
                return false;
            }
            arrays_checked += 1;
            i = j + 1;
            continue;
        }
        i += 1;
    }
    arrays_checked >= 1
}

/// 分面区域判定（销项复审三轮 P2「分面绑定」；P2-4 改**互不重叠连续段**）：
/// `start` 锚出现点依次为**段界**——[s1..s2)、[s2..s3)、…；段 i 的右界 =
/// min(下一个 start 锚, 本段 start 之后的第一个 end 锚)（end=None → 段尾即
/// 块尾；末段 end 锚找不到 → 该段划不出来，fail-closed 方向跳过）。每段独立
/// 判定「该窗全部片段都在本段内」，任一段满足全窗 → 命中；**跨段拼接不算**。
/// 旧实现「每个 start 锚划 [start..end)」的区间互相重叠（多面布局下第一面
/// 的区间会吞掉第二面），跨面拼接的锚能在重叠区凑数命中——P2-4 封堵。
/// start 锚一处都没有 → false（fail-closed，与各窗口「块不可得即失配」同口径）。
fn facet_region_contains_all(
    block: &str,
    start: &str,
    end: Option<&str>,
    fragments: &[&'static str],
) -> bool {
    let mut boundaries: Vec<usize> = Vec::new();
    let mut from = 0usize;
    while let Some(offset) = block[from..].find(start) {
        let at = from + offset;
        boundaries.push(at);
        from = at + start.len();
    }
    if boundaries.is_empty() {
        return false;
    }
    let total = block.len();
    for (index, &begin) in boundaries.iter().enumerate() {
        let end_at = end.and_then(|anchor| {
            block[begin + start.len()..]
                .find(anchor)
                .map(|rel| begin + start.len() + rel)
        });
        let finish = match (boundaries.get(index + 1), end_at) {
            // 下一个 start 锚在 end 锚之前（或本段无 end 锚）：段到下一个 start 为止。
            (Some(&next), Some(end_pos)) => next.min(end_pos),
            (Some(&next), None) => next,
            // 末段：有 end 锚则到 end 锚；end=None → 块尾；末段 end 锚找不到
            // → 该段划不出来（fail-closed 跳过）。
            (None, Some(end_pos)) => end_pos,
            (None, None) if end.is_none() => total,
            (None, None) => continue,
        };
        let region = &block[begin..finish];
        if fragments.iter().all(|fragment| region.contains(fragment)) {
            return true;
        }
    }
    false
}

fn rule_matches(rule: &Rule, code_sources: &BTreeMap<Source, String>, index: &MethodIndex) -> bool {
    // 分面区域窗：同窗全部片段必须落在同一段区间内——先按窗归组、逐窗判定
    // （任一区间满足即可）；主循环跳过这些 (窗, 片段) 对。
    let mut region_groups: Vec<(Window, Vec<&'static str>)> = Vec::new();
    for (window, fragment) in rule.windows {
        if let Window::MethodRegion { .. } = window {
            match region_groups.iter_mut().find(|(group, _)| group == window) {
                Some((_, fragments)) => fragments.push(fragment),
                None => region_groups.push((*window, vec![fragment])),
            }
        }
    }
    for (window, fragments) in &region_groups {
        let Window::MethodRegion {
            source,
            method,
            start,
            end,
        } = *window
        else {
            unreachable!("region_groups 只收集 MethodRegion");
        };
        let Some(block) = index.get(source, method) else {
            return false;
        };
        if !facet_region_contains_all(block, start, end, fragments) {
            return false;
        }
    }
    for (window, fragment) in rule.windows {
        // 参数表结构窗（MethodParamTableSingleEntry）：块内每个 parameters
        // 数组恰一个条目；块不可得 → 与其他窗口同口径失配（fail-closed）。
        if let Window::MethodParamTableSingleEntry(source, method) = window {
            // 结构扫描吃**原始块**：归一化把反引号折成双引号会伪造字符串边界
            // （模板串内含双引号时把两个对象折成一个），字符串/注释感知必须
            // 在真实词法边界上做（销项复审二轮 P2-1）。
            let Some(text) = index.get_raw(*source, method) else {
                return false;
            };
            if !method_block_param_tables_single_entry(text) {
                return false;
            }
            continue;
        }
        if let Window::MethodRegion { .. } = window {
            // 分面区域窗已在上方按窗归组判定（同窗全部片段必须同区间）。
            continue;
        }
        let Some(text) = window_text(window, code_sources, index) else {
            return false;
        };
        if !text.contains(fragment) {
            return false;
        }
    }
    true
}

// ============================================================
// 规则表（逐 Operation：单一方法 + 窗口 + 形状/语义片段）
// ============================================================
//
// 锚定纪律：`windows` 元组里每一串都是本机 0.2.1-rc.3 包内**真实出现的归一化片段**
// （归一化 = 去全部空白、反引号→双引号），且必须落在 `windows` 指定的窗口内。
// 窗口 = 目标方法自己的 descriptor 块（跨方法依赖显式列第二个窗口），因此兄弟
// 方法的同名字段**不能**替目标方法命中（B-1）。多表面副本（codec/strictCodec、
// 请求面/结果面）的规则逐条申报检测面（见 Mi-1 各规则注释）；Send 规则自 v9 起
// 进一步**分面绑定**（MethodRegion）：同一面内的片段必须落在同一段 [start..end)
// 区间——块内别处的同形串（区域错位）不再替该面买单。
//
// `method` 为**单一**原生方法名（model.rs `OperationContractMatch.method` 契约，
// N-9）；依赖的第二个方法/事件/HTTP 路由写进 `structural_rule` 文本。

struct Rule {
    operation: Operation,
    method: &'static str,
    /// 逐窗口归属的片段：`(窗口, 片段)`。同一操作可列多窗口，但每个片段只能
    /// 在它被声明的那个窗口内出现（P2-A：第二窗口的同名副本不得替第一窗口买单）。
    windows: &'static [(Window, &'static str)],
    structural_rule: &'static str,
    semantic_rule: &'static str,
}

use Source::*;

const RULES: &[Rule] = &[
    Rule {
        operation: Operation::Read,
        method: "session/list",
        windows: &[
            (Window::Method(SessionController, "session/list"), "name:'_request'"),
            (Window::Method(SessionController, "session/list"), "\"items\":{\"type\":\"array\""),
            (Window::Method(SessionController, "session/follow"), "mode:'stream'"),
            (Window::Method(SessionController, "session/follow"), "\"const\":\"snapshot\""),
        ],
        structural_rule: "session/list{_request} → {items[]}；session/follow(stream) → snapshot/event 帧",
        semantic_rule: "只读：list 不 resume Agent；follow 首帧 snapshot 带 cursor（不激活 Agent）",
    },
    // Send 单规则（2026-11-17 P1-1 复审收窄；rule_matches 是**逐窗口 AND**）：
    // 真机 asar 实读（typert.host.js SessionController + assertExactArguments）
    // 的唯一实证契约 = wire 参数是**单一 `request` 对象**（块内 `name: 'request'`
    // + `mode: 'strict'` 实锚），strict codec 校验 SessionPromptRequest =
    // {requestId, sessionId, mode, content}，结果带明确受理字段 accepted。
    // 曾并行的「ACP `{sessionId, prompt}`」窗口组**无实证**（真机 asar 内
    // `"required":["sessionId","prompt"` / `"prompt":{"type":"array"` 双锚缺席，
    // 而发送器恒发 request 包装）→ 已删除——杜绝「能力门放行发送器不支持的
    // 契约」。Send 只匹配真实放行、零派发兜底：契约失配时动作不创建/明确
    // Failed，草稿与幂等键保留，宿主升级后重探测再发送。
    // 分面绑定（v9，销项复审三轮 P2）：v8 的锚在整方法块内浮动——区域错位
    // 漏检（`accepted` 锚可被 request schema 里的诱饵满足、`mode:'strict'` 可被
    // 块内别处满足）。v9 起 anchor 按面重排进 `Window::MethodRegion`：参数表头面
    // [parameters:[..jsonSchema:)、请求 schema 面 [jsonSchema:..result:)、受理
    // 结果面 [result:..块尾)；同窗全部片段必须落在同一段区间（任一区间满足
    // 即命中；段界互不重叠——跨面拼接不算，P2-4 回归
    // send_rule_rejects_cross_face_stitched_anchors 钉死）。真机布局 parameters
    // 先、result 后，区间切分已用树内 PROVEN 夹具钉住
    // （send_rule_accepts_real_package_face_layout）。
    // 已知边界（如实申报）：真实包 prompt 请求 schema 有 codec/strictCodec
    // 两面，各面各划各的区间（每出现一次 start 锚划一段），任一面满足即可
    // ——**单面整体漂移不被检测**（另一面照常命中）；面内片段的区域错位/
    // 跨区拼凑已进检测面（P2-4：跨面拼接已封堵——段界互不重叠）。核心输入形状漂移只关 Send：FileUpload/ImageUpload 锚
    // receiptId/mediaType/upload 路由，与请求形状独立（逐字段漂移只关受影响
    // 子操作）。
    Rule {
        operation: Operation::Send,
        method: "session/prompt",
        // 分面绑定（v9，销项复审三轮 P2）：锚不再在整方法块内浮动——按面钉死。
        // 面切分与真机布局一致（parameters 先、result 后；多面渲染每面一份
        // 参数表+result，每出现一次 start 锚划一段区间，任一区间满足即命中）。
        windows: &[
            // 参数表头面 [parameters:[ .. jsonSchema:)：单一 `request` 对象参数
            // + strict 校验必须落在参数条目自己的头部——`mode:'strict'` 被挪到
            // 块内别处（如 parameters 与 result 之间）不再买单。
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "parameters:[",
                end: Some("jsonSchema:"),
            }, "name:'request'"),
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "parameters:[",
                end: Some("jsonSchema:"),
            }, "mode:'strict'"),
            // 请求 schema 面 [jsonSchema: .. result:)：四字段 required + 类型级
            // 判据（历史收紧保留）全部绑定在请求载荷自己的 schema 里——request
            // 区插入的诱饵不再替别的面买单。除 `"required":["type","text"]` 为
            // **整段顺序敏感**（换序即失配，方向 fail-closed，已申报）外，其余
            // 片段均不带闭括号，与真实包 requestId 带 description 的形态兼容。
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "jsonSchema:",
                end: Some("result:"),
            }, "\"required\":[\"requestId\",\"sessionId\",\"mode\",\"content\"]"),
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "jsonSchema:",
                end: Some("result:"),
            }, "\"requestId\":{\"type\":\"string\""),
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "jsonSchema:",
                end: Some("result:"),
            }, "\"sessionId\":{\"type\":\"string\""),
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "jsonSchema:",
                end: Some("result:"),
            }, "\"content\":{\"type\":\"array\""),
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "jsonSchema:",
                end: Some("result:"),
            }, "\"mode\":{\"type\":\"string\",\"enum\":[\"queue\",\"steer\"]}"),
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "jsonSchema:",
                end: Some("result:"),
            }, "\"text\":{\"type\":\"string\""),
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "jsonSchema:",
                end: Some("result:"),
            }, "\"type\":{\"type\":\"string\",\"const\":\"text\""),
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "jsonSchema:",
                end: Some("result:"),
            }, "\"required\":[\"type\",\"text\"]"),
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "jsonSchema:",
                end: Some("result:"),
            }, "\"const\":\"text\""),
            // 受理结果面 [result: .. 块尾)：accepted 声明类型/取值 + 必填——
            // 明确受理字段缺席=失配兜底（发送侧保留草稿与幂等键，不盲发）；
            // 请求 schema 里的诱饵（accepted 同形串）不再替本面买单。
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "result:",
                end: None,
            }, "\"accepted\":{\"type\":\"boolean\",\"const\":true}"),
            (Window::MethodRegion {
                source: SessionController,
                method: "session/prompt",
                start: "result:",
                end: None,
            }, "\"required\":[\"accepted\"]"),
            // 参数表结构窗（销项复审 P2 及二轮）：块内每个 parameters 数组
            // 恰一个条目——第二参数（任何字段顺序/拼写/参数间注释）即失配；
            // 真机多面渲染（每面一份参数表）天然兼容。
            (Window::MethodParamTableSingleEntry(SessionController, "session/prompt"), "parameters:["),
        ],
        structural_rule: "session/prompt{request:{requestId:string,sessionId:string,mode∈{queue,steer},content:array[text{type,text:string}|image|file]}} → {accepted:true}",
        semantic_rule: "accepted 只证受理（不等于回合成功）；requestId 由客户端铸造用于归因",
    },
    Rule {
        operation: Operation::FileUpload,
        method: "session/prompt",
        windows: &[
            (Window::Method(SessionController, "session/prompt"), "\"const\":\"file\""),
            (Window::Method(SessionController, "session/prompt"), "\"receiptId\":{\"type\":\"string\"}"),
            (Window::Whole(UploadProtocol), "FILE_UPLOAD_PATH='/api/session/uploadFileBinary'"),
            (Window::Whole(UploadRoute), "application/octet-stream"),
            (Window::Whole(UploadRoute), "uploadStream("),
        ],
        structural_rule: "POST /api/session/uploadFileBinary?sessionId=&name=（octet-stream）→ {receiptId,...}；session/prompt content[{type:file,receiptId}]",
        semantic_rule: "上传只产 receipt；消息绑定需 prompt 携带 receiptId（上传 ≠ 已发送）",
    },
    Rule {
        operation: Operation::ImageUpload,
        method: "session/prompt",
        windows: &[
            (Window::Method(SessionController, "session/prompt"), "\"const\":\"image\""),
            (Window::Method(SessionController, "session/prompt"), "\"mediaType\":{\"type\":\"string\",\"enum\":[\"image/png\",\"image/jpeg\",\"image/webp\",\"image/gif\"]}"),
            (Window::Method(SessionController, "session/follow"), "\"maxImageBytes\":{\"type\":\"number\""),
        ],
        structural_rule: "content[{type:image,mediaType∈png/jpeg/webp/gif,data}] + follow.projections.imageLimits{maxImageBytes,...}",
        semantic_rule: "图片按 mediaType 白名单与 imageLimits 准入；不匹配只关单项（不推倒文件上传）",
    },
    Rule {
        operation: Operation::ModelSelect,
        method: "session/selectModel",
        // Mi-1（轮 3）/ N5（轮 4）：`required` 形状在 codec 与 strictCodec 两面各有
        // 一份（真实包 0.2.1-rc.3 实读），用 additionalProperties 尾巴**逐面**锚定
        // ——任一面**改值**漂移都失配。**已知边界申报**：两面**互换**（codec 的
        // true ↔ strictCodec 的 false 对调）不在检测面（两个片段仍在，只是换了
        // 各自所在面）；如实申报：本规则不检测互换形态的漂移。
        windows: &[
            (Window::Method(SessionController, "session/selectModel"), "\"required\":[\"sessionId\",\"provider\",\"model\"],\"additionalProperties\":true"),
            (Window::Method(SessionController, "session/selectModel"), "\"required\":[\"sessionId\",\"provider\",\"model\"],\"additionalProperties\":false"),
            (Window::Method(SessionController, "session/selectModel"), "\"selected\":{\"type\":\"object\""),
            (Window::Method(SessionController, "session/selectModel"), "savethedefaultinthebackground."),
        ],
        structural_rule: "selectModel{sessionId,provider,model} → {selected{provider,model,reasoningEffort?}}",
        semantic_rule: "会话级生效 + 后台写默认模型 profile patch（显式副作用）；default 读取滞后一拍",
    },
    Rule {
        operation: Operation::ReasoningSelect,
        method: "session/selectModel",
        windows: &[
            (Window::Method(SessionController, "session/selectModel"), "\"reasoningEffort\":{\"type\":\"string\"}"),
            // DSH 0.2.2 的生成描述符仅记录具名结果 codec。档位由实际目录校验。
            (Window::Method(SessionController, "session/modelCatalog"), "typeSymbol:'@deepseek-ai/dsh-api-session-controller/types#ModelCatalog'"),
        ],
        structural_rule: "selectModel.reasoningEffort + modelCatalog.groups[].models[].reasoning{efforts[],defaultEffort}",
        semantic_rule: "推理档以目录 efforts 为准；目录匹配不外推全部推理等级可用",
    },
    Rule {
        operation: Operation::ApproveOnce,
        method: "$events/result",
        windows: &[
            (Window::Whole(RemoteEvents), "{event:'approval/request',mode:'waterfall'}"),
            (Window::Whole(ApprovalTypes), "constOUTCOMES=['allowed-once',"),
            (Window::Whole(StreamProtocol), "exactKeys(value,['clientId','eventId','outcome'])"),
            (Window::Whole(StreamProtocol), "outcome:Object.hasOwn(outcome,'value')"),
        ],
        structural_rule: "waterfall approval/request{eventId,agentId,request{toolName,callId,...}} → $events/result{clientId,eventId,outcome:{kind:'result',value}}",
        semantic_rule: "allowed-once 为一次性放行；审批仅 owner+interactions=answer Peer 可答；官方无永久允许取值",
    },
    Rule {
        operation: Operation::Reject,
        method: "$events/result",
        windows: &[
            (Window::Whole(RemoteEvents), "{event:'approval/request',mode:'waterfall'}"),
            (Window::Whole(ApprovalTypes), "constOUTCOMES=['allowed-once','rejected',"),
            (Window::Whole(StreamProtocol), "outcome:Object.hasOwn(outcome,'value')"),
        ],
        structural_rule: "$events/result{clientId,eventId,outcome:{kind:'result',value:'rejected'}}",
        semantic_rule: "rejected 与非允许取值同走 $events/result；decision 只取服务端合法值",
    },
    Rule {
        operation: Operation::SingleAnswer,
        method: "userQuestions/answer",
        windows: &[
            (Window::Method(UserQuestions, "userQuestions/answer"), "\"answers\":{\"type\":\"array\""),
            (Window::Method(UserQuestions, "userQuestions/answer"), "\"required\":[\"id\",\"selected\"]"),
            (Window::Method(UserQuestions, "userQuestions/answer"), "Theansweredquestionid."),
        ],
        structural_rule: "userQuestions/answer{agentId,callId,answer{answers[{id,selected[]}]}} → boolean",
        semantic_rule: "答案按题目原生 id 对位（selected 为 label 数组）；单选 = 每题一个 label",
    },
    Rule {
        operation: Operation::MultiAnswer,
        method: "userQuestions/answer",
        windows: &[
            (Window::Method(UserQuestions, "userQuestions/answer"), "\"answers\":{\"type\":\"array\""),
            (Window::Method(UserQuestions, "userQuestions/answer"), "multi-selectquestion."),
            (Window::Between(UserQuestions, "\"name\":\"AskUserQuestionItem\"", "\"name\":\"AskUserQuestionOption\""), "multiSelect?:boolean;"),
        ],
        structural_rule: "answers[].selected[] 多 label + AskUserQuestionItem.multiSelect?:boolean",
        semantic_rule: "多选依赖选项声明 multiSelect；该形状漂移只关多选（单选保留）",
    },
    Rule {
        operation: Operation::FreeAnswer,
        method: "userQuestions/answer",
        windows: &[
            (Window::Method(UserQuestions, "userQuestions/answer"), "\"custom\":{\"type\":\"string\",\"description\":\"Optionalfree-text"),
        ],
        structural_rule: "answers[].custom?:string（'Other' 自由文本，与 selected 并列）",
        semantic_rule: "自由作答以 custom 提交；跳过 = selected:[]（不伪造答案）",
    },
    Rule {
        operation: Operation::GroupAnswer,
        method: "userQuestions/answer",
        windows: &[
            (Window::Method(UserQuestions, "userQuestions/answer"), "Completestructuredanswerbatch,oneitemperquestionofthecall."),
            (Window::Method(UserQuestions, "userQuestions/answer"), "\"required\":[\"answers\"]"),
        ],
        structural_rule: "一次提交含全部题（one item per question）",
        semantic_rule: "分组一次提交；缺题/重复回复属业务拒绝（REPLY_QUEUED），不拆成多次派发",
    },
    Rule {
        operation: Operation::PlanReview,
        method: "userQuestions/answer",
        windows: &[
            (Window::Between(UserQuestions, "\"name\":\"AskUserQuestionIntent\"", "\"name\":\"AskUserQuestionItem\""), "kind:'plan-review'"),
            (Window::Between(UserQuestions, "\"name\":\"AskUserQuestionIntent\"", "\"name\":\"AskUserQuestionItem\""), "approve:string;"),
            (Window::Method(UserQuestions, "userQuestions/answer"), "\"required\":[\"answers\"]"),
        ],
        structural_rule: "AskUserQuestionIntent{kind:'plan-review',approve} + userQuestions/answer{answers[]}",
        semantic_rule: "plan-review 的裁决由 approve 命名的选项标签表达（不按选项位置推断）",
    },
    Rule {
        operation: Operation::CancelCurrent,
        method: "session/cancel",
        windows: &[
            (Window::Method(SessionController, "session/cancel"), "\"properties\":{\"sessionId\":{\"type\":\"string\"}},\"required\":[\"sessionId\"]"),
            (Window::Method(SessionController, "session/cancel"), "\"accepted\":{\"type\":\"boolean\",\"const\":true}"),
            (Window::Method(SessionController, "session/cancel"), "CanceloneactiveAgentturnwithoutdroppingitspendinginbox."),
        ],
        structural_rule: "session/cancel{sessionId} → {accepted:true}",
        semantic_rule: "只停当前回合、保留 pending inbox；不得包装成原生未提供的回合条件取消",
    },
    Rule {
        operation: Operation::QueueEdit,
        method: "session/updateQueue",
        windows: &[
            (Window::Method(SessionController, "session/updateQueue"), "\"const\":\"edit\""),
            (Window::Method(SessionController, "session/updateQueue"), "Mutateonestill-pendingqueueoccurrence"),
        ],
        structural_rule: "updateQueue{sessionId,itemId,action{kind:'edit',content[text]}}",
        semantic_rule: "只改仍 pending 的队列项；不存在 → session/queue-item-not-found（业务拒绝）",
    },
    Rule {
        operation: Operation::QueueRemove,
        method: "session/updateQueue",
        windows: &[
            (Window::Method(SessionController, "session/updateQueue"), "\"const\":\"remove\""),
        ],
        structural_rule: "updateQueue{sessionId,itemId,action{kind:'remove'}}",
        semantic_rule: "撤回首只作用于 pending 项；顺序变化只由 agent/inbox/spliced 回显",
    },
    Rule {
        operation: Operation::Steer,
        method: "session/updateQueue",
        windows: &[
            (Window::Method(SessionController, "session/updateQueue"), "\"const\":\"steer\""),
        ],
        structural_rule: "updateQueue{...,action{kind:'steer'}}",
        semantic_rule: "steer 是队列动作而非另一条 prompt；steer-unavailable 不降级别的方法",
    },
    Rule {
        operation: Operation::SessionCreate,
        method: "session/create",
        // Mi-1（轮 3）：结果面 agentPreset 形状带结果描述尾（真实包唯一出现）——
        // 结果面漂移可检测。**已知边界申报**：请求面 workspaceId/cwd/agentPreset
        // 在 codec 与 strictCodec 各有一份同源副本（裸形状片段不可区分），单独
        // 漂移不在本规则检测面（漂移检测以结果面 agentPreset + 结果散文为准）。
        windows: &[
            (Window::Method(SessionController, "session/create"), "\"workspaceId\":{\"type\":\"string\""),
            (Window::Method(SessionController, "session/create"), "\"cwd\":{\"type\":\"string\""),
            (Window::Method(SessionController, "session/create"), "\"agentPreset\":{\"type\":\"string\",\"description\":\"PresettheSessionruns,whenthedeploymentcomposesapresetroster.\""),
            (Window::Method(SessionController, "session/create"), "thecreatedorreusedSession"),
        ],
        structural_rule: "session/create{workspaceId|cwd,sessionId?,agentPreset?} → {sessionId,agentPreset?}",
        semantic_rule: "workspaceId 与 cwd 互斥；project_id 只能映射已授权工作区（不能传任意 cwd）",
    },
    Rule {
        operation: Operation::SessionRename,
        method: "session/rename",
        // Mi-1（轮 3）：结果面 title 形状与 seq 相连（`"title":{"type":"string"},
        // "seq":…`，真实包唯一出现）+ 请求面 codec 副本用 required+additionalProperties
        // 尾巴锚定。**已知边界申报**：请求面 strictCodec 的 title 副本单独漂移不在
        // 本规则检测面（漂移检测以结果面 + 请求 codec 面为准）。
        windows: &[
            (Window::Method(SessionController, "session/rename"), "\"title\":{\"type\":\"string\"},\"seq\":{\"type\":\"number\""),
            (Window::Method(SessionController, "session/rename"), "\"required\":[\"sessionId\",\"title\"],\"additionalProperties\":true"),
            (Window::Method(SessionController, "session/rename"), "theacceptedtitleanddurableeventsequence."),
        ],
        structural_rule: "session/rename{sessionId,title} → {title,seq}",
        semantic_rule: "标题生效以返回 seq 为准（会话事件序列）",
    },
    Rule {
        operation: Operation::SessionFork,
        method: "session/fork",
        windows: &[
            (Window::Method(SessionController, "session/fork"), "\"atSeq\":{\"type\":\"number\""),
            (Window::Method(SessionController, "session/fork"), "\"allowMigration\":{\"type\":\"boolean\""),
            (Window::Method(SessionController, "session/fork"), "Forkonecold-readableexacteventprefixintoanewSession."),
        ],
        structural_rule: "session/fork{sessionId,atSeq?,allowMigration?} → {sessionId}",
        semantic_rule: "省略 atSeq = 最近完整回合前缀；fork 产生新原生 sessionId（父子关系原生）",
    },
    Rule {
        operation: Operation::SessionSearch,
        method: "session/search",
        windows: &[
            (Window::Method(SessionController, "session/search"), "\"query\":{\"type\":\"string\"}"),
            (Window::Method(SessionController, "session/search"), "\"hasMore\":{\"type\":\"boolean\""),
        ],
        structural_rule: "session/search{query} → {items[],hasMore}",
        semantic_rule: "索引未启用的部署返回配置不支持（业务拒绝），不是协议缺失",
    },
    Rule {
        operation: Operation::Archive,
        method: "workspace/archiveSession",
        windows: &[
            (Window::Method(WorkspaceController, "workspace/archiveSession"), "\"archivedSessionIds\":{\"type\":\"array\""),
        ],
        structural_rule: "workspace/archiveSession{sessionId} → {archivedSessionIds[]}",
        semantic_rule: "返回值是整个归档集合（非本次变更集）：只按集合成员判定",
    },
    Rule {
        operation: Operation::Unarchive,
        method: "workspace/unarchiveSession",
        windows: &[
            (Window::Method(WorkspaceController, "workspace/unarchiveSession"), "\"archivedSessionIds\":{\"type\":\"array\""),
        ],
        structural_rule: "workspace/unarchiveSession{sessionId} → {archivedSessionIds[]}",
        semantic_rule: "与 archive 同一集合语义（取消归档后集合收缩）",
    },
    Rule {
        operation: Operation::Pin,
        method: "workspace/pinSession",
        windows: &[
            (Window::Method(WorkspaceController, "workspace/pinSession"), "\"pinnedSessionIds\":{\"type\":\"array\""),
        ],
        structural_rule: "workspace/pinSession{sessionId} → {pinnedSessionIds[]}",
        semantic_rule: "置顶只改排序集合；返回值是完整置顶集合",
    },
    Rule {
        operation: Operation::Unpin,
        method: "workspace/unpinSession",
        windows: &[
            (Window::Method(WorkspaceController, "workspace/unpinSession"), "\"pinnedSessionIds\":{\"type\":\"array\""),
        ],
        structural_rule: "workspace/unpinSession{sessionId} → {pinnedSessionIds[]}",
        semantic_rule: "取消置顶不改保存的会话顺序",
    },
    Rule {
        operation: Operation::SkillsList,
        method: "skills/list",
        windows: &[
            (Window::Method(SessionController, "skills/list"), "\"skills\":{\"type\":\"array\""),
            (Window::Method(SessionController, "skills/list"), "\"modelInvocable\":{\"type\":\"boolean\""),
        ],
        structural_rule: "skills/list{request{sessionId}} → {skills[{name,description,modelInvocable,path?}]}",
        semantic_rule: "只读目录（不加载 skill 正文）；skill 安装/启停无 Remote 面",
    },
    Rule {
        operation: Operation::CommandsList,
        method: "commands/list",
        // Mi-1（轮 3）：结果形状补锚——`commands/list` 的 result.jsonSchema 是
        // `{"type":"array","items":{"type":"object","properties":{"definitionId":…}}`
        // （本机 0.2.1-rc.3 实读，全文件唯一出现）。散文句与 wire 片段不得替
        // 形状买单：结果形状整体漂移必须失配。
        windows: &[
            (Window::Method(Commands, "commands/list"), "wire:'agentId'"),
            (Window::Method(Commands, "commands/list"), "name-sorteddescriptorsafterscopedshadowing."),
            (Window::Method(Commands, "commands/list"), "\"type\":\"array\",\"items\":{\"type\":\"object\",\"properties\":{\"definitionId\""),
        ],
        structural_rule: "commands/list{agentId} → [{name,description,input?}]",
        semantic_rule: "agent 级作用域（agentId 由会话身份解析）；只列不改权限",
    },
    Rule {
        operation: Operation::CommandExecute,
        method: "commands/execute",
        // Mi-1（轮 3）：`submittedAttachments` 补数组形状锚（`jsonSchema:{"type":
        // "array","items":{"oneOf"`，真实包实读）。**已知边界申报**：该形状在
        // codec 与 strictCodec 两面各有一份同源副本（单面漂移不单独报警），双面
        // 同改（形状整体改写）必失配。
        windows: &[
            (Window::Method(Commands, "commands/execute"), "completeslash-commandline."),
            (Window::Method(Commands, "commands/execute"), "submittedAttachments"),
            (Window::Method(Commands, "commands/execute"), "jsonSchema:{\"type\":\"array\",\"items\":{\"oneOf\""),
            (Window::Method(Commands, "commands/execute"), "\"const\":\"file\""),
        ],
        structural_rule: "commands/execute{agentId,line,submittedAttachments[]} → command/run+command/done 事件",
        semantic_rule: "执行可能改权限/触发压缩：逐项白名单 + 用户主动；line 不当普通 prompt",
    },
];

/// 规则表只读自检项（N-8：验收测试据此确认「每条规则至少含非方法名的形状/语义
/// 片段」，而不是只看规则存在）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleAudit {
    pub operation: Operation,
    /// 单一原生方法名。
    pub method: &'static str,
    /// 匹配窗口数（含跨方法依赖的显式窗口）。
    ///
    /// Nit-1（轮 3）申报：`window_count` 与 `fragment_count` **同源恒等**——本规则
    /// 表的窗口元组 `(Window, fragment)` 恰好每窗口携带一片段，两个数字必然相等。
    /// 保留两字段是为了审计语义分开可读（窗口覆盖面 vs 片段密度）；若未来规则
    /// 形态变为「一窗口多片段」，两者才会分叉。
    pub window_count: usize,
    /// 形状/语义片段数（不含方法名本身；与 `window_count` 同源，见上）。
    pub fragment_count: usize,
}

/// 规则表自检面（只读；不改变匹配行为）。
pub fn rule_audit() -> Vec<RuleAudit> {
    RULES
        .iter()
        .map(|rule| RuleAudit {
            operation: rule.operation,
            method: rule.method,
            window_count: rule.windows.len(),
            fragment_count: rule.windows.len(),
        })
        .collect()
}

/// 归一化：去全部空白、反引号统一成双引号。
///
/// 让匹配对打包器的压缩/美化都不敏感；规则本身要求形状片段而非「关键词出现」。
fn normalize(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_whitespace())
        .map(|ch| if ch == '`' { '"' } else { ch })
        .collect()
}

// ============================================================
// 词法路径工具（Windows 形态路径在 POSIX 主机上 std Path 不识别 '\'；
// 与 identity.rs 同款词法处理，不查文件系统）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn host(platform: &str, executable: &str) -> LocalHost {
        LocalHost {
            host_id: "local".into(),
            platform: platform.into(),
            profile_dir: PathBuf::from("/tmp/profile"),
            executable: PathBuf::from(executable),
            pid: 1,
            started_at_ms: 1,
            epoch: 1,
            authority: "127.0.0.1:1".into(),
            package_sha256: String::new(),
        }
    }

    /// N-11：Windows 分支是纯词法推导（不查文件系统、不实机验证）。
    #[test]
    fn windows_package_path_is_lexically_derived() {
        let target = host(
            "windows",
            r"D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe",
        );
        let path = package_path(&target).expect("windows path");
        assert_eq!(
            path.to_string_lossy(),
            r"D:\Program Files\Deepseek-Harness\resources\app.asar"
        );
    }

    /// Mac 分支同款词法推导。
    #[test]
    fn macos_package_path_is_lexically_derived() {
        let target = host(
            "macos",
            "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness",
        );
        let path = package_path(&target).expect("macos path");
        assert_eq!(
            path.to_string_lossy(),
            "/Applications/DeepSeek Harness.app/Contents/Resources/app.asar"
        );
    }

    /// 未知平台不猜（`DSH_UNSUPPORTED`）。
    #[test]
    fn unknown_platform_is_unsupported() {
        let err = package_path(&host("linux", "/usr/bin/dsh")).expect_err("unsupported");
        assert_eq!(err.code, codes::UNSUPPORTED);
    }

    /// 方法切块只认 `'ns/method':{` 形态；普通键、多段路径与**空段**（N8：
    /// `'a/'`、`'/b'`）不算方法。（输入 = 原始文本，这里用无空白形态。）
    #[test]
    fn method_blocks_only_accept_single_slash_names() {
        let text = concat!(
            "constx={methods:{",
            "'session/list':{items}'a/b/c':{x}'plainkey':{y}'skills/list':{z}",
            "'ns/':{empty}'/method':{empty2}",
            "},};",
        );
        let blocks = method_blocks(text);
        let names: Vec<&str> = blocks.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(names, vec!["session/list", "skills/list"]);
        assert!(blocks[0].2.contains("items"));
    }

    /// Major-2（单元）：methods 区**之外**的伪锚点——前置注释、字符串字面量、
    /// `export const TYPERT` 尾区——都不产生块，无法遮蔽区内的真 descriptor。
    #[test]
    fn method_blocks_ignore_pseudo_anchors_outside_methods_region() {
        let text = concat!(
            "/*doc:'session/prompt':{pseudo-fragments}*/",
            "constdoc=\"'session/list':{\";",
            "constx={methods:{'session/list':{a}},};",
            "exportconstTYPERT={model:{'session/list':{tail}}};",
        );
        let blocks = method_blocks(text);
        assert_eq!(blocks.len(), 1, "只允许 methods 区内的锚点：{blocks:?}");
        assert_eq!(blocks[0].0, "session/list");
        assert!(blocks[0].2.contains("a"), "{}", blocks[0].2);
        assert!(
            !blocks[0].2.contains("tail") && !blocks[0].2.contains("pseudo"),
            "块不得吞入区外内容：{}",
            blocks[0].2
        );
    }

    /// Major-2（单元）：methods 区内同键多块——内容一致取其一；分歧 → 来源多义。
    #[test]
    fn method_index_flags_divergent_duplicate_blocks() {
        let mut identical = BTreeMap::new();
        identical.insert(
            Source::Commands,
            concat!(
                "constx={methods:{",
                "'commands/list':{a:1},",
                "'commands/list':{a:1},",
                "'commands/execute':{b},",
                "},},",
            )
            .to_string(),
        );
        let (index, ambiguous) = MethodIndex::new(&identical);
        assert!(ambiguous.is_empty(), "内容一致的副本取其一：{ambiguous:?}");
        assert!(index.get(Source::Commands, "commands/list").is_some());
        assert!(index.get(Source::Commands, "commands/execute").is_some());

        let mut divergent = BTreeMap::new();
        divergent.insert(
            Source::Commands,
            concat!(
                "constx={methods:{",
                "'commands/list':{a:1},",
                "'commands/list':{a:2},",
                "},},",
            )
            .to_string(),
        );
        let (_, ambiguous) = MethodIndex::new(&divergent);
        assert!(
            ambiguous.contains(&Source::Commands),
            "同键内容分歧 → 来源多义"
        );
    }

    /// Major-2（单元）：字符串/注释感知——注释与字符串里的 `methods:{` 不是区域。
    #[test]
    fn methods_region_ignores_comment_and_string_occurrences() {
        let commented = "/*config:methods:{fake}*/constx={methods:{'a/b':{1}},};";
        let blocks = method_blocks(commented);
        assert_eq!(blocks.len(), 1, "注释里的 methods 不是区域起点");
        assert_eq!(blocks[0].0, "a/b");

        let in_string = "consthint='seemethods:{...}';constx={methods:{'a/b':{1}},};";
        let blocks = method_blocks(in_string);
        assert_eq!(blocks.len(), 1, "字符串里的 methods 不是区域起点");
        assert_eq!(blocks[0].0, "a/b");

        // 区域不配对（methods 对象永不闭合）→ 无锚点（不吞尾）。
        let unclosed = "constx={methods:{'a/b':{1},;";
        assert!(method_blocks(unclosed).is_empty());
    }

    /// N6（单元）：原始文本上的行注释（含 `methods:{` / 锚点形态诱饵）到换行为
    /// 止——不得吞掉后续 methods 区。
    #[test]
    fn method_blocks_survive_line_comment_before_region() {
        let text = concat!(
            "// note: the generator emits methods:{fake} and 'session/prompt':{x}\n",
            "const x = { methods: { 'a/b':{1} }, };",
        );
        let blocks = method_blocks(text);
        assert_eq!(blocks.len(), 1, "行注释不得吞掉后续区域");
        assert_eq!(blocks[0].0, "a/b");
    }

    /// N2（单元）：`submethods:` 里的 `methods:{` 子串不是区域起点（属性键前界）。
    #[test]
    fn methods_region_requires_property_key_position() {
        let text = concat!(
            "const shadow = { submethods: { 'a/b':{fake}, 'c/d':{fake2} }, };",
            "const x = { methods: { 'a/b':{real} }, };",
        );
        let blocks = method_blocks(text);
        assert_eq!(blocks.len(), 1, "伪区不得成为区域起点：{blocks:?}");
        assert!(blocks[0].2.contains("real"));
        assert!(!blocks[0].2.contains("fake"));
    }

    /// N4（单元）：块文本剥离注释（含旧锚点/片段），字符串内容保留（申报边界）。
    #[test]
    fn block_text_strips_comments_but_keeps_strings() {
        let text = concat!(
            "const x = { methods: {",
            "'a/b':{ keep: 1, /* stale 'c/d':{gone} and \"e/f\":{gone2} */ end: 2 },",
            "}, };",
        );
        let blocks = method_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert!(
            !blocks[0].2.contains("gone") && !blocks[0].2.contains("gone2"),
            "注释不进块文本：{}",
            blocks[0].2
        );
        assert!(blocks[0].1.contains("keep") && blocks[0].1.contains("end"));

        // 字符串里的 `/*` 不是注释：内容保留（字符串承载片段仍可达 = 已申报边界）。
        let in_string = "const x = { methods: { 'a/b':{ d: 'has /* kept */ mark' } }, };";
        let blocks = method_blocks(in_string);
        assert!(
            blocks[0].1.contains("kept"),
            "字符串内容保留：{}",
            blocks[0].1
        );
    }

    // ============================================================
    // 2026-11-16 宿主 session/prompt schema 漂移：形状探针 + Send 新窗口
    // ============================================================
    //
    // 锚点 provenance：2026-11-16 已装宿主 `/Applications/DeepSeek Harness.app`
    // （CFBundleShortVersionString = 0.2.2-rc.1）`app.asar` 实读——
    // * `dsh/desktop-runtime.json`：`release.version = "0.2.2-rc.1"`、
    //   `release.hostProtocolVersion = 4`；
    // * `@agentclientprotocol/sdk/dist/schema/zod.gen.js`：
    //   `zPromptRequest = { sessionId: zSessionId, prompt: z.array(zContentBlock) }`
    //   （ACP 形态；v2 同形）。
    // 测试输入仍是 synthetic 拼装（夹具纪律与全文件一致：结构真实、取值合成）。

    /// 内存 asar（16 字节前缀 + JSON 目录 + 顺序数据区；目录格式真实）。
    fn shape_asar(files: &[(&str, &str)]) -> Vec<u8> {
        let mut root = serde_json::json!({ "files": {} });
        let mut payload: Vec<u8> = Vec::new();
        let mut offset: u64 = 0;
        for (name, content) in files {
            let mut node = &mut root;
            let parts: Vec<&str> = name.trim_start_matches('/').split('/').collect();
            for (idx, part) in parts.iter().enumerate() {
                let files_obj = node
                    .get_mut("files")
                    .and_then(|v| v.as_object_mut())
                    .expect("files object");
                if idx + 1 == parts.len() {
                    files_obj.insert(
                        (*part).to_string(),
                        serde_json::json!({
                            "size": content.len() as u64,
                            "offset": offset.to_string(),
                        }),
                    );
                    break;
                }
                node = files_obj
                    .entry((*part).to_string())
                    .or_insert_with(|| serde_json::json!({"files": {}}));
            }
            offset += content.len() as u64;
            payload.extend_from_slice(content.as_bytes());
        }
        let json = serde_json::to_vec(&root).expect("asar directory json");
        let header_size = 8 + json.len() as u64;
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&(header_size as u32).to_le_bytes());
        out.extend_from_slice(&((json.len() as u32) + 6).to_le_bytes());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(&json);
        out.extend_from_slice(&payload);
        out
    }

    /// 内存包源：形状探针专用（只读假句柄）。
    struct ShapePackage(Vec<u8>);

    impl ContractFileSource for ShapePackage {
        fn open(&self, _path: &Path) -> Result<Box<dyn ContractFileReader + '_>, DshError> {
            Ok(Box::new(ShapeReader { bytes: &self.0 }))
        }
    }

    struct ShapeReader<'a> {
        bytes: &'a [u8],
    }

    impl ContractFileReader for ShapeReader<'_> {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn read_range(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, DshError> {
            let start = usize::try_from(offset)
                .map_err(|_| DshError::new(codes::STATE_IO, "shape fixture: offset overflow"))?;
            let end = start
                .checked_add(usize::try_from(len).unwrap_or(usize::MAX))
                .filter(|end| *end <= self.bytes.len())
                .ok_or_else(|| {
                    DshError::new(codes::STATE_IO, "shape fixture: range beyond package")
                })?;
            Ok(self.bytes[start..end].to_vec())
        }
    }

    /// ACP SDK zod.gen.js 的实读形态摘录（0.2.2-rc.1；zPromptRequest + zContentBlock
    /// text 面——归一化后命中探针的两个内容锚）。
    const ACP_ZOD_GEN_JS: &str = r#"
export const zPromptRequest = z.object({
    sessionId: zSessionId,
    prompt: z.array(zContentBlock),
    _meta: defaultOnError(z.record(z.string(), z.unknown()).nullish(), () => undefined),
});
export const zContentBlock = z.union([
    zTextContent.and(z.object({
        type: z.literal("text"),
    })),
    zImageContent.and(z.object({
        type: z.literal("image"),
    })),
]);
"#;

    /// desktop-runtime.json 新窗口形态（实读：0.2.2-rc.1）。
    const RUNTIME_JSON_NEW: &str = r#"{"schemaVersion":1,"release":{"schemaVersion":1,"version":"0.2.2-rc.1","hostProtocolVersion":4}}"#;

    const RUNTIME_ENTRY: &str = "/dsh/desktop-runtime.json";
    const ACP_ZOD_ENTRY: &str = "/dsh/node_modules/@agentclientprotocol/sdk/dist/schema/zod.gen.js";

    /// 仅有 ACP 形态而缺少实证 request 契约时，Send 必须失配。
    #[test]
    fn send_rule_rejects_acp_only_descriptor() {
        // SessionController 只有不含旧片段的 session/prompt 块（旧形状组失配）。
        let drifted_session = r#"
const $remote = createTypertRemoteInfo({
  methods: {
    'session/prompt': {
      service: 'sessionController',
      jsonSchema: {"type":"object","properties":{"sessionId":{"type":"string"},"prompt":{"type":"array"}},"required":["sessionId","prompt"]},
    },
  },
});
"#;
        let session_entry = format!(
            "/dsh/node_modules/@deepseek-ai/{}",
            Source::SessionController.suffix()
        );
        let files: Vec<(&str, &str)> = vec![
            (RUNTIME_ENTRY, RUNTIME_JSON_NEW),
            (ACP_ZOD_ENTRY, ACP_ZOD_GEN_JS),
            (session_entry.as_str(), drifted_session),
        ];
        let package = ShapePackage(shape_asar(&files));
        let report = read_report_with(
            &host(
                "macos",
                "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness",
            ),
            CONTRACT_RULE_VERSION,
            &package,
            &ContractBudget::DEFAULT,
        )
        .expect("read");
        assert!(
            !report.evidence.matches.contains_key(&Operation::Send),
            "仅有 ACP 形态时 Send 必须失配；unresolved={:?} inconclusive={:?}",
            report.unresolved,
            report.inconclusive
        );
    }

    // 规则版本随实证锚收紧而递增（精确断言见 `contract_rule_version_is_10`；
    // v7 的锚点集合由 send_rule_matches_proven_request_shape 与
    // send_rule_unmatched_when_* 系列共同钉住）。
    fn request_shape_matches_send(descriptor: &str) -> bool {
        let session_entry = format!(
            "/dsh/node_modules/@deepseek-ai/{}",
            Source::SessionController.suffix()
        );
        let package = ShapePackage(shape_asar(&[(session_entry.as_str(), descriptor)]));
        let report = read_report_with(
            &host(
                "macos",
                "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness",
            ),
            CONTRACT_RULE_VERSION,
            &package,
            &ContractBudget::DEFAULT,
        )
        .expect("read fixture");
        report.evidence.matches.contains_key(&Operation::Send)
    }

    const PROVEN_REQUEST_DESCRIPTOR: &str = r#"
const $remote = createTypertRemoteInfo({
  methods: {
    'session/prompt': {
      service: 'sessionController',
      parameters: [{name: 'request', mode: 'strict', jsonSchema: {"type":"object","properties":{"requestId":{"type":"string"},"sessionId":{"type":"string"},"mode":{"type":"string","enum":["queue","steer"]},"content":{"type":"array","items":{"properties":{"type":{"type":"string","const":"text"},"text":{"type":"string"}},"required":["type","text"]}}},"required":["requestId","sessionId","mode","content"]}}],
      result: {"type":"object","properties":{"accepted":{"type":"boolean","const":true}},"required":["accepted"]},
    },
  },
});
"#;

    const EXTRA_REQUIRED_PARAMETER: &str = ", { name: 'other', mode: 'strict', jsonSchema: {\"type\":\"object\",\"required\":[\"x\"]}}";

    #[test]
    fn send_rule_rejects_nested_array_leading_entry() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR.replace("parameters: [{", "parameters: [[], {");
        assert_ne!(descriptor, PROVEN_REQUEST_DESCRIPTOR);
        assert!(
            !request_shape_matches_send(&descriptor),
            "数组首项不能是嵌套数组"
        );
    }

    #[test]
    fn send_rule_rejects_nested_array_trailing_entry() {
        let descriptor =
            PROVEN_REQUEST_DESCRIPTOR.replace("}],\n      result:", "}, []],\n      result:");
        assert_ne!(descriptor, PROVEN_REQUEST_DESCRIPTOR);
        assert!(
            !request_shape_matches_send(&descriptor),
            "数组末项不能是嵌套数组"
        );
    }

    #[test]
    fn send_rule_rejects_string_first_extra_parameter() {
        let descriptor =
            PROVEN_REQUEST_DESCRIPTOR.replace("parameters: [{", "parameters: ['str', {");
        assert_ne!(descriptor, PROVEN_REQUEST_DESCRIPTOR);
        assert!(
            !request_shape_matches_send(&descriptor),
            "字符串首项不能被忽略"
        );
    }

    #[test]
    fn send_rule_allows_trailing_comma_single_parameter() {
        let descriptor =
            PROVEN_REQUEST_DESCRIPTOR.replace("}],\n      result:", "}, ],\n      result:");
        assert_ne!(descriptor, PROVEN_REQUEST_DESCRIPTOR);
        assert!(request_shape_matches_send(&descriptor), "单参数允许尾逗号");
    }

    #[test]
    fn send_rule_marks_normalized_equal_raw_different_duplicates_unresolved() {
        // 双引号字符串内的文本与模板串归一化后相同，原文却包含真实第二个参数。
        let first = PROVEN_REQUEST_DESCRIPTOR.replace(
            "}],\n      result:",
            " , description: \"`` }, { name: 'other', description: ``\"}],\n      result:",
        );
        let second = PROVEN_REQUEST_DESCRIPTOR.replace(
            "}],\n      result:",
            " , description: `\"` }, { name: 'other', description: `\"`}],\n      result:",
        );
        let first_block = method_blocks(&first).remove(0);
        let second_block = method_blocks(&second).remove(0);
        assert_ne!(first_block.1, second_block.1, "原文必须不同");
        assert_eq!(first_block.2, second_block.2, "归一化文本必须相同");
        assert!(request_shape_matches_send(&first), "首份单参数必须合法");
        assert!(
            !request_shape_matches_send(&second),
            "副本必须包含真实第二参数"
        );
        let descriptor = first.replace(
            "    },\n  },\n});",
            &format!("    }},\n    {},\n  }},\n}});", second_block.1),
        );
        let blocks = method_blocks(&descriptor);
        assert_eq!(blocks.len(), 2, "必须构造同键的两个方法块");
        let session_entry = format!(
            "/dsh/node_modules/@deepseek-ai/{}",
            Source::SessionController.suffix()
        );
        let package = ShapePackage(shape_asar(&[(session_entry.as_str(), &descriptor)]));
        let report = read_report_with(
            &host(
                "macos",
                "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness",
            ),
            CONTRACT_RULE_VERSION,
            &package,
            &ContractBudget::DEFAULT,
        )
        .expect("读取描述符");
        assert!(
            report.unresolved.contains(&Operation::Send),
            "原文不同的副本应使来源多义"
        );
        assert!(
            !report.evidence.matches.contains_key(&Operation::Send),
            "多义来源不得匹配 Send"
        );
    }

    /// 销项复审 P2-5（文件候选去重比 raw）：同后缀多副本 normalized 相等但
    /// **raw 不同**（双引号串 vs 模板串边界——第二参数真假在归一化后不可辨）
    /// → 该来源多义记入 unresolved，matches 不得取首文件命中。旧实现只比
    /// normalized（相等即忽略后续文件），raw 结构分歧被藏、首副本照常命中。
    #[test]
    fn send_rule_marks_raw_divergent_same_suffix_files_unresolved() {
        // 与块级歧义夹具（send_rule_marks_normalized_equal_raw_different_duplicates_unresolved）
        // 同构：双引号串内嵌反引号 vs 模板串内嵌双引号——raw 不同、归一化相同。
        let first = PROVEN_REQUEST_DESCRIPTOR.replace(
            "}],\n      result:",
            " , description: \"`` }, { name: 'other', description: ``\"}],\n      result:",
        );
        let second = PROVEN_REQUEST_DESCRIPTOR.replace(
            "}],\n      result:",
            " , description: `\"` }, { name: 'other', description: `\"`}],\n      result:",
        );
        let first_block = method_blocks(&first).remove(0);
        let second_block = method_blocks(&second).remove(0);
        assert_ne!(first_block.1, second_block.1, "两副本原文必须不同");
        assert_eq!(first_block.2, second_block.2, "两副本归一化文本必须相同");
        let suffix = Source::SessionController.suffix();
        let entry_a = format!("/dsh/node_modules/@deepseek-ai/{suffix}");
        let entry_b = format!("/dsh/node_modules/@deepseek-ai/dup/{suffix}");
        let files: Vec<(&str, &str)> =
            vec![(entry_a.as_str(), &first), (entry_b.as_str(), &second)];
        let package = ShapePackage(shape_asar(&files));
        let report = read_report_with(
            &host(
                "macos",
                "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness",
            ),
            CONTRACT_RULE_VERSION,
            &package,
            &ContractBudget::DEFAULT,
        )
        .expect("读取双副本包");
        assert!(
            report.unresolved.contains(&Operation::Send),
            "raw 分歧的同后缀副本必须使来源多义；unresolved={:?} inconclusive={:?}",
            report.unresolved,
            report.inconclusive
        );
        assert!(
            !report.evidence.matches.contains_key(&Operation::Send),
            "多义来源不得取首副本命中 Send"
        );
    }

    #[test]
    fn contract_rule_version_is_10() {
        assert_eq!(CONTRACT_RULE_VERSION, 10);
    }

    #[test]
    fn send_rule_rejects_accepted_anchor_outside_result() {
        const REQUEST_PROPERTIES: &str = "\"properties\":{\"requestId\"";
        const DECOY_PROPERTIES: &str = "\"properties\":{\"accepted\":{\"type\":\"boolean\",\"const\":true},\"decoy\":{\"required\":[\"accepted\"]},\"requestId\"";
        const ACCEPTED_RESULT: &str = "result: {\"type\":\"object\",\"properties\":{\"accepted\":{\"type\":\"boolean\",\"const\":true}},\"required\":[\"accepted\"]}";
        let descriptor = PROVEN_REQUEST_DESCRIPTOR
            .replace(REQUEST_PROPERTIES, DECOY_PROPERTIES)
            .replace(ACCEPTED_RESULT, "result: {\"type\":\"object\"}");
        assert!(
            descriptor.contains(DECOY_PROPERTIES),
            "必须在请求中插入诱饵"
        );
        assert!(
            !descriptor.contains(ACCEPTED_RESULT),
            "结果区必须移除受理结构"
        );
        assert!(
            !request_shape_matches_send(&descriptor),
            "请求中的受理锚不能代替结果区的受理结构"
        );
    }

    #[test]
    fn send_rule_rejects_strict_anchor_scattered() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR
            .replace("name: 'request', mode: 'strict'", "name: 'request'")
            .replace("      result:", "      mode: 'strict',\n      result:");
        assert!(descriptor.contains("name: 'request', jsonSchema:"));
        assert!(descriptor.contains("      mode: 'strict',\n      result:"));
        assert!(
            !request_shape_matches_send(&descriptor),
            "参数表外的 strict 锚不能代替请求参数自己的模式"
        );
    }

    #[test]
    fn send_rule_accepts_real_package_face_layout() {
        assert!(
            request_shape_matches_send(PROVEN_REQUEST_DESCRIPTOR),
            "实证包的参数、请求模式和结果结构均在正确区域时应匹配"
        );
    }

    /// 销项复审 P2-4（跨面拼接封堵）：请求 schema 面按**互不重叠的连续段**
    /// 判定——两个 jsonSchema: 面（第一面缺 sessionId 类型锚、第二面缺
    /// requestId 类型锚），九锚跨面拼齐但无单一完整面 → Send 不得命中。
    /// 旧实现「每个 start 锚划 [start..end)」区间互相重叠（第一面的区间
    /// [jsonSchema#1..result:) 吞掉第二面），跨面拼接的锚能在重叠区凑数命中。
    #[test]
    fn send_rule_rejects_cross_face_stitched_anchors() {
        // PROVEN 请求 schema 体（jsonSchema: { 与其闭括号 } 之间的逐字内容；
        // 不含最外层花括号——花括号由拼接面补齐）。
        const SCHEMA_BODY: &str = concat!(
            r#""type":"object","properties":{"requestId":{"type":"string"},"sessionId":{"type":"string"},"#,
            r#""mode":{"type":"string","enum":["queue","steer"]},"content":{"type":"array","items":{"properties":{"type":{"type":"string","const":"text"},"text":{"type":"string"}},"required":["type","text"]}}},"#,
            r#""required":["requestId","sessionId","mode","content"]"#,
        );
        const SESSION_ID_TYPE: &str = r#""sessionId":{"type":"string"},"#;
        const REQUEST_ID_TYPE: &str = r#""requestId":{"type":"string"},"#;
        // 面一缺 sessionId 类型锚；面二缺 requestId 类型锚——其余锚两面都有。
        let face_a = SCHEMA_BODY.replace(SESSION_ID_TYPE, "");
        let face_b = SCHEMA_BODY.replace(REQUEST_ID_TYPE, "");
        assert_ne!(face_a, SCHEMA_BODY, "面一必须去掉 sessionId 类型锚");
        assert_ne!(face_b, SCHEMA_BODY, "面二必须去掉 requestId 类型锚");
        assert!(
            face_a.contains("sessionId") && face_b.contains("requestId"),
            "required 全键列表仍在两面（缺的只是各自的类型锚）"
        );
        // 布局：两个 jsonSchema: 连续出现、result: 在两面之后——旧实现第一面
        // 区间 [jsonSchema#1..result:) 覆盖两面，跨面拼齐九锚。花括号逐字对齐
        // PROVEN（face_a 闭 schema+entry 两层，face_b 只闭自己一层）。
        let needle = String::from("jsonSchema: {") + SCHEMA_BODY + "}}],\n      result:"; // 闭 schema + entry 两层（PROVEN 逐字形态）
        let two_faces = String::from("jsonSchema: {")
            + &face_a
            + "}}],\n      jsonSchema: {"
            + &face_b
            + "},\n      result:";
        // 花括号配平核对：needle/two_faces 在 face_a 之后都要闭 schema+entry
        // 两层（PROVEN 的 `}}],`），face_b 只闭自己一层。
        assert!(needle.ends_with("}}],\n      result:"));
        assert!(two_faces.contains("}}],\n      jsonSchema: {"));
        let descriptor = PROVEN_REQUEST_DESCRIPTOR.replacen(&needle, &two_faces, 1);
        assert_ne!(descriptor, PROVEN_REQUEST_DESCRIPTOR, "必须构造双面布局");
        assert!(
            !request_shape_matches_send(&descriptor),
            "九锚跨面拼齐但无单一完整面 → Send 不得命中"
        );
        // 对照：单面完整形态（真机布局）保持命中。
        assert!(
            request_shape_matches_send(PROVEN_REQUEST_DESCRIPTOR),
            "单一完整面照常命中（封堵不误伤）"
        );
    }

    #[test]
    fn send_rule_rejects_extra_parameter_with_template_string_quotes() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR
            .replace("name: 'request'", "name: 'request', description: `\"`")
            .replace(
                "}],\n      result:",
                "}, { name: 'other', description: `\"`, mode: 'strict', jsonSchema: {\"type\":\"object\",\"required\":[\"x\"]} }],\n      result:",
            );
        assert!(descriptor.contains("name: 'request', description: `\"`"));
        assert!(descriptor.contains("name: 'other', description: `\"`"));
        assert!(
            !request_shape_matches_send(&descriptor),
            "模板串不得掩盖第二个参数"
        );
    }

    #[test]
    fn send_rule_rejects_extra_parameter_identifier_entry() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR.replace(
            "}],\n      result:",
            "},\n        requiredParameter\n      ],\n      result:",
        );
        assert!(descriptor.contains("requiredParameter"));
        assert!(
            !request_shape_matches_send(&descriptor),
            "标识符形式的第二个参数必须使发送契约失配"
        );
    }

    #[test]
    fn send_rule_rejects_extra_parameter_spread_entry() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR.replace(
            "}],\n      result:",
            "}, /* doc: extra param */ ...additionalParameters],\n      result:",
        );
        assert!(descriptor.contains("...additionalParameters"));
        assert!(
            !request_shape_matches_send(&descriptor),
            "展开形式的第二个参数必须使发送契约失配"
        );
    }

    #[test]
    fn send_rule_rejects_extra_parameter_with_reordered_fields() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR.replace(
            "}],\n      result:",
            "}, { mode: 'strict', name: 'other', jsonSchema: {\"type\":\"object\",\"required\":[\"x\"]} }],\n      result:",
        );
        assert_ne!(descriptor, PROVEN_REQUEST_DESCRIPTOR, "必须插入第二个参数");
        assert!(
            !request_shape_matches_send(&descriptor),
            "字段换序的第二个参数必须使发送契约失配"
        );
    }

    #[test]
    fn send_rule_rejects_extra_parameter_with_interposed_comment() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR.replace(
            "}],\n      result:",
            "} /* doc: extra param */, { name: 'other', mode: 'strict', jsonSchema: {\"type\":\"object\",\"required\":[\"x\"]} }],\n      result:",
        );
        assert_ne!(
            descriptor, PROVEN_REQUEST_DESCRIPTOR,
            "必须插入带注释的第二个参数"
        );
        assert!(
            !request_shape_matches_send(&descriptor),
            "参数间的注释不得使第二个参数逃过检查"
        );
    }

    #[test]
    fn send_rule_counts_single_request_parameter_only() {
        assert!(request_shape_matches_send(PROVEN_REQUEST_DESCRIPTOR));
    }

    #[test]
    fn send_rule_v7_rejects_extra_required_parameter() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR.replace(
            "}],\n      result:",
            &format!("}}{EXTRA_REQUIRED_PARAMETER}],\n      result:"),
        );
        let blocks = method_blocks(&descriptor);
        let block = blocks
            .iter()
            .find(|(name, _, _)| name == "session/prompt")
            .expect("方法块");
        // block.1 是原始块（raw）：断言用原样片段（归一化连接串是匹配器的
        // 内部表示，不再出现在块文本上）。
        assert!(
            block.1.contains(EXTRA_REQUIRED_PARAMETER),
            "追加参数必须位于方法块内"
        );
        assert!(
            !request_shape_matches_send(&descriptor),
            "第二个必填参数必须使发送契约失配"
        );
    }

    #[test]
    fn send_rule_v7_still_matches_single_parameter_proven_shape() {
        let blocks = method_blocks(PROVEN_REQUEST_DESCRIPTOR);
        let block = blocks
            .iter()
            .find(|(name, _, _)| name == "session/prompt")
            .expect("方法块");
        assert!(!block.1.contains("},{name:'"), "单参数实证形态不得命中负锚");
        assert!(request_shape_matches_send(PROVEN_REQUEST_DESCRIPTOR));
    }

    /// 实证的 request 参数与明确受理结果均在位时允许发送。
    #[test]
    fn send_rule_matches_proven_request_shape() {
        assert!(request_shape_matches_send(PROVEN_REQUEST_DESCRIPTOR));
    }

    /// 缺少受理字段时不得将请求契约视为匹配。
    #[test]
    fn send_rule_unmatched_when_acceptance_anchor_missing() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR
            .replace(
                "\"accepted\":{\"type\":\"boolean\",\"const\":true}",
                "\"other\":{\"type\":\"boolean\"}",
            )
            .replace("\"required\":[\"accepted\"]", "\"required\":[\"other\"]");
        assert!(!request_shape_matches_send(&descriptor));
    }

    /// request 参数名缺席时，即使旧的字段窗口齐全也不得放行。
    #[test]
    fn send_rule_unmatched_when_request_param_missing() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR.replace("name: 'request'", "name: 'other'");
        assert!(!request_shape_matches_send(&descriptor));
    }

    /// strict 校验缺席时不得放行。
    #[test]
    fn send_rule_unmatched_when_strict_validation_missing() {
        let descriptor = PROVEN_REQUEST_DESCRIPTOR.replace("mode: 'strict'", "mode: 'loose'");
        assert!(!request_shape_matches_send(&descriptor));
    }

    /// accepted 不在必填列表时不得放行，即使类型和值的锚仍在位。
    #[test]
    fn send_rule_unmatched_when_acceptance_not_required() {
        let descriptor =
            PROVEN_REQUEST_DESCRIPTOR.replace("\"required\":[\"accepted\"]", "\"required\":[]");
        assert!(!request_shape_matches_send(&descriptor));
    }

    /// 两代窗口同时漂移 → Send 失配（漂移检测不因 OR 语义被洗掉）。
    #[test]
    fn send_rule_unmatched_when_both_windows_drift() {
        let drifted_session = r#"
const $remote = createTypertRemoteInfo({
  methods: {
    'session/prompt': {
      service: 'sessionController',
      jsonSchema: {"type":"object","properties":{"sessionId":{"type":"string"}}},
    },
  },
});
"#;
        let drifted_acp = r#"
export const zPromptRequest = z.object({
    sessionId: zSessionId,
    prompt: z.string(),
});
"#;
        let session_entry = format!(
            "/dsh/node_modules/@deepseek-ai/{}",
            Source::SessionController.suffix()
        );
        let files: Vec<(&str, &str)> = vec![
            (RUNTIME_ENTRY, RUNTIME_JSON_NEW),
            (ACP_ZOD_ENTRY, drifted_acp),
            (session_entry.as_str(), drifted_session),
        ];
        let package = ShapePackage(shape_asar(&files));
        let report = read_report_with(
            &host(
                "macos",
                "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness",
            ),
            CONTRACT_RULE_VERSION,
            &package,
            &ContractBudget::DEFAULT,
        )
        .expect("read");
        assert!(
            !report.evidence.matches.contains_key(&Operation::Send),
            "两代窗口同时漂移必须失配"
        );
    }
}
