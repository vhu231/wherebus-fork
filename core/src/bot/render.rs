//! 消息排版：把 provider 返回的数据渲染成 Telegram HTML 文本。
//!
//! 与网页版同一条底线：没有到站预估就直说「暂无」，绝不用数组下标把
//! 无车辆编号的预估硬配给某辆车。
use crate::bot::store::{AlertField, AlertSettings, Habit, UserState, WatchSpec};
use crate::bot::watch::Approach;
use crate::models::{
    ArrivalEstimate, BusPosition, CrowdLevel, Fare, LineDetail, LineStop, RealTimeData, RunState,
    StopStatus,
};

/// Telegram HTML 模式只需转义这三个字符。
pub fn escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn run_state_text(state: RunState) -> &'static str {
    match state {
        RunState::Running => "运营中",
        RunState::NotOperating => "尚未运营",
        RunState::Stopped => "已停运",
        RunState::NoRealtime => "暂无实时数据",
    }
}

pub fn arrival_text(arrival: &ArrivalEstimate) -> String {
    match arrival {
        ArrivalEstimate::Arriving => "即将到站".into(),
        ArrivalEstimate::Approaching {
            stations_away,
            minutes_away,
            distance_m,
        } => {
            let mut parts = vec![format!("{stations_away} 站")];
            if let Some(minutes) = minutes_away {
                parts.push(format!("约 {minutes} 分钟"));
            }
            if let Some(distance) = distance_m {
                parts.push(format!("{distance} 米"));
            }
            parts.join(" · ")
        }
        ArrivalEstimate::NoService => "暂无班次".into(),
        ArrivalEstimate::Unknown => "暂无到站预估".into(),
    }
}

pub fn crowd_text(level: CrowdLevel) -> Option<&'static str> {
    match level {
        CrowdLevel::Spacious => Some("宽敞有座"),
        CrowdLevel::Normal => Some("适中"),
        CrowdLevel::Crowded => Some("拥挤站立"),
        CrowdLevel::Full => Some("满载"),
        CrowdLevel::Unknown => None,
    }
}

pub fn stop_status_note(status: StopStatus) -> Option<&'static str> {
    match status {
        StopStatus::Normal => None,
        StopStatus::NotStopping => Some("不停靠"),
        StopStatus::BoardOnly => Some("仅上客"),
        StopStatus::AlightOnly => Some("仅下客"),
        StopStatus::Temporary => Some("临时站"),
        StopStatus::OnDemand => Some("招呼站"),
        StopStatus::Express => Some("快线跳站"),
    }
}

/// 单辆车的可读描述，逻辑与 `web/bus-view.js` 的 describeBus 保持一致。
#[derive(Debug, Clone)]
pub struct BusView {
    pub identity: String,
    pub location: String,
    pub estimate: String,
    pub passed: bool,
    pub at_target: bool,
    pub description: Option<String>,
}

pub fn describe_bus(
    bus: &BusPosition,
    index: usize,
    stops: &[LineStop],
    target_order: u32,
) -> BusView {
    let position = stops.iter().position(|s| s.order == bus.station_index);
    let target_position = stops.iter().position(|s| s.order == target_order);
    let stop = position.and_then(|i| stops.get(i));
    let next = position.and_then(|i| stops.get(i + 1));
    let target = target_position.and_then(|i| stops.get(i));

    let passed = match (position, target_position) {
        (Some(position), Some(target_position)) => {
            position > target_position || (position == target_position && !bus.is_arriving)
        }
        _ => false,
    };
    let at_target = matches!((position, target_position), (Some(a), Some(b)) if a == b) && bus.is_arriving;

    let identity = if bus.bus_id.trim().is_empty() {
        format!("车辆 {}（上游未提供编号）", index + 1)
    } else {
        format!("车辆 {}", bus.bus_id)
    };

    let location = match stop {
        None => "当前站点未匹配".to_string(),
        Some(stop) if bus.is_arriving => format!("正在进站：{}", stop.name),
        Some(stop) => match next {
            Some(next) => format!("已离开：{} → 开往 {}", stop.name, next.name),
            None => format!("已离开：{}（末站）", stop.name),
        },
    };

    let estimate = if passed {
        "已驶过你的上车站".to_string()
    } else if at_target {
        "正在进入你的上车站".to_string()
    } else if target.is_some() {
        let mut parts = Vec::new();
        if let (Some(position), Some(target_position)) = (position, target_position)
            && target_position > position
        {
            parts.push(format!("站序相差 {} 站", target_position - position));
        }
        if let Some(seconds) = bus.travel_time_secs {
            parts.push(if seconds == 0 {
                "即将到站".to_string()
            } else {
                format!("约 {} 分钟", seconds.div_ceil(60))
            });
        }
        if let Some(distance) = bus.distance_to_station {
            parts.push(format!("距目标 {:.0} 米", distance));
        }
        if parts.is_empty() {
            "暂无该车到站预估".to_string()
        } else {
            parts.join(" · ")
        }
    } else {
        "暂无该车到站预估".to_string()
    };

    BusView {
        identity,
        location,
        estimate,
        passed,
        at_target,
        description: bus
            .state_description
            .clone()
            .filter(|value| !value.trim().is_empty()),
    }
}

