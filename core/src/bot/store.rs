//! 用户数据持久化：城市选择、收藏车次、查询习惯。
//!
//! 单个 JSON 文件 + 内存读写锁；后台定时落盘（先写临时文件再 rename，避免半截文件）。
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 按固定时区偏移换算本地小时（0-23）。不引入日期库。
pub fn local_hour(unix_secs: u64, tz_offset_hours: i64) -> u32 {
    let shifted = unix_secs as i64 + tz_offset_hours * 3600;
    let shifted = shifted.rem_euclid(86_400);
    (shifted / 3600) as u32
}

/// 按固定时区偏移格式化 HH:MM。
pub fn local_clock(unix_secs: u64, tz_offset_hours: i64) -> String {
    let shifted = (unix_secs as i64 + tz_offset_hours * 3600).rem_euclid(86_400);
    format!("{:02}:{:02}", shifted / 3600, (shifted % 3600) / 60)
}

/// 盯车卡片每几秒就刷新一次，精确到秒才能看出内容有变化。
pub fn local_clock_secs(unix_secs: u64, tz_offset_hours: i64) -> String {
    let shifted = (unix_secs as i64 + tz_offset_hours * 3600).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        shifted / 3600,
        (shifted % 3600) / 60,
        shifted % 60
    )
}

// ─── 提醒设置 ───

/// 盯车提醒参数。每个用户可单独设置，未设置时继承全局默认值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertSettings {
    /// 实时数据轮询间隔（秒）
    pub poll_secs: u64,
    /// 目标车还差几站时提醒（1 = 到前一站就提醒，0 = 只按距离提醒）
    pub alert_stations: u32,
    /// 目标车距上车站多少米以内开始重复提醒（0 = 关闭距离提醒）
    pub alert_distance_m: u32,
    /// 距离提醒的重复间隔（秒）
    pub repeat_secs: u64,
    /// 单次盯车最长时间（分钟）
    pub max_minutes: u64,
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self {
            poll_secs: 10,
            alert_stations: 1,
            alert_distance_m: 500,
            repeat_secs: 60,
            max_minutes: 60,
        }
    }
}

/// 可调的提醒参数项，供机器人按钮与 HTTP 接口共用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertField {
    Poll,
    Stations,
    Distance,
    Repeat,
    MaxMinutes,
}

impl AlertField {
    pub const ALL: [AlertField; 5] = [
        Self::Poll,
        Self::Stations,
        Self::Distance,
        Self::Repeat,
        Self::MaxMinutes,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Poll => "poll_secs",
            Self::Stations => "alert_stations",
            Self::Distance => "alert_distance_m",
            Self::Repeat => "repeat_secs",
            Self::MaxMinutes => "max_minutes",
        }
    }

    pub fn parse(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Poll => "刷新间隔",
            Self::Stations => "提前提醒",
            Self::Distance => "距离提醒",
            Self::Repeat => "重复间隔",
            Self::MaxMinutes => "最长盯车",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Self::Poll | Self::Repeat => "秒",
            Self::Stations => "站",
            Self::Distance => "米",
            Self::MaxMinutes => "分钟",
        }
    }

    /// 按钮 ➖/➕ 每次调整的步长。
    pub fn step(self) -> u64 {
        match self {
            Self::Poll => 5,
            Self::Stations => 1,
            Self::Distance => 100,
            Self::Repeat => 15,
            Self::MaxMinutes => 10,
        }
    }

    /// 合法区间（闭区间）。
    pub fn range(self) -> (u64, u64) {
        match self {
            // 低于 5 秒会明显增加上游压力
            Self::Poll => (5, 120),
            Self::Stations => (0, 10),
            Self::Distance => (0, 5000),
            Self::Repeat => (15, 600),
            Self::MaxMinutes => (5, 240),
        }
    }
}

impl AlertSettings {
    pub fn get(&self, field: AlertField) -> u64 {
        match field {
            AlertField::Poll => self.poll_secs,
            AlertField::Stations => self.alert_stations as u64,
            AlertField::Distance => self.alert_distance_m as u64,
            AlertField::Repeat => self.repeat_secs,
            AlertField::MaxMinutes => self.max_minutes,
        }
    }

    /// 写入并夹到合法区间，返回实际生效的值。
    pub fn set(&mut self, field: AlertField, value: u64) -> u64 {
        let (low, high) = field.range();
        let value = value.clamp(low, high);
        match field {
            AlertField::Poll => self.poll_secs = value,
            AlertField::Stations => self.alert_stations = value as u32,
            AlertField::Distance => self.alert_distance_m = value as u32,
            AlertField::Repeat => self.repeat_secs = value,
            AlertField::MaxMinutes => self.max_minutes = value,
        }
        value
    }

