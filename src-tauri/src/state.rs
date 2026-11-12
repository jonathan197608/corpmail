//! 应用状态：SQLite 单连接（`Mutex` 内部可变性）+ 业务配置读写
//!
//! 设计 §2/§13-R6：全库走单一 `Mutex<Connection>`，WAL 模式读写不互阻。
//! 账号密码不落库（经 `secrets` 走 OS keychain，keyfile 回退，设计 §5.4）。
//!
//! 锁策略：全部 Mutex/RwLock 一律 `unwrap_or_else(|e| e.into_inner())` 恢复中毒锁。
//! 中毒只说明某次持锁写入被 panic 打断，锁内数据本身通常仍可用；沿用 `.expect`
//! 会让一次 panic 演变为全应用连锁 panic（审计 P2-11）。

use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// settings 键名。键名即持久化契约：改名等于丢配置，必须随迁移同步
pub mod keys {
    pub const ACCOUNT_PREFIX: &str = "account_prefix";
    pub const ACCOUNT_DOMAIN: &str = "account_domain";
    pub const DISPLAY_NAME: &str = "display_name";
    /// 曾由发件箱自动推断过显示名（值 = 推断出的名字）：用户之后在设置页
    /// 主动清空显示名，不应在下次刷新时又被自动回填。随账号配置落在
    /// corpmail.toml；不写 settings 表（store_is_empty() 按其行数判定空库首设）
    pub const DISPLAY_NAME_AUTO: &str = "display_name_auto";
    pub const SMTP_HOST: &str = "smtp_host";
    pub const SMTP_PORT: &str = "smtp_port";
    /// 发信加密方式（auto|implicit|starttls|plain；auto = 按端口推断）
    pub const SMTP_SECURITY: &str = "smtp_security";
    pub const RECV_HOST: &str = "recv_host";
    pub const RECV_PORT: &str = "recv_port";
    /// 收信加密方式（auto|implicit|starttls|plain；auto = 按端口推断）
    pub const RECV_SECURITY: &str = "recv_security";
    /// 单封收件大小上限（MB）：超出只拉信头存占位邮件，避免整封驻留内存
    pub const MAX_MAIL_MB: &str = "max_mail_mb";
    pub const SYNC_ON_START: &str = "sync_on_start";
    pub const SYNC_INTERVAL_MIN: &str = "sync_interval_min";
    pub const FIRST_FETCH_DAYS: &str = "first_fetch_days";
    pub const INLINE_ATTACH_MAX_MB: &str = "inline_attach_max_mb";
    /// 单文件 Office 预览上限（MB，默认 20）
    pub const OFFICE_PREVIEW_MAX_MB: &str = "office_preview_max_mb";
    pub const CLEAN_SENT_BULK_DAYS: &str = "clean_sent_bulk_days";
    pub const KEEP_INBOX_DAYS: &str = "keep_inbox_days";
    pub const KEEP_SENT_DAYS: &str = "keep_sent_days";
    /// 签名档（M3-2，多行纯文本）：撰写新邮件时预填到正文底部；空 = 不启用
    pub const SIGNATURE: &str = "signature";
    /// 每页显示条数（默认 10，列表页顶栏可选；URL per 参数覆盖）
    pub const PAGE_SIZE: &str = "page_size";
    /// 联系人统计一次性修复标记（v11）：早期把 updated_at 写成入库时间，
    /// 启动时按历史邮件重算最近联系时间/往来次数，完成后置 1
    pub const CONTACTS_STATS_FIXED: &str = "contacts_stats_fixed";
    // 收件状态（M4，设计 §3 schema 注释"状态"节）
    pub const LAST_SYNC_AT: &str = "last_sync_at";
    pub const LAST_UIDVALIDITY: &str = "last_uidvalidity";
    pub const LAST_UID: &str = "last_uid";
    /// 发件箱（服务器已发送）独立游标：UIDVALIDITY 按邮箱独立，不可与收件箱共用
    pub const LAST_UIDVALIDITY_SENT: &str = "last_uidvalidity_sent";
    pub const LAST_UID_SENT: &str = "last_uid_sent";
    /// 首拉阶段化（发件箱优先）：各箱「首拉范围已完整拉取」标志（"1" = 完成）。
    /// 任一箱未完成时收件轮次按「先发件箱、后收件箱」串行推进——某箱中断
    /// 重试只针对未完成箱，已完成箱不再重跑；跨重启仍生效（标志持久在库）。
    /// 改动首拉范围（清游标重拉）时同步清除，重新进入阶段化模式
    pub const FIRST_PULL_DONE_SENT: &str = "first_pull_done_sent";
    pub const FIRST_PULL_DONE_INBOX: &str = "first_pull_done_inbox";
    /// 数据归属账号标记（DB settings 行，值 = 完整地址小写）。向导配置完成时
    /// 写入；重装/换机后向导指向既有数据目录时，据此判定库能否被当前账号
    /// 直接沿用（历史更早的库无此行，退回发件 From 模式推导，见 store_owner_conn）
    pub const STORE_OWNER: &str = "store_owner";
    /// 是否允许加载邮件正文里的远程图片（"1" = 允许，默认禁止）：关闭时
    /// `/proxy/image` 直接回内置透明占位图，**不发起任何网络请求**——
    /// 否则每打开一封营销邮件就把本机 IP 与阅读时间泄露给发件方（审计 R4/R36）
    pub const ALLOW_REMOTE_IMAGES: &str = "allow_remote_images";

    /// 自定义数据目录指针（存独立配置文件 corpmail.toml，随锚点目录常驻）
    pub const DATA_DIR: &str = "data_dir_override";
}

/// 账号域名默认值（设计确认项 K 原为固定后缀；自 v0.1.1 起作为可配置设置项）
pub const ACCOUNT_DOMAIN_DEFAULT: &str = "@chinatelecom.cn";

/// 预填默认值（设计 §8；向导中可改）
pub const SMTP_HOST_DEFAULT: &str = "smtp.chinatelecom.cn";
pub const SMTP_PORT_DEFAULT: &str = "465";
pub const RECV_HOST_DEFAULT: &str = "imap.chinatelecom.cn";
pub const RECV_PORT_DEFAULT: &str = "993";

/// 加密方式默认值：auto = 按端口推断（465/993 隐式 TLS，其余 STARTTLS）
pub const SECURITY_DEFAULT: &str = "auto";

/// 单封收件大小上限默认值（MB）
pub const MAX_MAIL_MB_DEFAULT: &str = "50";

/// 单封收件大小上限区间（MB）：下限 1（0 会让所有邮件都成占位），
/// 上限 200（再大单封解析峰值内存不可控，且企业邮箱极少超过 50MB）
pub const MAX_MAIL_MB_MIN: u32 = 1;
pub const MAX_MAIL_MB_MAX: u32 = 200;

/// 全部 settings 键的默认值（未配置时表单回显用）
pub fn defaults() -> Vec<(&'static str, &'static str)> {
    vec![
        (keys::ACCOUNT_DOMAIN, ACCOUNT_DOMAIN_DEFAULT),
        (keys::SMTP_HOST, SMTP_HOST_DEFAULT),
        (keys::SMTP_PORT, SMTP_PORT_DEFAULT),
        (keys::SMTP_SECURITY, SECURITY_DEFAULT),
        (keys::RECV_HOST, RECV_HOST_DEFAULT),
        (keys::RECV_PORT, RECV_PORT_DEFAULT),
        (keys::RECV_SECURITY, SECURITY_DEFAULT),
        (keys::MAX_MAIL_MB, MAX_MAIL_MB_DEFAULT),
        (keys::SYNC_ON_START, "true"),
        (keys::SYNC_INTERVAL_MIN, "30"),
        (keys::FIRST_FETCH_DAYS, "0"),
        (keys::INLINE_ATTACH_MAX_MB, "20"),
        (keys::CLEAN_SENT_BULK_DAYS, "365"),
        (keys::KEEP_INBOX_DAYS, "3650"),
        (keys::KEEP_SENT_DAYS, "3650"),
        (keys::SIGNATURE, ""),
        (keys::PAGE_SIZE, "10"),
        // 远程图片默认阻止：不联网即无隐私泄露，用户按需在设置里开启
        (keys::ALLOW_REMOTE_IMAGES, "0"),
    ]
}

/// 新邮件通知钩子类型（收件线程完成后调用，参数：本轮开始时刻、本轮新增数）
pub(crate) type NotifyHook = std::sync::Arc<dyn Fn(i64, i64) + Send + Sync>;

/// 托盘角标刷新钩子类型（未读数变化后调用；测试环境 None 不触发）
pub(crate) type TrayHook = std::sync::Arc<dyn Fn() + Send + Sync>;

/// 发件显示名推断的互斥与失败节流（审计 C4）。
/// 一个 Mutex 同时承担「check-then-act 全程互斥」与「推断失败 60s 不再扫库」，
/// 避免 8 个触发点并发各写一次 corpmail.toml。
struct NameProbe {
    last_miss: Option<std::time::Instant>,
}

/// AppState 的库与目录载体（审计 R19：db 连接与数据目录同一把锁绑定，
/// 见 AppState.store 字段注释——换库与换目录必须原子，读者不得见到错配组合）
struct Store {
    db: Connection,
    data_dir: PathBuf,
}

/// 迁移进度快照（后台迁移期间经事件回传前端展示）。
/// phase 语义：`attach`=等待收件收尾 `snapshot`=数据库快照(VACUUM) `attachments`=附件复制 `pointer`=写指针 `done`
#[derive(Clone, Debug, serde::Serialize)]
pub struct MigrationProgress {
    pub phase: &'static str,
    pub detail: String,
    /// 附件已复制文件数（非附件阶段恒 0）
    pub files_done: usize,
    /// 附件文件总数（snapshot/pointer 阶段为 None）
    pub files_total: Option<usize>,
}

/// 迁移执行器：由 setup 注入后台执行闭包（捕获 AppHandle：spawn 后台线程、
/// 事件回传、完成自动退出）。**未注入时（单元测试）start_migration 走同步执行**，
/// 保持既有测试语义。闭包返回 Err 表示「未启动」（如校验失败由调用方统一处理）
pub type MigrationExecutor = std::sync::Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// 迁移启动结果
#[derive(Debug)]
pub enum MigrateOutcome {
    /// 同步完成（未注入 executor，测试路径）
    Done,
    /// 后台迁移已启动（协议回调应立即返回，进度/结果经事件回传）
    Background,
}

/// 后台迁移「进行中」标志的 RAII 复位（审计 A5）。后台线程持有它，
/// 任何提前 return / panic 展开都会 Drop，标志不会永久卡在 true。
pub struct MigrationGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for MigrationGuard {
    fn drop(&mut self) {
        self.0
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 迁移冻结位的 RAII 复位（审计 A5）：构造即冻结，只有落盘成功后 `keep()`
/// 才把冻结保留到重启；任何失败或 panic 展开都自动解冻，用户可原地重试。
struct FreezeGuard<'a> {
    state: &'a AppState,
    keep: bool,
    /// 本 guard 是否「拥有」这次冻结（复查审计 R7）：首次置位与 A6 提前置位
    /// 都算拥有；只有并发同步迁移的后来者（prev=true 且未在后台进行中）不拥有
    /// ——它失败时不得把先到者正在用的冻结位清掉。
    /// A6 判据不能省：后台迁移由协议线程提前置 pending 且 in_progress=true，
    /// 若按「prev 已置位就不拥有」处理，失败将永不 FR 解冻 → 应用永久只读。
    owns: bool,
}

impl<'a> FreezeGuard<'a> {
    fn new(state: &'a AppState) -> Self {
        let prev = state
            .migration_pending
            .swap(true, std::sync::atomic::Ordering::SeqCst);
        let owns = !prev || state.migration_in_progress();
        Self {
            state,
            keep: false,
            owns,
        }
    }

