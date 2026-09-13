//! 机器人主循环与交互逻辑。
//!
//! 交互全部用行内键盘完成：选城市 → 搜线路 → 选上车站 → 看实时到站 → 收藏。
//! 用户数据（城市、收藏、习惯）通过 [`store::Store`] 落盘。
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde_json::Value;

use crate::bot::store::{
    AlertField, Favorite, Pending, Store, WatchSpec, local_clock, local_clock_secs, local_hour,
    now_secs,
};
use crate::bot::telegram::{
    CallbackQuery, Message, Telegram, Update, command_keyboard, inline_keyboard,
};
use crate::bot::{render, watch};
use crate::models::{BusRoute, LineDetail, RealTimeData};
use crate::provider::{self, BusDataProvider};
use crate::support::coord::wgs84_to_gcj02;

const COMMANDS: &[(&str, &str)] = &[
    ("start", "开始使用 / 返回主菜单"),
    ("city", "选择城市与数据源"),
    ("line", "按线路名搜索"),
    ("nearby", "附近站点（需要发送位置）"),
    ("fav", "我的收藏车次"),
    ("watch", "盯车：等某辆车到站时提醒我"),
    ("settings", "提醒设置"),
    ("habits", "我的乘车习惯统计"),
    ("help", "使用说明"),
];

/// 线路全量列表较重，按数据源缓存一段时间。
const LINES_CACHE_TTL: Duration = Duration::from_secs(600);
/// 线路详情（站点表）缓存，刷新实时数据时复用。
const DETAIL_CACHE_TTL: Duration = Duration::from_secs(300);
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(20);
const SEARCH_PAGE_SIZE: usize = 9;
const STATION_PAGE_SIZE: usize = 10;
const CITY_PAGE_SIZE: usize = 8;
const MAX_TOKENS: usize = 20_000;
/// 去重用的 update_id 环形窗口大小。
const MAX_SEEN_UPDATES: usize = 2_000;

const PROMPT_CITY: &str = "🏙 <b>选择城市</b>\n\n回复城市名或省份，例如「杭州」「泉州」「广东」。";
const PROMPT_LINE: &str = "🔍 <b>搜索线路</b>\n\n回复线路关键词，例如「K155」「372」「机场」。";

/// 一次交互的上下文。
pub(crate) struct Ctx {
    pub chat_id: i64,
    pub user_id: i64,
    /// 该用户的第几次操作，用于丢弃被新操作取代的旧渲染
    pub epoch: u64,
    /// 触发这次交互的消息（回调来的按钮所在消息）
    pub source: Option<i64>,
    /// 用户刚发了消息：卡片要重新发到聊天最下方，而不是留在上面被顶走
    pub fresh: bool,
    /// 交互开始时的盯车卡片；这条消息由盯车循环维护，不能被导航内容覆盖。
    /// 记在上下文里，是因为处理过程中盯车可能已经结束（例如「停止盯车」）。
    pub watch_card: Option<i64>,
}

// ─── 按钮动作 ───

/// 回调按钮携带的动作。callback_data 限长 64 字节，因此只放一个短 token，
/// 真实参数存在内存表里（重启后按钮失效，会提示用户重新查询）。
#[derive(Clone, Debug, PartialEq)]
enum Action {
    PickService {
        service: String,
        label: String,
    },
    CityPage {
        keyword: String,
        page: usize,
    },
    SearchPage {
        keyword: String,
        page: usize,
    },
    Line {
        service: String,
        direction: String,
        page: usize,
    },
    Live {
        service: String,
        direction: String,
        order: u32,
    },
    FavToggle {
        service: String,
        direction: String,
        order: u32,
    },
    FavRemove {
        service: String,
        direction: String,
        order: u32,
    },
    Station {
        service: String,
        name: String,
        lat: f64,
        lng: f64,
    },
    NearbyHere,
    /// 进入盯车设置：列出当前在途车辆供选择
    WatchSetup {
        service: String,
        direction: String,
        order: u32,
    },
    /// 开始盯车；`bus` 为 None 表示盯「最近的一班」
    WatchStart {
        service: String,
        direction: String,
        order: u32,
        bus: Option<String>,
    },
    WatchStop,
    /// 提醒设置：按步长调整某一项
    SettingsAdjust {
        field: AlertField,
        steps: i64,
    },
    SettingsReset,
}

#[derive(Default)]
struct TokenTable {
    next: AtomicU64,
    inner: Mutex<(HashMap<u64, Action>, VecDeque<u64>)>,
}

impl TokenTable {
    fn put(&self, action: Action) -> String {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.inner.lock();
        let (map, order) = &mut *guard;
        map.insert(id, action);
        order.push_back(id);
        while order.len() > MAX_TOKENS {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            }
        }
        format!("t:{id}")
    }

    fn get(&self, data: &str) -> Option<Action> {
        let id: u64 = data.strip_prefix("t:")?.parse().ok()?;
        self.inner.lock().0.get(&id).cloned()
    }
}

// ─── 并发与重复投递的防护 ───

/// 已处理过的 update_id。Telegram 在没收到确认时会重发同一条更新，
/// 重复执行会造成「一次点击、两次生效」。
#[derive(Default)]
struct SeenUpdates {
    inner: Mutex<(HashSet<i64>, VecDeque<i64>)>,
}

impl SeenUpdates {
    /// 第一次见到返回 true；重复的返回 false。
    fn accept(&self, update_id: i64) -> bool {
        let mut guard = self.inner.lock();
        let (seen, order) = &mut *guard;
        if !seen.insert(update_id) {
            return false;
        }
        order.push_back(update_id);
        while order.len() > MAX_SEEN_UPDATES {
            if let Some(old) = order.pop_front() {
                seen.remove(&old);
            }
        }
        true
    }
}

/// 每个用户的操作序号：连点按钮或中途改点别处时，
/// 先发起的那次请求即使后返回，也不再覆盖新界面。
#[derive(Default)]
struct Epochs {
    inner: Mutex<HashMap<i64, u64>>,
}

impl Epochs {
    fn begin(&self, user_id: i64) -> u64 {
        let mut epochs = self.inner.lock();
        let counter = epochs.entry(user_id).or_insert(0);
        *counter += 1;
        *counter
    }

    fn is_current(&self, user_id: i64, epoch: u64) -> bool {
        self.inner.lock().get(&user_id).copied().unwrap_or(0) == epoch
    }
}

/// 这次渲染应该改哪条消息：
/// 用户点的那条优先（相当于「点哪张卡片，哪张就变成主卡片」），
/// 但盯车卡片由盯车循环独占，不参与导航；都没有就返回 None，由调用方新发一条。
fn card_target(source: Option<i64>, card: Option<i64>, watch_card: Option<i64>) -> Option<i64> {
    match source {
        Some(id) if Some(id) != watch_card => Some(id),
        _ => card.filter(|id| Some(*id) != watch_card),
    }
}

// ─── 应用状态 ───

pub struct App {
    tg: Telegram,
    store: Arc<Store>,
    tokens: TokenTable,
    providers: Mutex<HashMap<String, Arc<dyn BusDataProvider>>>,
    lines_cache: Mutex<HashMap<String, (Instant, Arc<Vec<BusRoute>>)>>,
    detail_cache: Mutex<HashMap<String, (Instant, Arc<LineDetail>)>>,
    /// 每个用户同时只有一个盯车任务
    watches: Mutex<HashMap<i64, tokio::task::AbortHandle>>,
    /// 每个会话的主卡片消息 id：所有界面都渲染在这一条上
    cards: Mutex<HashMap<i64, i64>>,
    /// 每个用户的操作序号，用来丢弃被取代的旧渲染
    epochs: Epochs,
    /// 已经发过常驻命令键盘的会话（重启后会再发一次，客户端幂等）
    keyboards: Mutex<HashSet<i64>>,
    /// 已处理过的 update_id，避免重复投递被执行两次
    seen_updates: SeenUpdates,
    tz_offset: i64,
    started_at: u64,
    bot_username: String,
}

/// 上游调用统一加超时，并把错误转成能直接发给用户的中文说明。
async fn upstream<T>(
    future: impl std::future::Future<Output = Result<T, provider::ProviderError>>,
) -> Result<T, String> {
    match tokio::time::timeout(UPSTREAM_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("⚠️ 公交数据源暂时不可用：{error}")),
        Err(_) => Err("⚠️ 公交数据源响应超时，请稍后重试。".to_string()),
    }
}

