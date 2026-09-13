//! 盯车：按设定间隔轮询实时数据，原地更新卡片消息，并在目标车接近时主动推送提醒。
//!
//! 判断逻辑（`locate` / `decide_alert`）是纯函数，单独测试；异步循环只负责
//! 取数据、改消息、发提醒。沿用仓库底线：没有车辆编号的综合预估会被明确标注，
//! 不会假装成某一辆车。
use std::{sync::Arc, time::Duration};

use crate::bot::app::App;
use crate::bot::render;
use crate::bot::store::{AlertSettings, WatchSpec, now_secs};
use crate::models::{BusPosition, LineStop, RealTimeData};

/// 连续多少轮取不到数据就结束盯车，避免无意义地一直打上游。
const MAX_CONSECUTIVE_ERRORS: u32 = 6;

/// 目标车当前的位置快照。
#[derive(Debug, Clone, PartialEq)]
pub struct Approach {
    /// 车辆标识，或「综合预估」
    pub label: String,
    /// 车辆位置描述（综合预估没有位置）
    pub location: Option<String>,
    pub stations_away: Option<u32>,
    pub minutes_away: Option<u32>,
    pub distance_m: Option<u32>,
    /// 已经驶过上车站
    pub passed: bool,
    /// 正在进上车站
    pub at_target: bool,
    /// 数据来自没有车辆编号的综合预估
    pub anonymous: bool,
}

/// 找到要等的那辆车；返回 None 表示上游暂时没有它的数据。
pub fn locate(
    realtime: &RealTimeData,
    stops: &[LineStop],
    order: u32,
    target_bus: Option<&str>,
) -> Option<Approach> {
    let describe = |index: usize| -> Approach {
        let bus = &realtime.buses[index];
        let view = render::describe_bus(bus, index, stops, order);
        let (stations_away, passed, at_target) = progress(bus.station_index, bus.is_arriving, stops, order);
        Approach {
            label: view.identity,
            location: Some(view.location),
            stations_away,
            minutes_away: bus.travel_time_secs.map(|secs| secs.div_ceil(60)),
            distance_m: distance_to_target(bus, stops, order),
            passed,
            at_target,
            anonymous: false,
        }
    };

    if let Some(target) = target_bus {
        let index = realtime
            .buses
            .iter()
            .position(|bus| bus.bus_id == target)?;
        return Some(describe(index));
    }

    // 自动模式：取还没驶过上车站、且离得最近的一辆
    let nearest = realtime
        .buses
        .iter()
        .enumerate()
        .filter(|(_, bus)| {
            let (_, passed, _) = progress(bus.station_index, bus.is_arriving, stops, order);
            !passed
        })
        .min_by_key(|(_, bus)| proximity_key(bus, stops, order))
        .map(|(index, _)| index);

    if let Some(index) = nearest {
        return Some(describe(index));
    }

    // 上游只给了不带车辆编号的综合预估时，如实标注来源
    realtime
        .arrival_estimates
        .iter()
        .min_by_key(|estimate| (estimate.stations_away, estimate.minutes_away))
        .map(|estimate| Approach {
            label: "综合预估（上游未提供车辆编号）".to_string(),
            location: None,
            stations_away: Some(estimate.stations_away),
            minutes_away: Some(estimate.minutes_away),
            distance_m: Some(estimate.distance_m),
            passed: false,
            at_target: estimate.stations_away == 0,
            anonymous: true,
        })
}

/// 车辆到**上车站**的直线距离（米）。
///
/// 上游给的 `distance_to_station` 语义并不统一：实测掌上公交返回的是车辆到
/// 下一站的距离，二十多站开外的车也只有两三百米，直接拿来做「500 米内提醒」
/// 会对任何一辆车立刻触发。所以这里用车辆与上车站的经纬度自己算；
/// 缺少坐标时返回 None，宁可不提醒也不报一个错的距离。
pub fn distance_to_target(bus: &BusPosition, stops: &[LineStop], order: u32) -> Option<u32> {
    let stop = stops.iter().find(|stop| stop.order == order)?;
    let (lat, lng) = (bus.lat?, bus.lng?);
    if !valid_point(lat, lng) || !valid_point(stop.lat, stop.lng) {
        return None;
    }
    Some(crate::support::coord::haversine_distance_m(lat, lng, stop.lat, stop.lng).round() as u32)
}