    fn keep(&mut self) {
        self.keep = true;
        // 成功终态必须是冻结：并发场景下先失败者（拥有者）可能已解冻，
        // 这里幂等补回，避免「迁移成功却没冻结」导致继续写旧库
        self.state
            .migration_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for FreezeGuard<'_> {
    fn drop(&mut self) {
        if !self.keep && self.owns {
            self.state
                .migration_pending
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// 存储页统计缓存项（clean::estimate + 旧目录备份，避免反复全库扫描）
struct StorageStatsCache {
    at: std::time::Instant,
    estimate: crate::clean::Estimate,
    backup: Option<(u64, u64)>,
    /// 附件目录占用（审计 A9）：`dir_stats` 是全目录递归，原先每次打开任意
    /// 设置页都跑一遍，不在缓存里等于没优化
    attach: (u64, u64),
    /// 缓存归属的数据目录（审计 A12）：只看 TTL 时，`swap_store` 切库后
    /// 15s 内仍会把旧目录的统计显示在新目录页面上
    data_dir: PathBuf,
}

/// 存储页统计缓存有效期：库量会随收件变化，给短 TTL 容忍短暂旧值，
/// 换取打开存储页/迁移返回不重复全库扫描
const STORAGE_STATS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(15);

/// 附件复制进度的最小回传间隔（审计 A7）：按文件逐个 emit 会在 10 万附件量级
/// 打出 10 万次 IPC + DOM 写，反而拖住前端。最后一个文件强制补齐终值。
const PROGRESS_THROTTLE: std::time::Duration = std::time::Duration::from_millis(100);

/// 目录递归深度上限（审计 A13）：附件正常只有 {YYYY-MM}/{email_id}/ 两级，
/// 超过说明目录被污染或存在环，继续递归会栈溢出 / 无限循环
const MAX_DIR_DEPTH: usize = 16;

/// 迁移中断标记（审计 A8）：落盘开始前写入目标目录，成功后改写为终态哨兵。
/// 进程被强杀时它留在原地，下次 `validate_data_dir` 据此识别「上次未完成的
/// 本应用产物」并安全清理，而不是笼统报「目标目录已存在 corpmail.db」。
const MIGRATION_MARKER: &str = ".corpmail-migration-incomplete";

/// 迁移成功终态哨兵（复查审计 R3）：成功后写入目标目录。`recover` 见到它
/// 就知道目录里的 `corpmail.db` / `attachments` 是**已完成的有效迁移产物**，
/// 绝不能按半成品清理——防止「成功后 marker 删除失败、残留的 incomplete
/// 标记把好库当半成品误删」这一不可逆事故。两个标记都清空后才算收尾。
const MIGRATION_DONE_MARKER: &str = ".corpmail-migration-done";

pub struct AppState {
    /// 数据库连接 + 数据目录，同一把锁绑定（审计 R19）。
    /// 旧实现两把独立锁，swap_store 分两段换：中间窗口里并发读者会看到
    /// 「新库 + 旧目录」（附件写进旧目录、库行却记新路径）。合并后两者
    /// 只能在同一次加锁内一起换。用 Mutex 而非 RwLock：rusqlite 的
    /// Connection 含 RefCell（!Sync），RwLock 共享读需要 Store: Sync，
    /// 不成立；db 访问本就互斥，Mutex 对写方零损失，data_dir() 读方
    /// 会与库操作互斥（库查询都是毫秒级，换取换库原子性）
    store: Mutex<Store>,
    /// 显示名推断互斥 + 失败节流（收件完成时由 invalidate 主动放行）
    name_probe: Mutex<NameProbe>,
    /// 锚点目录（系统默认应用数据目录）：独立配置文件 corpmail.toml 在此，
    /// 不随数据目录迁移、不被备份删除影响（resolve_start_dir 亦从此定位）
    anchor_dir: PathBuf,
    /// 应用配置内存镜像（corpmail.toml；Arc 供发送 worker 每封取快照——
    /// 配置不落库后 dispatch 不能再从 conn 读 SMTP 参数）
    config: std::sync::Arc<Mutex<crate::conf::ConfigMap>>,
    /// 发送队列入口（M3：lib.rs 装配 start_send_worker 后有值；测试/无 worker 时为 None，仅状态机流转）
    send_tx: Mutex<Option<std::sync::mpsc::Sender<i64>>>,
    /// 发送 worker 的退出确认位（R16：worker 退出时置位；stop_send_worker 等它）
    send_worker_exited: Mutex<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    /// 已读服务器回写唤醒口（seen worker；None = 未装配/测试环境，本地已读照常生效）
    seen_tx: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    /// 新邮件通知钩子（收件线程完成后调用，参数 (本轮开始时刻, 本轮新增数)）。
    /// None = 未装配（测试环境）；生产由 setup 注入系统通知闭包（notify.rs）
    notify_hook: Mutex<Option<NotifyHook>>,
    /// 托盘角标刷新钩子（M4-1：未读数变化后由 UI 变更点 poke；生产由 setup
    /// 注入（lib.rs tray 模块刷新未读角标），发送调度线程每 20s 兑底重刷；
    /// None = 未装配（测试环境），poke 为空操作）
    tray_hook: Mutex<Option<TrayHook>>,
    /// 收件状态（M4 §6.2：手动触发防抖/进行中态展示；Arc 供收件线程更新）。
    /// pub(crate)：路由层回归测试需注入运行中态（C5 拒绝重拉）
    pub(crate) fetch_state: std::sync::Arc<Mutex<FetchState>>,
    /// 活跃收件线程数（含看门狗复位后仍在挂起的旧轮）。看门狗只复位 UI 状态，
    /// 线程本身因 imap 无网络超时仍可能卡死——达到上限即拒绝新轮，
    /// 防止挂起线程 + TcpStream + SQLite 连接无限累积（审计 P1-5）
    pub(crate) active_rounds: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// 已被看门狗判死（detached）但仍可能挂着的轮数：独立计数，达到上限才拒绝
    /// 新轮，正常情况下不因「上轮挂住」而让收件永久失效（审计 P1-13）
    pub(crate) detached_rounds: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// 当前轮的结算凭证（弱引用）：看门狗据此把判死的轮转 detached 并释放名额
    round_ticket: Mutex<std::sync::Weak<RoundTicket>>,
    /// 数据目录已迁移、等待重启的冻结位（审计 H2/R2）。
    /// 迁移成功后**保持到进程退出**：此后所有写入都必须显式失败而不是静默落旧库
    /// （重启后 resolve_start_dir 读新指针直接打开新库，冻结位随进程消失）。
    /// Arc 是为了让收件线程/发送 worker 不持 AppState 也能读到
    migration_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 侧栏计数的上次成功值（读库失败时兜底，不清零；审计 R63）
    counts_cache: Mutex<Option<HashMap<&'static str, i64>>>,
    /// 迁移执行器（setup 注入；未注入 = 测试，start_migration 走同步）
    migration_executor: Mutex<Option<MigrationExecutor>>,
    /// 后台迁移进行中（幂等防重复触发；进程内跨线程可见）
    migration_in_progress: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 存储页统计缓存（estimate + 旧目录备份；避免每次打开存储页反复全库扫描）
    stats_cache: Mutex<Option<StorageStatsCache>>,
}

/// 迁移冻结期的统一提示（路由 409 / 收件 / 发送 worker 共用同一句中文）
pub const MIGRATION_PENDING_MSG: &str = "数据已迁移，请重启应用后继续（当前进程内的修改不会保存）";

/// 后台迁移进行中的统一提示（审计 R9）。
///
/// `migration_pending` 一位置两义：A6 在协议线程提前置它来消写丢失窗口，于是
/// 「进行中」也表现为 pending=true。下游若一律按 pending 写文案，就会在迁移
/// 还跑着时告诉用户「已迁移，请重启」——诱导用户主动中断一次未完成的迁移。
/// 两句话必须分开，判定统一走 `migration_phase()`。
pub const MIGRATION_RUNNING_MSG: &str = "数据迁移正在进行中，请稍候…";

/// 迁移进行的收件拦截图示（与重启文案区分：此刻不该催重启，该说「暂不收取」）
pub const MIGRATION_RUNNING_FETCH_MSG: &str = "数据迁移进行中，暂不收取邮件";

/// 迁移阶段（审计 R9）：两个原子位组合出的三态。
///
/// 注意判定顺序——后台迁移期间 `in_progress` 与 `pending` 同时为 true，
/// 必须 `in_progress` 优先，否则 Running 永远判不出来。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationPhase {
    /// 无迁移活动，写入正常
    Idle,
    /// 后台迁移进行中（数据尚未落盘完成，写入已被提前冻结）
    Running,
    /// 迁移已完成、等待重启（新库已就绪，重启前写入一律拒绝）
    PendingRestart,
}

/// `reset_and_fetch` 的失败分类（路由据此选回显：Busy → 「收取进行中」提示，
/// 其余按保存失败处理）。可枚举场景用枚举，不用魔法串比对
#[derive(Debug)]
pub enum ResetFetchError {
    /// 一轮收件正在运行：清游标会立刻被运行轮写回，必须等本轮结束
    Busy,
    /// 其余失败（迁移冻结 / 未配置 / 落库失败）：附原因
    Other(String),
}

/// 迁移前等待在跑收件轮收尾的上限：超过即放弃本次迁移并提示稍后重试
const MIGRATION_FETCH_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// 切库前等待旧发送 worker 退出的上限（R16）：慢速 SMTP 发送可能超过它，
/// 超时留痕继续切换（旧 worker 此后写旧库文件，重启由补发扫描自愈）
const SEND_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// 收件状态快照（收件按钮 spinner/禁用与状态片段共用）
#[derive(Clone, Debug)]
pub struct FetchState {
    pub running: bool,
    /// 最近一轮新增封数
    pub new_count: i64,
    pub last_error: String,
    /// 刚完成（状态片段读到即清，路由层据此下发 mbx:fetchDone 事件做局部刷新）
    pub just_finished: bool,
    /// 轮次代际：每次开始新轮 +1；挂起轮的迟到 finish（epoch 不匹配）不得写状态
    pub epoch: u64,
    /// 当前轮开始时间（epoch 秒）：看门狗判定挂起依据
    pub started_at: i64,
    /// 本轮拉取进度（M6 分批入库）：已入库 / 应拉总数；正在首拉前为 0/0
    pub fetch_done: usize,
    pub fetch_total: usize,
    /// 当前同步阶段（"sent"=发件箱 / "inbox"=收件箱 / 空=空闲）
    pub phase: String,
    /// 当前阶段首个进度上报时刻（epoch 秒）：ETA 外推的计时起点；
    /// 0 = 尚无进度（无法估）。进入新阶段时重置
    pub progress_started_at: i64,
    /// 最近一次进度上报时刻（epoch 秒）：与 progress_started_at、fetch_done
    /// 共同计算吞吐。progress_started_at == 0 时无效
    pub progress_updated_at: i64,
    /// progress_started_at 时刻已完成数：ETA = 剩余 /（已增量 / 计时跨度）
    pub progress_started_done: usize,
}

/// 置一轮收件为进行中并返回新 epoch（fetch_cas_begin 与 reset_and_fetch
/// 共用；调用方必须已持有 fetch_state 锁）。返回该轮结算凭证
fn begin_round(s: &mut FetchState) -> u64 {
    s.running = true;
    s.just_finished = false;
    s.epoch += 1;
    s.started_at = crate::mail::now_epoch();
    s.last_error.clear();
    s.fetch_done = 0;
    s.fetch_total = 0;
    s.phase.clear();
    s.progress_started_at = 0;
    s.progress_updated_at = 0;
    s.progress_started_done = 0;
    s.epoch
}

/// 收件轮看门狗上限（秒）：imap 无网络超时，挂起轮超过此时长由轮询链复位，
/// 避免收件按钮永久卡死（挂起线程自身因 epoch 失配无法再污染新一轮状态）
const FETCH_WATCHDOG_SECS: i64 = 300;

/// 同时存在的收件线程上限（正常轮）。
/// imap crate 无会话级读超时（`client_builder.rs` 硬编码 `TcpStream::connect`，
/// 无注入口，`Client` 也无公开 `secure()`），黑洞丢包时一轮会永久挂起。
/// 因此看门狗超时后把该轮标为 detached **并释放名额**，让收件继续可用：
/// 正常轮上限 4（企业服务器偶发长响应时仍有并发余量），detached 轮独立计数
/// 上限 8（每轮占 1 个 TcpStream + 1 个 SQLite 连接，给资源一个明确上界）。
pub(crate) const MAX_CONCURRENT_FETCH_ROUNDS: usize = 4;
pub(crate) const MAX_DETACHED_FETCH_ROUNDS: usize = 8;

/// 一轮收件的名额凭证：正常结束归还 active，看门狗判死时转为 detached。
/// `released` 保证两条路径只结算一次（线程最终返回时 Drop 不再重复递减）。
struct RoundTicket {
    active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    detached: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    released: std::sync::atomic::AtomicBool,
}

impl RoundTicket {
    fn new(
        active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        detached: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self {
            active,
            detached,
            released: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 正常结束：只归还名额
    fn release(&self) {
        if !self
            .released
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.active
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// 看门狗判死：归还名额并计入 detached（线程可能永不返回）
    fn detach(&self) {
        if !self
            .released
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.active
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.detached
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// 拼接完整邮箱地址：前缀 + 域名（§8；域名可配置，缺前导 @ 自动补齐）
pub fn full_address(prefix: &str, domain: &str) -> String {
    let d = domain.trim().trim_start_matches('@');
    if d.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}@{d}")
    }
}

/// 收件（IMAP）连接参数快照。
///
/// `start_fetch`（收件轮次）与 `seen_params_of`（已读回写）共用同一装配口径，
/// 避免两处各按一遍读取顺序拼 host/port/加密/账号/密码而漂移。
#[derive(Clone)]
pub struct ImapParams {
    pub host: String,
    pub port: u16,
    pub security: crate::net::TlsMode,
    pub account: String,
    pub password: String,
}

impl AppState {
    /// 打开/初始化数据库（schema 幂等迁移在 db::open 内完成；锚点=数据目录本身，
    /// 适用于测试与未启用自定义目录的场景）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open(data_dir: &Path) -> Self {
        Self::open_with_anchor(data_dir, data_dir)
    }

    /// 打开/初始化数据库（lib.rs 专用：锚点目录存独立配置文件 corpmail.toml）。
    /// 失败返回原因，由调用方决定降级或报错——生产路径不得 panic：
    /// 数据目录失效（外置盘拔出/权限变更）时旧实现的 `.expect` 会让进程直接退出，
    /// Windows GUI 子系统无控制台，用户看到的现象就是「双击没反应」。
    pub fn try_open_with_anchor(data_dir: &Path, anchor_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| format!("数据目录无法创建（{}）：{e}", data_dir.display()))?;
        let mut conn = crate::db::open(&data_dir.join("corpmail.db"))
            .map_err(|e| format!("数据库无法打开（{}）：{e}", data_dir.display()))?;
        // 联系人库一次性回填（v4 升级/首启）：contacts 非空即跳过，失败不阻断启动。
        // 审计 R52：以下五个存量修复全部改「按 id 分块 + 每批独立事务 + 游标续跑」，
        // 修完只剩一次空查询。仍在启动路径同步执行而不另起后台线程：修复要写
        // 同一套表（且触发器会连带重写 FTS），后台并发写会与 UI 读互相等锁，
        // 还会让「修一半」的状态被界面看到；分块后单批 500 行已足够短。
        if let Err(err) = crate::mail::contacts::backfill(&mut conn) {
            eprintln!("[contacts] 历史邮件回填失败（忽略）：{err}");
        }
        // 联系人统计存量修复（v11 一次性）：修正早期把 updated_at 写成入库
        // 时间的问题（最近联系/往来次数按历史邮件重算），幂等，失败不阻断
        if let Err(err) = crate::mail::contacts::repair_stats(&mut conn) {
            eprintln!("[contacts] 统计修复失败（忽略）：{err}");
        }
        // 历史主题 GBK 乱码一次性修复（幂等，修完无 U+00xx 即空扫）
        if let Err(err) = crate::mail::repair_gbk_subjects(&mut conn) {
            eprintln!("[repair] 主题乱码修复失败（忽略）：{err}");
        }
        // 历史正文乱码一次性修复（v12）：优先用同源 body_html 派生文本写回，
        // 无 HTML 时按 GB18030 逆推；同步重算 body_tokens 保 FTS 一致。
        // marker 只跑一次，失败不阻断启动（显示侧另有 preview_text 兜底）
        if let Err(err) = crate::mail::repair_mojibake_bodies(&mut conn) {
            eprintln!("[repair] 正文乱码修复失败（忽略）：{err}");
        }
        // 会话归组键存量回填（v8 升级/首启）：在主题修复后执行，基于已修复 subject
        if let Err(err) = crate::mail::backfill_thread_keys(&mut conn) {
            eprintln!("[thread] 会话归组键回填失败（忽略）：{err}");
        }
        let state = Self {
            store: Mutex::new(Store {
                db: conn,
                data_dir: data_dir.to_path_buf(),
            }),
            name_probe: Mutex::new(NameProbe { last_miss: None }),
            anchor_dir: anchor_dir.to_path_buf(),
            config: std::sync::Arc::new(Mutex::new(crate::conf::load(anchor_dir)?)),
            send_tx: Mutex::new(None),
            send_worker_exited: Mutex::new(None),
            seen_tx: Mutex::new(None),
            notify_hook: Mutex::new(None),
            tray_hook: Mutex::new(None),
            fetch_state: std::sync::Arc::new(Mutex::new(FetchState {
                running: false,
                new_count: 0,
                last_error: String::new(),
                just_finished: false,
                epoch: 0,
                started_at: 0,
                fetch_done: 0,
                fetch_total: 0,
                phase: String::new(),
                progress_started_at: 0,
                progress_updated_at: 0,
                progress_started_done: 0,
            })),
            active_rounds: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            detached_rounds: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            round_ticket: Mutex::new(std::sync::Weak::<RoundTicket>::new()),
            migration_pending: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            counts_cache: Mutex::new(None),
            migration_executor: Mutex::new(None),
            migration_in_progress: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stats_cache: Mutex::new(None),
        };
        // 迁移后首次重启：旧目录遗留空库自动清理（见方法注释；有邮件则保留手动删）
        state.try_auto_clean_empty_old_backup();
        // 重装/换机沿用既有数据目录：启动时库里已有发件箱邮件，配置中显示名
        // 却为空 → 立即补齐，无需等首拉或用户打开设置页
        let _ = state.display_name();
        Ok(state)
    }

    /// 打开/初始化数据库（锚点=数据目录本身，测试便捷入口）。
    /// 生产入口为 `try_open_with_anchor`（返回 Result，不 panic）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open_with_anchor(data_dir: &Path, anchor_dir: &Path) -> Self {
        Self::try_open_with_anchor(data_dir, anchor_dir)
            .unwrap_or_else(|e| panic!("应用数据目录不可用：{e}"))
    }

    /// 启动目录解析（lib.rs setup 调用）：独立配置文件 corpmail.toml 的 data_dir_override
    /// 指针定位自定义数据目录；文件缺失/指针失效（目录被删、外置盘拔出）回退锚点目录。
    ///
    /// 有效性 = 目录存在且含 corpmail.db（迁移快照必有；空壳/失效目录不采纳，
    /// 安全回退锚点目录——迁移前的原数据就留在那里）
    pub fn resolve_start_dir(anchor: &Path) -> PathBuf {
        let usable = |p: &str| Path::new(p).join("corpmail.db").is_file();
        // 读不到配置就按锚点目录启动（AppState::open 会再读一次并给出启动失败
        // 提示，这里只负责别让"指针在但读不出"变成打开错误的库）
        let cfg = match crate::conf::load(anchor) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[state] {e}");
                return anchor.to_path_buf();
            }
        };
        if let Some(p) = cfg.get(keys::DATA_DIR).cloned().filter(|p| usable(p)) {
            return PathBuf::from(p);
        }
        anchor.to_path_buf()
    }

    /// 迁移数据目录到用户选定位置（M5：在线快照 + 重启生效，避免热切换连接的并发风险）。
    /// 指针写入独立配置文件 corpmail.toml（随锚点目录常驻，不随数据迁移、
    /// 不被备份删除影响）；原目录保留迁移前的库/附件备份，可由用户在
    /// 存储页手动删除（delete_old_backup）。
    /// 迁移期间收件线程可能写入新附件导致极小竞态窗口（附件文件缺失但元数据在），
    /// 重启后孤儿回收不会破坏元数据，重收件可补——概率极低，取舍见设计 §5.2 注记
    pub fn move_data_dir(&self, dest: &str) -> Result<String, String> {
        // 同步路径（无进度回调）：单元测试 / 未注入 executor 使用，保持既有行为
        self.migrate_impl(dest, &|_| {})
    }

    /// 带进度回传的迁移（后台执行器使用）：与 move_data_dir 同一落盘逻辑，仅透传进度回调
    pub fn move_data_dir_progress(
        &self,
        dest: &str,
        cb: &(dyn Fn(&MigrationProgress) + Sync),
    ) -> Result<String, String> {
        self.migrate_impl(dest, cb)
    }

    /// 迁移落盘统一实现（冻结 → 等收件轮 → 快照/附件/指针）；`cb` 为进度回调（可 no-op）
    fn migrate_impl(
        &self,
        dest: &str,
        cb: &(dyn Fn(&MigrationProgress) + Sync),
    ) -> Result<String, String> {
        // 已迁移待重启：再迁一次没有意义（新快照会覆盖目标库，且旧库仍在被本进程写）。
        // 例外：`migration_in_progress` 为 true 说明冻结位是本次后台迁移在协议线程
        // 里提前置的（审计 A6），不是上一次遗留的待重启状态，允许继续。
        if self.migration_pending() && !self.migration_in_progress() {
            return Err(MIGRATION_PENDING_MSG.into());
        }
        // 冻结写入（R2）：置位后新的收件轮 / 发送派发 / HTTP 写请求一律被挡，
        // 再等正在跑的收件轮收尾——此后 VACUUM INTO 的快照就是一致终点。
        // FreezeGuard 覆盖此后所有失败路径（含进度回调 panic）：未 keep 即解冻，
        // 否则 UI 会停在"请重启"却并没有完成迁移
        let mut freeze = FreezeGuard::new(self);
        let cur = self.data_dir();
        let dest_path = validate_data_dir(&cur, dest)?;
        cb(&MigrationProgress {
            phase: "attach",
            detail: "等待当前收取结束…".into(),
            files_done: 0,
            files_total: None,
        });
        if !self.wait_fetch_idle(MIGRATION_FETCH_WAIT) {
            return Err("正在收取邮件，请等本轮收件结束后再迁移".into());
        }
        // 落盘失败由 FreezeGuard 自动解冻，调用方只需透传错误
        self.migrate_snapshot(&cur, &dest_path, Some(cb))?;
        // 落盘已完成、指针已写：冻结必须保留到重启（R2）
        freeze.keep();
        Ok(format!(
            "数据已迁移到新目录，请重启应用后继续使用（重启前的一切修改都不会保存）。旧目录中若有邮件数据，\
             其备份可在 设置 → 存储管理 中手动删除（旧目录无邮件时重启后将自动清理）。新目录：{}",
            dest_path.display()
        ))
    }

    /// 迁移冻结期判定（路由/send worker/fetch 共用）。
    /// 只回答「写入是否被冻结」；要区分「进行中 / 待重启」用 `migration_phase()`
    pub fn migration_pending(&self) -> bool {
        self.migration_pending
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 冻结期的用户提示（路由 409 / 收件共用）：进行中说「稍候」，待重启才说「请重启」
    pub fn migration_block_msg(&self) -> &'static str {
        match self.migration_phase() {
            MigrationPhase::Running => MIGRATION_RUNNING_MSG,
            _ => MIGRATION_PENDING_MSG,
        }
    }

    /// 迁移阶段（审计 R9）：下游文案的唯一依据。
    /// `in_progress` 优先——后台迁移期间两位同为 true（A6 提前冻结）
    pub fn migration_phase(&self) -> MigrationPhase {
        if self.migration_in_progress() {
            MigrationPhase::Running
        } else if self.migration_pending() {
            MigrationPhase::PendingRestart
        } else {
            MigrationPhase::Idle
        }
    }

    /// 注入后台迁移执行器（lib.rs setup 调用；未注入 = 单元测试，start_migration 走同步）
    pub fn set_migration_executor(&self, exec: MigrationExecutor) {
        self.migration_executor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(exec);
    }

    /// 后台迁移是否进行中（前端/路由幂等护栏；executor 后台线程结束后清位）
    pub fn migration_in_progress(&self) -> bool {
        self.migration_in_progress
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 后台迁移结束（成功/失败）后清进行中标志（executor 后台线程调用）
    pub fn mark_migration_finished(&self) {
        self.migration_in_progress
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// 取「进行中」标志的 RAII 复位器：后台线程开头持有，panic 也能复位（审计 A5）
    pub fn migration_guard(&self) -> MigrationGuard {
        MigrationGuard(std::sync::Arc::clone(&self.migration_in_progress))
    }

    /// 启动数据目录迁移。
    /// - 已注入 executor（生产）→ 后台执行，立即返回 `Background`（协议回调据此秒回，
    ///   进度/结果经事件回传前端）；
    /// - 未注入（单元测试）→ 同步执行，返回 `Done`（保持既有测试语义）。
    pub fn start_migration(&self, dest: &str) -> Result<MigrateOutcome, String> {
        // 未选择目标目录：最前置的输入校验（表单留空即报此文案；validate_data_dir
        // 对空串会落到「请选择绝对路径目录」，语义不如「未选择」准确，且后台线程
        // 预校验同样需要此拦截——统一在入口判定）
        if dest.trim().is_empty() {
            return Err("未选择目标目录".into());
        }
        // 「后台正在跑」优先于「已迁移待重启」：A6 在协议线程提前置了冻结位，
        // 重复点击时用户该看到「进行中」，而不是误导性的「请重启」
        if self.migration_in_progress() {
            return Err(MIGRATION_RUNNING_MSG.into());
        }
        if self.migration_pending() {
            return Err(MIGRATION_PENDING_MSG.into());
        }
        // 读取执行器：未注入（单元测试 / e2e）走同步；注入了（生产）才需要判占与提前冻结
        let exec = self
            .migration_executor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(exec) = exec else {
            // 同步（测试 / e2e）：保持既有行为，不占后台标志
            self.move_data_dir(dest)?;
            return Ok(MigrateOutcome::Done);
        };
        // A4：原子判占。旧实现 load + store 之间隔着 validate，两个并发协议回调
        // 可同时通过 → 两次 spawn 打同一目标库并互删回滚产物。
        if self
            .migration_in_progress
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return Err(MIGRATION_RUNNING_MSG.into());
        }
        // A6：在协议线程内立即冻结。旧实现要等后台线程调度到 migrate_impl 才置位，
        // 这段窗口内通过路由的写请求会落旧库、赶不上 VACUUM INTO 快照。
        self.migration_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let started = (|| {
            validate_data_dir(&self.data_dir(), dest)?;
            exec(dest)
        })();
        if let Err(e) = started {
            // 启动失败：两个标志都复位，用户可原地重试（成功的复位由后台线程 guard 负责）
            self.migration_pending
                .store(false, std::sync::atomic::Ordering::SeqCst);
            self.mark_migration_finished();
            return Err(e);
        }
        Ok(MigrateOutcome::Background)
    }

    /// 等待在跑的收件轮收尾（迁移前调用）。收件线程每轮开头会检查冻结位并主动退出，
    /// 所以通常毫秒级返回；返回 false = 超时（调用方应放弃本次迁移）
    fn wait_fetch_idle(&self, limit: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + limit;
        loop {
            // 走 fetch_snapshot：顺带跑看门狗，挂死轮会在 300s 阈值后被判死复位
            if !self.fetch_snapshot().running {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// 迁移落盘（快照 → 附件 → 指针）。调用前必须已置冻结位并确认无在跑收件轮
    fn migrate_snapshot(
        &self,
        cur: &Path,
        dest_path: &Path,
        cb: Option<&(dyn Fn(&MigrationProgress) + Sync)>,
    ) -> Result<(), String> {
        // A13：附件根目录本身若是符号链接，整个复制会跑到目录外去。
        // 放在写 marker 之前校验，失败不留下需要自愈的残留
        let src_att = cur.join("attachments");
        if std::fs::symlink_metadata(&src_att)
            .map(|m| m.is_symlink())
            .unwrap_or(false)
        {
            return Err(format!(
                "附件根目录是符号链接（{}），拒绝迁移",
                src_att.display()
            ));
        }
        // 进度快照透传（cb 为 None 时即同步 no-op，保持既有行为）
        let emit = |phase: &'static str, detail: String, done: usize, total: Option<usize>| {
            if let Some(cb) = cb {
                cb(&MigrationProgress {
                    phase,
                    detail,
                    files_done: done,
                    files_total: total,
                });
            }
        };
        // 附件阶段按时间节流（A7）；锁外调 cb，避免 emit 期间占着锁。
        // (上次发出时间, 上次发出的文件数)：终值只补一次（A13 跳过符号链接后
        // 真实 copied 可能小于 total，不能靠「最后一个文件」凑齐 100%）
        let progress = Mutex::new((
            std::time::Instant::now()
                .checked_sub(PROGRESS_THROTTLE)
                .unwrap_or_else(std::time::Instant::now),
            0usize,
        ));
        let emit_files = |done: usize, total: usize| {
            if let Some(cb) = cb {
                let now = std::time::Instant::now();
                let force = done >= total;
                let should_emit = {
                    let mut st = progress.lock().unwrap_or_else(|e| e.into_inner());
                    if (force && st.1 >= total)
                        || (!force && now.duration_since(st.0) < PROGRESS_THROTTLE)
                    {
                        false // 终值已补过，或普通进度还在节流窗口内
                    } else {
                        *st = (now, done);
                        true
                    }
                };
                if should_emit {
                    cb(&MigrationProgress {
                        phase: "attachments",
                        detail: format!("复制附件 {done}/{total}"),
                        files_done: done,
                        files_total: Some(total),
                    });
                }
            }
        };
        let marker = dest_path.join(MIGRATION_MARKER);
        std::fs::write(&marker, b"incomplete").map_err(|e| format!("迁移标记写入失败：{e}"))?;
        // 1) 数据库在线快照（VACUUM INTO：WAL 安全的一致性副本，业务不中断）
        emit("snapshot", "正在快照数据库…".into(), 0, None);
        let new_db = dest_path.join("corpmail.db");

        // 2) 附件复制 + 3) 指针写入：任一步失败都回滚已产出物（快照库/附件副本），
        //    并恢复内存镜像中的指针——保证调用方（向导/存储页）原地重试不被
        //    "目标已存在 corpmail.db"拦截（C6 半保存态修复）
        // 本次实际写入的文件清单：回滚只删这些，不碰目标目录里任何既有内容
        let mut written: Vec<PathBuf> = Vec::new();
        // 附件复制进度计数（copy_dir_recursive 递归内共享）
        let copied = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let migrated: Result<(), String> = (|| {
            // 快照也走闭包（复查审计 R4）：VACUUM 失败原先在回滚块之前 `?` 返回，
            // marker 与半成品快照库会静默残留；并入后由下面的精确回滚统一收尾
            self.with_conn(|conn| {
                conn.execute("VACUUM INTO ?1", [new_db.to_string_lossy().as_ref()])
                    .map_err(|e| format!("数据库快照失败：{e}"))
            })?;
            if src_att.is_dir() {
                let total = count_attachment_files(&src_att);
                emit(
                    "attachments",
                    format!("正在复制附件（共 {total} 个）…"),
                    0,
                    Some(total),
                );
                copy_dir_recursive(
                    &src_att,
                    &dest_path.join("attachments"),
                    &mut written,
                    &copied,
                    total,
                    &emit_files,
                    0,
                )
                .map_err(|e| format!("附件目录复制失败：{e}"))?;
                // 强制补齐终值：跳过符号链接/深度截断时 copied < total，
                // 否则进度条会停在不到 100% 而迁移其实已成功
                emit_files(total, total);
            }
            emit(
                "pointer",
                "正在写入数据库指针…".into(),
                copied.load(std::sync::atomic::Ordering::SeqCst),
                None,
            );
            let (prev, save_res) = {
                let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
                let prev = cfg.get(keys::DATA_DIR).cloned();
                cfg.insert(
                    keys::DATA_DIR.to_string(),
                    dest_path.to_string_lossy().into_owned(),
                );
                let snapshot = cfg.clone();
                (prev, crate::conf::save(&self.anchor_dir, &snapshot))
            };
            save_res.map_err(|e| {
                // 配置写失败：镜像恢复指针（镜像必须反映文件内容）
                let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
                match prev {
                    Some(v) => {
                        cfg.insert(keys::DATA_DIR.to_string(), v);
                    }
                    None => {
                        cfg.remove(keys::DATA_DIR);
                    }
                }
                format!("配置文件写入失败：{e}")
            })
        })();
        if let Err(e) = migrated {
            // 精确回滚：只删本次复制出的文件，再回收本次创建的空目录。
            // 旧实现对 dest/attachments 直接 remove_dir_all——目标目录里用户
            // 原有的附件会被连带删除（指针写入失败即触发，破坏不可逆）。
            // 删不掉的必须上报（A8），并保留 marker 供下次 validate 自愈。
            let mut rollback_errors = Vec::new();
            // 直接 remove_file 而非先 exists()：VACUUM 失败可能留下 dangling
            // 符号链接（exists() 跟随链接看不到），remove_file 对不存在文件
            // 报 NotFound 可忽略（R4）
            if let Err(rb) = std::fs::remove_file(&new_db)
                && rb.kind() != std::io::ErrorKind::NotFound
            {
                rollback_errors.push(format!("快照库 {}：{rb}", new_db.display()));
            }
            for f in &written {
                if f.exists()
                    && let Err(rb) = std::fs::remove_file(f)
                {
                    rollback_errors.push(format!("附件 {}：{rb}", f.display()));
                }
            }
            crate::mail::prune_empty_dirs(&dest_path.join("attachments"));
            if rollback_errors.is_empty() {
                let _ = std::fs::remove_file(&marker);
                return Err(e);
            }
            eprintln!(
                "[state] 迁移回滚未完成，保留标记供下次自愈：{}",
                marker.display()
            );
            return Err(format!(
                "{e}；回滚未完成：{}（下次重试会自动清理残留）",
                rollback_errors.join("；")
            ));
        }
        // R3（复查审计）：成功后不直接删 marker 了事——写入终态哨兵。若 marker
        // 删除失败（磁盘错误/权限/占用），残留的 incomplete 标记会让
        // recover_incomplete_migration 把**已迁移的库与附件**当半成品删掉；
        // done 哨兵在场时 recover 只清标记、绝不碰数据。写哨兵失败要留痕
        // （此时目录可写已过 probe，失败属罕见环境问题）
        let done_marker = dest_path.join(MIGRATION_DONE_MARKER);
        if let Err(e) = std::fs::write(&done_marker, dest_path.to_string_lossy().as_bytes()) {
            eprintln!(
                "[state] 迁移成功但终态哨兵写入失败（{}）：{e}",
                done_marker.display()
            );
        }
        if let Err(e) = std::fs::remove_file(&marker) {
            eprintln!(
                "[state] 迁移成功但中断标记删除失败（{}）：{e}（终态哨兵已写入，recover 只清标记不删数据）",
                marker.display()
            );
        }

        Ok(())
    }

    /// 首设专用：空库直切数据目录（会话内立即生效，不迁移、不重启）。
    ///
    /// 与 move_data_dir 的分工：迁移服务"已用了一段时间、库里有数据"的场景
    /// （快照 + 重启生效）；首设向导选自定义目录时库里只有空 schema，
    /// 快照/复制/重启全是无谓成本——直接在新目录建库、换连接、写指针。
    /// 安全阀：库内有任何用户痕迹（邮件或配置行）都拒绝，宁可走迁移；
    /// 调用方（向导）以 `!configured && store_is_empty()` 判定走本路径。
    pub fn switch_data_dir_first_run(&self, dest: &str) -> Result<String, String> {
        let cur = self.data_dir();
        if !self.store_is_empty() {
            return Err("当前数据库非空，不能直接切换目录（请在 设置 → 存储管理 中迁移）".into());
        }
        let dest_path = validate_data_dir(&cur, dest)?;
        // 新库（schema 幂等迁移到最新版本；空库联系人回填为 no-op）
        let mut new_conn = crate::db::open(&dest_path.join("corpmail.db"))
            .map_err(|e| format!("新目录数据库创建失败：{e}"))?;
        if let Err(err) = crate::mail::contacts::backfill(&mut new_conn) {
            eprintln!("[contacts] 新目录历史回填失败（忽略）：{err}");
        }
        if let Err(err) = crate::mail::contacts::repair_stats(&mut new_conn) {
            eprintln!("[contacts] 新目录统计修复失败（忽略）：{err}");
        }
        // 指针先行：写失败则删掉刚建的新库文件回滚（此时尚未换连接，旧库仍在用）
        if let Err(e) = self.point_data_dir(&dest_path) {
            let _ = std::fs::remove_file(dest_path.join("corpmail.db"));
            return Err(e);
        }
        self.swap_store(new_conn, &dest_path);
        clean_empty_legacy_db(&cur, &self.anchor_dir);
        Ok(format!("数据目录已设置为 {}", dest_path.display()))
    }

    /// 首设专用：采纳目标目录中的既有数据库并直接沿用（重装/换机找回历史邮件）。
    ///
    /// 与 switch_data_dir_first_run（空目标建新库）互补：目标含 corpmail.db 时，
    /// 打开校验后按归属判定——
    /// - 库为空：视同直切，任何账号可用；
    /// - 归属账号与登录账号一致（store_owner 标记 > 发件 From 模式 > 收件人模式）：
    ///   会话内换连即生效，存量邮件、附件目录与 IMAP 收件游标全部保留，
    ///   收件从既有进度增量续拉，不重新首拉；
    /// - 归属他账号或无法判定：拒绝（不动指针、不动目标目录中任何文件），
    ///   防两个账号的邮件混入同一库。
    ///
    /// 安全阀：当前库非空（如配置文件丢失但锚点仍有数据）时拒绝——两处数据
    /// 无法合并，弃当前数据属于破坏性操作，应走迁移流程由用户决断。
    pub fn adopt_data_dir_existing(&self, dest: &str, account: &str) -> Result<String, String> {
        let cur = self.data_dir();
        if !self.store_is_empty() {
            return Err(
                "当前数据库非空，无法直接使用目标目录的既有数据（两处数据不能合并）。请清空“数据保存目录”沿用当前目录，或先在 设置 → 存储管理 迁移到空目录"
                    .into(),
            );
        }
        let dest_path = validate_adopt_dir(&cur, dest)?;
        let db_path = dest_path.join("corpmail.db");
        // 身份预检：核心表齐全才是本应用的数据目录（打开无关 SQLite 文件会
        // 建出空 schema，先探测避免在无关文件里写表）。非 SQLite/损坏文件
        // 同样在此被拒。
        {
            let probe =
                Connection::open(&db_path).map_err(|e| format!("目标数据库无法打开：{e}"))?;
            let tables: i64 = probe
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table'
                      AND name IN ('emails', 'settings', 'attachments')",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if tables != 3 {
                return Err(
                    "目标目录不是 CorpMail 数据目录（缺少邮件数据表），无法直接使用".into(),
                );
            }
        }
        let account_l = account.trim().to_ascii_lowercase();
        let mut new_conn =
            crate::db::open(&db_path).map_err(|e| format!("目标数据库无法打开：{e}"))?;
        // 归属判定：空库人人可用；非空必须能判定且一致
        let (mails, settings_rows): (i64, i64) = (
            new_conn
                .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
                .unwrap_or(-1),
            new_conn
                .query_row("SELECT count(*) FROM settings", [], |r| r.get(0))
                .unwrap_or(-1),
        );
        if mails < 0 || settings_rows < 0 {
            return Err("目标数据库无法读取（可能已损坏），无法直接使用".into());
        }
        if mails > 0 || settings_rows > 0 {
            match check_store_owner_conn(&new_conn, &account_l) {
                StoreOwner::Match => {}
                StoreOwner::Mismatch(owner) => {
                    return Err(format!(
                        "目标目录的数据属于账号 {owner}，与当前登录账号 {account_l} 不一致，已拒绝使用（避免两个账号的邮件混存）。请用原账号登录，或为当前账号另选空目录"
                    ));
                }
                StoreOwner::Unknown => {
                    return Err(
                        "无法确认目标目录数据的归属账号（库内没有可判断归属的发件记录），为安全起见不自动使用。请为当前账号另选空目录，或确认无需保留后清空该目录"
                            .into(),
                    );
                }
            }
        }
        // 旧版本库补维护（幂等，失败不阻断——与启动路径同口径）
        if let Err(err) = crate::mail::contacts::backfill(&mut new_conn) {
            eprintln!("[contacts] 采纳库历史回填失败（忽略）：{err}");
        }
        if let Err(err) = crate::mail::contacts::repair_stats(&mut new_conn) {
            eprintln!("[contacts] 采纳库统计修复失败（忽略）：{err}");
        }
        // 归属标记预写 + 指针提交（审计 P1-50 + R17）：先改目标库、指针是
        // 唯一提交点。指针写失败必须把目标库的归属标记还原——否则目标目录
        // 带上本账号标记却未被使用，他账号此后采纳该目录会被误拒（标记是
        // 归属判定的唯一精确依据）。标记写入失败则直接中止（指针未动、
        // 目标库原值未变）
        let owner_prev: Option<Option<String>> = new_conn
            .query_row(
                "SELECT v FROM settings WHERE k = ?1",
                [keys::STORE_OWNER],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| format!("目标数据库无法读取（可能已损坏）：{e}"))?;
        new_conn
            .execute(
                "INSERT INTO settings(k, v) VALUES (?1, ?2)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                rusqlite::params![keys::STORE_OWNER, account_l],
            )
            .map_err(|e| format!("目标目录写入归属标记失败（可能只读或已损坏）：{e}"))?;
        if let Err(e) = self.point_data_dir(&dest_path) {
            // 提交失败：回滚目标库归属标记（尽力；再失败留痕——标记残留的
            // 影响是他账号采纳被误拒，好过误收）
            let rollback = match &owner_prev {
                Some(v) => new_conn.execute(
                    "UPDATE settings SET v = ?1 WHERE k = ?2",
                    rusqlite::params![v, keys::STORE_OWNER],
                ),
                None => new_conn.execute("DELETE FROM settings WHERE k = ?1", [keys::STORE_OWNER]),
            };
            if let Err(rb) = rollback {
                eprintln!("[state] 归属标记回滚失败（目标库标记可能残留）：{rb}");
            }
            return Err(e);
        }
        self.swap_store(new_conn, &dest_path);
        clean_empty_legacy_db(&cur, &self.anchor_dir);
        Ok(format!(
            "已沿用既有数据目录 {}（历史邮件与收件进度直接沿用，增量续收）",
            dest_path.display()
        ))
    }

    /// 数据目录指针写入 corpmail.toml（直切/采纳共用）：失败时恢复内存镜像
    /// （镜像必须反映文件内容）并返回错误，不动目标目录
    fn point_data_dir(&self, dest_path: &Path) -> Result<(), String> {
        let (prev, save_res) = {
            let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
            let prev = cfg.get(keys::DATA_DIR).cloned();
            cfg.insert(
                keys::DATA_DIR.to_string(),
                dest_path.to_string_lossy().into_owned(),
            );
            let snapshot = cfg.clone();
            (prev, crate::conf::save(&self.anchor_dir, &snapshot))
        };
        if let Err(e) = save_res {
            let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
            match prev {
                Some(v) => {
                    cfg.insert(keys::DATA_DIR.to_string(), v);
                }
                None => {
                    cfg.remove(keys::DATA_DIR);
                }
            }
            return Err(format!("配置文件写入失败：{e}"));
        }
        Ok(())
    }

    /// 换库换目录（直切/采纳共用，审计 R16 + R19）：
    /// 1) 作废在跑收件轮并等收尾（epoch+1：其迟到结算因凭证失配被丢弃；
    ///    等名额归零上限 30s——imap 无读超时可能挂死，超时留痕继续）；
    /// 2) 旧发送 worker 退出确认（它持旧库独立连接与旧附件根，切库后
    ///    继续派发会把待发邮件写进旧库）；
    /// 3) 同一把写锁内换 db 与 data_dir（读者不会见到「新库 + 旧目录」）；
    /// 4) 按新目录重启发送 worker。
    ///
    /// 此后 set_batch/收件/附件全部落新目录，无需重启
    fn swap_store(&self, new_conn: Connection, dest_path: &Path) {
        self.quiesce_fetch_rounds();
        self.stop_send_worker();
        {
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.db = new_conn;
            store.data_dir = dest_path.to_path_buf();
        }
        self.start_send_worker();
        // A12：数据目录换了，TTL 内的旧目录统计必须立即作废
        self.invalidate_stats_cache();
    }

    /// 切库前静默收件轮（审计 R16）。收尾后 `clean_empty_legacy_db` 才安全执行
    /// （调用方紧随 swap_store 调它）——在跑轮持自己的旧库连接，换库瞬间
    /// 仍可能写旧库文件；归零等待保证它们全部收手
    fn quiesce_fetch_rounds(&self) {
        let bumped = {
            let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
            // 作废在跑轮：结算凭证失效（fetch_end / 批次推进都按 epoch 匹配，
            // C4 口径下批间自终止），running 由本函数在名额归零后复位
            s.epoch += 1;
            s.epoch
        };
        let deadline = std::time::Instant::now() + MIGRATION_FETCH_WAIT;
        while self.active_rounds.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            if std::time::Instant::now() >= deadline {
                eprintln!(
                    "[state] 切换数据目录前等待收件轮收尾超时（{}s，可能有挂死轮），继续切换",
                    MIGRATION_FETCH_WAIT.as_secs()
                );
                // 挂死轮：running 留给看门狗 300s 复位，这里不动
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // 名额归零：被作废轮的迟到结算已全部丢弃（epoch 失配），running 仍为
        // true——不复位的话 UI 停在「收取中」且新轮被 CAS 拒绝，最长卡 300s。
        // epoch 未再前进（== bumped）保证这之间没有新一轮合法开始（CAS 要求
        // running=false，而 running 一直为 true，双向互证）
        let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.running && s.epoch == bumped {
            s.running = false;
            s.phase.clear();
        }
    }

    /// 库内是否无任何用户数据（emails 与 settings 均为 0 行）。
    /// 首设"空库直切"的前置判定；计数失败按非空处理（宁可走安全的迁移流程）
    pub fn store_is_empty(&self) -> bool {
        self.with_conn(|c| {
            let mails: i64 = c
                .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
                .unwrap_or(1);
            let settings: i64 = c
                .query_row("SELECT count(*) FROM settings", [], |r| r.get(0))
                .unwrap_or(1);
            mails == 0 && settings == 0
        })
    }

    /// 停止发送 worker 并等退出确认（审计 R16）：取走通道（sender 全部
    /// drop → worker 下一轮 recv 得 Err 自行退出并置 exited 位）。上限
    /// SEND_STOP_WAIT：卡在慢速 SMTP 发送上时不再等（留痕后旧 worker 可能
    /// 继续写旧库文件——与切库前收件轮超时同一取舍，重启后由补发扫描自愈）
    fn stop_send_worker(&self) {
        let flag = {
            let mut guard = self.send_tx.lock().unwrap_or_else(|e| e.into_inner());
            *guard = None; // drop 全部 sender → worker 的 recv 返回 Err
            self.send_worker_exited
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        let Some(flag) = flag else {
            return; // 未装配（测试环境）：无事可停
        };
        let deadline = std::time::Instant::now() + SEND_STOP_WAIT;
        while !flag.load(std::sync::atomic::Ordering::Acquire) {
            if std::time::Instant::now() >= deadline {
                eprintln!(
                    "[send] 旧发送 worker 未在 {}s 内退出（可能在慢速发送中），继续切换",
                    SEND_STOP_WAIT.as_secs()
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// 当前数据目录快照（首设直切后会话内变化；读方一律经此取，不直接碰字段）。
    /// 与库操作互斥（同一把 Store 锁，审计 R19）——换库期间目录读取短暂等待
    pub fn data_dir(&self) -> PathBuf {
        self.store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .data_dir
            .clone()
    }

    /// 锚点目录（corpmail.toml 与凭据回退密钥所在；完成页展示目录提示用）
    pub fn anchor_dir(&self) -> &Path {
        &self.anchor_dir
    }

    /// 启动时自动清理"空的"旧目录遗留（迁移场景收尾）。
    ///
    /// 背景：move_data_dir 保留原目录库/附件作迁移备份（防误删），由用户在
    /// 存储页手动删除。但"装好首设后立即改目录"这类场景原库只有空 schema，
    /// 备份毫无价值还占用存储页的"迁移备份"展示位，误导用户。
    ///
    /// 时机：仅在重启后（本进程打开的是新目录库，与锚点遗留库零连接，
    /// 无 orphan-inode 风险——同 delete_old_backup 拒绝 pending 的理由）。
    /// 条件（任一不满足即保留，走手动删除路径）：
    /// - 已迁移（data_dir ≠ anchor）且锚点存在遗留 corpmail.db
    /// - 遗留库可只读打开且 emails 与 attachments 均为 0 行（彻底无数据；
    ///   打不开/计数失败一律保留——文件损坏时宁可留给用户处置）
    ///
    /// 命中即删锚点 corpmail.db[-wal/-shm] 与空的 attachments/（清理范围同
    /// delete_old_backup：corpmail.toml 与新目录数据不动）。
    fn try_auto_clean_empty_old_backup(&self) {
        if self.data_dir() == self.anchor_dir {
            return; // 未迁移（或回退锚点）：锚点库就是当前库，绝不能碰
        }
        let backup_db = self.anchor_dir.join("corpmail.db");
        if !backup_db.is_file() {
            return; // 无遗留（或此前已自动清理过）：无事可做
        }
        // 只读打开遗留库计数：不开写句柄，不留 -shm/-wal 变更
        let counted =
            Connection::open_with_flags(&backup_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .and_then(|conn| {
                    let mails: i64 =
                        conn.query_row("SELECT count(*) FROM emails", [], |r| r.get(0))?;
                    let atts: i64 =
                        conn.query_row("SELECT count(*) FROM attachments", [], |r| r.get(0))?;
                    Ok((mails, atts))
                });
        match counted {
            Ok((0, 0)) => { /* 空库：继续清理 */ }
            Ok((mails, atts)) => {
                eprintln!(
                    "[startup] 旧目录保留迁移备份（{mails} 封邮件 / {atts} 个附件），可在存储页手动删除"
                );
                return;
            }
            // 打不开/表不存在：损坏或异构文件，保留给用户判断
            Err(err) => {
                eprintln!("[startup] 旧目录遗留库无法核验（保留备份，不自动清理）：{err}");
                return;
            }
        }
        let mut freed = 0u64;
        let mut files = 0u64;
        for f in ["corpmail.db", "corpmail.db-wal", "corpmail.db-shm"] {
            let p = self.anchor_dir.join(f);
            if p.is_file() {
                freed += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                if let Err(err) = std::fs::remove_file(&p) {
                    eprintln!("[startup] 旧目录遗留 {f} 删除失败（停止自动清理）：{err}");
                    return;
                }
                files += 1;
            }
        }
        let att = self.anchor_dir.join("attachments");
        if att.is_dir() {
            let (b, n) = dir_stats(&att);
            match std::fs::remove_dir_all(&att) {
                Ok(()) => {
                    freed += b;
                    files += n;
                }
                Err(err) => {
                    eprintln!("[startup] 旧目录遗留附件目录删除失败（停止自动清理）：{err}");
                    return;
                }
            }
        }
        if files > 0 {
            eprintln!(
                "[startup] 旧目录遗留空库已自动清理（{} 个文件，{} 字节）；如需保留历史请从新目录备份",
                files, freed
            );
        }
    }

    /// 存储页统计（clean::estimate + 旧目录备份 + 附件占用）：短 TTL 缓存，
    /// 避免每次打开存储页/迁移返回时重复全库扫描（大库下可达秒级）
    pub fn storage_stats(
        &self,
    ) -> (
        crate::clean::Estimate,
        Option<(u64, u64)>,
        (u64, u64),
    ) {
        let now = std::time::Instant::now();
        // 先取目录快照：不要把 data_dir() 放进 stats_cache 锁的作用域里
        let dir = self.data_dir();
        if let Some(c) = self
            .stats_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && now.duration_since(c.at) < STORAGE_STATS_CACHE_TTL
            && c.data_dir == dir
        {
            return (c.estimate.clone(), c.backup, c.attach);
        }
        let estimate = crate::clean::estimate(self);
        let backup = self.old_backup_stats();
        let attach = self.attachments_stats();
        self.stats_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(StorageStatsCache {
                at: now,
                estimate: estimate.clone(),
                backup,
                attach,
                data_dir: dir,
            });
        (estimate, backup, attach)
    }

    /// 存储统计变化（清理/迁移）后失效缓存，下次打开重新计算
    pub fn invalidate_stats_cache(&self) {
        self.stats_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    /// 迁移备份统计：锚点（默认安装目录）中的 corpmail.db[-wal/-shm] + attachments/。
    /// 仅当已迁移（data_dir != anchor）且锚点存在备份文件时为 Some
    pub fn old_backup_stats(&self) -> Option<(u64, u64)> {
        if self.data_dir() == self.anchor_dir {
            return None;
        }
        let mut bytes = 0u64;
        let mut files = 0u64;
        for f in ["corpmail.db", "corpmail.db-wal", "corpmail.db-shm"] {
            if let Ok(md) = std::fs::metadata(self.anchor_dir.join(f))
                && md.is_file()
            {
                bytes += md.len();
                files += 1;
            }
        }
        let (b, n) = dir_stats(&self.anchor_dir.join("attachments"));
        bytes += b;
        files += n;
        (files > 0).then_some((bytes, files))
    }

    /// 删除迁移备份（存储页按钮，验收功能）：清锚点目录中的 corpmail.db[-wal/-shm]
    /// 与 attachments/；配置文件 corpmail.toml 与当前数据不受影响。
    /// 返回 (释放字节, 删除文件数)
    pub fn delete_old_backup(&self) -> Result<(u64, u64), String> {
        // 迁移尚未重启生效：本进程仍直连旧目录库，删掉它会让后续收件/删信写进
        // 已 unlink 的 inode——重启后新目录只剩迁移时的快照，期间变更全部丢失。
        // 必须先重启（指针生效、data_dir 变为新目录）才允许清理。
        // 放在"默认目录"判断之前：此时 data_dir 仍等于锚点，否则会误报"无迁移备份"
        if self.pending_data_dir().is_some() {
            return Err("数据目录迁移尚未重启生效，请先重启应用再删除旧目录备份".into());
        }
        if self.data_dir() == self.anchor_dir {
            return Err("当前使用默认数据目录，无迁移备份".into());
        }
        if !self.data_dir().join("corpmail.db").is_file() {
            return Err("当前数据目录缺少数据库文件，拒绝删除备份".into());
        }
        if self.old_backup_stats().is_none() {
            return Err("未发现可删除的迁移备份".into());
        }
        let mut freed = 0u64;
        let mut files = 0u64;
        for f in ["corpmail.db", "corpmail.db-wal", "corpmail.db-shm"] {
            let p = self.anchor_dir.join(f);
            if p.is_file() {
                let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                std::fs::remove_file(&p).map_err(|e| format!("删除 {f} 失败：{e}"))?;
                freed += sz;
                files += 1;
            }
        }
        let att = self.anchor_dir.join("attachments");
        if att.is_dir() {
            let (b, n) = dir_stats(&att);
            std::fs::remove_dir_all(&att).map_err(|e| format!("删除备份附件目录失败：{e}"))?;
            freed += b;
            files += n;
        }
        Ok((freed, files))
    }

    /// 收件状态快照（克隆；just_finished 读取后由 clear_fetch_finish 显式清除）
    pub fn fetch_snapshot(&self) -> FetchState {
        self.fetch_guard_hung();
        self.fetch_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 看门狗：挂起轮（超出 FETCH_WATCHDOG_SECS 无进展）强制复位。
    /// 由 fetch_snapshot（状态轮询链，进行中 2s 一次）驱动。复位即 epoch+1，
    /// 挂起线程即便之后网络恢复，其 finish 也会因 epoch 失配被丢弃。
    fn fetch_guard_hung(&self) {
        let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.running && crate::mail::now_epoch() - s.started_at > FETCH_WATCHDOG_SECS {
            eprintln!("[fetch] 看门狗：收件轮超 {FETCH_WATCHDOG_SECS}s 无进展，判定挂起并释放名额");
            // imap 无读超时，这一轮可能永不返回：把名额转成 detached，否则
            // 两轮挂住就再也发不出新的收件请求，用户只能重启（审计 P1-13）
            if let Some(ticket) = self
                .round_ticket
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .upgrade()
            {
                ticket.detach();
            }
            s.running = false;
            s.epoch += 1;
            s.last_error = format!(
                "收件超时（{} 分钟无进展，可能网络中断），已复位，可重试",
                FETCH_WATCHDOG_SECS / 60
            );
            s.just_finished = true;
        }
    }

    /// 清除"刚完成"标记（状态片段消费后调用，避免重复触发列表刷新）
    pub fn clear_fetch_finish(&self) {
        self.fetch_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .just_finished = false;
    }

    /// 启动一轮收件（§6.2：手动/启动/定时共用；进行中重复触发直接忽略——防抖）。
    ///
    /// 参数快照与状态 CAS 在调用线程完成后，网络拉取与入库交由独立线程执行
    /// （独立连接，同发送 worker 模式；完成后更新游标/last_sync_at 并置 just_finished）。
    /// 装配收件连接参数（host/端口/加密方式/账号/密码）。
    /// 收件轮次与「已读回写」共用同一实现；密码缺失返回 None（由调用方决定提示或跳过）。
    pub fn imap_params(&self) -> Option<ImapParams> {
        let d = defaults();
        let g = |k: &str| self.get_or(k, &d).unwrap_or_default();
        // 端口 0 是合法 u16 但不是合法目标（见 send.rs 同款注释），回退默认 993
        let port: u16 = g(keys::RECV_PORT)
            .parse()
            .ok()
            .filter(|p| *p > 0)
            .unwrap_or(993);
        let account = full_address(&g(keys::ACCOUNT_PREFIX), &g(keys::ACCOUNT_DOMAIN));
        let password = self.retrieve_password(&account).into_password()?;
        Some(ImapParams {
            host: g(keys::RECV_HOST),
            port,
            security: crate::net::TlsMode::resolve(port, &g(keys::RECV_SECURITY)),
            account,
            password,
        })
    }

    /// 读取指定邮箱的本地增量游标（启动快照与重试续拉共用）。
    pub fn last_cursor(&self, dest: crate::mail::MailDest) -> crate::mail::LastCursor {
        let (uv_key, uid_key) = cursor_keys(dest);
        self.with_conn(|conn| read_fetch_cursor(conn, uv_key, uid_key))
    }

    /// 装配新邮件通知钩子（lib.rs setup：打包 AppHandle 调 notify::on_fetch_finished）
    pub fn set_notify_hook(&self, hook: NotifyHook) {
        *self.notify_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// 装配托盘角标刷新钩子（lib.rs setup：注入 tray::refresh_badge 闭包）
    pub fn set_tray_hook(&self, hook: TrayHook) {
        *self.tray_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// 未读数变化后刷新托盘角标（UI 变更点调用；未装配钩子时为空操作）。
    /// 角标还由发送调度线程每 20s 兑底重刷——missed poke 在一个扫描周期内
    /// 自动收敛，无需穷举全部未读变更点
    pub fn poke_tray(&self) {
        if let Some(hook) = self
            .tray_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            hook();
        }
    }

    pub fn start_fetch(&self) {
        // 迁移冻结（R2）：快照已定格，此刻收件只会写进即将被抛弃的旧库。
        // R9：进行中与待重启分开报——迁移跑着时催用户「重启」会中断本次复制
        if self.migration_pending() {
            self.set_fetch_error(match self.migration_phase() {
                MigrationPhase::Running => MIGRATION_RUNNING_FETCH_MSG.to_string(),
                _ => MIGRATION_PENDING_MSG.to_string(),
            });
            return;
        }
        // 挂起轮上限（审计 P1-5/P1-13）：看门狗已把判死的轮转为 detached 并
        // 释放名额，所以这里只需拦「detached 累积到上限」这一种病态情况
        let detached = self
            .detached_rounds
            .load(std::sync::atomic::Ordering::SeqCst);
        if detached >= MAX_DETACHED_FETCH_ROUNDS {
            self.set_fetch_error(format!(
                "已有 {detached} 轮收件在后台无响应（网络可能中断），请检查网络后重启应用"
            ));
            return;
        }
        if self.active_rounds.load(std::sync::atomic::Ordering::SeqCst)
            >= MAX_CONCURRENT_FETCH_ROUNDS
        {
            self.set_fetch_error(format!(
                "已有 {MAX_CONCURRENT_FETCH_ROUNDS} 轮收件在后台运行，本次跳过"
            ));
            return;
        }
        // 未配置静默忽略：启动 2s 首收跑在向导之前，此时账号为空，若不拦截
        // 会以空账号写一条「凭据缺失」——用户完成向导后状态条立刻显示这条
        // 假错误（真实凭据其实已存好），且要等下一个定时周期才被正常轮冲掉
        if !self.configured() {
            return;
        }
        let Some(epoch) = self.fetch_cas_begin() else {
            return; // 已在收件中（防抖 §6.2）
        };
        self.dispatch_fetch_round(epoch);
    }

    /// 「清游标 + 起新轮」的原子入口（审计 R14，设置页范围变更/重新拉取用）。
    /// 旧实现是 check-then-act：先读快照判空闲 → 写库清游标 → start_fetch；
    /// 调度线程恰在判空闲与清游标之间起轮时，运行轮收尾会把游标写回旧值——
    /// 用户改了范围/点了重拉，实际既没清也没拉，且无任何反馈。
    /// 现在「判空闲 → 清游标 → 置 running」整段持有 fetch_state 锁：调度线程
    /// 的 start_fetch 只能在整段之前或之后完整发生。写库失败由 set_batch
    /// 的事务/回滚保证整体未清，游标保持原值
    pub fn reset_and_fetch(&self, pairs: &[(&str, &str)]) -> Result<(), ResetFetchError> {
        if self.migration_pending() {
            return Err(ResetFetchError::Other(self.migration_block_msg().into()));
        }
        let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.running {
            return Err(ResetFetchError::Busy);
        }
        // 游标/参数落库：失败即整体未动（db 键单事务；配置键两通道互回滚）
        self.set_batch(pairs).map_err(ResetFetchError::Other)?;
        let epoch = begin_round(&mut s);
        drop(s);
        self.dispatch_fetch_round(epoch);
        Ok(())
    }

    /// CAS 之后的派发段（start_fetch 与 reset_and_fetch 共用）。
    /// 任何失败路径必须 fetch_end 复位 running（CAS 已置位），否则状态永久
    /// 卡在进行中；成功路径读取游标快照后 spawn 收件线程
    fn dispatch_fetch_round(&self, epoch: u64) {
        // 二次确认冻结位：与 move_data_dir 的置位存在极窄竞态窗口（它先置位再等
        // running 归零），CAS 之后再查一次即可保证「冻结后绝不起新轮」
        if self.migration_pending() {
            let msg = self.migration_block_msg();
            self.fetch_end(epoch, 0, msg);
            return;
        }
        // 挂起轮/并发轮上限（原子入口跳过了 CAS 前检查，这里统一兜住；
        // start_fetch 路径重复检查无害——原子读）
        let detached = self
            .detached_rounds
            .load(std::sync::atomic::Ordering::SeqCst);
        if detached >= MAX_DETACHED_FETCH_ROUNDS {
            self.fetch_end(
                epoch,
                0,
                &format!(
                    "已有 {detached} 轮收件在后台无响应（网络可能中断），请检查网络后重启应用"
                ),
            );
            return;
        }
        if self.active_rounds.load(std::sync::atomic::Ordering::SeqCst)
            >= MAX_CONCURRENT_FETCH_ROUNDS
        {
            self.fetch_end(
                epoch,
                0,
                &format!("已有 {MAX_CONCURRENT_FETCH_ROUNDS} 轮收件在后台运行，本次跳过"),
            );
            return;
        }
        // 未配置静默收尾（不写错误：向导前改范围/点重拉不该在状态条留
        // 「凭据缺失」假错误，配置完成后下轮收取自然按已清的游标走首拉）
        if !self.configured() {
            self.fetch_end(epoch, 0, "");
            return;
        }
        let Some(conn_params) = self.imap_params() else {
            self.fetch_end(epoch, 0, "凭据缺失，请先在设置中保存密码");
            return;
        };
        let d = defaults();
        let g = |k: &str| self.get_or(k, &d).unwrap_or_default();
        let first_days = first_fetch_days_from_raw(&g(keys::FIRST_FETCH_DAYS));
        // 单封上限：超出只拉信头存占位（防止整封 + 附件 + gzip 副本同时驻留内存）
        let max_mail_bytes = max_mail_bytes_from(&g(keys::MAX_MAIL_MB));
        let last = self.last_cursor(crate::mail::MailDest::Inbox);
        let last_sent = self.last_cursor(crate::mail::MailDest::Sent);
        let fetch_state = std::sync::Arc::clone(&self.fetch_state);
        // 冻结位随 job 进线程：每轮 attempt 开头据此立刻收手（迁移正在等它收尾）
        let migration_flag = std::sync::Arc::clone(&self.migration_pending);
        // 名额凭证：正常结束 release，看门狗判死 detach（二者只结算一次）
        let ticket = std::sync::Arc::new(RoundTicket::new(
            std::sync::Arc::clone(&self.active_rounds),
            std::sync::Arc::clone(&self.detached_rounds),
        ));
        *self.round_ticket.lock().unwrap_or_else(|e| e.into_inner()) =
            std::sync::Arc::downgrade(&ticket);
        // 通知钩子提前 clone（spawn 要 'static，锁守卫不得跨线程）
        let notify = self
            .notify_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let cur_dir = self.data_dir();
        let db_path = cur_dir.join("corpmail.db");
        let data_dir = cur_dir;
        std::thread::spawn(move || {
            // RAII：任何退出路径（含 panic 越过 fetch_round 内部 catch_unwind）都归还名额
            let _guard = RoundGuard(ticket);
            fetch_round(FetchJob {
                fetch_state,
                db_path,
                data_dir,
                host: conn_params.host,
                port: conn_params.port,
                security: conn_params.security,
                max_mail_bytes,
                account: conn_params.account,
                password: conn_params.password,
                first_days,
                last,
                last_sent,
                epoch,
                notify,
                migration: migration_flag,
            });
        });
    }

    /// 写入一条收件错误（不改变 running：正在收件时由收件线程负责收尾）
    fn set_fetch_error(&self, msg: String) {
        let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.running {
            return;
        }
        s.last_error = msg;
        s.just_finished = true;
    }

    /// 收件状态 CAS：空闲→进行中（成功 true；已在收件返回 false）
    /// 开始新轮（CAS）：成功返回该轮 epoch（finish 写状态的凭证）
    fn fetch_cas_begin(&self) -> Option<u64> {
        let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.running {
            return None;
        }
        Some(begin_round(&mut s))
    }

    /// 收件结束（凭据缺失等同步失败路径；线程路径在闭包内直接更新 Arc）
    /// 同步失败收尾（start_fetch 线程前路径）。带 epoch 凭证校验
    fn fetch_end(&self, epoch: u64, new_count: i64, error: &str) {
        let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.epoch != epoch {
            return; // 看门狗已复位或新一轮已开始：迟到结果丢弃
        }
        s.running = false;
        s.new_count = new_count;
        s.last_error = error.into();
        s.just_finished = true;
    }

    /// 启动发送 worker（M3，设计 §2 单飞线程；生产装配于 lib.rs，测试不启动）。
    ///
    /// 独立连接 + MPSC 队列逐封派发（企业 SMTP 串行更稳）；
    /// 启动时扫描残留 queued 一并补发（崩溃恢复）。
    /// 发送队列装配：启动残留 queued 补发 + 后续入队即时派发。
    /// worker 持独立连接与配置镜像 Arc（配置源 corpmail.toml，不占库锁）
    pub fn start_send_worker(&self) {
        let mut guard = self.send_tx.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return;
        }
        let dir = self.data_dir();
        let Ok(conn) = crate::db::open(&dir.join("corpmail.db")) else {
            return;
        };
        let config_arc = std::sync::Arc::clone(&self.config);
        // 附件根目录：worker 线程独立取路径（不持 AppState；附件随信发送时读盘解压）
        let attach_root = dir.join("attachments");
        // 凭据回退密钥（锚点目录；库外文件，见 secrets::KEY_FILE）
        let key_file = self.key_file();
        // 迁移冻结位（R2）：冻结期间不派发，邮件留在 queued 由重启后的补发扫描接手
        let migration_flag = std::sync::Arc::clone(&self.migration_pending);
        let (tx, rx) = std::sync::mpsc::channel::<i64>();
        // 退出确认位（R16）：worker 线程退出时置位，stop_send_worker 据此等待
        let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        *self
            .send_worker_exited
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::clone(&exited));
        // 崩溃恢复 1/2：`sending` 是仅存在于进程内的中间态，跨进程无合法含义。
        // 进程在发送中被杀（关机/崩溃/强退）后该邮件会永久停在 sending：
        // 启动补发只查 queued、reopen_failed 只收 failed、详情页 2s 轮询永不停止。
        // 按 sent_at 分开处理：已写投递时间 → 补成 sent（不能诱导重发）；
        // 未写 → 复位 failed 并写明原因，把「是否重发」的决策交回用户
        let recovered = crate::mail::recover_interrupted_sends(&conn);
        if recovered > 0 {
            eprintln!("[send] 启动自愈：{recovered} 封中断的 sending 邮件已按投递事实复位");
        }
        // 崩溃恢复 2/2：残留 queued 补发（按 id 升序）
        let stale: Vec<i64> = conn
            .prepare("SELECT id FROM emails WHERE status = 'queued' ORDER BY id")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, i64>(0))
                    .map(|rows| rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();
        std::thread::spawn(move || {
            for id in rx {
                // 迁移冻结（R2）：已投递事实尚未落定就可能被本进程写回旧库，
                // 直接跳过不派发——邮件保持 queued，重启后由残留扫描补发
                if migration_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    eprintln!("[send] 迁移冻结中，邮件 {id} 保持排队（重启后补发）");
                    continue;
                }
                // 发送参数快照：每封派发前重读配置（corpmail.toml；改动即时生效，
                // 不落库——验收拍板简化）
                let smtp = {
                    let cfg = config_arc.lock().unwrap_or_else(|e| e.into_inner());
                    crate::mail::SmtpConfig::from_config(&cfg)
                };
                // 派发内部所有终态（含各类失败）均已落库；此处 Err 仅剩
                // "抢占失败/读库异常"两条管理性路径，记日志供排查即可。
                // catch_unwind：lettre/MIME 编码等任何 panic 不得打死 worker 线程
                // ——worker 一死 channel 关闭，此后所有 enqueue_send 静默无效，
                // 邮件停在 queued 直到重启（审计 P2-16）
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::mail::dispatch_one(
                        &conn,
                        &crate::mail::RealSender,
                        id,
                        &smtp,
                        &attach_root,
                        &key_file,
                    )
                }));
                match outcome {
                    Ok(Ok(_)) => {}
                    Ok(Err(err)) => eprintln!("[send] 邮件 {id} 派发未执行：{err}"),
                    Err(_) => {
                        eprintln!("[send] 邮件 {id} 派发 panic（已捕获，标记失败后继续）");
                        // 已抢占到 sending 的邮件必须落定，否则只能等下次启动自愈。
                        // 落定失败要留痕（审计 P1-50）：此时邮件停在 sending，
                        // 界面会一直显示"发送中"，用户只能等重启
                        if let Err(err) = conn.execute(
                            "UPDATE emails SET status = 'failed',
                                    error = '发送过程异常中断，请重发'
                             WHERE id = ?1 AND status = 'sending'",
                            [id],
                        ) {
                            eprintln!(
                                "[send] 邮件 {id} 标记失败未生效（停留 sending，重启后由残留扫描自愈）：{err}"
                            );
                        }
                    }
                }
            }
            // 通道关闭（所有 sender 已 drop）：置退出位供 stop_send_worker 确认
            exited.store(true, std::sync::atomic::Ordering::Release);
        });
        for id in stale {
            let _ = tx.send(id);
        }
        *guard = Some(tx);
    }

    /// 入队发送（无 worker 时为空操作：邮件停留 queued 态，下次启动补发）
    pub fn enqueue_send(&self, id: i64) {
        if let Some(tx) = self
            .send_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = tx.send(id);
        }
    }

    /// 装配已读回写 worker 的唤醒口（lib.rs 启动时调用一次）
    pub fn set_seen_channel(&self, tx: std::sync::mpsc::Sender<()>) {
        *self.seen_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    /// 唤醒已读回写 worker（合并窗口由 worker 侧实现）。
    /// 无 worker（测试/未装配）时为空操作：本地已读已生效，服务器状态下次收件自然对齐
    pub fn notify_seen(&self) {
        if let Some(tx) = self
            .seen_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = tx.send(());
        }
    }

    /// 当前登录账号（完整地址，只读配置、不触凭据）。
    /// seen 回写的归属口径（审计 R13）：mark_read 记账号、worker 按账号筛行
    pub fn current_account(&self) -> String {
        let d = defaults();
        let g = |k: &str| self.get_or(k, &d).unwrap_or_default();
        full_address(&g(keys::ACCOUNT_PREFIX), &g(keys::ACCOUNT_DOMAIN))
    }

    /// 是否已完成首启配置（向导判定依据：账号前缀已写入）
    pub fn configured(&self) -> bool {
        self.get(keys::ACCOUNT_PREFIX)
            .map(|v| v.filter(|s| !s.is_empty()).is_some())
            .unwrap_or(false)
    }

    /// 借出连接执行闭包（邮件存储层 mail 模块使用；锁随闭包返回释放）。
    /// 锁覆盖 db+data_dir 两者：闭包内不得再调用 `self.data_dir()`
    /// （同线程重入会死锁；目录快照先在闭包外取好）
    pub fn with_conn<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut store.db)
    }

    /// 配置镜像快照（外部线程/测试读 corpmail.toml 内存镜像用；worker 线程直接持 Arc）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn config_snapshot(&self) -> crate::conf::ConfigMap {
        self.config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 读键。配置键（conf::CONFIG_KEYS：数据目录/服务器/账号）仅从 corpmail.toml
    /// 内存镜像读（文件为唯一持久源，不落库）；其余读库
    pub fn get(&self, key: &str) -> Option<Option<String>> {
        if crate::conf::is_config_key(key) {
            let cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
            return Some(cfg.get(key).cloned());
        }
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let conn = &store.db;
        crate::db::settings::get(conn, key).ok()
    }

    /// 是否允许加载邮件正文里的远程图片（默认否，审计 R4/R36）。
    /// `/proxy/image` 在关闭时直接回占位图、不解析域名不建连——该判断必须在
    /// 构造任何网络客户端之前完成，故放在状态层而不是正文改写层。
    pub fn allow_remote_images(&self) -> bool {
        matches!(
            self.get(keys::ALLOW_REMOTE_IMAGES),
            Some(Some(v)) if v == "1" || v.eq_ignore_ascii_case("true")
        )
    }

    /// 读键，未设置回退默认值表
    pub fn get_or(&self, key: &str, defaults: &[(&'static str, &'static str)]) -> Option<String> {
        if let Some(Some(v)) = self.get(key) {
            return Some(v);
        }
        defaults
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_string())
    }

    /// 发件显示名（配置留空时从发件箱补齐，并写回 corpmail.toml）。
    ///
    /// 首设向导「发件显示名」选填；留空时服务器「已发送」邮件的 From 头带着
    /// 用户本人姓名，是本地最可靠的依据。命中后写回配置，后续读取直接命中
    /// 内存镜像不再扫库。写回失败不阻断业务（下次读取重试）。
    ///
    /// `display_name_auto` **只由设置·账号页「用户主动清空显示名」写入**（值 "1"）：
    /// 自动推断本身不写标记，否则一次推断错误会被永久固化、改名后也不再重试（审计 C2）。
    /// 推断失败按 60s 节流，收件完成后 `invalidate_display_name_probe()` 主动放行。
    pub fn display_name(&self) -> String {
        if let Some(v) = self.configured_display_name() {
            return v;
        }
        if self
            .get(keys::DISPLAY_NAME_AUTO)
            .flatten()
            .is_some_and(|v| !v.trim().is_empty())
        {
            return String::new();
        }
        // 未配置账号（首启向导）时不推断：库里的历史邮件可能属于别的账号
        if !self.configured() {
            return String::new();
        }
        // 启动、收件完成、5 处读取点共 8 个触发点：check-then-act 全程互斥，
        // 锁序固定 name_probe → config → db（其他路径不取 name_probe，无环）
        let mut probe = self.name_probe.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = self.configured_display_name() {
            return v;
        }
        if probe
            .last_miss
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60))
        {
            return String::new();
        }
        let d = defaults();
        let g = |k: &str| self.get_or(k, &d).unwrap_or_default();
        let account = full_address(&g(keys::ACCOUNT_PREFIX), &g(keys::ACCOUNT_DOMAIN));
        let name = self.with_conn(|c| {
            // 库被显式标记为别的账号时不得取用其姓名。只读显式 STORE_OWNER
            // 行，不用 store_owner_conn 的 From 众数推导：别名发件是正常场景，
            // 众数推导会把本账号库误判成他属（实测 4 封别名 + 1 封本账号）。
            // 无标记时靠「账号必须出现在 sent.from_addr」硬匹配兜底（审计 C3）。
            let owner = crate::db::settings::get(c, keys::STORE_OWNER)
                .ok()
                .flatten();
            if owner
                .as_deref()
                .is_some_and(|o| !o.trim().eq_ignore_ascii_case(account.trim()))
            {
                return None;
            }
            crate::mail::infer_display_name_conn(c, &account)
        });
        let Some(name) = name else {
            probe.last_miss = Some(std::time::Instant::now());
            return String::new();
        };
        // 只写 DISPLAY_NAME：标记专属于用户主动清空动作
        if let Err(e) = self.set_batch(&[(keys::DISPLAY_NAME, name.as_str())]) {
            eprintln!("[display] 发件显示名自动推断写回失败（忽略）：{e}");
        }
        probe.last_miss = None;
        name
    }

    fn configured_display_name(&self) -> Option<String> {
        match self.get(keys::DISPLAY_NAME) {
            Some(Some(v)) if !v.trim().is_empty() => Some(v.trim().to_string()),
            _ => None,
        }
    }

    /// 收件完成后调用：清掉推断失败节流，让新到的发件箱邮件立即参与推断
    pub fn invalidate_display_name_probe(&self) {
        let mut probe = self.name_probe.lock().unwrap_or_else(|e| e.into_inner());
        probe.last_miss = None;
    }

    /// 批量写。路由拆分（验收拍板简化）：配置键仅写独立配置文件 corpmail.toml
    /// （先写文件，成功后才处理库键）；非配置键走库单事务（全库单一写入口不变）。
    ///
    /// 两通道一体失败语义（审计 R10）：文件段在一次 config 锁内完成
    /// read-modify-write（旧实现拿三段锁，中间可被并发写插入造成镜像与文件漂移）；
    /// 库事务失败时同步回滚文件与内存镜像，不留「文件已改、库没改」的分裂态。
    /// 向导混合键（配置 + 库键同批）保持一次调用，原子性与本方法一致
    pub fn set_batch(&self, pairs: &[(&str, &str)]) -> Result<(), String> {
        let cfg_pairs: Vec<(&str, &str)> = pairs
            .iter()
            .copied()
            .filter(|(k, _)| crate::conf::is_config_key(k))
            .collect();
        let db_pairs: Vec<(&str, &str)> = pairs
            .iter()
            .copied()
            .filter(|(k, _)| !crate::conf::is_config_key(k))
            .collect();
        if cfg_pairs.is_empty() {
            return self.write_db_pairs(&db_pairs);
        }
        // 文件段：单锁 read-modify-write——快照旧值、改镜像、落文件一气呵成
        let (prev, save_res) = {
            let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
            let prev: Vec<(String, Option<String>)> = cfg_pairs
                .iter()
                .map(|(k, _)| ((*k).to_string(), cfg.get(*k).cloned()))
                .collect();
            for (k, v) in &cfg_pairs {
                cfg.insert((*k).to_string(), (*v).to_string());
            }
            let snapshot = cfg.clone();
            (prev, crate::conf::save(&self.anchor_dir, &snapshot))
        };
        if let Err(e) = save_res {
            // 文件没写进去：只还原镜像（文件在盘上的内容未变）
            let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
            for (k, old) in prev {
                match old {
                    Some(v) => {
                        cfg.insert(k, v);
                    }
                    None => {
                        cfg.remove(&k);
                    }
                }
            }
            return Err(format!("配置文件写入失败：{e}"));
        }
        if !db_pairs.is_empty()
            && let Err(e) = self.write_db_pairs(&db_pairs)
        {
            // 库键失败：文件已带新值——必须把 prev 写回镜像与文件，否则
            // 配置面（host/账号/目录指针这类关键字段）静默半生效
            if let Err(rb) = self.restore_config_to(&prev) {
                eprintln!("[state] 配置回滚失败（corpmail.toml 可能与库不一致，请人工检查）：{rb}");
            }
            return Err(e);
        }
        Ok(())
    }

    /// 库键写入段（单事务：全成或全败，无需额外回滚）
    fn write_db_pairs(&self, db_pairs: &[(&str, &str)]) -> Result<(), String> {
        if db_pairs.is_empty() {
            return Ok(());
        }
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let conn = &mut store.db;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for (k, v) in db_pairs {
            tx.execute(
                "INSERT INTO settings(k, v) VALUES (?1, ?2)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                [k, v],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// 把配置镜像恢复到 prev 快照并写回文件（set_batch 库段失败回滚用）。
    /// 再失败只返回错误交调用方留痕：文件 IO 已坏时没有更优动作
    fn restore_config_to(&self, prev: &[(String, Option<String>)]) -> Result<(), String> {
        let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
        for (k, old) in prev {
            match old {
                Some(v) => {
                    cfg.insert(k.clone(), v.clone());
                }
                None => {
                    cfg.remove(k);
                }
            }
        }
        let snapshot = cfg.clone();
        crate::conf::save(&self.anchor_dir, &snapshot)
    }

    /// 数据库文件大小（字节；设置·存储页"邮箱数据库大小"；含 WAL 与索引开销）
    pub fn db_size_bytes(&self) -> u64 {
        ["corpmail.db", "corpmail.db-wal"]
            .iter()
            .filter_map(|f| std::fs::metadata(self.data_dir().join(f)).ok())
            .map(|m| m.len())
            .sum()
    }

    /// 侧栏文件夹计数（列表屏冗余展示）。
    ///
    /// 旧实现一条 `SELECT SUM(folder='inbox') … FROM emails` 全表聚合，每次
    /// 渲染（列表页 + 设置页各一次）都扫全部行；现在四个计数各自走索引
    /// （`idx_folder_sort` 前缀 + `idx_inbox_unread` 部分索引），且**失败时
    /// 返回上次成功值**而不是清零——角标突然变 0 会被理解成"邮件被删了"
    /// （审计 R63）
    pub fn folder_counts(&self) -> HashMap<&'static str, i64> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let conn = &store.db;
        match Self::count_mailboxes(conn) {
            Ok(counts) => {
                *self.counts_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(counts.clone());
                counts
            }
            Err(err) => {
                eprintln!("[ui] 文件夹计数读取失败（沿用上次值）：{err}");
                self.counts_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(Self::zero_counts)
            }
        }
    }

    /// 四个角标计数：每个查询都带 folder 谓词，各自命中索引
    /// （`idx_folder_sort` 前缀 / `idx_inbox_unread` 部分索引），不再全表 SUM
    fn count_mailboxes(conn: &Connection) -> rusqlite::Result<HashMap<&'static str, i64>> {
        let by_folder = |folder: &str| -> rusqlite::Result<i64> {
            conn.prepare_cached("SELECT count(*) FROM emails WHERE folder = ?1")?
                .query_row([folder], |r| r.get(0))
        };
        let inbox_unread = conn
            .prepare_cached("SELECT count(*) FROM emails WHERE folder = 'inbox' AND unread = 1")?
            .query_row([], |r| r.get(0))?;
        Ok(HashMap::from([
            ("inbox", by_folder("inbox")?),
            ("inbox_unread", inbox_unread),
            ("sent", by_folder("sent")?),
            ("draft", by_folder("draft")?),
        ]))
    }

    /// 无历史值时的兜底（等价于旧行为的"全 0"，但只出现在首次读就失败的情况）
    fn zero_counts() -> HashMap<&'static str, i64> {
        HashMap::from([
            ("inbox", 0i64),
            ("inbox_unread", 0),
            ("sent", 0),
            ("draft", 0),
        ])
    }

    /// 回退密钥文件路径（锚点目录，与 corpmail.toml 同处）。
    /// 放锚点而非数据目录：数据目录可被迁到外置盘/U 盘，密钥跟着走就失去
    /// 「密钥与密文分离」的意义（审计：库内容易外流，备份/同步盘/误传）
    pub fn key_file(&self) -> PathBuf {
        self.anchor_dir.join(crate::secrets::KEY_FILE)
    }

    /// 存密码（secrets 封装；返回是否发生 keyfile 降级）
    pub fn store_password(&self, account: &str, password: &str) -> Result<bool, String> {
        let key_file = self.key_file();
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let conn = &store.db;
        crate::secrets::store(conn, &key_file, account, password)
    }

    /// 读密码（keychain 优先，keyfile 回退）。
    /// 命中进程内缓存时不取 DB 锁：keychain 授权窗口可能长等，不能连带阻塞 UI。
    /// 未命中时同样不得带着 DB 锁进 keychain（审计 R15）——两段式：
    /// 先无锁探 keychain，命中直接返回；未命中再取锁做 keyfile 回退与缓存写入
    pub fn retrieve_password(&self, account: &str) -> crate::secrets::GetResult {
        if let Some(hit) = crate::secrets::cached(account) {
            return hit;
        }
        let key_file = self.key_file();
        // keychain 段：不碰库。授权弹窗期间数据库照常可读写
        if let Some(hit) = crate::secrets::keychain_get(account) {
            return hit;
        }
        // keyfile 段：需要库连接（密文行在 settings 里），此时才取锁
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let conn = &store.db;
        crate::secrets::retrieve_fallback(conn, &key_file, account)
    }

    /// 清除已存凭据（测试清理用；设置页账号只读化后生产路径暂不触达，
    /// 保留给后续"退出账号"类功能）。
    /// `secrets::clear` 返回 Err 时只留痕：本方法当前无生产调用点，
    /// 没有可回显的界面（接线到"退出账号"时再改为 Result 向上传播）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn clear_password(&self, account: &str) {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let conn = &store.db;
        if let Err(err) = crate::secrets::clear(conn, account) {
            eprintln!("[secrets] 账号 {account} 密码清除失败：{err}");
        }
    }

