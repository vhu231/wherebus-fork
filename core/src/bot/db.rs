//! SQLite 持久化：用户、收藏、习惯、盯车任务、全局设置与管理员口令。
//!
//! 每次改动立即写库（一次事务写完一个用户的完整状态），进程被杀也不会丢数据。
use std::collections::HashMap;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

use super::store::{
    AlertSettings, Favorite, GlobalSettings, Habit, Pending, UserState, WatchSpec,
};

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS users (
    id                INTEGER PRIMARY KEY,
    display_name      TEXT    NOT NULL DEFAULT '',
    username          TEXT,
    service           TEXT,
    city_label        TEXT,
    queries           INTEGER NOT NULL DEFAULT 0,
    first_seen        INTEGER NOT NULL DEFAULT 0,
    last_seen         INTEGER NOT NULL DEFAULT 0,
    last_lat          REAL,
    last_lng          REAL,
    pending           TEXT    NOT NULL DEFAULT 'None',
    banned            INTEGER NOT NULL DEFAULT 0,
    -- 个人提醒设置；全为 NULL 表示跟随全局默认值
    alert_poll_secs   INTEGER,
    alert_stations    INTEGER,
    alert_distance_m  INTEGER,
    alert_repeat_secs INTEGER,
    alert_max_minutes INTEGER
);

CREATE TABLE IF NOT EXISTS favorites (
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    service      TEXT    NOT NULL,
    direction    TEXT    NOT NULL,
    stop_order   INTEGER NOT NULL,
    line_name    TEXT    NOT NULL,
    station_name TEXT    NOT NULL,
    city_label   TEXT    NOT NULL,
    added_at     INTEGER NOT NULL,
    hits         INTEGER NOT NULL DEFAULT 0,
    last_at      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, service, direction, stop_order)
);

CREATE TABLE IF NOT EXISTS habits (
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key          TEXT    NOT NULL,
    label        TEXT    NOT NULL,
    service      TEXT    NOT NULL,
    line_name    TEXT    NOT NULL,
    direction    TEXT    NOT NULL,
    station_name TEXT    NOT NULL,
    stop_order   INTEGER NOT NULL,
    -- 24 个整数的 JSON 数组，下标即本地小时
    hours        TEXT    NOT NULL,
    total        INTEGER NOT NULL,
    last_at      INTEGER NOT NULL,
    PRIMARY KEY (user_id, key)
);