/// 0/0 与超范围的坐标都是上游缺数据时的占位值。
fn valid_point(lat: f64, lng: f64) -> bool {
    lat.is_finite()
        && lng.is_finite()
        && (lat != 0.0 || lng != 0.0)
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lng)
}

/// 由近到远的排序键：先看还差几站，再看上游给的预计时间与直线距离。
/// 位置在线路上对不上的车（站序匹配不到）排在最后。
///
/// 列表展示与「最近的一班」的自动跟随共用这个标准，
/// 这样列表里的第一辆就是自动模式会跟的那辆。
pub fn proximity_key(bus: &BusPosition, stops: &[LineStop], order: u32) -> (u32, u32, u32) {
    let (stations_away, _, _) = progress(bus.station_index, bus.is_arriving, stops, order);
    (
        stations_away.unwrap_or(u32::MAX),
        bus.travel_time_secs.unwrap_or(u32::MAX),
        distance_to_target(bus, stops, order).unwrap_or(u32::MAX),
    )
}

/// 车辆相对上车站的站数差与状态。
fn progress(
    station_index: u32,
    is_arriving: bool,
    stops: &[LineStop],
    order: u32,
) -> (Option<u32>, bool, bool) {
    let position = stops.iter().position(|stop| stop.order == station_index);
    let target = stops.iter().position(|stop| stop.order == order);
    match (position, target) {
        (Some(position), Some(target)) => {
            let passed = position > target || (position == target && !is_arriving);
            let at_target = position == target && is_arriving;
            let stations_away = target.checked_sub(position).map(|gap| gap as u32);
            (stations_away, passed, at_target)
        }
        _ => (None, false, false),
    }
}

/// 已经发过哪些提醒。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AlertState {
    pub stations_alerted: bool,
    pub last_distance_alert: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Alert {
    /// 车到了设定的站数阈值（默认前一站），只提醒一次
    Stations(u32),
    /// 车进入设定的距离阈值，按重复间隔反复提醒
    Distance(u32),
    /// 车正在进站
    Arriving,
    /// 车已驶过上车站
    Passed,
}

impl Alert {
    /// 是否应当结束本次盯车。
    pub fn is_final(self) -> bool {
        matches!(self, Self::Arriving | Self::Passed)
    }
}

/// 这一轮是否需要推送提醒。纯函数，便于单测。
pub fn decide_alert(
    approach: &Approach,
    alerts: &AlertSettings,
    state: &AlertState,
    now: u64,
) -> Option<Alert> {
    if approach.at_target {
        return Some(Alert::Arriving);
    }
    if approach.passed {
        return Some(Alert::Passed);
    }
    // 站数提醒只发一次，之后交给距离提醒重复播报
    if alerts.alert_stations > 0
        && !state.stations_alerted
        && approach
            .stations_away
            .is_some_and(|stations| stations <= alerts.alert_stations)
    {
        return Some(Alert::Stations(approach.stations_away.unwrap_or(0)));
    }
    if alerts.alert_distance_m > 0
        && approach
            .distance_m
            .is_some_and(|distance| distance <= alerts.alert_distance_m)
        && now.saturating_sub(state.last_distance_alert) >= alerts.repeat_secs
    {
        return Some(Alert::Distance(approach.distance_m.unwrap_or(0)));
    }
    None
}