    /// keychain 是否处于 keyfile 降级态（设置·账号提示用）
    pub fn secrets_degraded(&self, account: &str) -> bool {
        let key_file = self.key_file();
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let conn = &store.db;
        crate::secrets::degraded(conn, &key_file, account)
    }

    /// 待生效的数据目录（M5 迁移）：读 corpmail.toml 指针；
    /// 重启后 resolve_start_dir 应用指针、其值 == 当前目录，提示自然消失
    pub fn pending_data_dir(&self) -> Option<String> {
        let over = match self.get(keys::DATA_DIR) {
            Some(Some(v)) if !v.trim().is_empty() => v,
            _ => return None,
        };
        let pending = PathBuf::from(&over);
        (pending != self.data_dir() && pending.join("corpmail.db").is_file()).then_some(over)
    }

    /// 数据目录下的独立附件占用（目录不存在 → 0）：(字节, 文件数)
    pub fn attachments_stats(&self) -> (u64, u64) {
        dir_stats(&self.data_dir().join("attachments"))
    }
}

/// 数据目录目标校验（迁移与首设直切共用）：绝对路径、不与当前目录互相嵌套、
/// 目标须为空（无 corpmail.db / attachments——避免与既有文件混同导致回滚无法
/// 区分"本次写入"与"用户原有文件"）、可创建可写。通过返回归一化后的目标路径。
fn validate_data_dir(cur: &Path, dest: &str) -> Result<PathBuf, String> {
    let dest_path = normalize_dest(dest)?;
    if dest_path == cur {
        return Err("目标目录与当前数据目录相同".into());
    }
    if cur.starts_with(&dest_path) || dest_path.starts_with(cur) {
        return Err("目标目录不能位于当前数据目录内（或反向包含）".into());
    }
    std::fs::create_dir_all(&dest_path).map_err(|e| format!("目标目录无法创建：{e}"))?;
    // A8：上次进程被强杀留下的半成品，凭本应用自己写的标记安全清理。
    // 必须放在「目标须为空」检查之前，否则用户会被「请选择空目录」卡死在原目录
    recover_incomplete_migration(&dest_path)?;
    if dest_path.join("corpmail.db").exists() {
        return Err("目标目录已存在 corpmail.db，请选择空目录".into());
    }
    if dest_path.join("attachments").exists() {
        return Err("目标目录已存在 attachments 目录，为免与已有文件混同请选择空目录".into());
    }
    let probe = dest_path.join(".corpmail-write-probe");
    // 上次进程在清理前被杀可能残留探针：先自愈清理，失败不得静默
    if probe.exists()
        && let Err(e) = std::fs::remove_file(&probe)
    {
        eprintln!("[state] 残留写入探针清理失败（{}）：{e}", probe.display());
    }
    std::fs::write(&probe, b"ok").map_err(|e| format!("目标目录不可写：{e}"))?;
    if let Err(e) = std::fs::remove_file(&probe) {
        eprintln!("[state] 写入探针清理失败（{}）：{e}", probe.display());
    }
    Ok(dest_path)
}

/// 自愈上次中断的迁移产物（审计 A8）。只有本应用写的 `MIGRATION_MARKER`
/// 存在时才动手；`validate_data_dir` 本就要求目标不含 `corpmail.db` /
/// `attachments`，因此带标记时这两处必定是上次迁移写出的，不会误伤用户文件。
/// R3（复查审计）：`MIGRATION_DONE_MARKER` 在场 = 上次迁移**已成功**（marker
/// 删除失败的残留），库与附件是有效数据——只清两个标记，绝不删数据。
fn recover_incomplete_migration(dest: &Path) -> Result<(), String> {
    let marker = dest.join(MIGRATION_MARKER);
    let done = dest.join(MIGRATION_DONE_MARKER);
    if !marker.exists() {
        // 无 incomplete 标记：顺手清掉孤立的 done 哨兵（上次成功迁移后目录
        // 未被复用，哨兵失去意义；清不掉只留痕，不影响判定）
        if done.exists()
            && let Err(e) = std::fs::remove_file(&done)
        {
            eprintln!("[state] 终态哨兵清理失败（{}）：{e}", done.display());
        }
        return Ok(());
    }
    if done.exists() {
        // 迁移成功过：只清标记，数据必须保留。清不掉要报错——不能带着
        // incomplete 标记放行，否则下次会被当半成品清理
        let mut errs = Vec::new();
        if let Err(e) = std::fs::remove_file(&marker) {
            errs.push(format!("迁移标记清理失败（{}）：{e}", marker.display()));
        }
        if let Err(e) = std::fs::remove_file(&done) {
            errs.push(format!("终态哨兵清理失败（{}）：{e}", done.display()));
        }
        if !errs.is_empty() {
            return Err(errs.join("；"));
        }
        eprintln!(
            "[state] 目标目录为已完成的迁移（终态哨兵在场），仅清理标记：{}",
            dest.display()
        );
        return Ok(());
    }
    let db = dest.join("corpmail.db");
    if db.exists() {
        std::fs::remove_file(&db)
            .map_err(|e| format!("上次迁移残留库清理失败（{}）：{e}", db.display()))?;
    }
    let att = dest.join("attachments");
    if att.is_dir() {
        std::fs::remove_dir_all(&att)
            .map_err(|e| format!("上次迁移残留附件清理失败（{}）：{e}", att.display()))?;
    }
    std::fs::remove_file(&marker)
        .map_err(|e| format!("迁移标记清理失败（{}）：{e}", marker.display()))?;
    eprintln!("[state] 已清理上次未完成的迁移残留：{}", dest.display());
    Ok(())
}

/// 数据目录目标路径归一化：去首尾空白与尾斜杠（Windows "C:" 盘符根带反斜杠
/// 保留，不得被清成非绝对路径），要求绝对路径。迁移校验、首设直切校验、
/// 向导"目标目录是否含既有库"探测共用，保证各处口径一致。
pub(crate) fn normalize_dest(dest: &str) -> Result<PathBuf, String> {
    let raw = dest.trim();
    let trimmed = raw.trim_end_matches(['/', '\\']);
    let dest_str = if trimmed.is_empty() {
        raw.to_string()
    } else if trimmed.len() == 2 && trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    };
    let dest_path = PathBuf::from(dest_str);
    if !dest_path.is_absolute() {
        return Err("请选择绝对路径目录".into());
    }
    Ok(dest_path)
}