/// 按环境变量准备机器人；没有配置令牌时返回 None，此时只跑网页版。
pub async fn start(store: Arc<Store>) -> anyhow::Result<Option<Arc<App>>> {
    let Some(token) = std::env::var("TELEGRAM_BOT_TOKEN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };

    let tz_offset = std::env::var("WHEREBUS_BOT_TZ")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|offset| (-12..=14).contains(offset))
        .unwrap_or(8);
    let tg = Telegram::new(&token)?;
    let me = tg.get_me().await?;
    let username = me.username.clone().unwrap_or_else(|| me.first_name.clone());

    let app = Arc::new(App {
        tg,
        store: Arc::clone(&store),
        tokens: TokenTable::default(),
        providers: Mutex::new(HashMap::new()),
        lines_cache: Mutex::new(HashMap::new()),
        detail_cache: Mutex::new(HashMap::new()),
        watches: Mutex::new(HashMap::new()),
        cards: Mutex::new(HashMap::new()),
        epochs: Epochs::default(),
        keyboards: Mutex::new(HashSet::new()),
        seen_updates: SeenUpdates::default(),
        tz_offset,
        started_at: now_secs(),
        bot_username: username.clone(),
    });

    println!(
        "机器人：@{username} · 已有用户 {} · 时区 UTC{:+}",
        store.user_count(),
        tz_offset,
    );
    if let Err(error) = app.tg.set_my_commands(COMMANDS).await {
        eprintln!("[bot] 注册命令菜单失败：{error}");
    }
    app.restore_watches();
    Ok(Some(app))
}

impl App {
    /// 长轮询主循环，直到 `shutdown` 完成。
    pub async fn poll_updates(self: &Arc<Self>, shutdown: impl std::future::Future<Output = ()>) {
        let mut offset = 0i64;
        let mut backoff = 1u64;
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => return,
                updates = self.tg.get_updates(offset) => match updates {
                    Ok(updates) => {
                        backoff = 1;
                        for update in updates {
                            offset = offset.max(update.update_id + 1);
                            // 去重与序号都在这里按投递顺序完成，之后才并发处理：
                            // 这样「最新一次操作」的判断与用户实际点击顺序一致
                            if !self.seen_updates.accept(update.update_id) {
                                continue;
                            }
                            let Some(user_id) = update_actor(&update) else {
                                continue;
                            };
                            let epoch = self.epochs.begin(user_id);
                            let app = Arc::clone(self);
                            tokio::spawn(async move { app.handle(update, epoch).await });
                        }
                    }
                    Err(error) => {
                        let wait = error.retry_after().unwrap_or(backoff);
                        eprintln!("[bot] 拉取更新失败：{error}（{wait}s 后重试）");
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        backoff = (backoff * 2).min(30);
                    }
                },
            }
        }
    }
}

impl App {
    fn hour(&self) -> u32 {
        local_hour(now_secs(), self.tz_offset)
    }

    pub(crate) fn clock(&self) -> String {
        local_clock(now_secs(), self.tz_offset)
    }

    pub(crate) fn clock_secs(&self) -> String {
        local_clock_secs(now_secs(), self.tz_offset)
    }

    pub(crate) fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub(crate) fn bot_username(&self) -> &str {
        &self.bot_username
    }

    pub(crate) fn started_at(&self) -> u64 {
        self.started_at
    }

    pub(crate) fn tz_offset(&self) -> i64 {
        self.tz_offset
    }

    pub(crate) fn active_watch_count(&self) -> usize {
        self.watches.lock().len()
    }

    // ─── 盯车 ───

    /// 启动后恢复重启前仍在进行的盯车。
    pub(crate) fn restore_watches(self: &Arc<Self>) {
        for (user_id, spec) in self.store.active_watches() {
            let alerts = self.store.alerts_for(user_id);
            // 超时的任务不恢复，直接清掉
            if now_secs().saturating_sub(spec.started_at) >= alerts.max_minutes * 60 {
                self.store.update(user_id, |user| user.watch = None);
                continue;
            }
            self.spawn_watch(user_id, spec);
        }
        let restored = self.active_watch_count();
        if restored > 0 {
            println!("[bot] 已恢复 {restored} 个盯车任务");
        }
    }

    pub(crate) fn spawn_watch(self: &Arc<Self>, user_id: i64, spec: WatchSpec) {
        let app = Arc::clone(self);
        let handle = tokio::spawn(watch::run(Arc::clone(&app), user_id, spec));
        if let Some(previous) = self.watches.lock().insert(user_id, handle.abort_handle()) {
            previous.abort();
        }
    }

    /// 结束盯车：清理状态、收尾卡片、告知用户。
    pub(crate) async fn finish_watch(&self, user_id: i64, reason: &str) {
        let spec = self.store.update(user_id, |user| user.watch.take());
        self.watches.lock().remove(&user_id);
        let Some(spec) = spec else { return };

        let markup = inline_keyboard(vec![
            vec![(
                "🔔 再盯一次".into(),
                self.tokens.put(Action::WatchStart {
                    service: spec.service.clone(),
                    direction: spec.direction.clone(),
                    order: spec.order,
                    bus: spec.target_bus.clone(),
                }),
            )],
            vec![(
                "🚌 查看到站".into(),
                self.tokens.put(Action::Live {
                    service: spec.service.clone(),
                    direction: spec.direction.clone(),
                    order: spec.order,
                }),
            )],
            home_row(),
        ]);
        let text = format!(
            "⏹ <b>盯车已结束</b>\n{} @ {}\n\n{}",
            render::escape(&spec.line_name),
            render::escape(&spec.station_name),
            render::escape(reason),
        );
        self.edit(spec.chat_id, spec.card_message_id, &text, Some(markup))
            .await;
    }

    /// 外部（管理后台 / 小程序）主动停止盯车。
    pub(crate) async fn stop_watch(&self, user_id: i64, reason: &str) -> bool {
        // 先在独立语句里取出句柄，确保锁在 finish_watch 之前就已释放
        let handle = self.watches.lock().remove(&user_id);
        match handle {
            Some(handle) => {
                handle.abort();
                self.finish_watch(user_id, reason).await;
                true
            }
            None => {
                // 没有运行中的任务，但状态里可能有残留
                let had = self.store.get(user_id).watch.is_some();
                if had {
                    self.finish_watch(user_id, reason).await;
                }
                had
            }
        }
    }

    /// 盯车卡片上的按钮。
    pub(crate) fn watch_keyboard(&self, _user_id: i64) -> Value {
        inline_keyboard(vec![
            vec![
                ("⏹ 停止盯车".into(), self.tokens.put(Action::WatchStop)),
                ("⚙️ 提醒设置".into(), "m:settings".into()),
            ],
            home_row(),
        ])
    }

    /// 线路详情 + 实时数据，盯车循环每轮调用。
    pub(crate) async fn realtime_snapshot(
        &self,
        service: &str,
        direction: &str,
        order: u32,
    ) -> Result<(Arc<LineDetail>, RealTimeData), String> {
        let detail = self.line_detail(service, direction).await?;
        let provider = self.provider_for(service);
        let key = direction.to_string();
        let realtime = upstream(async move { provider.realtime(&key, order).await }).await?;
        Ok((detail, realtime))
    }

    /// 主动推送的提醒消息。
    pub(crate) async fn notify(&self, chat_id: i64, text: &str) {
        if let Err(error) = self.tg.send_alert(chat_id, text).await {
            eprintln!("[bot] 推送提醒失败：{error}");
        }
    }

    fn provider_for(&self, service: &str) -> Arc<dyn BusDataProvider> {
        if let Some(existing) = self.providers.lock().get(service) {
            return Arc::clone(existing);
        }
        let created = provider::create_provider(service);
        self.providers
            .lock()
            .insert(service.to_string(), Arc::clone(&created));
        created
    }

    // ─── 卡片：一个会话只维护一条可交互的消息 ───

    /// 组装一次交互的上下文：谁、点在哪条消息上、第几次操作。
    /// `epoch` 由拉取循环按投递顺序分配。
    fn context(&self, chat_id: i64, user_id: i64, source: Option<i64>, epoch: u64) -> Ctx {
        Ctx {
            chat_id,
            user_id,
            epoch,
            source,
            fresh: source.is_none(),
            watch_card: self.watch_card_of(user_id),
        }
    }

    /// 这次操作是否仍是该用户最新的一次。连点按钮、或等数据时又点了别处，
    /// 旧操作取回数据后不再覆盖新界面。
    fn is_current(&self, ctx: &Ctx) -> bool {
        self.epochs.is_current(ctx.user_id, ctx.epoch)
    }

    fn card_of(&self, chat_id: i64) -> Option<i64> {
        self.cards.lock().get(&chat_id).copied()
    }

    /// 盯车卡片由盯车循环自己刷新，不能被导航内容覆盖。
    fn watch_card_of(&self, user_id: i64) -> Option<i64> {
        self.store
            .get(user_id)
            .watch
            .map(|watch| watch.card_message_id)
    }