/// 提醒消息文案。
pub fn alert_text(alert: Alert, spec: &WatchSpec, approach: &Approach) -> String {
    let line = render::escape(&spec.line_name);
    let station = render::escape(&spec.station_name);
    let who = render::escape(&approach.label);
    let detail = {
        let mut parts = Vec::new();
        if let Some(stations) = approach.stations_away {
            parts.push(format!("还有 {stations} 站"));
        }
        if let Some(minutes) = approach.minutes_away {
            parts.push(format!("约 {minutes} 分钟"));
        }
        if let Some(distance) = approach.distance_m {
            parts.push(format!("直线 {distance} 米"));
        }
        parts.join(" · ")
    };
    match alert {
        Alert::Stations(stations) => format!(
            "🔔 <b>{line}</b> 还有 {stations} 站到「{station}」\n{who}\n{detail}\n\n准备上车。"
        ),
        Alert::Distance(distance) => format!(
            "🔔 <b>{line}</b> 距离「{station}」直线只剩 {distance} 米\n{who}\n{detail}"
        ),
        Alert::Arriving => {
            format!("🚏 <b>{line}</b> 正在进站「{station}」\n{who}\n\n盯车结束。")
        }
        Alert::Passed => format!(
            "⚪️ <b>{line}</b> 已驶过「{station}」\n{who}\n\n盯车结束，需要的话再开一次。"
        ),
    }
}