CREATE TABLE IF NOT EXISTS watches (
    user_id         INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    service         TEXT    NOT NULL,
    city_label      TEXT    NOT NULL,
    line_name       TEXT    NOT NULL,
    direction       TEXT    NOT NULL,
    station_name    TEXT    NOT NULL,
    stop_order      INTEGER NOT NULL,
    chat_id         INTEGER NOT NULL,
    card_message_id INTEGER NOT NULL,
    target_bus      TEXT,
    started_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

pub struct Db {
    conn: Mutex<Connection>,
    path: String,
}

impl Db {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .map_err(|error| anyhow::anyhow!("打开数据库 {path} 失败: {error}"))?;
        // WAL 让读写互不阻塞；busy_timeout 避免偶发的 SQLITE_BUSY
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_string(),
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    // ─── 元数据 ───

    pub fn meta_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta_delete(&self, key: &str) -> anyhow::Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }

    // ─── 全局设置 ───

    pub fn load_settings(&self) -> anyhow::Result<GlobalSettings> {
        let mut settings = GlobalSettings::default();
        if let Some(value) = self.meta_get("defaults")? {
            settings.defaults = serde_json::from_str(&value).unwrap_or_default();
        }
        if let Some(value) = self.meta_get("allow_new_users")? {
            settings.allow_new_users = value != "0";
        }
        settings.defaults = settings.defaults.clamped();
        Ok(settings)
    }

    pub fn save_settings(&self, settings: &GlobalSettings) -> anyhow::Result<()> {
        self.meta_set("defaults", &serde_json::to_string(&settings.defaults)?)?;
        self.meta_set(
            "allow_new_users",
            if settings.allow_new_users { "1" } else { "0" },
        )
    }

    // ─── 用户 ───

    pub fn load_users(&self) -> anyhow::Result<HashMap<i64, UserState>> {
        let conn = self.conn.lock();
        let mut users: HashMap<i64, UserState> = HashMap::new();

        let mut statement = conn.prepare(
            "SELECT id, display_name, username, service, city_label, queries, first_seen,
                    last_seen, last_lat, last_lng, pending, banned,
                    alert_poll_secs, alert_stations, alert_distance_m, alert_repeat_secs,
                    alert_max_minutes
             FROM users",
        )?;
        let rows = statement.query_map([], |row| {
            let last_lat: Option<f64> = row.get(8)?;
            let last_lng: Option<f64> = row.get(9)?;
            // SQLite 只有有符号整数，u64 字段统一按 i64 读出再转换
            let poll: Option<i64> = row.get(12)?;
            let number = |index: usize, fallback: i64| -> i64 {
                row.get::<_, Option<i64>>(index)
                    .ok()
                    .flatten()
                    .unwrap_or(fallback)
                    .max(0)
            };
            let alerts = poll.map(|poll_secs| {
                AlertSettings {
                    poll_secs: poll_secs.max(0) as u64,
                    alert_stations: number(13, 1) as u32,
                    alert_distance_m: number(14, 500) as u32,
                    repeat_secs: number(15, 60) as u64,
                    max_minutes: number(16, 60) as u64,
                }
                .clamped()
            });
            Ok((
                row.get::<_, i64>(0)?,
                UserState {
                    display_name: row.get(1)?,
                    username: row.get(2)?,
                    service: row.get(3)?,
                    city_label: row.get(4)?,
                    queries: row.get(5)?,
                    first_seen: row.get::<_, i64>(6)?.max(0) as u64,
                    last_seen: row.get::<_, i64>(7)?.max(0) as u64,
                    last_location: last_lat.zip(last_lng),
                    pending: Pending::from_str(&row.get::<_, String>(10)?),
                    banned: row.get::<_, i64>(11)? != 0,
                    alerts,
                    favorites: Vec::new(),
                    habits: Vec::new(),
                    watch: None,
                },
            ))
        })?;
        for row in rows {
            let (id, state) = row?;
            users.insert(id, state);
        }

        let mut statement = conn.prepare(
            "SELECT user_id, service, direction, stop_order, line_name, station_name, city_label,
                    added_at, hits, last_at
             FROM favorites ORDER BY added_at",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                Favorite {
                    service: row.get(1)?,
                    direction: row.get(2)?,
                    order: row.get(3)?,
                    line_name: row.get(4)?,
                    station_name: row.get(5)?,
                    city_label: row.get(6)?,
                    added_at: row.get::<_, i64>(7)?.max(0) as u64,
                    hits: row.get(8)?,
                    last_at: row.get::<_, i64>(9)?.max(0) as u64,
                },
            ))
        })?;
        for row in rows {
            let (id, favorite) = row?;
            if let Some(user) = users.get_mut(&id) {
                user.favorites.push(favorite);
            }
        }

        let mut statement = conn.prepare(
            "SELECT user_id, key, label, service, line_name, direction, station_name, stop_order,
                    hours, total, last_at
             FROM habits",
        )?;
        let rows = statement.query_map([], |row| {
            let hours: String = row.get(8)?;
            Ok((
                row.get::<_, i64>(0)?,
                Habit {
                    key: row.get(1)?,
                    label: row.get(2)?,
                    service: row.get(3)?,
                    line_name: row.get(4)?,
                    direction: row.get(5)?,
                    station_name: row.get(6)?,
                    order: row.get(7)?,
                    hours: serde_json::from_str(&hours).unwrap_or_else(|_| vec![0; 24]),
                    total: row.get(9)?,
                    last_at: row.get::<_, i64>(10)?.max(0) as u64,
                },
            ))
        })?;
        for row in rows {
            let (id, habit) = row?;
            if let Some(user) = users.get_mut(&id) {
                user.habits.push(habit);
            }
        }

        let mut statement = conn.prepare(
            "SELECT user_id, service, city_label, line_name, direction, station_name, stop_order,
                    chat_id, card_message_id, target_bus, started_at
             FROM watches",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                WatchSpec {
                    service: row.get(1)?,
                    city_label: row.get(2)?,
                    line_name: row.get(3)?,
                    direction: row.get(4)?,
                    station_name: row.get(5)?,
                    order: row.get(6)?,
                    chat_id: row.get(7)?,
                    card_message_id: row.get(8)?,
                    target_bus: row.get(9)?,
                    started_at: row.get::<_, i64>(10)?.max(0) as u64,
                },
            ))
        })?;
        for row in rows {
            let (id, watch) = row?;
            if let Some(user) = users.get_mut(&id) {
                user.watch = Some(watch);
            }
        }

        Ok(users)
    }

    /// 整体写入一个用户的状态（用户行 + 收藏 + 习惯 + 盯车），一次事务完成。
    pub fn save_user(&self, id: i64, user: &UserState) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let transaction = conn.transaction()?;
        let alerts = user.alerts;
        transaction.execute(
            "INSERT INTO users (id, display_name, username, service, city_label, queries,
                                first_seen, last_seen, last_lat, last_lng, pending, banned,
                                alert_poll_secs, alert_stations, alert_distance_m,
                                alert_repeat_secs, alert_max_minutes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(id) DO UPDATE SET
                display_name = excluded.display_name, username = excluded.username,
                service = excluded.service, city_label = excluded.city_label,
                queries = excluded.queries, first_seen = excluded.first_seen,
                last_seen = excluded.last_seen, last_lat = excluded.last_lat,
                last_lng = excluded.last_lng, pending = excluded.pending,
                banned = excluded.banned,
                alert_poll_secs = excluded.alert_poll_secs,
                alert_stations = excluded.alert_stations,
                alert_distance_m = excluded.alert_distance_m,
                alert_repeat_secs = excluded.alert_repeat_secs,
                alert_max_minutes = excluded.alert_max_minutes",
            params![
                id,
                user.display_name,
                user.username,
                user.service,
                user.city_label,
                user.queries,
                user.first_seen as i64,
                user.last_seen as i64,
                user.last_location.map(|(lat, _)| lat),
                user.last_location.map(|(_, lng)| lng),
                user.pending.as_str(),
                user.banned as i64,
                alerts.map(|a| a.poll_secs as i64),
                alerts.map(|a| a.alert_stations),
                alerts.map(|a| a.alert_distance_m),
                alerts.map(|a| a.repeat_secs as i64),
                alerts.map(|a| a.max_minutes as i64),
            ],
        )?;

        transaction.execute("DELETE FROM favorites WHERE user_id = ?1", [id])?;
        for favorite in &user.favorites {
            transaction.execute(
                "INSERT INTO favorites (user_id, service, direction, stop_order, line_name,
                                        station_name, city_label, added_at, hits, last_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    id,
                    favorite.service,
                    favorite.direction,
                    favorite.order,
                    favorite.line_name,
                    favorite.station_name,
                    favorite.city_label,
                    favorite.added_at as i64,
                    favorite.hits,
                    favorite.last_at as i64,
                ],
            )?;
        }

        transaction.execute("DELETE FROM habits WHERE user_id = ?1", [id])?;
        for habit in &user.habits {
            transaction.execute(
                "INSERT INTO habits (user_id, key, label, service, line_name, direction,
                                     station_name, stop_order, hours, total, last_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    id,
                    habit.key,
                    habit.label,
                    habit.service,
                    habit.line_name,
                    habit.direction,
                    habit.station_name,
                    habit.order,
                    serde_json::to_string(&habit.hours)?,
                    habit.total,
                    habit.last_at as i64,
                ],
            )?;
        }

        transaction.execute("DELETE FROM watches WHERE user_id = ?1", [id])?;
        if let Some(watch) = &user.watch {
            transaction.execute(
                "INSERT INTO watches (user_id, service, city_label, line_name, direction,
                                      station_name, stop_order, chat_id, card_message_id,
                                      target_bus, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    id,
                    watch.service,
                    watch.city_label,
                    watch.line_name,
                    watch.direction,
                    watch.station_name,
                    watch.order,
                    watch.chat_id,
                    watch.card_message_id,
                    watch.target_bus,
                    watch.started_at as i64,
                ],
            )?;
        }

        transaction.commit()?;
        Ok(())
    }

    pub fn delete_user(&self, id: i64) -> anyhow::Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM users WHERE id = ?1", [id])?;
        Ok(())
    }

    /// 把旧版本的 JSON 数据文件导入数据库，只在数据库还没有用户时执行一次。
    pub fn import_legacy_json(&self, path: &std::path::Path) -> anyhow::Result<usize> {
        #[derive(serde::Deserialize, Default)]
        struct Legacy {
            #[serde(default)]
            users: HashMap<i64, UserState>,
            #[serde(default)]
            settings: GlobalSettings,
        }
        let bytes = std::fs::read(path)?;
        let legacy: Legacy = serde_json::from_slice(&bytes)?;
        for (id, user) in &legacy.users {
            self.save_user(*id, user)?;
        }
        self.save_settings(&legacy.settings)?;
        Ok(legacy.users.len())
    }
}