/// 采纳既有库（首设直连）的路径形态校验：绝对路径、不与当前目录互相嵌套、
/// 含 corpmail.db（无库可采纳）。不设"目标须为空"约束——既有库正是采纳对象；
/// 读写能力由随后的 db::open 实际验证。
fn validate_adopt_dir(cur: &Path, dest: &str) -> Result<PathBuf, String> {
    let dest_path = normalize_dest(dest)?;
    if dest_path == cur {
        return Err("目标目录与当前数据目录相同".into());
    }
    if cur.starts_with(&dest_path) || dest_path.starts_with(cur) {
        return Err("目标目录不能位于当前数据目录内（或反向包含）".into());
    }
    if !dest_path.join("corpmail.db").is_file() {
        return Err("目标目录中没有 corpmail.db，不是可沿用的数据目录".into());
    }
    Ok(dest_path)
}

/// 直切/采纳后的旧目录（仅锚点默认目录）空库清理：调用方前置已保证旧库为空，
/// 删掉启动时新建的空库文件——否则存储页会把它当"迁移备份"展示，误导用户。
/// 非默认旧目录（异常历史）不动用户文件。
fn clean_empty_legacy_db(cur: &Path, anchor: &Path) {
    if cur == anchor {
        for f in ["corpmail.db", "corpmail.db-wal", "corpmail.db-shm"] {
            let _ = std::fs::remove_file(cur.join(f));
        }
    }
}