/// 盯车主循环。被 [`App::stop_watch`] abort 时直接退出，善后由调用方完成。
pub async fn run(app: Arc<App>, user_id: i64, spec: WatchSpec) {
    let mut state = AlertState::default();
    let mut errors = 0u32;

    loop {
        // 每轮重新读取设置，用户在小程序或机器人里改动会立即生效
        let alerts = app.store().alerts_for(user_id);
        let user = app.store().get(user_id);
        if user.banned {
            app.finish_watch(user_id, "账号已被停用，盯车结束。").await;
            return;
        }
        if now_secs().saturating_sub(spec.started_at) >= alerts.max_minutes * 60 {
            app.finish_watch(user_id, "⏱ 已达到最长盯车时间，盯车结束。")
                .await;
            return;
        }

        match app.realtime_snapshot(&spec.service, &spec.direction, spec.order).await {
            Ok((detail, realtime)) => {
                errors = 0;
                let approach = locate(
                    &realtime,
                    &detail.topology.stations,
                    spec.order,
                    spec.target_bus.as_deref(),
                );
                let card = render::watch_card(
                    &spec,
                    approach.as_ref(),
                    &alerts,
                    realtime.run_state,
                    &app.clock_secs(),
                );
                app.edit(spec.chat_id, spec.card_message_id, &card, Some(app.watch_keyboard(user_id)))
                    .await;

                if let Some(approach) = approach
                    && let Some(alert) = decide_alert(&approach, &alerts, &state, now_secs())
                {
                    let text = alert_text(alert, &spec, &approach);
                    app.notify(spec.chat_id, &text).await;
                    match alert {
                        Alert::Stations(_) => state.stations_alerted = true,
                        Alert::Distance(_) => state.last_distance_alert = now_secs(),
                        _ => {}
                    }
                    if alert.is_final() {
                        app.finish_watch(user_id, "盯车已结束。").await;
                        return;
                    }
                }
            }
            Err(error) => {
                errors += 1;
                if errors >= MAX_CONSECUTIVE_ERRORS {
                    app.finish_watch(
                        user_id,
                        &format!("连续 {errors} 次获取实时数据失败，盯车已停止。\n{error}"),
                    )
                    .await;
                    return;
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(alerts.poll_secs.max(1))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ArrivalDetail, CrowdLevel, RunState, StopStatus};

    fn stops() -> Vec<LineStop> {
        (1..=6)
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

    fn bus(id: &str, station_index: u32, is_arriving: bool, distance: Option<f64>) -> BusPosition {
        BusPosition {
            bus_id: id.into(),
            station_index,
            is_arriving,
            lat: None,
            lng: None,
            angle: None,
            distance_to_station: distance,
            travel_time_secs: Some(180),
            station_name: None,
            crowd_status: CrowdLevel::Unknown,
            track_segment_index: None,
            state_description: None,
        }
    }

    fn realtime(buses: Vec<BusPosition>, estimates: Vec<ArrivalDetail>) -> RealTimeData {
        RealTimeData {
            run_state: RunState::Running,
            plan_time: None,
            buses,
            station_arrivals: vec![],
            segments: vec![],
            arrival_estimates: estimates,
        }
    }

    #[test]
    fn named_target_is_tracked_even_when_another_bus_is_closer() {
        let data = realtime(
            vec![bus("A", 4, false, Some(300.0)), bus("B", 2, false, Some(2400.0))],
            vec![],
        );
        let found = locate(&data, &stops(), 5, Some("B")).unwrap();
        assert_eq!(found.label, "车辆 B");
        assert_eq!(found.stations_away, Some(3));
        // 没有坐标就没有距离：上游那个字段是到下一站的，不能当成到上车站
        assert_eq!(found.distance_m, None);
        assert!(!found.passed);
        // 指定的车还没出现在实时数据里
        assert!(locate(&data, &stops(), 5, Some("C")).is_none());
    }

    #[test]
    fn auto_mode_picks_the_nearest_bus_that_has_not_passed() {
        let data = realtime(
            vec![
                bus("已过站", 6, false, Some(100.0)),
                bus("远", 1, false, Some(3000.0)),
                bus("近", 3, false, Some(900.0)),
            ],
            vec![],
        );
        let found = locate(&data, &stops(), 5, None).unwrap();
        assert_eq!(found.label, "车辆 近");
        assert_eq!(found.stations_away, Some(2));
    }

    #[test]
    fn distance_is_measured_to_the_boarding_stop() {
        let mut stops = stops();
        // 上车站放在一个具体坐标上
        stops[4].lat = 24.8740;
        stops[4].lng = 118.6760;

        let mut near = bus("近", 4, false, Some(9999.0));
        near.lat = Some(24.8745);
        near.lng = Some(118.6762);
        let distance = distance_to_target(&near, &stops, 5).unwrap();
        // 约 60 米，与上游给的 9999 无关
        assert!((40..90).contains(&distance), "实际 {distance} 米");

        // 缺坐标、或坐标是 0/0 占位值时不猜距离
        let mut blank = bus("无坐标", 4, false, Some(120.0));
        blank.lat = None;
        blank.lng = None;
        assert_eq!(distance_to_target(&blank, &stops, 5), None);
        let mut zero = bus("零坐标", 4, false, Some(120.0));
        zero.lat = Some(0.0);
        zero.lng = Some(0.0);
        assert_eq!(distance_to_target(&zero, &stops, 5), None);
        // 站点没有坐标时同样不猜
        assert_eq!(distance_to_target(&near, &stops, 1), None);
    }

    #[test]
    fn buses_sort_from_nearest_to_farthest() {
        let stops = stops();
        let mut buses = vec![
            bus("远", 1, false, Some(3000.0)),
            bus("位置对不上", 99, false, Some(0.0)),
            bus("近", 4, false, Some(200.0)),
            bus("中", 3, false, Some(900.0)),
        ];
        buses.sort_by_key(|bus| proximity_key(bus, &stops, 5));
        let order: Vec<&str> = buses.iter().map(|bus| bus.bus_id.as_str()).collect();
        // 站序对不上的排最后，不会因为上游给的 0 米而排到最前
        assert_eq!(order, vec!["近", "中", "远", "位置对不上"]);

        // 站数相同时看预计时间，再看距离
        let mut same_stop = vec![
            BusPosition { travel_time_secs: Some(600), ..bus("慢", 3, false, Some(800.0)) },
            BusPosition { travel_time_secs: Some(120), ..bus("快", 3, false, Some(900.0)) },
        ];
        same_stop.sort_by_key(|bus| proximity_key(bus, &stops, 5));
        assert_eq!(same_stop[0].bus_id, "快");
    }

    #[test]
    fn anonymous_estimates_are_labelled_as_such() {
        let data = realtime(
            vec![],
            vec![
                ArrivalDetail { stations_away: 4, minutes_away: 9, distance_m: 2000 },
                ArrivalDetail { stations_away: 1, minutes_away: 3, distance_m: 600 },
            ],
        );
        let found = locate(&data, &stops(), 5, None).unwrap();
        assert!(found.anonymous);
        assert!(found.label.contains("未提供车辆编号"));
        assert_eq!(found.stations_away, Some(1));
        assert_eq!(found.location, None);
        // 完全没有数据时不编造
        assert!(locate(&realtime(vec![], vec![]), &stops(), 5, None).is_none());
    }

    fn approach(stations: Option<u32>, distance: Option<u32>) -> Approach {
        Approach {
            label: "车辆 A".into(),
            location: None,
            stations_away: stations,
            minutes_away: None,
            distance_m: distance,
            passed: false,
            at_target: false,
            anonymous: false,
        }
    }

    #[test]
    fn station_alert_fires_once_then_distance_alert_repeats() {
        let alerts = AlertSettings::default(); // 前 1 站 / 500 米 / 重复 60 秒
        let mut state = AlertState::default();

        // 还有 3 站：不提醒
        assert_eq!(decide_alert(&approach(Some(3), Some(1800)), &alerts, &state, 100), None);

        // 到前一站：提醒一次
        assert_eq!(
            decide_alert(&approach(Some(1), Some(1200)), &alerts, &state, 100),
            Some(Alert::Stations(1))
        );
        state.stations_alerted = true;
        assert_eq!(decide_alert(&approach(Some(1), Some(1200)), &alerts, &state, 110), None);

        // 进入 500 米：重复提醒，但要等满重复间隔
        assert_eq!(
            decide_alert(&approach(Some(1), Some(400)), &alerts, &state, 200),
            Some(Alert::Distance(400))
        );
        state.last_distance_alert = 200;
        assert_eq!(decide_alert(&approach(Some(1), Some(300)), &alerts, &state, 230), None);
        assert_eq!(
            decide_alert(&approach(Some(1), Some(200)), &alerts, &state, 260),
            Some(Alert::Distance(200))
        );
    }

    #[test]
    fn arriving_and_passed_end_the_watch() {
        let alerts = AlertSettings::default();
        let state = AlertState::default();
        let arriving = Approach { at_target: true, ..approach(Some(0), Some(0)) };
        assert_eq!(decide_alert(&arriving, &alerts, &state, 0), Some(Alert::Arriving));
        assert!(Alert::Arriving.is_final());

        let passed = Approach { passed: true, ..approach(None, None) };
        assert_eq!(decide_alert(&passed, &alerts, &state, 0), Some(Alert::Passed));
        assert!(Alert::Passed.is_final());
        assert!(!Alert::Distance(10).is_final());
    }

    #[test]
    fn thresholds_of_zero_disable_their_alert() {
        let alerts = AlertSettings {
            alert_stations: 0,
            alert_distance_m: 0,
            ..AlertSettings::default()
        };
        let state = AlertState::default();
        assert_eq!(decide_alert(&approach(Some(0), Some(10)), &alerts, &state, 9999), None);
    }

    #[test]
    fn alert_text_mentions_line_station_and_numbers() {
        let spec = WatchSpec {
            service: "hz571801".into(),
            city_label: "杭州 · 掌上公交".into(),
            line_name: "1路".into(),
            direction: "1路:1".into(),
            station_name: "运河天地".into(),
            order: 3,
            chat_id: 42,
            card_message_id: 7,
            target_bus: Some("A".into()),
            started_at: 0,
        };
        let text = alert_text(Alert::Distance(420), &spec, &approach(Some(1), Some(420)));
        assert!(text.contains("1路"));
        assert!(text.contains("运河天地"));
        assert!(text.contains("420 米"));
    }
}