/// 线路详情页文本。
pub fn line_detail_text(detail: &LineDetail, city_label: &str) -> String {
    let meta = &detail.meta;
    let time = |value: &Option<crate::models::ServiceTime>| match value {
        Some(value) => value.to_string(),
        None => "—".to_string(),
    };
    let fare = match &meta.fare {
        Fare::Text(value) => value.clone(),
        Fare::Unknown => "暂无信息".to_string(),
    };

    let mut text = format!(
        "🚌 <b>{}</b>\n{} → {}\n<i>{}</i>\n\n首班 {} · 末班 {} · 票价 {}\n共 {} 站",
        escape(&detail.name),
        escape(
            detail
                .topology
                .start
                .as_str()
                .unwrap_or("起点待更新")
        ),
        escape(detail.topology.end.as_str().unwrap_or("终点待更新")),
        escape(city_label),
        time(&meta.first_service),
        time(&meta.last_service),
        escape(&fare),
        detail.topology.stations.len(),
    );
    if let Some(company) = meta.company.as_ref().filter(|v| !v.trim().is_empty()) {
        text.push_str(&format!("\n运营单位：{}", escape(company)));
    }
    if let Some(notes) = meta.notes.as_ref().filter(|v| !v.trim().is_empty()) {
        text.push_str(&format!("\n提示：{}", escape(notes)));
    }
    text.push_str("\n\n选择你的上车站，查看实时到站：");
    text
}

/// 实时到站页文本。
pub fn live_text(
    detail: &LineDetail,
    realtime: &RealTimeData,
    order: u32,
    city_label: &str,
    clock: &str,
    is_favorite: bool,
) -> String {
    let stops = &detail.topology.stations;
    let target = stops.iter().find(|s| s.order == order);
    let target_name = target.map(|s| s.name.as_str()).unwrap_or("未选择");

    let mut text = format!(
        "🚌 <b>{}</b>{}\n上车站：<b>{}</b>（第 {} 站）\n状态：{} · <i>{}</i>\n",
        escape(&detail.name),
        if is_favorite { " ⭐" } else { "" },
        escape(target_name),
        order,
        run_state_text(realtime.run_state),
        escape(city_label),
    );

    if let Some(note) = target.and_then(|s| stop_status_note(s.status)) {
        text.push_str(&format!("站点提示：{note}\n"));
    }
    if let Some(plan) = realtime.plan_time.as_ref().filter(|v| !v.trim().is_empty()) {
        text.push_str(&format!("计划发车：{}\n", escape(plan)));
    }

    text.push('\n');
    if realtime.buses.is_empty() && realtime.arrival_estimates.is_empty() {
        text.push_str("当前没有车辆到站预估。\n");
    }

    if !realtime.buses.is_empty() {
        text.push_str(&format!("<b>在途车辆（{}）</b>\n", realtime.buses.len()));
        for (index, bus) in realtime.buses.iter().enumerate() {
            let view = describe_bus(bus, index, stops, order);
            let marker = if view.at_target {
                "🟢"
            } else if view.passed {
                "⚪️"
            } else {
                "🔵"
            };
            text.push_str(&format!(
                "{marker} {}\n   {}\n   到「{}」：{}\n",
                escape(&view.identity),
                escape(&view.location),
                escape(target_name),
                escape(&view.estimate),
            ));
            if let Some(crowd) = crowd_text(bus.crowd_status) {
                text.push_str(&format!("   车厢：{crowd}\n"));
            }
            if let Some(description) = &view.description {
                text.push_str(&format!("   上游提示：{}\n", escape(description)));
            }
        }
    }

    if !realtime.arrival_estimates.is_empty() {
        text.push_str("\n<b>综合到站预估</b>（上游未关联车辆编号，单独列出）\n");
        for estimate in &realtime.arrival_estimates {
            text.push_str(&format!(
                "· {} 站 · 约 {} 分钟 · {} 米\n",
                estimate.stations_away, estimate.minutes_away, estimate.distance_m
            ));
        }
    }

    text.push_str(&format!("\n更新于 {clock}"));
    text
}