    /// 按步长增减（按钮用），返回实际生效的值。
    pub fn adjust(&mut self, field: AlertField, steps: i64) -> u64 {
        let current = self.get(field) as i64;
        let next = (current + steps * field.step() as i64).max(0) as u64;
        self.set(field, next)
    }

    /// 载入历史数据后统一收敛到合法区间。
    pub fn clamped(mut self) -> Self {
        for field in AlertField::ALL {
            let value = self.get(field);
            self.set(field, value);
        }
        self
    }
}

// ─── 盯车任务 ───

/// 一次盯车：在某线路某站台等某辆车。持久化后重启可恢复。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchSpec {
    pub service: String,
    pub city_label: String,
    pub line_name: String,
    pub direction: String,
    pub station_name: String,
    pub order: u32,
    pub chat_id: i64,
    /// 需要原地更新的卡片消息
    pub card_message_id: i64,
    /// 指定车辆编号；None 表示「最近的一班」
    pub target_bus: Option<String>,
    pub started_at: u64,
}

impl WatchSpec {
    pub fn label(&self) -> String {
        match &self.target_bus {
            Some(bus) => format!("{} @ {} · 车辆 {}", self.line_name, self.station_name, bus),
            None => format!("{} @ {} · 最近一班", self.line_name, self.station_name),
        }
    }
}

/// 收藏的一个「车次」：某条线路在某个上车站的组合。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Favorite {
    pub service: String,
    pub city_label: String,
    pub line_name: String,
    pub direction: String,
    pub station_name: String,
    pub order: u32,
    pub added_at: u64,
    #[serde(default)]
    pub hits: u32,
    #[serde(default)]
    pub last_at: u64,
}

impl Favorite {
    pub fn label(&self) -> String {
        format!("{} @ {}", self.line_name, self.station_name)
    }
}

/// 一条线路的使用习惯：24 小时查询分布 + 累计次数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Habit {
    pub key: String,
    pub label: String,
    pub service: String,
    pub line_name: String,
    pub direction: String,
    pub station_name: String,
    pub order: u32,
    /// 定长 24，索引即本地小时
    pub hours: Vec<u32>,
    pub total: u32,
    pub last_at: u64,
}

impl Habit {
    /// 当前小时 ±1 的热度，用于「此刻推荐」。
    pub fn score_at(&self, hour: u32) -> u32 {
        let at = |h: i64| -> u32 {
            let h = h.rem_euclid(24) as usize;
            self.hours.get(h).copied().unwrap_or(0)
        };
        let hour = hour as i64;
        at(hour) * 3 + at(hour - 1) + at(hour + 1)
    }
}

/// 文本输入的等待状态（机器人当前在等用户回什么）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pending {
    #[default]
    None,
    /// 等待输入城市名
    City,
    /// 等待输入线路关键词
    Line,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserState {
    /// 选中的数据源 id（provider::available_services 的 id）
    pub service: Option<String>,
    /// 「城市 · 数据源」展示名
    pub city_label: Option<String>,
    #[serde(default)]
    pub favorites: Vec<Favorite>,
    #[serde(default)]
    pub habits: Vec<Habit>,
    #[serde(default)]
    pub queries: u32,
    #[serde(default)]
    pub first_seen: u64,
    #[serde(default)]
    pub last_seen: u64,
    /// 最近一次定位（已转换为 GCJ-02，与 provider 入参一致）
    #[serde(default)]
    pub last_location: Option<(f64, f64)>,
    #[serde(default)]
    pub pending: Pending,
    /// 个人提醒设置；None 表示跟随全局默认值
    #[serde(default)]
    pub alerts: Option<AlertSettings>,
    /// 正在进行的盯车任务（重启后会恢复）
    #[serde(default)]
    pub watch: Option<WatchSpec>,
    /// 被管理员停用后不再响应任何请求
    #[serde(default)]
    pub banned: bool,
    /// Telegram 昵称与用户名，仅用于管理界面展示
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub username: Option<String>,
}

/// 全局默认值，由管理员在管理后台修改。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalSettings {
    pub defaults: AlertSettings,
    /// 关闭后，没有记录的新用户无法开始使用
    pub allow_new_users: bool,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            defaults: AlertSettings::default(),
            allow_new_users: true,
        }
    }
}