/// 库归属账号与登录账号的比对结论（首设时判定既有目录能否直接沿用）
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StoreOwner {
    /// 归属一致：可沿用（库为空的情形由调用方先行判空放行）
    Match,
    /// 归属明确但他属：拒绝沿用（携带库内推导出的账号，用于提示）
    Mismatch(String),
    /// 库非空但无法判定归属：拒绝沿用（宁可保守）
    Unknown,
}

/// 推导库的归属账号（完整地址小写；无法判定 → None）。依据按可信度排序：
/// 1. `store_owner` 标记行（本应用配置完成时写入，权威）；
/// 2. 已发送/草稿邮件中出现最多的 From（schema 约定 direction=out 的
///    from_addr 即登录账号，与联系人"自身地址"推导同源，天然覆盖别名）；
/// 3. 收件邮件 To/Cc 中出现最多的地址（纯收件库兜底；邮件列表场景可能
///    推导出列表地址——结果只用于一致性比对，推导偏差只会更保守地拒绝）。
pub(crate) fn store_owner_conn(conn: &Connection) -> Option<String> {
    use rusqlite::OptionalExtension;
    // ① 标记行
    if let Some(m) = conn
        .query_row(
            "SELECT v FROM settings WHERE k = ?1",
            [keys::STORE_OWNER],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    {
        let m = m.trim().to_ascii_lowercase();
        if !m.is_empty() {
            return Some(m);
        }
    }
    // ② 发件 From 模式（out 邮件的 from_addr = 登录账号；空库无发件 → 空）
    let sent = crate::mail::contacts::self_addr_conn(conn);
    if !sent.is_empty() {
        return Some(sent);
    }
    // ③ 收件收件人模式：逗号分隔地址去掉空白后计数取最大（(次数, 地址)
    //    元组取 max 保证并列时结果确定）
    let mut counts: HashMap<String, i64> = HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT to_addrs, cc_addrs FROM emails WHERE direction = 'in'")
    else {
        return None;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return None;
    };
    while let Ok(Some(row)) = rows.next() {
        for col in [0usize, 1usize] {
            if let Ok(list) = row.get::<_, String>(col) {
                for part in list.split(',') {
                    let a = part.trim().to_ascii_lowercase();
                    if a.contains('@') {
                        *counts.entry(a).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|(a, n)| (*n, a.clone()))
        .map(|(a, _)| a)
}

/// 库归属与登录账号比对（account 大小写不敏感）
pub(crate) fn check_store_owner_conn(conn: &Connection, account: &str) -> StoreOwner {
    let want = account.trim().to_ascii_lowercase();
    match store_owner_conn(conn) {
        Some(o) if o == want => StoreOwner::Match,
        Some(o) => StoreOwner::Mismatch(o),
        None => StoreOwner::Unknown,
    }
}

/// 目录递归统计：(字节, 文件数)；目录不存在 → (0, 0)（附件占用与迁移备份共用）。
/// 符号链接一律跳过、深度封顶（A13）：`metadata()` 会解引用，可能把目录外
/// 的文件算进来，遇到自指链接还会无限递归
fn dir_stats(p: &Path) -> (u64, u64) {
    fn walk(p: &Path, depth: usize) -> (u64, u64) {
        if depth > MAX_DIR_DEPTH {
            return (0, 0);
        }
        let mut bytes = 0;
        let mut files = 0;
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                match std::fs::symlink_metadata(e.path()) {
                    Ok(m) if m.is_file() => {
                        bytes += m.len();
                        files += 1;
                    }
                    Ok(m) if m.is_dir() => {
                        let (b, f) = walk(&e.path(), depth + 1);
                        bytes += b;
                        files += f;
                    }
                    _ => {}
                }
            }
        }
        (bytes, files)
    }
    walk(p, 0)
}

/// 递归复制目录（M5 迁移用：附件目录两级结构 {YYYY-MM}/{email_id}/ 文件）；
/// 目标已存在的同名文件覆盖，缺失目录逐级创建。写入成功的目标路径追加进
/// `written`，供失败回滚精确删除本次产出物（不误伤目标目录既有文件）
fn copy_dir_recursive(
    src: &Path,
    dest: &Path,
    written: &mut Vec<PathBuf>,
    copied: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
    total: usize,
    emit_files: &(dyn Fn(usize, usize) + Sync),
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_DIR_DEPTH {
        return Err(format!(
            "附件目录层级超过 {MAX_DIR_DEPTH} 层（{}），拒绝继续复制",
            src.display()
        ));
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("创建目录失败：{e}"))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("读取目录失败：{e}"))? {
        let entry = entry.map_err(|e| format!("遍历失败：{e}"))?;
        // A13：symlink_metadata 不解引用。符号链接（文件或目录）一律跳过，
        // 否则 std::fs::copy 会把链接目标的内容带进新目录，链接环还会失控递归
        let md = entry
            .path()
            .symlink_metadata()
            .map_err(|e| format!("读取类型失败：{e}"))?;
        if md.is_symlink() {
            continue;
        }
        let to = dest.join(entry.file_name());
        if md.is_dir() {
            copy_dir_recursive(&entry.path(), &to, written, copied, total, emit_files, depth + 1)?;
        } else if md.is_file() {
            std::fs::copy(entry.path(), &to).map_err(|e| format!("复制文件失败：{e}"))?;
            let n = copied.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            // 节流策略在调用方闭包里（A7）：这里只报当前计数
            emit_files(n, total);
            written.push(to);
        }
    }
    Ok(())
}

/// 统计目录内文件总数（附件复制进度用；失败目录按 0 处理，不影响复制本体）。
/// 口径必须与 `copy_dir_recursive` 一致：同样跳符号链接、同样深度封顶，
/// 否则进度条的 total 与实际复制数对不上
fn count_attachment_files(dir: &Path) -> usize {
    fn walk(dir: &Path, depth: usize) -> usize {
        if depth > MAX_DIR_DEPTH {
            return 0;
        }
        let mut n = 0;
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                match std::fs::symlink_metadata(e.path()) {
                    Ok(m) if m.is_dir() => n += walk(&e.path(), depth + 1),
                    Ok(m) if m.is_file() => n += 1,
                    _ => {}
                }
            }
        }
        n
    }
    walk(dir, 0)
}