/// 盯车卡片：每轮轮询后原地更新的那条消息。
pub fn watch_card(
    spec: &WatchSpec,
    approach: Option<&Approach>,
    alerts: &AlertSettings,
    run_state: RunState,
    clock: &str,
) -> String {
    let mut text = format!(
        "🔔 <b>盯车中</b> · {}\n上车站：<b>{}</b>（第 {} 站）\n等待：{}\n状态：{} · <i>{}</i>\n\n",
        escape(&spec.line_name),
        escape(&spec.station_name),
        spec.order,
        match &spec.target_bus {
            Some(bus) => format!("车辆 {}", escape(bus)),
            None => "最近的一班".to_string(),
        },
        run_state_text(run_state),
        escape(&spec.city_label),
    );

    match approach {
        None => text.push_str("上游暂时没有这辆车的位置，继续盯着。\n"),
        Some(approach) => {
            text.push_str(&format!("🚌 {}\n", escape(&approach.label)));
            if let Some(location) = &approach.location {
                text.push_str(&format!("   {}\n", escape(location)));
            }
            let mut parts = Vec::new();
            if let Some(stations) = approach.stations_away {
                parts.push(format!("还有 {stations} 站"));
            }
            if let Some(minutes) = approach.minutes_away {
                parts.push(format!("约 {minutes} 分钟"));
            }
            if let Some(distance) = approach.distance_m {
                parts.push(format!("{distance} 米"));
            }
            if parts.is_empty() {
                text.push_str("   上游没有给出到站预估\n");
            } else {
                text.push_str(&format!("   {}\n", parts.join(" · ")));
            }
            if approach.anonymous {
                text.push_str("   （该预估未关联车辆编号，仅供参考）\n");
            }
        }
    }

    text.push_str(&format!(
        "\n提醒规则：还有 {} 站时提醒一次；{} 米内每 {} 秒重复提醒\n每 {} 秒刷新 · 更新于 {}",
        alerts.alert_stations, alerts.alert_distance_m, alerts.repeat_secs, alerts.poll_secs, clock,
    ));
    text
}

/// 提醒设置页文本。
pub fn alert_settings_text(alerts: &AlertSettings, using_defaults: bool) -> String {
    let mut text = format!(
        "⚙️ <b>提醒设置</b>{}\n\n",
        if using_defaults { "（当前跟随全局默认值）" } else { "" }
    );
    for field in AlertField::ALL {
        let value = alerts.get(field);
        let note = match field {
            AlertField::Stations if value == 0 => "（已关闭站数提醒）",
            AlertField::Distance if value == 0 => "（已关闭距离提醒）",
            _ => "",
        };
        text.push_str(&format!(
            "· {}：{} {}{}\n",
            field.label(),
            value,
            field.unit(),
            note
        ));
    }
    text.push_str("\n用下面的 ➖ ➕ 调整，设置对之后的盯车立即生效。");
    text
}

/// 24 小时查询分布的字符柱状图。
pub fn sparkline(values: &[u32]) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return "—".to_string();
    }
    values
        .iter()
        .map(|value| {
            if *value == 0 {
                '·'
            } else {
                let level = ((*value as f64 / max as f64) * (BLOCKS.len() - 1) as f64).round();
                BLOCKS[level as usize]
            }
        })
        .collect()
}

/// 习惯统计页文本。
pub fn habits_text(user: &UserState, hour: u32) -> String {
    if user.habits.is_empty() {
        return "📊 <b>你的乘车习惯</b>\n\n还没有查询记录。查几次实时到站后，这里会显示你的常用线路和高峰时段。".to_string();
    }

    let histogram = user.hour_histogram();
    let mut text = format!(
        "📊 <b>你的乘车习惯</b>\n\n累计查询 {} 次 · 收藏 {} 条 · 记录线路 {} 条\n\n<b>常用线路</b>\n",
        user.queries,
        user.favorites.len(),
        user.habits.len(),
    );

    for (index, habit) in user.top_habits(5).iter().enumerate() {
        text.push_str(&format!(
            "{}. {} — {} 次\n",
            index + 1,
            escape(&habit.label),
            habit.total
        ));
    }

    text.push_str(&format!(
        "\n<b>查询时段分布</b>\n<code>{}</code>\n<code>0     6     12    18   23</code>\n",
        sparkline(&histogram)
    ));

    let busiest = histogram
        .iter()
        .enumerate()
        .max_by_key(|(_, count)| **count)
        .filter(|(_, count)| **count > 0);
    if let Some((peak_hour, count)) = busiest {
        text.push_str(&format!("高峰时段：{peak_hour:02}:00（{count} 次）\n"));
    }

    let suggestions = user.suggestions(hour, 3);
    if suggestions.is_empty() {
        text.push_str(&format!("\n{hour:02} 点这个时段还没有形成规律。"));
    } else {
        text.push_str(&format!("\n<b>{hour:02} 点你通常查</b>\n"));
        for habit in suggestions {
            text.push_str(&format!(
                "· {}（{} 次）\n",
                escape(&habit.label),
                habit.total
            ));
        }
    }
    text
}