pub fn habit_key(service: &str, direction: &str, order: u32) -> String {
    format!("{service}|{direction}|{order}")
}

impl UserState {
    pub fn favorite_index(&self, service: &str, direction: &str, order: u32) -> Option<usize> {
        self.favorites
            .iter()
            .position(|f| f.service == service && f.direction == direction && f.order == order)
    }

    pub fn is_favorite(&self, service: &str, direction: &str, order: u32) -> bool {
        self.favorite_index(service, direction, order).is_some()
    }

    /// 收藏 / 取消收藏，返回 true 表示现在是收藏状态。
    pub fn toggle_favorite(&mut self, candidate: Favorite) -> bool {
        match self.favorite_index(&candidate.service, &candidate.direction, candidate.order) {
            Some(index) => {
                self.favorites.remove(index);
                false
            }
            None => {
                self.favorites.push(Favorite {
                    added_at: now_secs(),
                    ..candidate
                });
                true
            }
        }
    }

    /// 记录一次实时查询：累计次数、按小时分布、收藏命中次数。
    pub fn record_query(
        &mut self,
        service: &str,
        line_name: &str,
        direction: &str,
        station_name: &str,
        order: u32,
        hour: u32,
    ) {
        let now = now_secs();
        self.queries = self.queries.saturating_add(1);
        self.last_seen = now;

        let key = habit_key(service, direction, order);
        let hour = (hour.min(23)) as usize;
        match self.habits.iter_mut().find(|h| h.key == key) {
            Some(habit) => {
                if habit.hours.len() < 24 {
                    habit.hours.resize(24, 0);
                }
                habit.hours[hour] = habit.hours[hour].saturating_add(1);
                habit.total = habit.total.saturating_add(1);
                habit.last_at = now;
                habit.label = format!("{line_name} @ {station_name}");
                habit.line_name = line_name.to_string();
                habit.station_name = station_name.to_string();
            }
            None => {
                let mut hours = vec![0u32; 24];
                hours[hour] = 1;
                self.habits.push(Habit {
                    key,
                    label: format!("{line_name} @ {station_name}"),
                    service: service.to_string(),
                    line_name: line_name.to_string(),
                    direction: direction.to_string(),
                    station_name: station_name.to_string(),
                    order,
                    hours,
                    total: 1,
                    last_at: now,
                });
            }
        }

        if let Some(index) = self.favorite_index(service, direction, order) {
            let favorite = &mut self.favorites[index];
            favorite.hits = favorite.hits.saturating_add(1);
            favorite.last_at = now;
        }
    }

    /// 按总次数排序的常用线路。
    pub fn top_habits(&self, limit: usize) -> Vec<&Habit> {
        let mut habits: Vec<&Habit> = self.habits.iter().collect();
        habits.sort_by(|a, b| b.total.cmp(&a.total).then(b.last_at.cmp(&a.last_at)));
        habits.into_iter().take(limit).collect()
    }

    /// 「此刻你通常查」：当前时段热度最高的线路。
    pub fn suggestions(&self, hour: u32, limit: usize) -> Vec<&Habit> {
        let mut habits: Vec<&Habit> = self
            .habits
            .iter()
            .filter(|habit| habit.score_at(hour) > 0)
            .collect();
        habits.sort_by(|a, b| {
            b.score_at(hour)
                .cmp(&a.score_at(hour))
                .then(b.total.cmp(&a.total))
        });
        habits.into_iter().take(limit).collect()
    }