/// 首拉天数解析：0 = 全部（UID SEARCH ALL，合法值，不得作非法过滤）；
/// >0 = SINCE 天数（7/30/90 等）；解析失败（未配置/非法）兜底默认 30（与 defaults 一致）。
///
/// 修复：旧实现 `.parse().ok().filter(|v| *v > 0).unwrap_or(7)` 把"全部"(=0)
/// 误判为非法并回退到 7 天，导致用户在设置里选"全部"实际只拉近 7 天邮件。
/// 上限 3650 天（10 年）：值来自配置文件/表单，负数或天文数字只会让
/// `now - days*86400` 溢出或扫全库，钳到合理区间。
fn first_fetch_days_from_raw(raw: &str) -> i64 {
    raw.trim().parse::<i64>().unwrap_or(30).clamp(0, 3650)
}

/// 单封收件上限（字节）：解析失败/未配置取默认 50MB；超出区间钳制到 1..=200MB
/// （设置页与收件线程同源，防手改配置把上限推成天文数字或 0）
pub fn max_mail_bytes_from(raw: &str) -> u64 {
    let mb = raw
        .trim()
        .parse::<u32>()
        .unwrap_or(50)
        .clamp(MAX_MAIL_MB_MIN, MAX_MAIL_MB_MAX);
    u64::from(mb) * 1024 * 1024
}