/// 收藏按钮文案。
pub fn habit_button_label(habit: &Habit) -> String {
    format!("🕘 {}", habit.label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ArrivalDetail, RouteTopology, Terminal};

    fn stops() -> Vec<LineStop> {
        (1..=5)
            .map(|order| LineStop {
                id: None,
                name: format!("第{order}站"),
                alias: None,
                order,
                lat: 0.0,
                lng: 0.0,
                status: StopStatus::Normal,
                track_index: None,
            })
            .collect()
    }

    fn bus(station_index: u32, is_arriving: bool) -> BusPosition {
        BusPosition {
            bus_id: "粤B12345".into(),
            station_index,
            is_arriving,
            lat: None,
            lng: None,
            angle: None,
            distance_to_station: None,
            travel_time_secs: None,
            station_name: None,
            crowd_status: CrowdLevel::Unknown,
            track_segment_index: None,
            state_description: None,
        }
    }

    #[test]
    fn bus_before_target_reports_station_gap() {
        let view = describe_bus(&bus(2, false), 0, &stops(), 4);
        assert!(!view.passed);
        assert!(view.location.contains("已离开：第2站 → 开往 第3站"));
        assert!(view.estimate.contains("站序相差 2 站"));
    }

    #[test]
    fn bus_at_and_past_target_are_distinguished() {
        let at_target = describe_bus(&bus(4, true), 0, &stops(), 4);
        assert!(at_target.at_target);
        assert_eq!(at_target.estimate, "正在进入你的上车站");

        let left_target = describe_bus(&bus(4, false), 0, &stops(), 4);
        assert!(left_target.passed);
        assert_eq!(left_target.estimate, "已驶过你的上车站");

        let past = describe_bus(&bus(5, true), 0, &stops(), 4);
        assert!(past.passed);
    }

    #[test]
    fn missing_bus_id_is_labelled_not_guessed() {
        let mut raw = bus(2, false);
        raw.bus_id = String::new();
        let view = describe_bus(&raw, 3, &stops(), 4);
        assert_eq!(view.identity, "车辆 4（上游未提供编号）");
    }

    #[test]
    fn live_text_says_when_there_is_nothing_to_show() {
        let detail = LineDetail {
            id: None,
            name: "M375".into(),
            direction_id: "M375:1".into(),
            reverse_id: None,
            topology: RouteTopology {
                start: Terminal::named("起点"),
                end: Terminal::named("终点"),
                stations: stops(),
                track_points: vec![],
            },
            meta: Default::default(),
        };
        let realtime = RealTimeData {
            run_state: RunState::NoRealtime,
            plan_time: None,
            buses: vec![],
            station_arrivals: vec![],
            segments: vec![],
            arrival_estimates: vec![],
        };
        let text = live_text(&detail, &realtime, 3, "深圳 · 掌上公交", "08:30", true);
        assert!(text.contains("当前没有车辆到站预估"));
        assert!(text.contains("暂无实时数据"));
        assert!(text.contains("⭐"));

        let with_estimates = RealTimeData {
            arrival_estimates: vec![ArrivalDetail {
                stations_away: 2,
                minutes_away: 5,
                distance_m: 900,
            }],
            ..realtime
        };
        let text = live_text(&detail, &with_estimates, 3, "深圳", "08:30", false);
        assert!(text.contains("上游未关联车辆编号"));
        assert!(!text.contains("当前没有车辆到站预估"));
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(escape("A<b>&C"), "A&lt;b&gt;&amp;C");
    }

    #[test]
    fn sparkline_handles_empty_history() {
        assert_eq!(sparkline(&[0; 24]), "—");
        let mut values = [0u32; 24];
        values[8] = 10;
        values[9] = 5;
        let line = sparkline(&values);
        assert_eq!(line.chars().count(), 24);
        assert_eq!(line.chars().nth(8), Some('█'));
        assert_eq!(line.chars().nth(0), Some('·'));
    }
}