    /// 把界面渲染到会话的卡片上。
    ///
    /// 点按钮时就地编辑那张卡片；用户发消息后卡片会被自己的消息顶到上面，
    /// 这时重新发一张到最下方，并删掉旧的，保证「菜单永远在最新位置」。
    pub(crate) async fn show(&self, ctx: &Ctx, text: &str, markup: Option<Value>) {
        if !self.is_current(ctx) {
            return;
        }
        let existing = self.card_of(ctx.chat_id);
        if ctx.fresh {
            self.repost_card(ctx, text, markup, existing).await;
            return;
        }

        let Some(target) = card_target(ctx.source, existing, ctx.watch_card) else {
            self.repost_card(ctx, text, markup, existing).await;
            return;
        };
        match self
            .tg
            .edit_message_text(ctx.chat_id, target, text, markup.clone())
            .await
        {
            // 内容没变化说明界面已经是这样了，同样算成功
            Ok(_) => {
                self.cards.lock().insert(ctx.chat_id, target);
            }
            Err(error) if error.is_not_modified() => {
                self.cards.lock().insert(ctx.chat_id, target);
            }
            Err(error) => {
                eprintln!("[bot] 卡片 {target} 无法编辑（{error}），改为新发一条");
                self.repost_card(ctx, text, markup, existing).await;
            }
        }
    }

    /// 新发一张卡片到最下方，再删掉旧的那张（先发后删，中间不会没有卡片）。
    async fn repost_card(
        &self,
        ctx: &Ctx,
        text: &str,
        markup: Option<Value>,
        previous: Option<i64>,
    ) {
        let Some(message_id) = self.send_card(ctx.chat_id, text, markup).await else {
            return;
        };
        self.cards.lock().insert(ctx.chat_id, message_id);
        // 盯车卡片不归导航管，别误删
        if let Some(previous) = previous.filter(|id| *id != message_id && Some(*id) != ctx.watch_card)
        {
            if self.tg.delete_message(ctx.chat_id, previous).await.is_err() {
                // 删不掉（超过 48 小时等）就至少让旧卡片不可点
                let _ = self
                    .tg
                    .edit_message_text(ctx.chat_id, previous, "⬇️ 已在下面的新卡片继续", None)
                    .await;
            }
        }
    }

    /// 发一张新卡片；会话里还没有常驻命令键盘时顺带补上。
    async fn send_card(&self, chat_id: i64, text: &str, markup: Option<Value>) -> Option<i64> {
        let needs_keyboard = !self.keyboards.lock().contains(&chat_id);
        if !needs_keyboard {
            return match self.tg.send_message(chat_id, text, markup).await {
                Ok(message) => Some(message.message_id),
                Err(error) => {
                    eprintln!("[bot] 发送卡片失败：{error}");
                    None
                }
            };
        }

        // 一条消息只能带一种键盘：先带常驻键盘发出去，再补上这张卡片的按钮
        match self
            .tg
            .send_message(chat_id, text, Some(command_keyboard()))
            .await
        {
            Ok(message) => {
                self.keyboards.lock().insert(chat_id);
                if markup.is_some() {
                    let _ = self
                        .tg
                        .edit_message_text(chat_id, message.message_id, text, markup)
                        .await;
                }
                Some(message.message_id)
            }
            Err(error) => {
                eprintln!("[bot] 发送卡片失败：{error}");
                None
            }
        }
    }

    pub(crate) async fn send(&self, chat_id: i64, text: &str, markup: Option<Value>) {
        if let Err(error) = self.tg.send_message(chat_id, text, markup).await {
            eprintln!("[bot] 发送消息失败：{error}");
        }
    }

    /// 直接编辑指定消息（盯车卡片用，不走主卡片逻辑）。
    pub(crate) async fn edit(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        markup: Option<Value>,
    ) {
        match self
            .tg
            .edit_message_text(chat_id, message_id, text, markup.clone())
            .await
        {
            Ok(_) => {}
            Err(error) if error.is_not_modified() => {}
            Err(error) => {
                eprintln!("[bot] 编辑消息失败：{error}，改为新发一条");
                self.send(chat_id, text, markup).await;
            }
        }
    }

    async fn handle(self: Arc<Self>, update: Update, epoch: u64) {
        if let Some(message) = update.message {
            self.handle_message(message, epoch).await;
        } else if let Some(callback) = update.callback_query {
            self.handle_callback(callback, epoch).await;
        }
    }

    // ─── 消息 ───

    /// 停用 / 停止接纳新用户的统一闸门；返回 false 表示不再继续处理。
    async fn admit(&self, ctx: &Ctx, from: Option<&crate::bot::telegram::User>) -> bool {
        if self.store.get(ctx.user_id).banned {
            self.show(ctx, "你的账号已被管理员停用。", None).await;
            return false;
        }
        if !self.store.settings().allow_new_users && !self.store.exists(ctx.user_id) {
            self.show(ctx, "机器人当前不接受新用户，请联系管理员。", None)
                .await;
            return false;
        }
        if let Some(from) = from {
            // 管理界面需要看得懂是谁，这里顺手记下昵称
            let name = from.first_name.clone();
            let username = from.username.clone();
            self.store.update(ctx.user_id, |user| {
                user.display_name = name;
                user.username = username;
            });
        }
        true
    }

    async fn handle_message(self: &Arc<Self>, message: Message, epoch: u64) {
        let chat_id = message.chat.id;
        let user_id = message.from.as_ref().map(|u| u.id).unwrap_or(chat_id);
        // 文本命令也渲染到主卡片，不再每条命令堆一张新卡
        let ctx = self.context(chat_id, user_id, None, epoch);
        if !self.admit(&ctx, message.from.as_ref()).await {
            return;
        }

        if let Some(location) = message.location {
            self.on_location(&ctx, location.latitude, location.longitude)
                .await;
            return;
        }

        let Some(text) = message.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
            return;
        };

        // 常驻键盘的按钮发来的就是这些文案，等同于对应命令
        if let Some(command) = keyboard_command(text) {
            self.on_command(&ctx, command, "").await;
            return;
        }

        if let Some((command, argument)) = parse_command(text) {
            self.on_command(&ctx, &command, &argument).await;
            return;
        }