/// 收件线程名额归还（Drop 时结算：正常结束 release，被看门狗判死则已在 detach
/// 中结算过，这里不会重复递减）
struct RoundGuard(std::sync::Arc<RoundTicket>);

impl Drop for RoundGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// 单轮收件自动尝试次数（首次 + 2 次重试）：大邮箱首拉的 IMAP 会话可能被
/// 服务器/防火墙中途掐断（企业服务器常限制单连接时长或命令数），或遇瞬时
/// 网络抖动。游标已按批持久化（断点续传），重试从 last.uid+1 增量续拉，
/// 代价仅为重连；imap crate 无会话级读超时 API，挂死连接由看门狗兜底
const FETCH_ATTEMPTS: usize = 3;

/// 第 n 次失败后的重试等待（秒）：给服务器限流/网络抖动留恢复窗口。
/// 指数增长（15s → 30s → 60s…，上限 120s）：轮内只剩两次重试，
/// 但同一口径在长故障下不会退化成固定频率敲门（审计 R62）
fn fetch_retry_delay(attempt: u32) -> u64 {
    crate::mail::backoff_secs(attempt, 15, 120)
}

/// 从轮次专用连接读当前游标（重试前调用）：上一轮尝试的批次已持久化，
/// 重读游标即可从断点续传，无需重拉已入库邮件（入库侧另有去重兜底）
fn read_fetch_cursor(conn: &Connection, uv_key: &str, uid_key: &str) -> crate::mail::LastCursor {
    let parse = |key: &str| -> u32 {
        crate::db::settings::get(conn, key)
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    crate::mail::LastCursor {
        uidvalidity: parse(uv_key),
        uid: parse(uid_key),
    }
}

/// 首拉完成标志读取（"1" = 该箱首拉范围已完整拉取）。
/// 标志持久在库：中断重试（同轮 attempt 或重启后的新轮）都据此判断
/// "发件箱拉取是否完成"，未完成的箱才需要重跑
fn first_pull_done(conn: &Connection, dest: crate::mail::MailDest) -> bool {
    let key = match dest {
        crate::mail::MailDest::Sent => keys::FIRST_PULL_DONE_SENT,
        crate::mail::MailDest::Inbox => keys::FIRST_PULL_DONE_INBOX,
    };
    crate::db::settings::get(conn, key)
        .ok()
        .flatten()
        .is_some_and(|v| v == "1")
}

/// 置首拉完成标志。失败仅记日志：标志丢失的代价是下轮多发一次全量查询，
/// 由 remote_uid 入库去重兜底，不值得为它判定整轮失败
fn mark_first_pull_done(conn: &Connection, dest: crate::mail::MailDest) {
    let key = match dest {
        crate::mail::MailDest::Sent => keys::FIRST_PULL_DONE_SENT,
        crate::mail::MailDest::Inbox => keys::FIRST_PULL_DONE_INBOX,
    };
    if let Err(err) = crate::db::settings::set(conn, key, "1") {
        eprintln!("[fetch] 首拉完成标志写入失败（{key}）：{err}");
    }
}

/// 同步目标 → 游标键对（收件箱沿用历史键，发件箱独立键）
fn cursor_keys(dest: crate::mail::MailDest) -> (&'static str, &'static str) {
    match dest {
        crate::mail::MailDest::Inbox => (keys::LAST_UIDVALIDITY, keys::LAST_UID),
        crate::mail::MailDest::Sent => (keys::LAST_UIDVALIDITY_SENT, keys::LAST_UID_SENT),
    }
}

/// 收件轮次任务参数（start_fetch 快照 → fetch_round 线程体）
struct FetchJob {
    fetch_state: std::sync::Arc<Mutex<FetchState>>,
    db_path: PathBuf,
    data_dir: PathBuf,
    host: String,
    port: u16,
    /// 加密方式（端口推断 + 配置覆盖）
    security: crate::net::TlsMode,
    /// 单封大小上限（字节）：超出只拉信头
    max_mail_bytes: u64,
    account: String,
    password: String,
    first_days: i64,
    last: crate::mail::LastCursor,
    /// 发件箱游标（先于收件箱同步）
    last_sent: crate::mail::LastCursor,
    /// 本轮代际（fetch_cas_begin 发放；finish 时的凭证）
    epoch: u64,
    /// 新邮件通知钩子（完成且 added>0 时在本线程内触发；None = 测试/未装配）
    notify: Option<NotifyHook>,
    /// 迁移冻结位（R2）：每轮 attempt 开头检查，已冻结则立刻收手交还名额
    migration: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// 收件轮次主体（start_fetch 线程执行体）：拉取 → 入库 → 游标推进 → 状态复位。
/// 独立连接访问数据库（与主连接、发送 worker 并行，WAL 读写不互阻）。
/// 收件游标持久化（需求2 断点续传；C4 并发加固）：单事务写 uidvalidity/uid
/// （可选 last_sync_at）。单调性以 DB 侧现值为准——内存快照看不见其它轮已写值，
/// 看门狗复位后的旧轮与本轮并发写时，若用内存快照判断会用旧游标改写新轮已推进的值。
/// uidvalidity 变化时 UID 空间重置，uid 无条件采纳（允许变小）。
/// /// 顺序约束：调用方必须先成功入库再调本函数——“游标只越过已入库邮件”
/// 是断点不漏信的根基。
fn persist_fetch_cursor(
    conn: &mut Connection,
    uv_key: &str,
    uid_key: &str,
    uidvalidity: u32,
    uid: u32,
    sync_at: bool,
) -> Result<(), String> {
    let tx = conn
        .transaction()
        .map_err(|e| format!("游标事务开启失败：{e}"))?;
    let cur_uv: u32 = crate::db::settings::get(&tx, uv_key)
        .map_err(|e| format!("uidvalidity 读取失败：{e}"))?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let cur_uid: u32 = crate::db::settings::get(&tx, uid_key)
        .map_err(|e| format!("uid 读取失败：{e}"))?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    crate::db::settings::set(&tx, uv_key, &uidvalidity.to_string())
        .map_err(|e| format!("uidvalidity 写入失败：{e}"))?;
    if uid > 0 && (uidvalidity != cur_uv || uid > cur_uid) {
        crate::db::settings::set(&tx, uid_key, &uid.to_string())
            .map_err(|e| format!("uid 写入失败：{e}"))?;
    }
    if sync_at {
        crate::db::settings::set(
            &tx,
            keys::LAST_SYNC_AT,
            &crate::mail::now_epoch().to_string(),
        )
        .map_err(|e| format!("last_sync_at 写入失败：{e}"))?;
    }
    tx.commit().map_err(|e| format!("游标提交失败：{e}"))
}

/// 单阶段同步参数（收件箱/发件箱共用；避免两套 on_batch 闭包漂移）
struct FetchPhase<'a> {
    conn: &'a mut Connection,
    data_dir: &'a Path,
    fetch_state: &'a std::sync::Arc<Mutex<FetchState>>,
    epoch: u64,
    host: &'a str,
    port: u16,
    security: crate::net::TlsMode,
    account: &'a str,
    password: &'a str,
    dest: crate::mail::MailDest,
    first_days: i64,
    last: crate::mail::LastCursor,
    max_mail_bytes: u64,
    added: &'a mut usize,
}

/// 单阶段同步：设阶段标记 → 分批拉取入库 → 每批持久化对应邮箱游标。
/// 发件箱不存在时返回 Ok(None)（跳过，不影响收件箱）。
fn fetch_phase(p: FetchPhase<'_>) -> Result<Option<(u32, u32)>, String> {
    let FetchPhase {
        conn,
        data_dir,
        fetch_state,
        epoch,
        host,
        port,
        security,
        account,
        password,
        dest,
        first_days,
        last,
        max_mail_bytes,
        added,
    } = p;
    let (uv_key, uid_key) = cursor_keys(dest);
    {
        let mut s = fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.epoch == epoch {
            s.phase = dest.phase_str().to_string();
            s.fetch_done = 0;
            s.fetch_total = 0;
            s.started_at = crate::mail::now_epoch();
            // ETA 计时随阶段重置：跨阶段的吞吐不可比（发件箱常是占位/小件批）
            s.progress_started_at = 0;
            s.progress_updated_at = 0;
            s.progress_started_done = 0;
        }
    }
    // 分批拉取：每批解析后立即入库并推进进度/游标；任一 panic 由上层 catch_unwind 兜住
    let mut on_batch = |mails: &[crate::mail::IncomingMail],
                        done: usize,
                        total: usize,
                        uidvalidity: u32,
                        batch_max: u32|
     -> Result<(), String> {
        // C4：看门狗已复位/新一轮已开始 → 立即终止本轮（Err 中断后续批次下载）
        {
            let s = fetch_state.lock().unwrap_or_else(|e| e.into_inner());
            if s.epoch != epoch {
                return Err("收件轮已中止".into());
            }
        }
        *added += crate::mail::store_incoming_conn(conn, data_dir, mails, dest)?;
        // 断点续传：每批成功入库后立即持久化本邮箱游标（DB 侧单调）；
        // 入库失败提前返回，游标不推进，靠去重兜底
        persist_fetch_cursor(conn, uv_key, uid_key, uidvalidity, batch_max, false)?;
        let mut s = fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.epoch == epoch {
            // ETA 计时锚点：首次进度上报起表，后续以增量吞吐外推剩余时间
            let now = crate::mail::now_epoch();
            if s.progress_started_at == 0 {
                s.progress_started_at = now;
                s.progress_started_done = done;
            }
            s.progress_updated_at = now;
            s.fetch_done = done;
            s.fetch_total = total;
            // 进展即续命：大邮箱首拉远超看门狗 300s，批间进展说明轮次健康存活
            s.started_at = crate::mail::now_epoch();
        }
        Ok(())
    };
    crate::mail::RealFetcher.fetch_batched(
        host,
        port,
        security,
        account,
        password,
        dest,
        first_days,
        last,
        max_mail_bytes,
        &mut on_batch,
    )
}

/// 执行单个同步阶段并捕获 panic：任一阶段的 panic 不得拖垮整轮
/// （发件箱异常也不阻断收件箱）。两个阶段共用同一包装，避免各写一遍。
fn guarded_phase<F>(label: &str, f: F) -> Result<Option<(u32, u32)>, String>
where
    F: FnOnce() -> Result<Option<(u32, u32)>, String>,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        eprintln!("[fetch] {label}同步 panic（已捕获）");
        Err(format!("{label}同步内部异常"))
    })
}