    /// 24 小时查询分布合计。
    pub fn hour_histogram(&self) -> [u32; 24] {
        let mut totals = [0u32; 24];
        for habit in &self.habits {
            for (hour, count) in habit.hours.iter().enumerate().take(24) {
                totals[hour] = totals[hour].saturating_add(*count);
            }
        }
        totals
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Snapshot {
    #[serde(default)]
    users: HashMap<i64, UserState>,
    #[serde(default)]
    settings: GlobalSettings,
}

pub struct Store {
    path: PathBuf,
    users: RwLock<HashMap<i64, UserState>>,
    settings: RwLock<GlobalSettings>,
    dirty: AtomicBool,
}

impl Store {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let snapshot = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Snapshot>(&bytes).map_err(|e| {
                anyhow::anyhow!(
                    "用户数据文件 {} 解析失败: {e}（请修复或移走该文件）",
                    path.display()
                )
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Snapshot::default(),
            Err(error) => {
                return Err(anyhow::anyhow!("读取 {} 失败: {error}", path.display()));
            }
        };
        let settings = GlobalSettings {
            defaults: snapshot.settings.defaults.clamped(),
            ..snapshot.settings
        };
        Ok(Self {
            path,
            users: RwLock::new(snapshot.users),
            settings: RwLock::new(settings),
            dirty: AtomicBool::new(false),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn user_count(&self) -> usize {
        self.users.read().len()
    }

    pub fn get(&self, user_id: i64) -> UserState {
        self.users.read().get(&user_id).cloned().unwrap_or_default()
    }

    pub fn exists(&self, user_id: i64) -> bool {
        self.users.read().contains_key(&user_id)
    }

    /// 管理后台用：所有用户的快照，按最近活跃排序。
    pub fn all_users(&self) -> Vec<(i64, UserState)> {
        let mut users: Vec<(i64, UserState)> = self
            .users
            .read()
            .iter()
            .map(|(id, state)| (*id, state.clone()))
            .collect();
        users.sort_by(|a, b| b.1.last_seen.cmp(&a.1.last_seen));
        users
    }

    /// 删除一个用户的全部数据。
    pub fn remove(&self, user_id: i64) -> bool {
        let removed = self.users.write().remove(&user_id).is_some();
        if removed {
            self.dirty.store(true, Ordering::Relaxed);
        }
        removed
    }

    pub fn settings(&self) -> GlobalSettings {
        self.settings.read().clone()
    }

    pub fn update_settings<R>(&self, edit: impl FnOnce(&mut GlobalSettings) -> R) -> R {
        let result = {
            let mut settings = self.settings.write();
            let result = edit(&mut settings);
            settings.defaults = settings.defaults.clamped();
            result
        };
        self.dirty.store(true, Ordering::Relaxed);
        result
    }

    /// 用户的生效提醒设置：个人设置优先，否则用全局默认值。
    pub fn alerts_for(&self, user_id: i64) -> AlertSettings {
        self.get(user_id)
            .alerts
            .unwrap_or_else(|| self.settings().defaults)
            .clamped()
    }

    /// 所有仍在进行的盯车任务（启动时用于恢复）。
    pub fn active_watches(&self) -> Vec<(i64, WatchSpec)> {
        self.users
            .read()
            .iter()
            .filter_map(|(id, state)| state.watch.clone().map(|watch| (*id, watch)))
            .collect()
    }

    /// 修改某个用户的数据并标记待落盘。
    pub fn update<R>(&self, user_id: i64, edit: impl FnOnce(&mut UserState) -> R) -> R {
        let result = {
            let mut users = self.users.write();
            let state = users.entry(user_id).or_default();
            if state.first_seen == 0 {
                state.first_seen = now_secs();
            }
            state.last_seen = now_secs();
            edit(state)
        };
        self.dirty.store(true, Ordering::Relaxed);
        result
    }

    /// 有改动时写盘：临时文件 + rename，保证原文件不会被写坏。
    pub fn flush(&self) -> anyhow::Result<()> {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        let snapshot = Snapshot {
            users: self.users.read().clone(),
            settings: self.settings.read().clone(),
        };
        let bytes = serde_json::to_vec_pretty(&snapshot)?;
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, &bytes)?;
        // Windows 上 rename 不会覆盖已存在的文件，先删除旧文件
        if self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
        if let Err(error) = std::fs::rename(&temporary, &self.path) {
            self.dirty.store(true, Ordering::Relaxed);
            return Err(error.into());
        }
        Ok(())
    }

    /// 后台定时落盘任务。
    pub fn spawn_autosave(self: &Arc<Self>, every: std::time::Duration) {
        let store = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            loop {
                ticker.tick().await;
                if let Err(error) = store.flush() {
                    tracing::error!("[bot] 用户数据写入失败: {error}");
                    eprintln!("[bot] 用户数据写入失败: {error}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn favorite() -> Favorite {
        Favorite {
            service: "shenzhen".into(),
            city_label: "深圳 · 掌上公交".into(),
            line_name: "M375".into(),
            direction: "M375:1".into(),
            station_name: "科技园".into(),
            order: 5,
            ..Default::default()
        }
    }

    #[test]
    fn toggle_favorite_adds_then_removes() {
        let mut user = UserState::default();
        assert!(user.toggle_favorite(favorite()));
        assert!(user.is_favorite("shenzhen", "M375:1", 5));
        assert_eq!(user.favorites.len(), 1);
        assert!(!user.toggle_favorite(favorite()));
        assert!(user.favorites.is_empty());
    }

    #[test]
    fn habits_accumulate_per_hour_and_bump_favorite_hits() {
        let mut user = UserState::default();
        user.toggle_favorite(favorite());
        for _ in 0..3 {
            user.record_query("shenzhen", "M375", "M375:1", "科技园", 5, 8);
        }
        user.record_query("shenzhen", "M375", "M375:1", "科技园", 5, 18);
        user.record_query("shenzhen", "42", "42:1", "会展中心", 2, 18);

        assert_eq!(user.queries, 5);
        assert_eq!(user.favorites[0].hits, 4);
        let histogram = user.hour_histogram();
        assert_eq!(histogram[8], 3);
        assert_eq!(histogram[18], 2);
        assert_eq!(user.top_habits(1)[0].label, "M375 @ 科技园");
        // 早高峰只有 M375 有记录
        assert_eq!(user.suggestions(8, 3).len(), 1);
        assert_eq!(user.suggestions(8, 3)[0].line_name, "M375");
        // 凌晨没有任何记录时不硬凑推荐
        assert!(user.suggestions(3, 3).is_empty());
    }

    #[test]
    fn store_roundtrips_through_disk() {
        let path = std::env::temp_dir().join(format!("wherebus-bot-test-{}.json", now_secs()));
        let store = Store::load(&path).unwrap();
        store.update(42, |user| {
            user.service = Some("shenzhen".into());
            user.toggle_favorite(favorite());
            user.record_query("shenzhen", "M375", "M375:1", "科技园", 5, 8);
        });
        store.flush().unwrap();

        let reloaded = Store::load(&path).unwrap();
        let user = reloaded.get(42);
        assert_eq!(user.service.as_deref(), Some("shenzhen"));
        assert_eq!(user.favorites.len(), 1);
        assert_eq!(user.habits[0].total, 1);
        assert_eq!(reloaded.get(7).favorites.len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn alert_settings_stay_inside_their_range() {
        let mut alerts = AlertSettings::default();
        assert_eq!(alerts.poll_secs, 10);
        assert_eq!(alerts.alert_stations, 1);
        assert_eq!(alerts.alert_distance_m, 500);

        // 减到下界后不再变小
        assert_eq!(alerts.adjust(AlertField::Poll, -10), 5);
        assert_eq!(alerts.adjust(AlertField::Poll, -1), 5);
        assert_eq!(alerts.adjust(AlertField::Distance, 5), 1000);
        assert_eq!(alerts.set(AlertField::Distance, 99_999), 5000);
        assert_eq!(alerts.set(AlertField::Stations, 0), 0);

        let wild = AlertSettings {
            poll_secs: 1,
            alert_stations: 99,
            alert_distance_m: 999_999,
            repeat_secs: 0,
            max_minutes: 0,
        }
        .clamped();
        assert_eq!(wild.poll_secs, 5);
        assert_eq!(wild.alert_stations, 10);
        assert_eq!(wild.repeat_secs, 15);
        assert_eq!(AlertField::parse("alert_distance_m"), Some(AlertField::Distance));
        assert_eq!(AlertField::parse("nope"), None);
    }

    #[test]
    fn personal_alerts_override_global_defaults() {
        let path = std::env::temp_dir().join(format!("wherebus-bot-alerts-{}.json", now_secs()));
        let store = Store::load(&path).unwrap();
        assert_eq!(store.alerts_for(1).poll_secs, 10);

        store.update_settings(|settings| settings.defaults.poll_secs = 20);
        assert_eq!(store.alerts_for(1).poll_secs, 20);

        store.update(1, |user| {
            let mut alerts = AlertSettings::default();
            alerts.poll_secs = 30;
            user.alerts = Some(alerts);
        });
        assert_eq!(store.alerts_for(1).poll_secs, 30);
        assert_eq!(store.alerts_for(2).poll_secs, 20);

        store.flush().unwrap();
        let reloaded = Store::load(&path).unwrap();
        assert_eq!(reloaded.settings().defaults.poll_secs, 20);
        assert_eq!(reloaded.alerts_for(1).poll_secs, 30);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn local_time_uses_fixed_offset() {
        // 1970-01-01 00:00 UTC + 8h
        assert_eq!(local_hour(0, 8), 8);
        assert_eq!(local_clock(0, 8), "08:00");
        assert_eq!(local_clock_secs(3661, 0), "01:01:01");
        assert_eq!(local_hour(3600 * 20, 8), 4);
        assert_eq!(local_hour(0, -5), 19);
    }
}