        // 非命令文本：按当前等待状态解释
        match self.store.get(user_id).pending {
            Pending::City => {
                let (body, markup) = self.cities_view(user_id, text, 0);
                self.show(&ctx, &body, markup).await;
            }
            _ => {
                if self.store.get(user_id).service.is_some() {
                    let (body, markup) = self.search_view(user_id, text, 0).await;
                    self.show(&ctx, &body, markup).await;
                } else {
                    self.store
                        .update(user_id, |user| user.pending = Pending::City);
                    self.show(&ctx, PROMPT_CITY, Some(self.prompt_keyboard()))
                        .await;
                }
            }
        }
    }

    async fn on_command(self: &Arc<Self>, ctx: &Ctx, command: &str, argument: &str) {
        let user_id = ctx.user_id;
        match command {
            "start" | "home" => {
                self.store
                    .update(user_id, |user| user.pending = Pending::None);
                let (text, markup) = self.home_view(user_id);
                self.show(ctx, &text, markup).await;
            }
            "help" => {
                self.show(ctx, HELP_TEXT, Some(inline_keyboard(vec![home_row()])))
                    .await
            }
            "city" | "cities" => {
                if argument.is_empty() {
                    self.store
                        .update(user_id, |user| user.pending = Pending::City);
                    self.show(ctx, PROMPT_CITY, Some(self.prompt_keyboard()))
                        .await;
                } else {
                    let (text, markup) = self.cities_view(user_id, argument, 0);
                    self.show(ctx, &text, markup).await;
                }
            }
            "line" | "search" | "bus" => {
                if self.store.get(user_id).service.is_none() {
                    self.store
                        .update(user_id, |user| user.pending = Pending::City);
                    self.show(ctx, PROMPT_CITY, Some(self.prompt_keyboard()))
                        .await;
                    return;
                }
                if argument.is_empty() {
                    self.store
                        .update(user_id, |user| user.pending = Pending::Line);
                    self.show(ctx, PROMPT_LINE, Some(self.prompt_keyboard()))
                        .await;
                } else {
                    let (text, markup) = self.search_view(user_id, argument, 0).await;
                    self.show(ctx, &text, markup).await;
                }
            }
            "nearby" | "near" => self.ask_location(ctx).await,
            "fav" | "favs" | "favorites" | "favourite" => {
                let (text, markup) = self.favorites_view(user_id);
                self.show(ctx, &text, Some(markup)).await;
            }
            "watch" | "watching" => {
                let (text, markup) = self.watch_view(user_id);
                self.show(ctx, &text, Some(markup)).await;
            }
            "settings" | "setting" | "alerts" => {
                let (text, markup) = self.settings_view(user_id);
                self.show(ctx, &text, Some(markup)).await;
            }
            "habits" | "stats" | "me" => {
                let (text, markup) = self.habits_view(user_id);
                self.show(ctx, &text, Some(markup)).await;
            }
            "cancel" => {
                self.store
                    .update(user_id, |user| user.pending = Pending::None);
                let (text, markup) = self.home_view(user_id);
                self.show(ctx, &format!("已取消当前输入。\n\n{text}"), markup)
                    .await;
            }
            _ => {
                let (text, markup) = self.home_view(user_id);
                self.show(
                    ctx,
                    &format!("不认识这个命令，用下方键盘或发送 /help 查看用法。\n\n{text}"),
                    markup,
                )
                .await;
            }
        }
    }

    /// 等待用户输入时的卡片按钮。
    fn prompt_keyboard(&self) -> Value {
        inline_keyboard(vec![vec![("↩️ 取消".into(), "m:home".into())]])
    }

    async fn on_location(&self, ctx: &Ctx, latitude: f64, longitude: f64) {
        if !(latitude.is_finite() && longitude.is_finite()) {
            self.show(
                ctx,
                "位置无效，请重新发送。",
                Some(inline_keyboard(vec![home_row()])),
            )
            .await;
            return;
        }
        let Some(service) = self.store.get(ctx.user_id).service else {
            self.store
                .update(ctx.user_id, |user| user.pending = Pending::City);
            self.show(ctx, PROMPT_CITY, Some(self.prompt_keyboard()))
                .await;
            return;
        };
        // provider 接受 GCJ-02 坐标，与网页端同一套换算
        let (lat, lng) = wgs84_to_gcj02(latitude, longitude);
        self.store.update(ctx.user_id, |user| {
            user.last_location = Some((lat, lng));
            user.pending = Pending::None;
        });
        let (text, markup) = self.nearby_view(&service, lat, lng).await;
        self.show(ctx, &text, markup).await;
    }

    async fn ask_location(&self, ctx: &Ctx) {
        let user = self.store.get(ctx.user_id);
        if user.service.is_none() {
            self.store
                .update(ctx.user_id, |user| user.pending = Pending::City);
            self.show(ctx, PROMPT_CITY, Some(self.prompt_keyboard()))
                .await;
            return;
        }

        let mut rows: Vec<Vec<(String, String)>> = Vec::new();
        if user.last_location.is_some() {
            rows.push(vec![(
                "📍 用上次的位置".into(),
                self.tokens.put(Action::NearbyHere),
            )]);
        }
        rows.push(home_row());
        self.show(
            ctx,
            "📍 <b>附近站点</b>\n\n点下方键盘里的「📍 附近站点」即可共享位置。位置只用于查询附近站点，会保存为你最近一次坐标。",
            Some(inline_keyboard(rows)),
        )
        .await;
    }

    // ─── 回调 ───

    async fn handle_callback(self: &Arc<Self>, callback: CallbackQuery, epoch: u64) {
        let user_id = callback.from.id;
        let Some(message) = callback.message.as_ref() else {
            let _ = self
                .tg
                .answer_callback_query(&callback.id, "消息太旧，请重新发送命令。", true)
                .await;
            return;
        };
        let chat_id = message.chat.id;
        let data = callback.data.clone().unwrap_or_default();
        let ctx = self.context(chat_id, user_id, Some(message.message_id), epoch);

        if !self.admit(&ctx, Some(&callback.from)).await {
            let _ = self
                .tg
                .answer_callback_query(&callback.id, "账号不可用", true)
                .await;
            return;
        }

        if let Some(menu) = data.strip_prefix("m:") {
            let _ = self.tg.answer_callback_query(&callback.id, "", false).await;
            self.handle_menu(&ctx, menu).await;
            return;
        }

        let Some(action) = self.tokens.get(&data) else {
            let _ = self
                .tg
                .answer_callback_query(
                    &callback.id,
                    "按钮已过期（机器人重启过），请重新查询。",
                    true,
                )
                .await;
            return;
        };

        let _ = self
            .tg
            .answer_callback_query(&callback.id, "查询中…", false)
            .await;

        match action {
            Action::PickService { service, label } => {
                self.store.update(user_id, |user| {
                    user.service = Some(service.clone());
                    user.city_label = Some(label.clone());
                    user.pending = Pending::None;
                });
                let (text, markup) = self.home_view(user_id);
                self.show(
                    &ctx,
                    &format!("✅ 已切换到 <b>{}</b>\n\n{text}", render::escape(&label)),
                    markup,
                )
                .await;
            }
            Action::CityPage { keyword, page } => {
                let (text, markup) = self.cities_view(user_id, &keyword, page);
                self.show(&ctx, &text, markup).await;
            }
            Action::SearchPage { keyword, page } => {
                let (text, markup) = self.search_view(user_id, &keyword, page).await;
                self.show(&ctx, &text, markup).await;
            }
            Action::Line {
                service,
                direction,
                page,
            } => {
                let (text, markup) = self.line_view(user_id, &service, &direction, page).await;
                self.show(&ctx, &text, markup).await;
            }
            Action::Live {
                service,
                direction,
                order,
            } => {
                let (text, markup) = self
                    .live_view(user_id, &service, &direction, order, true)
                    .await;
                self.show(&ctx, &text, markup).await;
            }
            Action::FavToggle {
                service,
                direction,
                order,
            } => {
                self.toggle_favorite(user_id, &service, &direction, order)
                    .await;
                // 收藏按钮只是重绘同一页，不再算作一次新的查询
                let (text, markup) = self
                    .live_view(user_id, &service, &direction, order, false)
                    .await;
                self.show(&ctx, &text, markup).await;
            }
            Action::FavRemove {
                service,
                direction,
                order,
            } => {
                self.store.update(user_id, |user| {
                    if let Some(index) = user.favorite_index(&service, &direction, order) {
                        user.favorites.remove(index);
                    }
                });
                let (text, markup) = self.favorites_view(user_id);
                self.show(&ctx, &text, Some(markup)).await;
            }
            Action::Station {
                service,
                name,
                lat,
                lng,
            } => {
                let (text, markup) = self.station_view(&service, &name, lat, lng).await;
                self.show(&ctx, &text, markup).await;
            }
            Action::NearbyHere => {
                let user = self.store.get(user_id);
                match (user.service, user.last_location) {
                    (Some(service), Some((lat, lng))) => {
                        let (text, markup) = self.nearby_view(&service, lat, lng).await;
                        self.show(&ctx, &text, markup).await;
                    }
                    _ => {
                        self.show(
                            &ctx,
                            "还没有保存过位置，请用 /nearby 发送位置。",
                            Some(inline_keyboard(vec![home_row()])),
                        )
                        .await;
                    }
                }
            }
            Action::WatchSetup {
                service,
                direction,
                order,
            } => {
                let (text, markup) = self.watch_setup_view(&service, &direction, order).await;
                self.show(&ctx, &text, markup).await;
            }
            Action::WatchStart {
                service,
                direction,
                order,
                bus,
            } => {
                self.start_watch(&ctx, &service, &direction, order, bus)
                    .await;
            }
            Action::WatchStop => {
                let stopped = self.stop_watch(user_id, "你手动停止了盯车。").await;
                let (text, markup) = self.home_view(user_id);
                let prefix = if stopped {
                    "⏹ 盯车已停止。\n\n"
                } else {
                    "当前没有进行中的盯车。\n\n"
                };
                self.show(&ctx, &format!("{prefix}{text}"), markup).await;
            }
            Action::SettingsAdjust { field, steps } => {
                let defaults = self.store.settings().defaults;
                self.store.update(user_id, |user| {
                    let mut alerts = user.alerts.unwrap_or(defaults);
                    alerts.adjust(field, steps);
                    user.alerts = Some(alerts);
                });
                let (text, markup) = self.settings_view(user_id);
                self.show(&ctx, &text, Some(markup)).await;
            }
            Action::SettingsReset => {
                self.store.update(user_id, |user| user.alerts = None);
                let (text, markup) = self.settings_view(user_id);
                self.show(&ctx, &text, Some(markup)).await;
            }
        }
    }

    async fn handle_menu(self: &Arc<Self>, ctx: &Ctx, menu: &str) {
        let user_id = ctx.user_id;
        match menu {
            "home" => {
                self.store
                    .update(user_id, |user| user.pending = Pending::None);
                let (text, markup) = self.home_view(user_id);
                self.show(ctx, &text, markup).await;
            }
            "fav" => {
                let (text, markup) = self.favorites_view(user_id);
                self.show(ctx, &text, Some(markup)).await;
            }
            "habit" => {
                let (text, markup) = self.habits_view(user_id);
                self.show(ctx, &text, Some(markup)).await;
            }
            "city" => {
                self.store
                    .update(user_id, |user| user.pending = Pending::City);
                self.show(ctx, PROMPT_CITY, Some(self.prompt_keyboard()))
                    .await;
            }
            "search" => {
                self.store
                    .update(user_id, |user| user.pending = Pending::Line);
                self.show(ctx, PROMPT_LINE, Some(self.prompt_keyboard()))
                    .await;
            }
            "nearby" => self.ask_location(ctx).await,
            "settings" => {
                let (text, markup) = self.settings_view(user_id);
                self.show(ctx, &text, Some(markup)).await;
            }
            "watch" => {
                let (text, markup) = self.watch_view(user_id);
                self.show(ctx, &text, Some(markup)).await;
            }
            "help" => {
                self.show(ctx, HELP_TEXT, Some(inline_keyboard(vec![home_row()])))
                    .await
            }
            _ => {}
        }
    }

    // ─── 各视图 ───

    /// 主菜单卡片。命令都在常驻键盘上，这里只放跟内容有关的按钮。
    fn home_view(&self, user_id: i64) -> (String, Option<Value>) {
        let user = self.store.get(user_id);
        let hour = self.hour();
        let city = user.city_label.clone().unwrap_or_else(|| "未选择".into());

        let mut text = format!(
            "🚏 <b>WhereBus</b> · 实时公交\n\n当前城市：<b>{}</b>\n收藏 {} 条 · 累计查询 {} 次\n",
            render::escape(&city),
            user.favorites.len(),
            user.queries,
        );

        let mut rows: Vec<Vec<(String, String)>> = Vec::new();
        let suggestions = user.suggestions(hour, 3);
        if suggestions.is_empty() {
            text.push_str("\n用下方键盘选功能，或直接发送线路名（如「K155」）搜索。");
        } else {
            text.push_str(&format!(
                "\n<b>{hour:02} 点你常查的</b>（点按钮直接看到站）：\n"
            ));
            for habit in &suggestions {
                rows.push(vec![(
                    render::habit_button_label(habit),
                    self.tokens.put(Action::Live {
                        service: habit.service.clone(),
                        direction: habit.direction.clone(),
                        order: habit.order,
                    }),
                )]);
            }
            text.push_str("\n其余功能在下方键盘里。");
        }

        let markup = if rows.is_empty() {
            None
        } else {
            Some(inline_keyboard(rows))
        };
        (text, markup)
    }

    /// 城市选择界面。
    fn cities_view(&self, user_id: i64, keyword: &str, page: usize) -> (String, Option<Value>) {
        let keyword = keyword.trim();
        let matches: Vec<(String, String, String)> = provider::available_services()
            .into_iter()
            .filter(|entry| entry.provider != "Debug")
            .filter(|entry| {
                keyword.is_empty()
                    || entry.city.name().contains(keyword)
                    || entry.city.province().contains(keyword)
                    || entry.provider.contains(keyword)
            })
            .map(|entry| {
                (
                    entry.id.to_string(),
                    format!("{} · {}", entry.city.name(), entry.provider),
                    entry.city.province().to_string(),
                )
            })
            .collect();

        if matches.is_empty() {
            let text = format!(
                "没有找到匹配「{}」的城市。换个关键词试试（支持城市名或省份）。注意：只有已接入数据源的城市才能查到。",
                render::escape(keyword)
            );
            return (text, Some(self.prompt_keyboard()));
        }

        let pages = matches.len().div_ceil(CITY_PAGE_SIZE).max(1);
        let page = page.min(pages - 1);
        let slice = &matches[page * CITY_PAGE_SIZE..((page + 1) * CITY_PAGE_SIZE).min(matches.len())];

        let mut rows: Vec<Vec<(String, String)>> = slice
            .iter()
            .map(|(id, label, province)| {
                vec![(
                    format!("{province} · {label}"),
                    self.tokens.put(Action::PickService {
                        service: id.clone(),
                        label: label.clone(),
                    }),
                )]
            })
            .collect();
        rows.push(self.pager(pages, page, |page| Action::CityPage {
            keyword: keyword.to_string(),
            page,
        }));
        rows.retain(|row| !row.is_empty());
        rows.push(vec![("🏠 主菜单".into(), "m:home".into())]);

        let text = format!(
            "找到 {} 个匹配的城市 · 数据源（第 {}/{} 页）\n选择一个作为你的默认城市：",
            matches.len(),
            page + 1,
            pages
        );
        self.store
            .update(user_id, |user| user.pending = Pending::None);
        (text, Some(inline_keyboard(rows)))
    }

    async fn all_lines(&self, service: &str) -> Result<Arc<Vec<BusRoute>>, String> {
        if let Some((at, cached)) = self.lines_cache.lock().get(service)
            && at.elapsed() < LINES_CACHE_TTL
        {
            return Ok(Arc::clone(cached));
        }
        let provider = self.provider_for(service);
        let lines = Arc::new(upstream(async move { provider.all_lines().await }).await?);
        self.lines_cache
            .lock()
            .insert(service.to_string(), (Instant::now(), Arc::clone(&lines)));
        Ok(lines)
    }

    pub(crate) async fn line_detail(&self, service: &str, direction: &str) -> Result<Arc<LineDetail>, String> {
        let key = format!("{service}|{direction}");
        if let Some((at, cached)) = self.detail_cache.lock().get(&key)
            && at.elapsed() < DETAIL_CACHE_TTL
        {
            return Ok(Arc::clone(cached));
        }
        let provider = self.provider_for(service);
        let owned = direction.to_string();
        let detail = Arc::new(upstream(async move { provider.line_detail(&owned).await }).await?);
        self.detail_cache
            .lock()
            .insert(key, (Instant::now(), Arc::clone(&detail)));
        Ok(detail)
    }

    /// 线路搜索结果界面。
    async fn search_view(
        &self,
        user_id: i64,
        keyword: &str,
        page: usize,
    ) -> (String, Option<Value>) {
        let keyword = keyword.trim().to_string();
        self.store
            .update(user_id, |user| user.pending = Pending::None);
        let Some(service) = self.store.get(user_id).service else {
            return (PROMPT_CITY.to_string(), Some(self.prompt_keyboard()));
        };

        let lines = match self.all_lines(&service).await {
            Ok(lines) => lines,
            Err(error) => return (error, Some(inline_keyboard(vec![home_row()]))),
        };

        let needle = keyword.to_lowercase();
        let matches: Vec<&BusRoute> = lines
            .iter()
            .filter(|route| {
                route.name.to_lowercase().contains(&needle)
                    || route.endpoints.origin.contains(&keyword)
                    || route.endpoints.terminus.contains(&keyword)
            })
            .collect();

        if matches.is_empty() {
            let text = format!(
                "没有找到匹配「{}」的线路。换个关键词，或用 /city 确认城市是否正确。",
                render::escape(&keyword)
            );
            return (text, Some(self.prompt_keyboard()));
        }

        let pages = matches.len().div_ceil(SEARCH_PAGE_SIZE).max(1);
        let page = page.min(pages - 1);
        let slice =
            &matches[page * SEARCH_PAGE_SIZE..((page + 1) * SEARCH_PAGE_SIZE).min(matches.len())];

        let mut text = format!(
            "🔍 「{}」找到 {} 条线路（第 {}/{} 页）\n\n",
            render::escape(&keyword),
            matches.len(),
            page + 1,
            pages
        );
        let mut rows: Vec<Vec<(String, String)>> = Vec::new();
        let mut current: Vec<(String, String)> = Vec::new();
        for route in slice {
            text.push_str(&format!(
                "🚌 <b>{}</b>\n   {} → {}\n",
                render::escape(&route.name),
                render::escape(
                    Some(route.endpoints.origin.as_str())
                        .filter(|v| !v.is_empty())
                        .unwrap_or("起点待更新")
                ),
                render::escape(
                    Some(route.endpoints.terminus.as_str())
                        .filter(|v| !v.is_empty())
                        .unwrap_or("终点待更新")
                ),
            ));
            // 同名线路有上下行两条，按钮上带终点站才能区分
            current.push((
                match route.endpoints.terminus.as_str() {
                    "" => truncate(&route.name, 14),
                    terminus => format!("{}→{}", truncate(&route.name, 8), truncate(terminus, 6)),
                },
                self.tokens.put(Action::Line {
                    service: service.clone(),
                    direction: route.direction_id.clone(),
                    page: 0,
                }),
            ));
            if current.len() == 3 {
                rows.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            rows.push(current);
        }
        let pager = self.pager(pages, page, |page| Action::SearchPage {
            keyword: keyword.clone(),
            page,
        });
        if !pager.is_empty() {
            rows.push(pager);
        }
        rows.push(vec![("🏠 主菜单".into(), "m:home".into())]);
        (text, Some(inline_keyboard(rows)))
    }

    async fn line_view(
        &self,
        user_id: i64,
        service: &str,
        direction: &str,
        page: usize,
    ) -> (String, Option<Value>) {
        let detail = match self.line_detail(service, direction).await {
            Ok(detail) => detail,
            Err(error) => return (error, Some(inline_keyboard(vec![home_row()]))),
        };
        let city_label = self
            .store
            .get(user_id)
            .city_label
            .unwrap_or_else(|| service.to_string());
        let key = detail.direction_id.clone();

        let stops = &detail.topology.stations;
        let pages = stops.len().div_ceil(STATION_PAGE_SIZE).max(1);
        let page = page.min(pages - 1);
        let slice = &stops[page * STATION_PAGE_SIZE..((page + 1) * STATION_PAGE_SIZE).min(stops.len())];

        let mut text = render::line_detail_text(&detail, &city_label);
        text.push_str(&format!("（第 {}/{} 页）", page + 1, pages));

        let mut rows: Vec<Vec<(String, String)>> = Vec::new();
        let mut current: Vec<(String, String)> = Vec::new();
        for stop in slice {
            let flag = match render::stop_status_note(stop.status) {
                Some(_) => "*",
                None => "",
            };
            current.push((
                format!("{} {}{}", stop.order, truncate(&stop.name, 10), flag),
                self.tokens.put(Action::Live {
                    service: service.to_string(),
                    direction: key.clone(),
                    order: stop.order,
                }),
            ));
            if current.len() == 2 {
                rows.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            rows.push(current);
        }

        let pager = self.pager(pages, page, |page| Action::Line {
            service: service.to_string(),
            direction: key.clone(),
            page,
        });
        if !pager.is_empty() {
            rows.push(pager);
        }
        if let Some(reverse) = detail.reverse_id.clone() {
            rows.push(vec![(
                "🔄 换方向".into(),
                self.tokens.put(Action::Line {
                    service: service.to_string(),
                    direction: reverse,
                    page: 0,
                }),
            )]);
        }
        rows.push(home_row());
        (text, Some(inline_keyboard(rows)))
    }

    /// `record` 为 false 时只重绘（例如收藏按钮引起的刷新），不计入查询习惯。
    async fn live_view(
        &self,
        user_id: i64,
        service: &str,
        direction: &str,
        order: u32,
        record: bool,
    ) -> (String, Option<Value>) {
        let detail = match self.line_detail(service, direction).await {
            Ok(detail) => detail,
            Err(error) => return (error, Some(inline_keyboard(vec![home_row()]))),
        };
        let key = detail.direction_id.clone();
        let provider = self.provider_for(service);
        let realtime = {
            let query_key = key.clone();
            match upstream(async move { provider.realtime(&query_key, order).await }).await {
                Ok(realtime) => realtime,
                Err(error) => {
                    let rows = vec![
                        vec![(
                            "🔄 重试".into(),
                            self.tokens.put(Action::Live {
                                service: service.to_string(),
                                direction: key.clone(),
                                order,
                            }),
                        )],
                        home_row(),
                    ];
                    return (error, Some(inline_keyboard(rows)));
                }
            }
        };

        let station_name = detail
            .topology
            .stations
            .iter()
            .find(|stop| stop.order == order)
            .map(|stop| stop.name.clone())
            .unwrap_or_else(|| format!("第 {order} 站"));

        let user = self.store.update(user_id, |user| {
            if record {
                user.record_query(
                    service,
                    &detail.name,
                    &key,
                    &station_name,
                    order,
                    local_hour(now_secs(), self.tz_offset),
                );
            }
            user.clone()
        });
        let is_favorite = user.is_favorite(service, &key, order);
        let city_label = user.city_label.unwrap_or_else(|| service.to_string());

        let text = render::live_text(
            &detail,
            &realtime,
            order,
            &city_label,
            &self.clock(),
            is_favorite,
        );

        let rows = vec![
            vec![
                (
                    "🔄 刷新".into(),
                    self.tokens.put(Action::Live {
                        service: service.to_string(),
                        direction: key.clone(),
                        order,
                    }),
                ),
                (
                    if is_favorite {
                        "💔 取消收藏".into()
                    } else {
                        "⭐ 收藏这一趟".to_string()
                    },
                    self.tokens.put(Action::FavToggle {
                        service: service.to_string(),
                        direction: key.clone(),
                        order,
                    }),
                ),
            ],
            vec![(
                "🔔 盯这趟车（到站前提醒我）".into(),
                self.tokens.put(Action::WatchSetup {
                    service: service.to_string(),
                    direction: key.clone(),
                    order,
                }),
            )],
            vec![
                (
                    "🚏 换上车站".into(),
                    self.tokens.put(Action::Line {
                        service: service.to_string(),
                        direction: key.clone(),
                        page: (order.saturating_sub(1) as usize) / STATION_PAGE_SIZE,
                    }),
                ),
                ("⭐ 我的收藏".into(), "m:fav".into()),
            ],
            home_row(),
        ];
        (text, Some(inline_keyboard(rows)))
    }

    /// 盯车第一步：让用户挑要等哪辆车。
    async fn watch_setup_view(
        &self,
        service: &str,
        direction: &str,
        order: u32,
    ) -> (String, Option<Value>) {
        let (detail, realtime) = match self.realtime_snapshot(service, direction, order).await {
            Ok(snapshot) => snapshot,
            Err(error) => return (error, Some(inline_keyboard(vec![home_row()]))),
        };
        let stops = &detail.topology.stations;
        let station_name = stops
            .iter()
            .find(|stop| stop.order == order)
            .map(|stop| stop.name.clone())
            .unwrap_or_else(|| format!("第 {order} 站"));

        let mut text = format!(
            "🔔 <b>盯车</b>\n{} · 上车站「{}」\n\n选择你要等的车：\n",
            render::escape(&detail.name),
            render::escape(&station_name),
        );
        let mut rows: Vec<Vec<(String, String)>> = vec![vec![(
            "⚡ 最近的一班（自动跟随）".into(),
            self.tokens.put(Action::WatchStart {
                service: service.to_string(),
                direction: detail.direction_id.clone(),
                order,
                bus: None,
            }),
        )]];

        for (index, bus) in realtime.buses.iter().enumerate() {
            let view = render::describe_bus(bus, index, stops, order);
            if view.passed {
                continue;
            }
            text.push_str(&format!(
                "🚌 {}\n   {}\n   {}\n",
                render::escape(&view.identity),
                render::escape(&view.location),
                render::escape(&view.estimate),
            ));
            // 没有车辆编号就没法在后续轮询里认出同一辆车，只能用自动模式
            if bus.bus_id.trim().is_empty() {
                continue;
            }
            rows.push(vec![(
                format!("🚌 {}", truncate(&view.identity, 22)),
                self.tokens.put(Action::WatchStart {
                    service: service.to_string(),
                    direction: detail.direction_id.clone(),
                    order,
                    bus: Some(bus.bus_id.clone()),
                }),
            )]);
        }

        if rows.len() == 1 {
            text.push_str(
                "\n上游当前没有可单独指定的车辆（没有车辆编号或暂无在途车），可以先用「最近的一班」。\n",
            );
        }
        let alerts_hint = self.store.settings().defaults;
        text.push_str(&format!(
            "\n默认规则：还有 {} 站时提醒一次，{} 米内每 {} 秒重复提醒，每 {} 秒刷新一次卡片。可在 /settings 修改。",
            alerts_hint.alert_stations,
            alerts_hint.alert_distance_m,
            alerts_hint.repeat_secs,
            alerts_hint.poll_secs,
        ));
        rows.push(vec![("⚙️ 提醒设置".into(), "m:settings".into())]);
        rows.push(home_row());
        (text, Some(inline_keyboard(rows)))
    }

    /// 建立卡片消息并启动盯车循环。
    pub(crate) async fn start_watch(
        self: &Arc<Self>,
        ctx: &Ctx,
        service: &str,
        direction: &str,
        order: u32,
        bus: Option<String>,
    ) {
        let (user_id, chat_id) = (ctx.user_id, ctx.chat_id);
        if self.store.get(user_id).watch.is_some() {
            self.stop_watch(user_id, "已被新的盯车任务替换。").await;
        }

        let detail = match self.line_detail(service, direction).await {
            Ok(detail) => detail,
            Err(error) => {
                self.show(ctx, &error, Some(inline_keyboard(vec![home_row()])))
                    .await;
                return;
            }
        };
        // 收藏可能指向已经调整过的线路，站序对不上就别开始盯了
        let Some(station_name) = detail
            .topology
            .stations
            .iter()
            .find(|stop| stop.order == order)
            .map(|stop| stop.name.clone())
        else {
            let markup = inline_keyboard(vec![
                vec![(
                    "🚏 重新选上车站".into(),
                    self.tokens.put(Action::Line {
                        service: service.to_string(),
                        direction: detail.direction_id.clone(),
                        page: 0,
                    }),
                )],
                home_row(),
            ]);
            self.show(
                ctx,
                &format!(
                    "「{}」当前没有第 {order} 站，线路站点可能已调整，请重新选择上车站。",
                    render::escape(&detail.name)
                ),
                Some(markup),
            )
            .await;
            return;
        };
        let city_label = self
            .store
            .get(user_id)
            .city_label
            .unwrap_or_else(|| service.to_string());

        // 盯车卡片独立于主卡片：它每隔几秒自己刷新，不该被导航覆盖
        let card = match self
            .tg
            .send_message(chat_id, "🔔 盯车已启动，正在获取实时数据…", None)
            .await
        {
            Ok(message) => message,
            Err(error) => {
                eprintln!("[bot] 创建盯车卡片失败：{error}");
                return;
            }
        };

        let spec = WatchSpec {
            service: service.to_string(),
            city_label,
            line_name: detail.name.clone(),
            direction: detail.direction_id.clone(),
            station_name,
            order,
            chat_id,
            card_message_id: card.message_id,
            target_bus: bus,
            started_at: now_secs(),
        };
        self.store
            .update(user_id, |user| user.watch = Some(spec.clone()));
        self.spawn_watch(user_id, spec);

        // 主卡片回到菜单，避免和盯车卡片显示同一条线路
        let (text, markup) = self.home_view(user_id);
        self.show(ctx, &format!("🔔 盯车已启动，见下方卡片。\n\n{text}"), markup)
            .await;
    }

    fn settings_view(&self, user_id: i64) -> (String, Value) {
        let user = self.store.get(user_id);
        let alerts = self.store.alerts_for(user_id);
        let text = render::alert_settings_text(&alerts, user.alerts.is_none());

        let mut rows: Vec<Vec<(String, String)>> = AlertField::ALL
            .into_iter()
            .map(|field| {
                vec![
                    ("➖".into(), self.tokens.put(Action::SettingsAdjust { field, steps: -1 })),
                    (
                        format!("{} {}{}", field.label(), alerts.get(field), field.unit()),
                        "m:settings".into(),
                    ),
                    ("➕".into(), self.tokens.put(Action::SettingsAdjust { field, steps: 1 })),
                ]
            })
            .collect();
        rows.push(vec![(
            "↩️ 恢复默认".into(),
            self.tokens.put(Action::SettingsReset),
        )]);
        rows.push(home_row());
        (text, inline_keyboard(rows))
    }

    /// 盯车状态页（/watch 用）。
    fn watch_view(&self, user_id: i64) -> (String, Value) {
        let user = self.store.get(user_id);
        match user.watch {
            Some(spec) => (
                format!(
                    "🔔 <b>正在盯车</b>\n{}\n\n卡片消息会每 {} 秒自动更新；也可以直接停止。",
                    render::escape(&spec.label()),
                    self.store.alerts_for(user_id).poll_secs,
                ),
                inline_keyboard(vec![
                    vec![("⏹ 停止盯车".into(), self.tokens.put(Action::WatchStop))],
                    vec![("⚙️ 提醒设置".into(), "m:settings".into())],
                    home_row(),
                ]),
            ),
            None if user.favorites.is_empty() => (
                "🔔 <b>盯车</b>\n\n还没有进行中的盯车。\n先搜索线路并选好上车站，在实时到站页点「🔔 盯这趟车」即可。"
                    .to_string(),
                inline_keyboard(vec![
                    vec![("🔍 搜线路".into(), "m:search".into())],
                    home_row(),
                ]),
            ),
            None => {
                let mut rows: Vec<Vec<(String, String)>> = user
                    .favorites
                    .iter()
                    .map(|favorite| {
                        vec![(
                            format!("🔔 {}", truncate(&favorite.label(), 24)),
                            self.tokens.put(Action::WatchSetup {
                                service: favorite.service.clone(),
                                direction: favorite.direction.clone(),
                                order: favorite.order,
                            }),
                        )]
                    })
                    .collect();
                rows.push(home_row());
                (
                    "🔔 <b>盯车</b>\n\n从收藏里选一个车次开始盯车，或先搜索线路。".to_string(),
                    inline_keyboard(rows),
                )
            }
        }
    }

    async fn toggle_favorite(&self, user_id: i64, service: &str, direction: &str, order: u32) {
        let detail = self.line_detail(service, direction).await.ok();
        let (line_name, station_name) = match detail.as_deref() {
            Some(detail) => (
                detail.name.clone(),
                detail
                    .topology
                    .stations
                    .iter()
                    .find(|stop| stop.order == order)
                    .map(|stop| stop.name.clone())
                    .unwrap_or_else(|| format!("第 {order} 站")),
            ),
            None => (direction.to_string(), format!("第 {order} 站")),
        };
        let city_label = self
            .store
            .get(user_id)
            .city_label
            .unwrap_or_else(|| service.to_string());

        self.store.update(user_id, |user| {
            user.toggle_favorite(Favorite {
                service: service.to_string(),
                city_label: city_label.clone(),
                line_name: line_name.clone(),
                direction: direction.to_string(),
                station_name: station_name.clone(),
                order,
                added_at: now_secs(),
                hits: 0,
                last_at: 0,
            })
        });
    }

    fn favorites_view(&self, user_id: i64) -> (String, Value) {
        let user = self.store.get(user_id);
        if user.favorites.is_empty() {
            return (
                "⭐ <b>我的收藏</b>\n\n还没有收藏。查看任意线路的实时到站后，点「⭐ 收藏这一趟」即可保存线路 + 上车站的组合。".to_string(),
                inline_keyboard(vec![
                    vec![("🔍 搜线路".into(), "m:search".into())],
                    home_row(),
                ]),
            );
        }

        let mut favorites: Vec<&Favorite> = user.favorites.iter().collect();
        favorites.sort_by(|a, b| b.hits.cmp(&a.hits).then(b.last_at.cmp(&a.last_at)));

        let mut text = format!("⭐ <b>我的收藏</b>（{} 条）\n\n", favorites.len());
        let mut rows: Vec<Vec<(String, String)>> = Vec::new();
        for favorite in favorites {
            text.push_str(&format!(
                "🚌 <b>{}</b> @ {}\n   {} · 查过 {} 次\n",
                render::escape(&favorite.line_name),
                render::escape(&favorite.station_name),
                render::escape(&favorite.city_label),
                favorite.hits,
            ));
            rows.push(vec![
                (
                    format!("🚌 {}", truncate(&favorite.label(), 24)),
                    self.tokens.put(Action::Live {
                        service: favorite.service.clone(),
                        direction: favorite.direction.clone(),
                        order: favorite.order,
                    }),
                ),
                (
                    "🗑".into(),
                    self.tokens.put(Action::FavRemove {
                        service: favorite.service.clone(),
                        direction: favorite.direction.clone(),
                        order: favorite.order,
                    }),
                ),
            ]);
        }
        rows.push(home_row());
        (text, inline_keyboard(rows))
    }

    fn habits_view(&self, user_id: i64) -> (String, Value) {
        let user = self.store.get(user_id);
        let hour = self.hour();
        let text = render::habits_text(&user, hour);
        let mut rows: Vec<Vec<(String, String)>> = user
            .top_habits(3)
            .into_iter()
            .map(|habit| {
                vec![(
                    render::habit_button_label(habit),
                    self.tokens.put(Action::Live {
                        service: habit.service.clone(),
                        direction: habit.direction.clone(),
                        order: habit.order,
                    }),
                )]
            })
            .collect();
        rows.push(vec![("⭐ 我的收藏".into(), "m:fav".into())]);
        rows.push(home_row());
        (text, inline_keyboard(rows))
    }

    async fn nearby_view(&self, service: &str, lat: f64, lng: f64) -> (String, Option<Value>) {
        let provider = self.provider_for(service);
        let stations = match upstream(async move { provider.nearby_stations(lat, lng).await }).await
        {
            Ok(stations) => stations,
            Err(error) => return (error, Some(inline_keyboard(vec![home_row()]))),
        };
        if stations.is_empty() {
            return (
                "附近没有查到站点。可能是坐标不在该城市范围内，或数据源没有覆盖。".to_string(),
                Some(inline_keyboard(vec![home_row()])),
            );
        }

        let mut stations = stations;
        stations.sort_by_key(|station| station.distance_m);
        stations.truncate(10);

        let mut text = format!("📍 <b>附近站点</b>（{} 个）\n\n", stations.len());
        let mut rows: Vec<Vec<(String, String)>> = Vec::new();
        for station in &stations {
            text.push_str(&format!(
                "🚏 <b>{}</b> · 约 {} 米\n",
                render::escape(&station.name),
                station.distance_m
            ));
            rows.push(vec![(
                format!("🚏 {} · {}m", truncate(&station.name, 16), station.distance_m),
                self.tokens.put(Action::Station {
                    service: service.to_string(),
                    name: station.name.clone(),
                    lat,
                    lng,
                }),
            )]);
        }
        rows.push(home_row());
        (text, Some(inline_keyboard(rows)))
    }

    async fn station_view(
        &self,
        service: &str,
        station: &str,
        lat: f64,
        lng: f64,
    ) -> (String, Option<Value>) {
        let provider = self.provider_for(service);
        let name = station.to_string();
        let lines = match upstream(async move { provider.station_lines(&name, lat, lng).await }).await
        {
            Ok(lines) => lines,
            Err(error) => return (error, Some(inline_keyboard(vec![home_row()]))),
        };
        if lines.is_empty() {
            return (
                format!("「{}」没有查到经过的线路。", render::escape(station)),
                Some(inline_keyboard(vec![home_row()])),
            );
        }

        let mut lines = lines;
        lines.sort_by_key(|line| line.arrival.proximity_score());

        let mut text = format!(
            "🚏 <b>{}</b>\n经过 {} 条线路（按最近到站排序）\n\n",
            render::escape(station),
            lines.len()
        );
        let mut rows: Vec<Vec<(String, String)>> = Vec::new();
        let mut current: Vec<(String, String)> = Vec::new();
        for line in lines.iter().take(20) {
            text.push_str(&format!(
                "🚌 <b>{}</b> → {}\n   {} · {}\n",
                render::escape(&line.name),
                render::escape(
                    Some(line.endpoints.terminus.as_str())
                        .filter(|v| !v.is_empty())
                        .unwrap_or("终点待更新")
                ),
                render::arrival_text(&line.arrival),
                render::run_state_text(line.run_state),
            ));
            current.push((
                truncate(&line.name, 14),
                self.tokens.put(Action::Live {
                    service: service.to_string(),
                    direction: line.direction_id.clone(),
                    order: line.station_order.max(1),
                }),
            ));
            if current.len() == 3 {
                rows.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            rows.push(current);
        }
        rows.push(vec![("📍 重新查附近".into(), self.tokens.put(Action::NearbyHere))]);
        rows.push(home_row());
        (text, Some(inline_keyboard(rows)))
    }

    /// 翻页按钮行；只有一页时返回空行（调用方会丢弃）。
    fn pager(
        &self,
        pages: usize,
        page: usize,
        make: impl Fn(usize) -> Action,
    ) -> Vec<(String, String)> {
        let mut row = Vec::new();
        if pages <= 1 {
            return row;
        }
        if page > 0 {
            row.push(("⬅️ 上一页".into(), self.tokens.put(make(page - 1))));
        }
        if page + 1 < pages {
            row.push(("➡️ 下一页".into(), self.tokens.put(make(page + 1))));
        }
        row
    }
}

/// 常驻键盘上的按钮文案对应哪个命令。
fn keyboard_command(text: &str) -> Option<&'static str> {
    match text {
        "🔍 搜线路" => Some("line"),
        "📍 附近站点" => Some("nearby"),
        "⭐ 我的收藏" => Some("fav"),
        "🔔 盯车" => Some("watch"),
        "⚙️ 提醒设置" => Some("settings"),
        "📊 我的习惯" => Some("habits"),
        "🏙 切换城市" => Some("city"),
        "❓ 使用说明" => Some("help"),
        _ => None,
    }
}

/// 这条更新属于哪个用户；拿不到就不处理（例如频道消息）。
fn update_actor(update: &Update) -> Option<i64> {
    if let Some(message) = &update.message {
        return Some(message.from.as_ref().map_or(message.chat.id, |from| from.id));
    }
    update.callback_query.as_ref().map(|callback| callback.from.id)
}

fn home_row() -> Vec<(String, String)> {
    vec![("🏠 主菜单".into(), "m:home".into())]
}

/// 按字符截断按钮文案（Telegram 按钮太长会被挤压）。
fn truncate(raw: &str, max_chars: usize) -> String {
    if raw.chars().count() <= max_chars {
        return raw.to_string();
    }
    let mut out: String = raw.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 解析 `/command@bot 参数`，返回 (命令, 参数)。
fn parse_command(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix('/')?;
    let (head, argument) = match rest.split_once(char::is_whitespace) {
        Some((head, argument)) => (head, argument.trim()),
        None => (rest, ""),
    };
    let command = head.split('@').next().unwrap_or(head).to_lowercase();
    if command.is_empty() {
        return None;
    }
    Some((command, argument.to_string()))
}

const HELP_TEXT: &str = "🚏 <b>WhereBus 使用说明</b>\n\n\
功能都在输入框下方的键盘里，点一下就行：\n\
🔍 搜线路 · 📍 附近站点（直接共享位置）\n\
⭐ 我的收藏 · 🔔 盯车\n\
⚙️ 提醒设置 · 📊 我的习惯\n\
🏙 切换城市 · ❓ 使用说明\n\n\
<b>怎么用</b>\n\
1️⃣ 先「🏙 切换城市」选好城市与数据源（会记住）\n\
2️⃣ 「🔍 搜线路」或直接发送线路名（如「K155」）\n\
3️⃣ 在线路里点上车站，查看实时到站\n\
4️⃣ 点「⭐ 收藏这一趟」保存「线路 + 上车站」，之后从「⭐ 我的收藏」一键刷新\n\
5️⃣ 点「🔔 盯这趟车」开始盯车：卡片按设定间隔自动刷新，车快到时主动提醒你\n\n\
<b>盯车</b>：选好线路与上车站后，可以指定等某一辆车，或跟随「最近的一班」。\
默认在目标车还差 1 站时提醒一次，进入 500 米后每 60 秒重复提醒，每 10 秒刷新一次卡片；\
这些都能在「⚙️ 提醒设置」里改。车进站或已驶过时自动结束。\n\n\
界面始终只有一张卡片，并且会跟着你的消息移到最新位置；盯车卡片与到站提醒是单独的消息。\n\n\
数据来自公开的第三方公交数据源，没有任何模拟数据；上游异常时会如实提示。\n\
你的城市选择、收藏和查询统计保存在运行机器人的服务器上。";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_ignore_bot_suffix_and_case() {
        assert_eq!(
            parse_command("/City@WhereBusBot 深圳"),
            Some(("city".into(), "深圳".into()))
        );
        assert_eq!(parse_command("/fav"), Some(("fav".into(), String::new())));
        assert_eq!(parse_command("M375"), None);
        assert_eq!(parse_command("/"), None);
    }

    #[test]
    fn tokens_round_trip_and_expire_oldest_first() {
        let table = TokenTable::default();
        let action = Action::Live {
            service: "shenzhen".into(),
            direction: "M375:1".into(),
            order: 5,
        };
        let key = table.put(action.clone());
        assert!(key.len() <= 64);
        assert_eq!(table.get(&key), Some(action));
        assert_eq!(table.get("t:999999"), None);
        assert_eq!(table.get("m:home"), None);
    }

    #[test]
    fn token_table_is_bounded() {
        let table = TokenTable::default();
        let first = table.put(Action::NearbyHere);
        for _ in 0..MAX_TOKENS {
            table.put(Action::NearbyHere);
        }
        assert_eq!(table.get(&first), None, "最旧的 token 应被淘汰");
    }

    #[test]
    fn duplicate_updates_are_ignored() {
        let seen = SeenUpdates::default();
        assert!(seen.accept(7));
        // Telegram 重发同一条更新时不再执行第二次
        assert!(!seen.accept(7));
        assert!(seen.accept(8));

        // 窗口滚动后，很久以前的 id 允许再次进入（正常使用中不会复用 id）
        for id in 100..(100 + MAX_SEEN_UPDATES as i64) {
            seen.accept(id);
        }
        assert!(seen.accept(7));
    }

    #[test]
    fn only_the_latest_interaction_renders() {
        let epochs = Epochs::default();
        let first = epochs.begin(42);
        assert!(epochs.is_current(42, first));

        // 用户又点了一次：先发起的那次不再有权覆盖界面
        let second = epochs.begin(42);
        assert!(!epochs.is_current(42, first));
        assert!(epochs.is_current(42, second));

        // 不同用户互不影响
        let other = epochs.begin(7);
        assert!(epochs.is_current(7, other));
        assert!(epochs.is_current(42, second));
        assert!(!epochs.is_current(7, second + 10));
    }

    #[test]
    fn keyboard_buttons_map_to_commands() {
        assert_eq!(keyboard_command("⭐ 我的收藏"), Some("fav"));
        assert_eq!(keyboard_command("🔔 盯车"), Some("watch"));
        assert_eq!(keyboard_command("🏙 切换城市"), Some("city"));
        // 普通文字仍然按线路关键词处理
        assert_eq!(keyboard_command("K155"), None);
        assert_eq!(keyboard_command("我的收藏"), None);
    }

    #[test]
    fn update_actor_reads_the_acting_user() {
        let message: Update = serde_json::from_value(serde_json::json!({
            "update_id": 1,
            "message": {"message_id": 5, "chat": {"id": 42, "type": "private"},
                        "from": {"id": 7, "first_name": "A"}, "text": "/start"}
        }))
        .unwrap();
        assert_eq!(update_actor(&message), Some(7));

        let callback: Update = serde_json::from_value(serde_json::json!({
            "update_id": 2,
            "callback_query": {"id": "c", "from": {"id": 9, "first_name": "B"}, "data": "m:home"}
        }))
        .unwrap();
        assert_eq!(update_actor(&callback), Some(9));

        let empty: Update = serde_json::from_value(serde_json::json!({"update_id": 3})).unwrap();
        assert_eq!(update_actor(&empty), None);
    }

    #[test]
    fn navigation_never_overwrites_the_watch_card() {
        // 点主卡片上的按钮：就地更新这张卡片
        assert_eq!(card_target(Some(10), Some(10), None), Some(10));
        // 点的是另一张旧卡片：那张变成主卡片
        assert_eq!(card_target(Some(11), Some(10), None), Some(11));
        // 点的是盯车卡片上的按钮（例如「提醒设置」）：渲染到主卡片
        assert_eq!(card_target(Some(99), Some(10), Some(99)), Some(10));
        // 文本命令没有来源消息：用主卡片
        assert_eq!(card_target(None, Some(10), None), Some(10));
        // 主卡片正好是盯车卡片，或还没有卡片：返回 None，由调用方新发一条
        assert_eq!(card_target(None, Some(99), Some(99)), None);
        assert_eq!(card_target(None, None, None), None);
    }

    #[test]
    fn button_labels_are_truncated() {
        assert_eq!(truncate("科技园北区总站", 4), "科技园…");
        assert_eq!(truncate("M375", 10), "M375");
    }
}