/// 执行单个同步阶段（收件箱/发件箱共用）：装配 FetchPhase 并加 panic 保护。
/// 两阶段仅 dest/last 不同，集中于此避免调用处重复整套参数装配。
#[allow(clippy::too_many_arguments)]
fn sync_phase(
    conn: &mut Connection,
    data_dir: &Path,
    fetch_state: &std::sync::Arc<Mutex<FetchState>>,
    epoch: u64,
    host: &str,
    port: u16,
    security: crate::net::TlsMode,
    account: &str,
    password: &str,
    first_days: i64,
    max_mail_bytes: u64,
    dest: crate::mail::MailDest,
    last: crate::mail::LastCursor,
    added: &mut usize,
) -> Result<Option<(u32, u32)>, String> {
    let label = match dest {
        crate::mail::MailDest::Sent => "发件箱",
        crate::mail::MailDest::Inbox => "收件箱",
    };
    guarded_phase(label, || {
        fetch_phase(FetchPhase {
            conn,
            data_dir,
            fetch_state,
            epoch,
            host,
            port,
            security,
            account,
            password,
            dest,
            first_days,
            last,
            max_mail_bytes,
            added,
        })
    })
}

fn fetch_round(job: FetchJob) {
    let FetchJob {
        fetch_state,
        db_path,
        data_dir,
        host,
        port,
        security,
        max_mail_bytes,
        account,
        password,
        first_days,
        last,
        last_sent,
        epoch,
        notify,
        migration,
    } = job;
    // 本轮开始时刻：完成后作为新增邮件查询窗口下界（通知用）。
    // 取在拆 job 后、首个入库前：宁可窗口稍宽（前一轮尾部重叠由进程内
    // id 去重兜住），不可狭窄漏掉本轮早期入库的邮件。
    let round_started_at = crate::mail::now_epoch();
    // finish 携带轮次凭证：epoch 失配（看门狗已复位/新一轮已开始）则仅记日志不写状态，
    // 防止挂起轮迟到完成污染当前轮的展示
    let finish = |new_count: i64, error: String| {
        let mut s = fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.epoch != epoch {
            eprintln!("[fetch] 迟到结果（epoch {epoch} ≠ {}）丢弃", s.epoch);
            return;
        }
        s.running = false;
        s.new_count = new_count;
        s.last_error = error;
        s.just_finished = true;
        s.phase.clear();
    };
    let Ok(mut conn) = crate::db::open(&db_path) else {
        finish(0, "本地数据库打开失败".into());
        return;
    };
    let mut added = 0usize;
    let mut last_err = String::new();
    let mut done = false;
    // 首拉阶段化（发件箱优先）：任一箱首拉未完成时，本轮按「先发件箱、
    // 后收件箱」严格串行——发件箱未完成就中断/失败，重试只针对发件箱
    // （收件箱不开始）；发件箱完成后才开始收件箱，收件箱中断重试时同样
    // 只重试收件箱。完成标志持久在库，跨 attempt 与跨重启均生效。
    // 两箱均完成后走常规增量轮（发件箱失败不阻断收件箱的既有容错保持）
    let mut sent_pending = !first_pull_done(&conn, crate::mail::MailDest::Sent);
    let inbox_pending = !first_pull_done(&conn, crate::mail::MailDest::Inbox);
    let phased = sent_pending || inbox_pending;
    if phased {
        eprintln!(
            "[fetch] 首拉阶段化（发件箱优先）：待完成 = {}",
            if sent_pending && inbox_pending {
                "发件箱 → 收件箱"
            } else if sent_pending {
                "发件箱"
            } else {
                "收件箱"
            }
        );
    }
    for attempt in 1..=FETCH_ATTEMPTS {
        // 迁移冻结（R2）：快照已定格，本轮立即收手交还 running 名额
        // （move_data_dir 正在等它；继续拉只会写进即将被抛弃的旧库）
        if migration.load(std::sync::atomic::Ordering::SeqCst) {
            finish(added as i64, MIGRATION_PENDING_MSG.to_string());
            return;
        }
        let (cur_sent, cur_inbox) = if attempt == 1 {
            (last_sent, last)
        } else {
            (
                read_fetch_cursor(&conn, keys::LAST_UIDVALIDITY_SENT, keys::LAST_UID_SENT),
                read_fetch_cursor(&conn, keys::LAST_UIDVALIDITY, keys::LAST_UID),
            )
        };
        if attempt > 1 {
            eprintln!("[fetch] 自动重试 {attempt}/{FETCH_ATTEMPTS}（上次失败：{last_err}）");
            std::thread::sleep(std::time::Duration::from_secs(fetch_retry_delay(
                (attempt - 1) as u32,
            )));
        }
        // 网络探活：把"根本不可达"在 10s 内结束
        if let Err(e) = crate::net::probe_tcp(&host, port) {
            last_err = format!("{e}；服务器不可达");
            continue;
        }
        // 发件箱提示文案（完成态拼接给用户；增量轮沿用原容错文案）
        let mut sent_note = String::new();
        if phased {
            // —— 阶段一：发件箱（未完成才执行；完成后各 attempt 直接跳过）——
            if sent_pending {
                let outcome = sync_phase(
                    &mut conn,
                    &data_dir,
                    &fetch_state,
                    epoch,
                    &host,
                    port,
                    security,
                    &account,
                    &password,
                    first_days,
                    max_mail_bytes,
                    crate::mail::MailDest::Sent,
                    cur_sent,
                    &mut added,
                );
                match outcome {
                    // 未发现发件箱（中文服务器 mUTF-7 名称等）：视为完成并置标志，
                    // 否则每轮重复探测；用户可见提示保留
                    Ok(None) => {
                        sent_note = "未发现服务器「已发送」邮箱，已跳过发件箱同步".to_string();
                        mark_first_pull_done(&conn, crate::mail::MailDest::Sent);
                        sent_pending = false;
                    }
                    Ok(Some(_)) => {
                        mark_first_pull_done(&conn, crate::mail::MailDest::Sent);
                        sent_pending = false;
                    }
                    Err(e) => {
                        // 发件箱未完成：收件箱不开始，下个 attempt 只重试发件箱
                        eprintln!("[fetch] 阶段化模式：发件箱同步失败（重试前不拉收件箱）：{e}");
                        last_err = e;
                        let aborted =
                            fetch_state.lock().unwrap_or_else(|e| e.into_inner()).epoch != epoch;
                        if aborted {
                            // 看门狗复位/新一轮已开始：finish 的 epoch 校验会丢弃
                            // 迟到结果，此处仅停止重试
                            eprintln!("[fetch] 轮次已失效（看门狗复位/新一轮开始）：{last_err}");
                            break;
                        }
                        continue;
                    }
                }
            }
        } else {
            // —— 增量轮：发件箱先同步（失败不阻断收件箱——发件箱命名/权限差异
            //    不该拖垮收件；既有容错语义保持）——
            let sent_outcome = sync_phase(
                &mut conn,
                &data_dir,
                &fetch_state,
                epoch,
                &host,
                port,
                security,
                &account,
                &password,
                first_days,
                max_mail_bytes,
                crate::mail::MailDest::Sent,
                cur_sent,
                &mut added,
            );
            sent_note = match &sent_outcome {
                Ok(None) => "未发现服务器「已发送」邮箱，已跳过发件箱同步".to_string(),
                Ok(Some(_)) => String::new(),
                Err(e) => {
                    eprintln!("[fetch] 发件箱同步失败（继续收件箱）：{e}");
                    e.clone()
                }
            };
        }
        // —— 阶段二：收件箱 ——
        let inbox_outcome = sync_phase(
            &mut conn,
            &data_dir,
            &fetch_state,
            epoch,
            &host,
            port,
            security,
            &account,
            &password,
            first_days,
            max_mail_bytes,
            crate::mail::MailDest::Inbox,
            cur_inbox,
            &mut added,
        );
        match inbox_outcome {
            Ok(Some((uidvalidity, max_uid))) => {
                if phased {
                    mark_first_pull_done(&conn, crate::mail::MailDest::Inbox);
                }
                // 收尾游标（与每批持久化共用同一 DB 侧单调实现）：写 last_sync_at
                if let Err(err) = persist_fetch_cursor(
                    &mut conn,
                    keys::LAST_UIDVALIDITY,
                    keys::LAST_UID,
                    uidvalidity,
                    max_uid,
                    true,
                ) {
                    eprintln!("[fetch] 游标写入失败（本轮邮件已入库，下轮重拉去重兜底）：{err}");
                }
                eprintln!("[fetch] 完成：入库 {added} 封（发件箱+收件箱）");
                let msg = if sent_note.is_empty() {
                    String::new()
                } else {
                    format!("收件完成，但发件箱未同步：{sent_note}")
                };
                finish(added as i64, msg);
                // 新邮件摘要通知（异步钩子在收件线程内触发；失败静默不影响主流程）
                if let Some(hook) = notify {
                    hook(round_started_at, added as i64);
                }
                done = true;
                break;
            }
            Ok(None) => last_err = "收件箱不可用".into(),
            Err(e) => last_err = e,
        }
        let aborted = fetch_state.lock().unwrap_or_else(|e| e.into_inner()).epoch != epoch;
        if aborted {
            eprintln!("[fetch] 轮次已失效（看门狗复位/新一轮开始），停止重试：{last_err}");
            break;
        }
    }
    if !done {
        eprintln!("[fetch] 失败：{last_err}");
        finish(0, last_err);
    }
}

#[cfg(test)]
#[path = "tests/state.rs"]
mod tests;
