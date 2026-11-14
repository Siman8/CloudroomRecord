#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::Command,
    sync::{mpsc, Mutex},
    thread,
    time::Duration as StdDuration,
};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State,
};
use tauri_plugin_autostart::ManagerExt as AutoStartManagerExt;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const OVERNIGHT_CUTOFF: (u32, u32) = (2, 0);
const OVERTIME_BOUNDARY_GRACE_MINUTES: i64 = 3;
const MIN_OVERTIME_SESSION_MINUTES: i64 = 30;
const EARLY_DEPARTURE_GRACE_MINUTES: i64 = 10;
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_REPOSITORY: &str = "Siman8/CloudroomRecord";

struct AppState {
    db: Mutex<Connection>,
    notified: Mutex<HashSet<String>>,
    first_open_date: NaiveDate,
}

#[derive(Clone, Debug, Serialize)]
struct AttendanceSettings {
    work_start: NaiveTime,
    work_end: NaiveTime,
    flexible_enabled: bool,
    flexible_minutes: i64,
    notify_at_end: bool,
    notify_first_unlock: bool,
}

impl Default for AttendanceSettings {
    fn default() -> Self {
        Self {
            work_start: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            work_end: NaiveTime::from_hms_opt(18, 0, 0).unwrap(),
            flexible_enabled: true,
            flexible_minutes: 30,
            notify_at_end: true,
            notify_first_unlock: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RawEvent {
    timestamp: String,
    kind: String,
    source: String,
    event_id: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DailyRecord {
    day: String,
    day_type: String,
    #[serde(default)]
    calendar_name: Option<String>,
    #[serde(default)]
    calendar_marker: Option<String>,
    start_time: Option<String>,
    expected_end: Option<String>,
    end_time: Option<String>,
    overtime_minutes: i64,
    status: String,
    manual: bool,
    note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CalendarInfo {
    last_sync: Option<String>,
    cached_years: Vec<i32>,
    message: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CalendarApi {
    code: i32,
    holiday: std::collections::HashMap<String, CalendarApiDay>,
}

#[derive(Serialize, Deserialize)]
struct CalendarApiDay {
    holiday: bool,
    name: Option<String>,
    date: Option<String>,
}

fn data_file(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("attendance.sqlite3"))
}

fn open_db(path: &PathBuf) -> Result<Connection, String> {
    let db = Connection::open(path).map_err(|e| e.to_string())?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS daily_records(day TEXT PRIMARY KEY, data TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS raw_events(event_key TEXT PRIMARY KEY, day TEXT NOT NULL, data TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS idx_events_day ON raw_events(day);
         CREATE TABLE IF NOT EXISTS calendar_days(day TEXT PRIMARY KEY, day_type TEXT NOT NULL, name TEXT NOT NULL, manual INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS company_workdays(month TEXT PRIMARY KEY, day TEXT NOT NULL UNIQUE);
         CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    ).map_err(|e| e.to_string())?;
    Ok(db)
}

fn parse_date(day: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|_| "日期格式应为 YYYY-MM-DD".to_string())
}

fn parse_clock_time(value: &str) -> Result<NaiveTime, String> {
    let value = value.trim();
    NaiveTime::parse_from_str(value, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(value, "%H:%M"))
        .map_err(|_| format!("时间格式无效：收到“{value}”，应为 HH:MM"))
}

fn read_attendance_settings(db: &Connection) -> Result<AttendanceSettings, String> {
    let read = |key: &str| -> Result<Option<String>, String> {
        db.query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())
    };
    let defaults = AttendanceSettings::default();
    let work_start = read("work_start")?
        .and_then(|value| NaiveTime::parse_from_str(&value, "%H:%M").ok())
        .unwrap_or(defaults.work_start);
    let work_end = read("work_end")?
        .and_then(|value| NaiveTime::parse_from_str(&value, "%H:%M").ok())
        .unwrap_or(defaults.work_end);
    let flexible_enabled = read("flexible_enabled")?
        .map(|value| value == "1")
        .unwrap_or(defaults.flexible_enabled);
    let flexible_minutes = read("flexible_minutes")?
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| (0..=180).contains(value))
        .unwrap_or(defaults.flexible_minutes);
    let notify_at_end = read("notify_at_end")?
        .map(|value| value == "1")
        .unwrap_or(defaults.notify_at_end);
    let notify_first_unlock = read("notify_first_unlock")?
        .map(|value| value == "1")
        .unwrap_or(defaults.notify_first_unlock);
    if work_end <= work_start {
        return Err("下班时间必须晚于上班时间".to_string());
    }
    Ok(AttendanceSettings {
        work_start,
        work_end,
        flexible_enabled,
        flexible_minutes,
        notify_at_end,
        notify_first_unlock,
    })
}

fn cutoff(day: NaiveDate) -> NaiveDateTime {
    day.succ_opt()
        .unwrap_or(day)
        .and_hms_opt(OVERNIGHT_CUTOFF.0, OVERNIGHT_CUTOFF.1, 0)
        .unwrap()
}

fn default_day_type(day: NaiveDate) -> &'static str {
    match day.weekday() {
        chrono::Weekday::Sat if day.day() <= 7 => "workday",
        chrono::Weekday::Sat | chrono::Weekday::Sun => "restday",
        _ => "workday",
    }
}

fn first_saturday(day: NaiveDate) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(day.year(), day.month(), 1).unwrap();
    let days_until_saturday =
        (5_i64 - i64::from(first.weekday().num_days_from_monday())).rem_euclid(7);
    first + Duration::days(days_until_saturday)
}

fn company_workday_date(db: &Connection, day: NaiveDate) -> Result<NaiveDate, String> {
    let first = first_saturday(day);
    let month_key = format!("{:04}-{:02}", day.year(), day.month());
    let manual_company_day: Option<String> = db
        .query_row(
            "SELECT day FROM company_workdays WHERE month=?1",
            [&month_key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some(value) = manual_company_day {
        return parse_date(&value);
    }
    let calendar_entry: Option<(String, bool)> = db
        .query_row(
            "SELECT day_type,manual FROM calendar_days WHERE day=?1",
            [first.to_string()],
            |row| Ok((row.get(0)?, row.get::<_, i32>(1)? != 0)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if !matches!(calendar_entry, Some((ref kind, false)) if kind == "restday") {
        return Ok(first);
    }

    let mut candidate = first + Duration::days(7);
    loop {
        let special_date: Option<bool> = db
            .query_row(
                "SELECT manual FROM calendar_days WHERE day=?1",
                [candidate.to_string()],
                |row| row.get::<_, i32>(0).map(|manual| manual != 0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        match special_date {
            Some(false) => candidate += Duration::days(7),
            _ => return Ok(candidate),
        }
    }
}

fn day_type(db: &Connection, day: NaiveDate) -> Result<String, String> {
    let override_value: Option<String> = db
        .query_row(
            "SELECT day_type FROM calendar_days WHERE day=?1 AND manual=1",
            [day.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some(value) = override_value {
        return Ok(value);
    }
    if company_workday_date(db, day)? == day {
        return Ok("workday".into());
    }
    let cached: Option<String> = db
        .query_row(
            "SELECT day_type FROM calendar_days WHERE day=?1",
            [day.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(cached.unwrap_or_else(|| default_day_type(day).to_string()))
}

fn calendar_metadata(
    db: &Connection,
    day: NaiveDate,
) -> Result<(Option<String>, Option<String>), String> {
    let entry: Option<(String, String, bool)> = db
        .query_row(
            "SELECT day_type,name,manual FROM calendar_days WHERE day=?1",
            [day.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i32>(2)? != 0)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if matches!(entry.as_ref(), Some((_, _, true))) {
        return Ok((Some("手动调整".into()), Some("调".into())));
    }
    let first = first_saturday(day);
    if company_workday_date(db, day)? == day {
        let month_key = format!("{:04}-{:02}", day.year(), day.month());
        let explicitly_selected: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM company_workdays WHERE month=?1 AND day=?2)",
                params![month_key, day.to_string()],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let shifted = day != first;
        if let Some((kind, name, false)) = entry.as_ref() {
            if kind == "workday" && !name.is_empty() {
                return Ok((Some(format!("{name} · 公司上班")), Some("班".into())));
            }
            if kind == "restday" && !name.is_empty() {
                return Ok((Some(format!("{name} · 公司顺延上班")), Some("公司".into())));
            }
        }
        return Ok((
            Some(if explicitly_selected {
                "手动指定公司上班日".into()
            } else if shifted {
                "公司顺延上班".into()
            } else {
                "公司固定上班".into()
            }),
            Some("公司".into()),
        ));
    }
    if let Some((kind, name, false)) = entry {
        let marker = if kind == "restday" { "休" } else { "班" };
        return Ok(((!name.is_empty()).then_some(name), Some(marker.into())));
    }
    Ok((None, None))
}

fn load_events(db: &Connection, day: NaiveDate) -> Result<Vec<RawEvent>, String> {
    let mut statement = db
        .prepare("SELECT data FROM raw_events WHERE day=?1 ORDER BY event_key")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([day.to_string()], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    rows.map(|row| {
        serde_json::from_str(&row.map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    })
    .collect()
}

fn calculate_day_with_settings(
    day: NaiveDate,
    kind: &str,
    events: &[RawEvent],
    now: NaiveDateTime,
    settings: &AttendanceSettings,
) -> DailyRecord {
    let mut instants: Vec<(NaiveDateTime, &str)> = events
        .iter()
        .filter_map(|event| {
            let parsed = chrono::DateTime::parse_from_rfc3339(&event.timestamp)
                .ok()
                .map(|v| v.naive_local())
                .or_else(|| {
                    NaiveDateTime::parse_from_str(&event.timestamp, "%Y-%m-%dT%H:%M:%S").ok()
                });
            parsed.map(|time| (time, event.kind.as_str()))
        })
        .filter(|(time, _)| *time >= day.and_hms_opt(0, 0, 0).unwrap() && *time <= cutoff(day))
        .collect();
    instants.sort_by_key(|v| v.0);

    let is_unlock = |kind: &&str| *kind == "unlock" || *kind == "startup";
    let is_lock = |kind: &&str| *kind == "lock" || *kind == "shutdown";
    let start = instants
        .iter()
        .find(|(_, kind)| is_unlock(kind))
        .map(|(time, _)| *time);
    let end = instants
        .iter()
        .rev()
        .find(|(_, kind)| is_lock(kind))
        .map(|(time, _)| *time);
    let completed_end = end.filter(|end| {
        !instants
            .iter()
            .any(|(time, kind)| is_unlock(kind) && time > end)
    });
    let reached_cutoff = now >= cutoff(day);
    let workday = kind == "workday";
    let mut unlock_session = None;
    let mut qualified_overtime_end = None;
    for (time, event_kind) in &instants {
        if is_unlock(event_kind) {
            unlock_session.get_or_insert(*time);
        } else if is_lock(event_kind) {
            if let Some(session_start) = unlock_session.take() {
                if (*time - session_start).num_minutes() >= MIN_OVERTIME_SESSION_MINUTES {
                    qualified_overtime_end = Some(*time);
                }
            }
        }
    }
    if reached_cutoff {
        if let Some(session_start) = unlock_session {
            let cutoff_time = cutoff(day);
            if (cutoff_time - session_start).num_minutes() >= MIN_OVERTIME_SESSION_MINUTES {
                qualified_overtime_end = Some(cutoff_time);
            }
        }
    }
    let late_minutes = start
        .map(|time| (time.time() - settings.work_start).num_minutes().max(0))
        .unwrap_or(0);
    let expected = workday.then_some(start).flatten().map(|_| {
        day.and_time(settings.work_end)
            + Duration::minutes(if settings.flexible_enabled {
                late_minutes.min(settings.flexible_minutes)
            } else {
                0
            })
    });
    let late_outside_flex = if settings.flexible_enabled {
        late_minutes > settings.flexible_minutes
    } else {
        late_minutes > 0
    };
    let day_is_complete = day < now.date() || reached_cutoff;
    let status = if start.is_none() {
        if workday && day_is_complete {
            "无打卡记录"
        } else if day == now.date() {
            "待打卡"
        } else {
            "待补录"
        }
    } else if workday && late_outside_flex {
        "迟到"
    } else if completed_end.is_some() {
        if workday
            && completed_end.zip(expected).is_some_and(|(end, expected)| {
                let ended_early = end.time() <= settings.work_end
                    || (end < expected
                        && (expected - end).num_minutes() > EARLY_DEPARTURE_GRACE_MINUTES);
                ended_early
            })
        {
            "提前下班"
        } else {
            "正常"
        }
    } else if reached_cutoff {
        "整夜未锁"
    } else {
        "进行中"
    }
    .to_string();
    let calculation_end = qualified_overtime_end;
    let raw_duration = match (start, calculation_end, expected, workday) {
        (Some(_start), Some(end), Some(expected), true) => end - expected,
        (Some(start), Some(end), _, false) => end - start,
        _ => Duration::zero(),
    };
    let duration_seconds = raw_duration.num_seconds().max(0);
    let interval_seconds = 30 * 60;
    let remainder_seconds = duration_seconds % interval_seconds;
    let grace_seconds = OVERTIME_BOUNDARY_GRACE_MINUTES * 60;
    let completed_intervals = duration_seconds / interval_seconds;
    let rounded_intervals = if interval_seconds - remainder_seconds <= grace_seconds {
        completed_intervals + 1
    } else {
        completed_intervals
    };
    let rounded_minutes = rounded_intervals * 30;
    let overtime_minutes = if workday && duration_seconds < 2 * 60 * 60 {
        0
    } else {
        rounded_minutes
    };
    DailyRecord {
        day: day.to_string(),
        day_type: kind.to_string(),
        calendar_name: None,
        calendar_marker: None,
        start_time: start.map(|v| v.format("%H:%M").to_string()),
        expected_end: expected.map(|v| v.format("%H:%M").to_string()),
        end_time: completed_end.map(|v| v.format("%H:%M").to_string()),
        overtime_minutes,
        status,
        manual: false,
        note: String::new(),
    }
}

#[cfg(test)]
fn calculate_day(
    day: NaiveDate,
    kind: &str,
    events: &[RawEvent],
    now: NaiveDateTime,
) -> DailyRecord {
    calculate_day_with_settings(day, kind, events, now, &AttendanceSettings::default())
}

fn recalculate_day(
    db: &Connection,
    day: NaiveDate,
    now: NaiveDateTime,
) -> Result<DailyRecord, String> {
    let existing: Option<String> = db
        .query_row(
            "SELECT data FROM daily_records WHERE day=?1",
            [day.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some(json) = existing {
        if let Ok(record) = serde_json::from_str::<DailyRecord>(&json) {
            if record.manual {
                let mut preserved = record;
                preserved.day_type = day_type(db, day)?;
                (preserved.calendar_name, preserved.calendar_marker) = calendar_metadata(db, day)?;
                let updated = serde_json::to_string(&preserved).map_err(|e| e.to_string())?;
                db.execute(
                    "UPDATE daily_records SET data=?2 WHERE day=?1",
                    params![day.to_string(), updated],
                )
                .map_err(|e| e.to_string())?;
                return Ok(preserved);
            }
        }
    }
    let kind = day_type(db, day)?;
    let events = load_events(db, day)?;
    let settings = read_attendance_settings(db)?;
    let mut record = calculate_day_with_settings(day, &kind, &events, now, &settings);
    (record.calendar_name, record.calendar_marker) = calendar_metadata(db, day)?;
    let json = serde_json::to_string(&record).map_err(|e| e.to_string())?;
    db.execute("INSERT INTO daily_records(day,data) VALUES(?1,?2) ON CONFLICT(day) DO UPDATE SET data=excluded.data", params![record.day, json]).map_err(|e| e.to_string())?;
    Ok(record)
}

#[tauri::command]
fn get_record(state: State<'_, AppState>, day: String) -> Result<serde_json::Value, String> {
    let date = parse_date(&day)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let record = recalculate_day(&db, date, Local::now().naive_local())?;
    let events = load_events(&db, date)?;
    Ok(serde_json::json!({"record": record, "events": events}))
}

#[tauri::command]
fn get_month(
    state: State<'_, AppState>,
    year: i32,
    month: u32,
) -> Result<Vec<DailyRecord>, String> {
    let first = NaiveDate::from_ymd_opt(year, month, 1).ok_or("月份无效")?;
    let last = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .unwrap()
    .pred_opt()
    .unwrap();
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let mut result = Vec::new();
    let mut date = first;
    while date <= last {
        result.push(recalculate_day(&db, date, Local::now().naive_local())?);
        date = date.succ_opt().unwrap();
    }
    Ok(result)
}

#[tauri::command]
fn save_record(
    app: AppHandle,
    state: State<'_, AppState>,
    mut record: DailyRecord,
) -> Result<(), String> {
    parse_date(&record.day)?;
    if !["workday", "restday"].contains(&record.day_type.as_str()) {
        return Err("日期类型无效".into());
    }
    record.manual = true;
    record.status = if record.note.is_empty() {
        "人工修改".into()
    } else {
        record.note.clone()
    };
    let json = serde_json::to_string(&record).map_err(|e| e.to_string())?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    db.execute("INSERT INTO daily_records(day,data) VALUES(?1,?2) ON CONFLICT(day) DO UPDATE SET data=excluded.data", params![record.day, json]).map_err(|e| e.to_string())?;
    db.execute("INSERT INTO calendar_days(day,day_type,name,manual) VALUES(?1,?2,'手动设置',1) ON CONFLICT(day) DO UPDATE SET day_type=excluded.day_type,name='手动设置',manual=1", params![record.day, record.day_type]).map_err(|e| e.to_string())?;
    wake_notification_scheduler(&app);
    Ok(())
}

#[tauri::command]
fn set_day_type(
    app: AppHandle,
    state: State<'_, AppState>,
    day: String,
    selected_type: String,
) -> Result<(), String> {
    parse_date(&day)?;
    if !["workday", "restday", "auto"].contains(&selected_type.as_str()) {
        return Err("日期类型无效".into());
    }
    let db = state.db.lock().map_err(|e| e.to_string())?;
    if selected_type == "auto" {
        db.execute(
            "DELETE FROM calendar_days WHERE day=?1 AND manual=1",
            [&day],
        )
        .map_err(|e| e.to_string())?;
    } else {
        db.execute("INSERT INTO calendar_days(day,day_type,name,manual) VALUES(?1,?2,'手动设置',1) ON CONFLICT(day) DO UPDATE SET day_type=excluded.day_type,name='手动设置',manual=1", params![day, selected_type]).map_err(|e| e.to_string())?;
    }
    let effective = day_type(&db, parse_date(&day)?)?;
    db.execute(
        "UPDATE daily_records SET data=json_set(data,'$.day_type',?2) WHERE day=?1",
        params![day, effective],
    )
    .map_err(|e| e.to_string())?;
    wake_notification_scheduler(&app);
    Ok(())
}

#[cfg(windows)]
fn query_windows_events(start: NaiveDate, end: NaiveDate) -> Result<Vec<RawEvent>, String> {
    windows_events::query(start, end)
}

#[cfg(not(windows))]
fn query_windows_events(_start: NaiveDate, _end: NaiveDate) -> Result<Vec<RawEvent>, String> {
    Err("读取 Windows 事件日志仅支持 Windows 系统".into())
}

#[cfg(windows)]
mod windows_events {
    use super::RawEvent;
    use chrono::{Local, NaiveDate};
    use quick_xml::{events::Event, Reader};
    use std::{ffi::c_void, mem::size_of, ptr};
    use windows_sys::Win32::{
        Foundation::{
            GetLastError, ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS,
        },
        System::{
            EventLog::{
                EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryForwardDirection,
                EvtRender, EvtRenderEventXml,
            },
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    const TOKEN_QUERY: u32 = 0x0008;
    const EVT_HANDLE_BATCH: usize = 32;

    struct Handle(isize);
    impl Drop for Handle {
        fn drop(&mut self) {
            if self.0 != 0 {
                unsafe { EvtClose(self.0) };
            }
        }
    }

    struct Token(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
            }
        }
    }

    fn current_user_sid() -> Result<String, String> {
        use windows_sys::Win32::Security::TOKEN_USER;
        unsafe {
            let mut token = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(format!(
                    "无法获取当前用户身份（Windows 错误 {}）",
                    GetLastError()
                ));
            }
            let token = Token(token);
            let mut needed = 0;
            windows_sys::Win32::Security::GetTokenInformation(
                token.0,
                windows_sys::Win32::Security::TokenUser,
                ptr::null_mut(),
                0,
                &mut needed,
            );
            if needed < size_of::<TOKEN_USER>() as u32 {
                return Err(format!(
                    "无法读取当前用户身份（Windows 错误 {}）",
                    GetLastError()
                ));
            }
            let mut buffer = vec![0_u8; needed as usize];
            if windows_sys::Win32::Security::GetTokenInformation(
                token.0,
                windows_sys::Win32::Security::TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
            {
                return Err(format!(
                    "无法读取当前用户身份（Windows 错误 {}）",
                    GetLastError()
                ));
            }
            let user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
            let sid = user.User.Sid;
            let mut text = ptr::null_mut();
            if windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(sid, &mut text)
                == 0
            {
                return Err(format!(
                    "无法解析当前用户 SID（Windows 错误 {}）",
                    GetLastError()
                ));
            }
            let mut length = 0;
            while *text.add(length) != 0 {
                length += 1;
            }
            let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
            windows_sys::Win32::Foundation::LocalFree(text.cast::<c_void>());
            Ok(value)
        }
    }

    fn render_xml(event: isize) -> Result<String, String> {
        unsafe {
            let mut size = 0;
            let mut count = 0;
            let ok = EvtRender(
                0,
                event,
                EvtRenderEventXml,
                0,
                ptr::null_mut(),
                &mut size,
                &mut count,
            );
            if ok != 0 || GetLastError() != ERROR_INSUFFICIENT_BUFFER {
                return Err(format!(
                    "读取 Windows 事件内容失败（Windows 错误 {}）",
                    GetLastError()
                ));
            }
            let mut buffer = vec![0_u64; (size as usize + 7) / 8];
            if EvtRender(
                0,
                event,
                EvtRenderEventXml,
                size,
                buffer.as_mut_ptr().cast(),
                &mut size,
                &mut count,
            ) == 0
            {
                return Err(format!(
                    "读取 Windows 事件内容失败（Windows 错误 {}）",
                    GetLastError()
                ));
            }
            let text = buffer.as_ptr().cast::<u16>();
            let length = (0..size as usize / 2)
                .find(|&index| *text.add(index) == 0)
                .unwrap_or(size as usize / 2);
            Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
                text, length,
            )))
        }
    }

    #[derive(Debug)]
    struct EventFields {
        event_id: u32,
        record_id: i64,
        timestamp: String,
        provider: String,
        user_sid: Option<String>,
        data: Vec<(Option<String>, String)>,
    }

    fn read_event_fields(xml: &str) -> Result<EventFields, String> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut stack = Vec::<String>::new();
        let mut event_id = None;
        let mut record_id = None;
        let mut timestamp = None;
        let mut provider = None;
        let mut user_sid = None;
        let mut data = Vec::new();
        let mut current_data: Option<(Option<String>, String)> = None;
        loop {
            match reader
                .read_event()
                .map_err(|e| format!("解析 Windows 事件 XML 失败：{e}"))?
            {
                Event::Start(tag) => {
                    let name = String::from_utf8_lossy(tag.name().as_ref()).into_owned();
                    if name == "Provider" {
                        provider = tag
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"Name")
                            .and_then(|a| a.unescape_value().ok())
                            .map(|v| v.into_owned());
                    }
                    if name == "Data" {
                        let attr = tag
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"Name")
                            .and_then(|a| a.unescape_value().ok())
                            .map(|v| v.into_owned());
                        current_data = Some((attr, String::new()));
                    }
                    stack.push(name);
                }
                Event::Empty(tag) => {
                    if tag.name().as_ref() == b"TimeCreated" {
                        timestamp = tag
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"SystemTime")
                            .and_then(|a| a.unescape_value().ok())
                            .map(|v| v.into_owned());
                    } else if tag.name().as_ref() == b"Provider" {
                        provider = tag
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"Name")
                            .and_then(|a| a.unescape_value().ok())
                            .map(|v| v.into_owned());
                    } else if tag.name().as_ref() == b"Security" {
                        user_sid = tag
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"UserID")
                            .and_then(|a| a.unescape_value().ok())
                            .map(|v| v.into_owned());
                    }
                }
                Event::Text(text) => {
                    let value = text.decode().map_err(|e| e.to_string())?.into_owned();
                    match stack.last().map(String::as_str) {
                        Some("EventID") => event_id = value.parse().ok(),
                        Some("EventRecordID") => record_id = value.parse().ok(),
                        Some("Data") => {
                            if let Some(field) = current_data.as_mut() {
                                field.1.push_str(&value);
                            }
                        }
                        _ => {}
                    }
                }
                Event::End(tag) => {
                    if tag.name().as_ref() == b"Data" {
                        if let Some(field) = current_data.take() {
                            data.push(field);
                        }
                    }
                    stack.pop();
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(EventFields {
            event_id: event_id.ok_or("事件缺少 ID")?,
            record_id: record_id.unwrap_or_default(),
            timestamp: timestamp.ok_or("事件缺少时间戳")?,
            provider: provider.ok_or("事件缺少提供程序名称")?,
            user_sid,
            data,
        })
    }

    fn query_channel(channel: &str, xpath: &str) -> Result<Vec<String>, String> {
        let channel_w: Vec<u16> = channel.encode_utf16().chain(Some(0)).collect();
        let query_w: Vec<u16> = xpath.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let query = EvtQuery(
                0,
                channel_w.as_ptr(),
                query_w.as_ptr(),
                EvtQueryChannelPath | EvtQueryForwardDirection,
            );
            if query == 0 {
                let error = GetLastError();
                return Err(if error == ERROR_ACCESS_DENIED {
                    format!("当前 Windows 用户无权读取“{channel}”事件日志，请检查该日志通道权限。")
                } else if error == 15007 {
                    format!("系统中不存在“{channel}”事件日志通道。")
                } else {
                    format!("查询“{channel}”事件日志失败（Windows 错误 {error}）。")
                });
            }
            let query = Handle(query);
            let mut output = Vec::new();
            loop {
                let mut events = [0_isize; EVT_HANDLE_BATCH];
                let mut returned = 0;
                if EvtNext(
                    query.0,
                    events.len() as u32,
                    events.as_mut_ptr(),
                    1000,
                    0,
                    &mut returned,
                ) == 0
                {
                    let error = GetLastError();
                    if error == ERROR_NO_MORE_ITEMS {
                        break;
                    }
                    return Err(format!(
                        "读取“{channel}”事件日志失败（Windows 错误 {error}）。"
                    ));
                }
                for event in events.iter().take(returned as usize) {
                    let event = Handle(*event);
                    output.push(render_xml(event.0)?);
                }
            }
            Ok(output)
        }
    }

    pub fn query(start: NaiveDate, end: NaiveDate) -> Result<Vec<RawEvent>, String> {
        let local_start = start
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .earliest()
            .ok_or("无法解析本地起始时间")?;
        let local_end = end
            .succ_opt()
            .unwrap_or(end)
            .and_hms_opt(2, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .latest()
            .ok_or("无法解析本地结束时间")?;
        let start = local_start
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let end = local_end
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let winlogon_query = format!(
            "*[System[EventID=811 and TimeCreated[@SystemTime >= '{}' and @SystemTime <= '{}']]]",
            start, end
        );
        let system_query = format!("*[System[(EventID=1074 or EventID=6005 or EventID=6006 or EventID=12 or EventID=13) and TimeCreated[@SystemTime >= '{}' and @SystemTime <= '{}']]]", start, end);
        let sid = current_user_sid()?;
        let mut records = Vec::new();
        for xml in query_channel("Microsoft-Windows-Winlogon/Operational", &winlogon_query)? {
            let event = read_event_fields(&xml)?;
            let field = |key: &str| {
                event
                    .data
                    .iter()
                    .find(|(name, _)| name.as_deref() == Some(key))
                    .map(|(_, value)| value.as_str())
                    .unwrap_or("")
            };
            if event.event_id == 811
                && event.user_sid.as_deref() == Some(sid.as_str())
                && field("SubscriberName") == "TermSrv"
            {
                let kind = match field("Event") {
                    "4" => "lock",
                    "5" => "unlock",
                    _ => continue,
                };
                records.push(RawEvent {
                    timestamp: format_event_time(&event.timestamp)?,
                    kind: kind.into(),
                    source: "windows-winlogon-log".into(),
                    event_id: Some(event.record_id),
                });
            }
        }
        for xml in query_channel("System", &system_query)? {
            let event = read_event_fields(&xml)?;
            let kind = if event.event_id == 1074
                || event.event_id == 6006
                || (event.event_id == 13 && event.provider == "Microsoft-Windows-Kernel-General")
            {
                "shutdown"
            } else if event.event_id == 6005
                || (event.event_id == 12 && event.provider == "Microsoft-Windows-Kernel-General")
            {
                "startup"
            } else {
                continue;
            };
            records.push(RawEvent {
                timestamp: format_event_time(&event.timestamp)?,
                kind: kind.into(),
                source: "windows-system-log".into(),
                event_id: Some(event.record_id),
            });
        }
        records.sort_by(|left, right| left.timestamp.cmp(&right.timestamp));
        Ok(records)
    }

    fn format_event_time(raw: &str) -> Result<String, String> {
        chrono::DateTime::parse_from_rfc3339(raw)
            .map(|value| value.with_timezone(&Local).to_rfc3339())
            .map_err(|e| format!("事件时间戳格式无效：{e}"))
    }

    #[cfg(test)]
    mod tests {
        use super::read_event_fields;

        #[test]
        fn parses_windows_event_xml_fields() {
            let xml = r#"<Event><System><Provider Name="Microsoft-Windows-Winlogon"/><EventID>811</EventID><EventRecordID>42</EventRecordID><TimeCreated SystemTime="2026-11-12T09:00:00.000Z"/><Security UserID="S-1-5-21-123"/></System><EventData><Data Name="SubscriberName">TermSrv</Data><Data Name="Event">5</Data></EventData></Event>"#;
            let event = read_event_fields(xml).unwrap();

            assert_eq!(event.event_id, 811);
            assert_eq!(event.record_id, 42);
            assert_eq!(event.timestamp, "2026-11-12T09:00:00.000Z");
            assert_eq!(event.provider, "Microsoft-Windows-Winlogon");
            assert_eq!(event.user_sid.as_deref(), Some("S-1-5-21-123"));
            assert_eq!(event.data[0].0.as_deref(), Some("SubscriberName"));
            assert_eq!(event.data[0].1, "TermSrv");
            assert_eq!(event.data[1].0.as_deref(), Some("Event"));
            assert_eq!(event.data[1].1, "5");
        }

        #[test]
        fn reports_missing_required_event_fields() {
            let error = read_event_fields("<Event><System/></Event>").unwrap_err();
            assert_eq!(error, "事件缺少 ID");
        }
    }
}

#[tauri::command]
fn sync_month_events(
    app: AppHandle,
    state: State<'_, AppState>,
    year: i32,
    month: u32,
) -> Result<serde_json::Value, String> {
    let first = NaiveDate::from_ymd_opt(year, month, 1).ok_or("月份无效")?;
    let next_month = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .ok_or("月份无效")?;
    let last = next_month.pred_opt().ok_or("月份无效")?;
    let events = query_windows_events(first, last)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let added = persist_synced_events(&db, &events, first, last)?;
    wake_notification_scheduler(&app);
    Ok(serde_json::json!({"synced":true,"found":events.len(),"added":added}))
}

fn recalculate_all_automatic_records(db: &Connection) -> Result<(), String> {
    let days = {
        let mut statement = db
            .prepare("SELECT day FROM daily_records WHERE json_extract(data,'$.manual') != 1")
            .map_err(|e| e.to_string())?;
        let days = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        days
    };
    for day in days {
        recalculate_day(db, parse_date(&day)?, Local::now().naive_local())?;
    }
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
fn save_attendance_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    work_start: String,
    work_end: String,
    flexible_enabled: bool,
    flexible_minutes: i64,
) -> Result<(), String> {
    let start = parse_clock_time(&work_start).map_err(|e| format!("上班{e}"))?;
    let end = parse_clock_time(&work_end).map_err(|e| format!("下班{e}"))?;
    if end <= start {
        return Err("下班时间必须晚于上班时间".to_string());
    }
    if !(0..=180).contains(&flexible_minutes) || flexible_minutes % 5 != 0 {
        return Err("弹性上班时长必须是 0 到 180 之间的 5 分钟倍数".to_string());
    }
    let db = state.db.lock().map_err(|e| e.to_string())?;
    for (key, value) in [
        ("work_start", work_start),
        ("work_end", work_end),
        (
            "flexible_enabled",
            if flexible_enabled { "1" } else { "0" }.to_string(),
        ),
        ("flexible_minutes", flexible_minutes.to_string()),
    ] {
        db.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])
            .map_err(|e| e.to_string())?;
    }
    recalculate_all_automatic_records(&db)?;
    wake_notification_scheduler(&app);
    Ok(())
}

#[tauri::command]
fn set_notification_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    notify_at_end: bool,
    notify_first_unlock: bool,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    for (key, enabled) in [
        ("notify_at_end", notify_at_end),
        ("notify_first_unlock", notify_first_unlock),
    ] {
        db.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, if enabled { "1" } else { "0" }],
        )
        .map_err(|e| e.to_string())?;
    }
    wake_notification_scheduler(&app);
    Ok(())
}

#[tauri::command]
fn set_company_workday(state: State<'_, AppState>, day: String) -> Result<(), String> {
    let date = parse_date(&day)?;
    if date.weekday() != chrono::Weekday::Sat {
        return Err("公司上班日必须选择周六".into());
    }
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let month = format!("{:04}-{:02}", date.year(), date.month());
    db.execute(
        "INSERT INTO company_workdays(month,day) VALUES(?1,?2) ON CONFLICT(month) DO UPDATE SET day=excluded.day",
        params![month, day],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn persist_synced_events(
    db: &Connection,
    events: &[RawEvent],
    first: NaiveDate,
    last: NaiveDate,
) -> Result<usize, String> {
    let mut added = 0;
    for event in events {
        let dt = chrono::DateTime::parse_from_rfc3339(&event.timestamp)
            .map_err(|e| e.to_string())?
            .naive_local();
        let day = if dt.time() <= NaiveTime::from_hms_opt(2, 0, 0).unwrap() {
            dt.date().pred_opt().unwrap_or(dt.date())
        } else {
            dt.date()
        };
        let key = format!(
            "{}:{}:{}",
            event.event_id.unwrap_or_default(),
            event.timestamp,
            event.kind
        );
        let json = serde_json::to_string(event).map_err(|e| e.to_string())?;
        added += db
            .execute(
                "INSERT OR IGNORE INTO raw_events(event_key,day,data) VALUES(?1,?2,?3)",
                params![key, day.to_string(), json],
            )
            .map_err(|e| e.to_string())?;
    }
    let mut day = first;
    while day <= last {
        recalculate_day(db, day, Local::now().naive_local())?;
        day = day.succ_opt().unwrap();
    }
    Ok(added)
}

fn mark_month_events_synced(db: &Connection, year: i32, month: u32) -> Result<(), String> {
    db.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO NOTHING",
        params![
            format!("events_month_synced:{year:04}-{month:02}"),
            Local::now().to_rfc3339()
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn fetch_calendar(year: i32) -> Result<CalendarApi, String> {
    let url = format!("https://timor.tech/api/holiday/year/{year}/");
    let response: CalendarApi = ureq::get(&url)
        .timeout(StdDuration::from_secs(12))
        .call()
        .map_err(|e| format!("节假日日历同步失败：{e}"))?
        .into_json()
        .map_err(|e| format!("无法解析节假日日历：{e}"))?;
    if response.code != 0 {
        return Err(format!("节假日日历接口返回错误码 {}", response.code));
    }
    if response.holiday.is_empty() {
        return Err(format!("{year} 年节假日日历接口未返回日期数据，未更新缓存"));
    }
    Ok(response)
}

fn persist_calendar(db: &Connection, year: i32, response: CalendarApi) -> Result<(), String> {
    if response.holiday.is_empty() {
        return Err(format!("{year} 年节假日日历接口未返回日期数据，未更新缓存"));
    }
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    for entry in response.holiday.values() {
        let day = entry.date.clone().ok_or("日历日期缺失")?;
        let kind = if entry.holiday { "restday" } else { "workday" };
        tx.execute("INSERT INTO calendar_days(day,day_type,name,manual) VALUES(?1,?2,?3,0) ON CONFLICT(day) DO UPDATE SET day_type=CASE WHEN calendar_days.manual=1 THEN calendar_days.day_type ELSE excluded.day_type END,name=CASE WHEN calendar_days.manual=1 THEN calendar_days.name ELSE excluded.name END", params![day, kind, entry.name.clone().unwrap_or_default()]).map_err(|e| e.to_string())?;
    }
    let synced = Local::now().to_rfc3339();
    tx.execute("INSERT INTO metadata(key,value) VALUES('calendar_last_sync',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&synced]).map_err(|e| e.to_string())?;
    tx.execute("INSERT INTO metadata(key,value) VALUES('calendar_last_year',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [year.to_string()]).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

fn calendar_info(db: &Connection, message: Option<String>) -> Result<CalendarInfo, String> {
    let last_sync = db
        .query_row(
            "SELECT value FROM metadata WHERE key='calendar_last_sync'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let mut stmt = db
        .prepare("SELECT DISTINCT substr(day,1,4) FROM calendar_days ORDER BY 1")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let cached_years = rows
        .filter_map(Result::ok)
        .filter_map(|v| v.parse().ok())
        .collect();
    Ok(CalendarInfo {
        last_sync,
        cached_years,
        message,
    })
}

#[tauri::command]
fn get_calendar_info(state: State<'_, AppState>) -> Result<CalendarInfo, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    calendar_info(&db, None)
}

#[tauri::command]
fn export_backup(state: State<'_, AppState>) -> Result<Vec<u8>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "cloudroom-backup-{}-{nonce}.sqlite3",
        std::process::id()
    ));
    db.backup(rusqlite::DatabaseName::Main, &path, None)
        .map_err(|e| format!("导出数据库备份失败：{e}"))?;
    let result = fs::read(&path).map_err(|e| format!("读取数据库备份失败：{e}"));
    let _ = fs::remove_file(&path);
    result
}

#[tauri::command]
fn import_backup(data: Vec<u8>, state: State<'_, AppState>) -> Result<(), String> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "cloudroom-import-{}-{nonce}.sqlite3",
        std::process::id()
    ));
    fs::write(&path, data).map_err(|e| format!("读取备份文件失败：{e}"))?;
    let backup = Connection::open(&path).map_err(|e| format!("无法打开备份文件：{e}"))?;
    backup
        .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        .map_err(|e| format!("备份文件校验失败：{e}"))
        .and_then(|result| {
            if result == "ok" {
                Ok(())
            } else {
                Err(format!("备份文件校验失败：{result}"))
            }
        })?;
    for table in [
        "daily_records",
        "raw_events",
        "calendar_days",
        "company_workdays",
        "metadata",
    ] {
        backup
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |_| Ok(()),
            )
            .map_err(|_| format!("备份文件缺少必要数据表：{table}"))?;
    }
    let result = (|| {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        db.execute(
            "ATTACH DATABASE ?1 AS backupdb",
            [path.to_string_lossy().as_ref()],
        )
        .map_err(|e| format!("无法读取备份数据库：{e}"))?;
        let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
        tx.execute_batch(
            "DELETE FROM daily_records;
         DELETE FROM raw_events;
         DELETE FROM calendar_days;
         DELETE FROM company_workdays;
         DELETE FROM metadata;",
        )
        .map_err(|e| e.to_string())?;
        for table in [
            "daily_records",
            "raw_events",
            "calendar_days",
            "company_workdays",
            "metadata",
        ] {
            tx.execute(
                &format!("INSERT INTO main.{table} SELECT * FROM backupdb.{table}"),
                [],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        db.execute_batch("DETACH DATABASE backupdb")
            .map_err(|e| e.to_string())
    })();
    drop(backup);
    let _ = fs::remove_file(&path);
    result
}

#[tauri::command]
fn export_csv(state: State<'_, AppState>, year: i32, month: u32) -> Result<String, String> {
    let records = get_month(state, year, month)?;
    let mut csv = String::from("日期,日期类型,上班时间,预计下班,实际下班,加班分钟,状态,备注\n");
    for r in records {
        let line = format!(
            "{},{},{},{},{},{},{},{}\n",
            r.day,
            if r.day_type == "workday" {
                "工作日"
            } else {
                "休息日"
            },
            r.start_time.unwrap_or_default(),
            r.expected_end.unwrap_or_default(),
            r.end_time.unwrap_or_default(),
            r.overtime_minutes,
            r.status.replace(',', "，"),
            r.note.replace(',', "，").replace('\n', " ")
        );
        csv.push_str(&line);
    }
    Ok(csv)
}

#[tauri::command]
fn get_settings(app: AppHandle, state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let info = calendar_info(&db, None)?;
    let autostart = app.autolaunch().is_enabled().map_err(|e| e.to_string())?;
    let path = data_file(&app)?;
    let intro_shown: Option<String> = db
        .query_row(
            "SELECT value FROM metadata WHERE key='intro_shown'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let theme = db
        .query_row(
            "SELECT value FROM metadata WHERE key='theme_mode'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "auto".to_string());
    Ok(
        serde_json::json!({"autostart":autostart,"data_path":path.to_string_lossy(),"calendar":info,"first_open_date":state.first_open_date,"intro_pending":intro_shown.as_deref()!=Some("1"),"version":APP_VERSION,"theme":theme,"attendance":read_attendance_settings(&db)?}),
    )
}

#[tauri::command]
fn set_theme_mode(mode: String, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if !matches!(mode.as_str(), "auto" | "light" | "dark") {
        return Err("主题模式无效".to_string());
    }
    let db = state.db.lock().map_err(|e| e.to_string())?;
    db.execute("INSERT INTO metadata(key,value) VALUES('theme_mode',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&mode])
        .map_err(|e| e.to_string())?;
    drop(db);
    #[cfg(windows)]
    if let Some(window) = app.get_webview_window("main") {
        set_native_titlebar_theme(&window, &mode)?;
    }
    #[cfg(not(windows))]
    let _ = app;
    Ok(())
}

#[cfg(windows)]
fn set_native_titlebar_theme(window: &tauri::WebviewWindow, mode: &str) -> Result<(), String> {
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};

    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    if mode == "auto" {
        // Attribute 20 with a zero value clears the app override and delegates to Windows.
        let value = 0u32;
        let result = unsafe {
            DwmSetWindowAttribute(
                hwnd.0 as _,
                20,
                (&value as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        };
        if result < 0 {
            return Err(format!("重置自动标题栏主题失败：HRESULT {result}"));
        }
        return Ok(());
    }
    let value = u32::from(mode == "dark");
    let result = unsafe {
        DwmSetWindowAttribute(
            hwnd.0 as _,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&value as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    if result < 0 {
        return Err(format!("设置标题栏主题失败：HRESULT {result}"));
    }
    Ok(())
}

#[cfg(windows)]
fn system_proxy_for_url(url: &str) -> Result<Option<String>, String> {
    use std::{ffi::c_void, ptr};
    use windows_sys::Win32::{
        Networking::WinHttp::{
            WinHttpCloseHandle, WinHttpGetIEProxyConfigForCurrentUser, WinHttpGetProxyForUrl,
            WinHttpOpen, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_AUTOPROXY_CONFIG_URL,
            WINHTTP_AUTOPROXY_OPTIONS, WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WINHTTP_PROXY_INFO,
        },
        System::Com::CoTaskMemFree,
    };

    unsafe fn take_wide(value: *mut u16) -> Option<String> {
        if value.is_null() {
            return None;
        }
        let mut len = 0;
        while *value.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(value, len));
        CoTaskMemFree(value.cast::<c_void>());
        Some(text)
    }

    let url_wide: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let agent: Vec<u16> = "CloudroomRecord".encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut user_config = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        let has_config = WinHttpGetIEProxyConfigForCurrentUser(&mut user_config) != 0;
        let pac_url = if has_config {
            take_wide(user_config.lpszAutoConfigUrl)
        } else {
            None
        };
        let static_proxy = if has_config {
            take_wide(user_config.lpszProxy)
        } else {
            None
        };
        if has_config && !user_config.lpszProxyBypass.is_null() {
            CoTaskMemFree(user_config.lpszProxyBypass.cast::<c_void>());
        }

        let Some(pac_url) = pac_url else {
            return Ok(static_proxy.and_then(|proxy| {
                proxy.split(';').find_map(|entry| {
                    let candidate = entry
                        .split_once('=')
                        .map(|(_, value)| value.trim())
                        .unwrap_or(entry.trim());
                    (!candidate.is_empty()).then(|| normalize_proxy(candidate))
                })
            }));
        };

        let session = WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            ptr::null(),
            ptr::null(),
            0,
        );
        if session.is_null() {
            return Err("无法初始化 Windows 系统代理解析器".into());
        }
        let pac_wide: Vec<u16> = pac_url.encode_utf16().chain(Some(0)).collect();
        let mut options = WINHTTP_AUTOPROXY_OPTIONS::default();
        options.dwFlags = WINHTTP_AUTOPROXY_CONFIG_URL;
        options.lpszAutoConfigUrl = pac_wide.as_ptr();
        options.fAutoLogonIfChallenged = 1;
        let mut info = WINHTTP_PROXY_INFO::default();
        let ok = WinHttpGetProxyForUrl(session, url_wide.as_ptr(), &mut options, &mut info) != 0;
        WinHttpCloseHandle(session);
        if !ok {
            return Err("Windows 系统代理 PAC 脚本解析失败，请检查代理配置".into());
        }
        let proxy = if info.dwAccessType == WINHTTP_ACCESS_TYPE_NO_PROXY {
            None
        } else {
            take_wide(info.lpszProxy).map(|value| normalize_proxy(&value))
        };
        if !info.lpszProxyBypass.is_null() {
            CoTaskMemFree(info.lpszProxyBypass.cast::<c_void>());
        }
        Ok(proxy)
    }
}

#[cfg(not(windows))]
fn system_proxy_for_url(_url: &str) -> Result<Option<String>, String> {
    Ok(None)
}

fn normalize_proxy(proxy: &str) -> String {
    if proxy.contains("://") {
        proxy.to_string()
    } else {
        format!("http://{proxy}")
    }
}

fn update_agent(url: &str, connect_timeout: u64, read_timeout: u64) -> Result<ureq::Agent, String> {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(StdDuration::from_secs(connect_timeout))
        .timeout_read(StdDuration::from_secs(read_timeout));
    if let Some(proxy) = system_proxy_for_url(url)? {
        let proxy = ureq::Proxy::new(&proxy)
            .map_err(|error| format!("Windows 系统代理地址无效：{error}"))?;
        builder = builder.proxy(proxy);
    }
    Ok(builder.build())
}

#[tauri::command]
async fn check_for_updates() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(check_for_updates_blocking)
        .await
        .map_err(|error| format!("更新检查任务失败：{error}"))?
}

fn check_for_updates_blocking() -> Result<serde_json::Value, String> {
    let manifest_url =
        format!("https://github.com/{GITHUB_REPOSITORY}/releases/latest/download/latest.json");
    let response: serde_json::Value = update_agent(&manifest_url, 5, 10)?
        .get(&manifest_url)
        .set("User-Agent", "CloudroomRecord")
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(status, response) => {
                format!(
                    "无法读取 GitHub 更新清单（HTTP {status}）：{}",
                    response.status_text()
                )
            }
            other => format!("无法连接 GitHub 更新清单：{other}"),
        })?
        .into_json()
        .map_err(|error| format!("无法解析 GitHub 更新清单：{error}"))?;

    let latest_version = response["version"].as_str().ok_or("更新清单缺少版本号")?;
    let latest_version = latest_version.strip_prefix('v').unwrap_or(latest_version);
    let html_url = response["html_url"]
        .as_str()
        .ok_or("更新清单缺少 Release 页面地址")?;
    let download_url = response["download_url"]
        .as_str()
        .ok_or("更新清单缺少安装包地址")?;
    let parsed_download_url =
        url::Url::parse(download_url).map_err(|_| "更新清单中的安装包地址无效")?;
    let expected_prefix = format!("/{}/releases/download/", GITHUB_REPOSITORY);
    if parsed_download_url.scheme() != "https"
        || parsed_download_url.host_str() != Some("github.com")
        || !parsed_download_url.path().starts_with(&expected_prefix)
    {
        return Err("更新清单中的安装包地址不属于本仓库".into());
    }
    let asset_name = response["asset_name"]
        .as_str()
        .ok_or("更新清单缺少安装包文件名")?;
    Ok(serde_json::json!({
        "current_version": APP_VERSION,
        "latest_version": latest_version,
        "update_available": version_is_newer(latest_version, APP_VERSION),
        "html_url": html_url,
        "asset_name": asset_name,
        "download_url": download_url,
        "digest": response["digest"].as_str(),
        "notes": response["notes"].as_str().unwrap_or("")
    }))
}

#[tauri::command]
async fn download_update(
    app: AppHandle,
    download_url: String,
    expected_digest: Option<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        download_update_blocking(app, download_url, expected_digest)
    })
    .await
    .map_err(|error| format!("更新下载任务失败：{error}"))?
}

fn download_update_blocking(
    app: AppHandle,
    download_url: String,
    expected_digest: Option<String>,
) -> Result<String, String> {
    let parsed = url::Url::parse(&download_url).map_err(|_| "更新下载地址无效".to_string())?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || !parsed.path().contains("/releases/download/")
    {
        return Err("更新下载地址不是该仓库的 Release 资产".to_string());
    }

    let response = update_agent(&download_url, 10, 30)?
        .get(&download_url)
        .set("User-Agent", "CloudroomRecord")
        .call()
        .map_err(|error| format!("下载更新失败：{error}"))?;
    let total = response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    const MAX_UPDATE_BYTES: u64 = 500 * 1024 * 1024;
    if total > MAX_UPDATE_BYTES {
        return Err("安装包超过 500 MB，已取消下载".to_string());
    }

    let cache = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("updates");
    fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
    let installer = cache.join("CloudroomRecord-update.exe");
    let partial = cache.join("CloudroomRecord-update.exe.part");
    let mut file = fs::File::create(&partial).map_err(|e| e.to_string())?;
    let mut reader = response.into_reader();
    let mut hasher = Sha256::new();
    let mut downloaded = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    let started = std::time::Instant::now();
    let mut signature = [0_u8; 2];
    loop {
        let count = reader.read(&mut buffer).map_err(|e| {
            let _ = fs::remove_file(&partial);
            format!("读取更新数据失败或超时：{e}")
        })?;
        if count == 0 {
            break;
        }
        if started.elapsed() > StdDuration::from_secs(5 * 60) {
            let _ = fs::remove_file(&partial);
            return Err("更新下载超过 5 分钟，已取消下载".to_string());
        }
        let signature_len = signature.len();
        if downloaded == 0 && count >= signature_len {
            signature.copy_from_slice(&buffer[..signature_len]);
        }
        downloaded += count as u64;
        if downloaded > MAX_UPDATE_BYTES {
            let _ = fs::remove_file(&partial);
            return Err("安装包超过 500 MB，已取消下载".to_string());
        }
        file.write_all(&buffer[..count])
            .map_err(|e| e.to_string())?;
        hasher.update(&buffer[..count]);
        let _ = app.emit(
            "update-download-progress",
            serde_json::json!({"downloaded": downloaded, "total": total}),
        );
    }
    if downloaded < 2 || signature != *b"MZ" {
        let _ = fs::remove_file(&partial);
        return Err("下载内容不是有效的 Windows 安装程序".to_string());
    }
    file.sync_all().map_err(|e| e.to_string())?;
    let actual_digest = format!("sha256:{:x}", hasher.finalize());
    if let Some(expected) = expected_digest {
        if expected != actual_digest {
            let _ = fs::remove_file(&partial);
            return Err("安装包 SHA-256 校验失败，已删除损坏文件".to_string());
        }
    }
    fs::rename(&partial, &installer).map_err(|e| e.to_string())?;
    Ok(installer.to_string_lossy().into_owned())
}

#[tauri::command]
fn install_update(app: AppHandle, installer_path: String) -> Result<(), String> {
    let cache = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("updates");
    let installer = PathBuf::from(installer_path);
    if installer != cache.join("CloudroomRecord-update.exe") || !installer.is_file() {
        return Err("找不到已下载的更新安装包".to_string());
    }
    Command::new(&installer)
        .spawn()
        .map_err(|e| format!("无法启动安装程序：{e}"))?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
fn should_check_updates(state: State<'_, AppState>) -> Result<bool, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let last: Option<String> = db
        .query_row(
            "SELECT value FROM metadata WHERE key='update_last_check'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(last.as_deref() != Some(&Local::now().date_naive().to_string()))
}

#[tauri::command]
fn mark_updates_checked(state: State<'_, AppState>) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    db.execute("INSERT INTO metadata(key,value) VALUES('update_last_check',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [Local::now().date_naive().to_string()])
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn version_is_newer(latest: &str, current: &str) -> bool {
    let numbers = |version: &str| {
        version
            .split_once('-')
            .unwrap_or((version, ""))
            .0
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    match (numbers(latest), numbers(current)) {
        (Some(latest), Some(current)) => {
            for (left, right) in latest.iter().zip(current.iter()) {
                if left != right {
                    return left > right;
                }
            }
            latest.len() > current.len()
        }
        _ => latest != current,
    }
}

#[tauri::command]
fn open_release_page(url: String) -> Result<(), String> {
    let expected_prefix = format!("https://github.com/{GITHUB_REPOSITORY}/releases");
    if !url.starts_with(&expected_prefix) {
        return Err("Release 地址无效".to_string());
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", &url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| error.to_string())?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = url;
        Err("当前平台暂不支持打开 Release 页面".to_string())
    }
}

#[tauri::command]
fn complete_intro(state: State<'_, AppState>) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    db.execute(
        "INSERT INTO metadata(key,value) VALUES('intro_shown','1') ON CONFLICT(key) DO UPDATE SET value='1'",
        [],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    if enabled {
        app.autolaunch().enable().map_err(|e| e.to_string())?;
    } else {
        app.autolaunch().disable().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(body).show();
}

fn claim_notification(db: &Connection, key: &str) -> bool {
    db.execute(
        "INSERT OR IGNORE INTO metadata(key,value) VALUES(?1,?2)",
        params![format!("notification:{key}"), Local::now().to_rfc3339()],
    )
    .map(|changed| changed == 1)
    .unwrap_or(false)
}

fn wake_notification_scheduler(app: &AppHandle) {
    if let Some(sender) = app.try_state::<mpsc::Sender<()>>() {
        let _ = sender.send(());
    }
}

fn sync_calendar_if_due(app: &AppHandle) {
    let state = app.state::<AppState>();
    let year = Local::now().year();
    let calendar_due = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            let cached_this_year: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM calendar_days WHERE substr(day,1,4)=?1 AND manual=0)",
                    [year.to_string()],
                    |row| row.get(0),
                )
                .ok()?;
            let last_attempt: Option<String> = db
                .query_row(
                    "SELECT value FROM metadata WHERE key='calendar_last_attempt'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .ok()
                .flatten();
            let attempted_today = last_attempt
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
                .map(|value| value.with_timezone(&Local).date_naive() == Local::now().date_naive())
                .unwrap_or(false);
            Some(!cached_this_year && !attempted_today)
        })
        .unwrap_or(false);
    if calendar_due {
        if let Ok(db) = state.db.lock() {
            let attempted = Local::now().to_rfc3339();
            let _ = db.execute("INSERT INTO metadata(key,value) VALUES('calendar_last_attempt',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&attempted]);
        }
        match fetch_calendar(year) {
            Ok(calendar) => {
                if let Ok(db) = state.db.lock() {
                    match persist_calendar(&db, year, calendar) {
                        Ok(()) => {
                            let _ = app.emit("calendar-updated", ());
                        }
                        Err(error) => notify(
                            app,
                            "节假日日历未保存",
                            &format!("{error}。已有缓存不会被覆盖。"),
                        ),
                    }
                }
            }
            Err(error) => {
                if let Ok(mut notified) = state.notified.lock() {
                    let key = format!("calendar-error:{}", Local::now().date_naive());
                    if notified.insert(key) {
                        notify(
                            app,
                            "节假日日历同步失败",
                            &format!("{error}。将继续使用本地缓存。"),
                        );
                    }
                }
            }
        }
    }
}

fn notification_deadline(app: &AppHandle, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let state = app.state::<AppState>();
    let db = state.db.lock().ok()?;
    let today = now.date();
    let record = recalculate_day(&db, today, now).ok()?;
    let settings = read_attendance_settings(&db).ok()?;
    let expected_end = record
        .expected_end
        .as_deref()
        .and_then(|value| NaiveTime::parse_from_str(value, "%H:%M").ok())
        .map(|time| today.and_time(time));
    let cutoff_day = if now.time() < NaiveTime::from_hms_opt(2, 0, 0)? {
        today
    } else {
        today.succ_opt()?
    };
    let cutoff_at = cutoff_day.and_hms_opt(2, 0, 0)?;
    [
        settings
            .notify_at_end
            .then_some(expected_end)
            .flatten()
            .filter(|time| *time > now),
        Some(cutoff_at).filter(|time| *time > now),
    ]
    .into_iter()
    .flatten()
    .min()
}

fn poll_and_notify(app: &AppHandle) {
    let state = app.state::<AppState>();
    let now = Local::now().naive_local();
    let from = now.date().pred_opt().unwrap_or(now.date());
    if let Ok(db) = state.db.lock() {
        let settings = read_attendance_settings(&db).ok();
        for day in [from, now.date()] {
            if let Ok(record) = recalculate_day(&db, day, now) {
                if let Some(expected) = record
                    .expected_end
                    .as_deref()
                    .and_then(|t| NaiveTime::parse_from_str(t, "%H:%M").ok())
                    .map(|t| day.and_time(t))
                {
                    let key = format!("{}:end", day);
                    if settings
                        .as_ref()
                        .is_some_and(|settings| settings.notify_at_end)
                        && day == now.date()
                        && now >= expected
                        && now < expected + Duration::hours(12)
                        && claim_notification(&db, &key)
                    {
                        let message = settings
                            .as_ref()
                            .map(|settings| end_notification_message(&record, settings))
                            .unwrap_or_else(|| {
                                format!(
                                    "今天预计下班时间是 {}。",
                                    record.expected_end.as_deref().unwrap_or_default()
                                )
                            });
                        notify(app, "下班时间到了", &message);
                    }
                }
                if record.status == "整夜未锁" {
                    let key = format!("{}:cutoff", day);
                    if claim_notification(&db, &key) {
                        notify(
                            app,
                            "考勤异常提醒",
                            "截至凌晨 2:00 未检测到锁屏事件，记录已按 2:00 截止，请核对。",
                        );
                    }
                }
            }
        }
    };
}

fn end_notification_message(record: &DailyRecord, settings: &AttendanceSettings) -> String {
    let mut message = format!(
        "今天预计下班时间是 {}。",
        record.expected_end.as_deref().unwrap_or_default()
    );
    if record.day_type == "workday" {
        if let Some(start) = record
            .start_time
            .as_deref()
            .and_then(|value| parse_clock_time(value).ok())
        {
            if arrival_is_late(start, settings) {
                let allowed_until = settings.work_start
                    + Duration::minutes(if settings.flexible_enabled {
                        settings.flexible_minutes
                    } else {
                        0
                    });
                message.push_str(&format!(
                    "早上迟到异常：首次解锁 {}，超过允许上班时间 {}。",
                    start.format("%H:%M:%S"),
                    allowed_until.format("%H:%M"),
                ));
            }
        }
    }
    message
}

fn notify_first_unlock(app: &AppHandle, event: &RawEvent) {
    let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(&event.timestamp) else {
        return;
    };
    let local = timestamp.with_timezone(&Local);
    let day = local.date_naive();
    let state = app.state::<AppState>();
    let Ok(db) = state.db.lock() else {
        return;
    };
    let Ok(settings) = read_attendance_settings(&db) else {
        return;
    };
    if !settings.notify_first_unlock
        || day != Local::now().date_naive()
        || event.kind != "unlock"
        || day_type(&db, day).ok().as_deref() != Some("workday")
    {
        return;
    }
    let key = format!("{}:first-unlock", day);
    if claim_notification(&db, &key) {
        let start = local.time();
        let expected_end = expected_end_for_start(day, start, &settings);
        notify(
            app,
            "今日首次解锁已记录",
            &first_unlock_message(start, expected_end, &settings),
        );
    }
}

fn arrival_is_late(start: NaiveTime, settings: &AttendanceSettings) -> bool {
    let allowed_minutes = if settings.flexible_enabled {
        settings.flexible_minutes
    } else {
        0
    };
    start > settings.work_start + Duration::minutes(allowed_minutes)
}

fn expected_end_for_start(
    day: NaiveDate,
    start: NaiveTime,
    settings: &AttendanceSettings,
) -> NaiveDateTime {
    let late_minutes = (start - settings.work_start).num_minutes().max(0);
    day.and_time(settings.work_end)
        + Duration::minutes(if settings.flexible_enabled {
            late_minutes.min(settings.flexible_minutes)
        } else {
            0
        })
}

fn first_unlock_message(
    start: NaiveTime,
    expected_end: NaiveDateTime,
    settings: &AttendanceSettings,
) -> String {
    let start_text = start.format("%H:%M:%S");
    let expected_text = expected_end.format("%H:%M");
    if arrival_is_late(start, settings) {
        let allowed_until = settings.work_start
            + Duration::minutes(if settings.flexible_enabled {
                settings.flexible_minutes
            } else {
                0
            });
        format!(
            "首次解锁时间：{start_text}。已超过允许上班时间（{allowed_until}），请注意迟到异常。预计下班时间：{expected_text}。"
        )
    } else {
        format!("首次解锁时间：{start_text}。预计下班时间：{expected_text}。")
    }
}

fn notify_on_session_unlock(app: &AppHandle) {
    let now = Local::now();
    let day = now.date_naive();
    let state = app.state::<AppState>();
    let Ok(db) = state.db.lock() else {
        return;
    };
    let Ok(settings) = read_attendance_settings(&db) else {
        return;
    };
    if !settings.notify_first_unlock || day_type(&db, day).ok().as_deref() != Some("workday") {
        return;
    }
    let mut statement = match db.prepare("SELECT data FROM raw_events WHERE day=?1") {
        Ok(statement) => statement,
        Err(_) => return,
    };
    let rows = match statement.query_map([day.to_string()], |row| row.get::<_, String>(0)) {
        Ok(rows) => rows,
        Err(_) => return,
    };
    for json in rows.flatten() {
        if let Ok(event) = serde_json::from_str::<RawEvent>(&json) {
            if event.kind == "unlock"
                && chrono::DateTime::parse_from_rfc3339(&event.timestamp)
                    .ok()
                    .is_some_and(|timestamp| timestamp.with_timezone(&Local).date_naive() == day)
            {
                return;
            }
        }
    }
    let key = format!("{day}:first-unlock");
    if claim_notification(&db, &key) {
        let start = now.time();
        let expected_end = expected_end_for_start(day, start, &settings);
        notify(
            app,
            "今日首次解锁已记录",
            &first_unlock_message(start, expected_end, &settings),
        );
    }
}

#[cfg(windows)]
fn listen_for_session_unlock(app: AppHandle) -> Result<(), String> {
    use std::sync::OnceLock;
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION},
        UI::WindowsAndMessaging::{
            CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage,
            WNDCLASSW,
        },
    };

    static APP: OnceLock<AppHandle> = OnceLock::new();
    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        use windows_sys::Win32::{
            System::RemoteDesktop::WTSUnRegisterSessionNotification,
            UI::WindowsAndMessaging::{DefWindowProcW, WM_WTSSESSION_CHANGE, WTS_SESSION_UNLOCK},
        };
        if message == WM_WTSSESSION_CHANGE && wparam as u32 == WTS_SESSION_UNLOCK {
            if let Some(app) = APP.get() {
                notify_on_session_unlock(app);
                let app = app.clone();
                thread::spawn(move || sync_recent_events(&app, true));
            }
            return 0;
        }
        if message == 0x0002 {
            let _ = WTSUnRegisterSessionNotification(hwnd);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    APP.set(app)
        .map_err(|_| "解锁事件监听器重复初始化".to_string())?;
    const CLASS_NAME: &[u16] = &[
        67, 108, 111, 117, 100, 114, 111, 111, 109, 82, 101, 99, 111, 114, 100, 85, 110, 108, 111,
        99, 107, 0,
    ];
    unsafe {
        let instance =
            windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: CLASS_NAME.as_ptr(),
        };
        if RegisterClassW(&class) == 0 {
            return Err("注册 Windows 解锁事件窗口失败".into());
        }
        let hwnd = CreateWindowExW(
            0,
            CLASS_NAME.as_ptr(),
            CLASS_NAME.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err("创建 Windows 解锁事件窗口失败".into());
        }
        if WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) == 0 {
            return Err("订阅 Windows 解锁通知失败".into());
        }
        let mut message = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

fn notification_scheduler(app: AppHandle, wake: mpsc::Receiver<()>) {
    loop {
        let now = Local::now().naive_local();
        poll_and_notify(&app);
        let next = notification_deadline(&app, now)
            .map(|deadline| (deadline - now).to_std().unwrap_or(StdDuration::ZERO))
            .unwrap_or_else(|| StdDuration::from_secs(12 * 60 * 60));
        match wake.recv_timeout(next) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn sync_recent_events(app: &AppHandle, allow_first_unlock_notification: bool) {
    let now = Local::now().naive_local();
    let from = now.date().pred_opt().unwrap_or(now.date());
    let to = now.date();
    match query_windows_events(from, to) {
        Ok(events) => {
            let state = app.state::<AppState>();
            let mut first_unlock_to_notify = None;
            let first_unlock = events
                .iter()
                .filter(|event| event.kind == "unlock")
                .filter_map(|event| {
                    chrono::DateTime::parse_from_rfc3339(&event.timestamp)
                        .ok()
                        .filter(|timestamp| timestamp.with_timezone(&Local).date_naive() == to)
                        .map(|_| event)
                })
                .min_by_key(|event| event.timestamp.clone())
                .cloned();
            if let Ok(db) = state.db.lock() {
                for event in events {
                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&event.timestamp) {
                        let local = dt.naive_local();
                        let day = if local.time() <= NaiveTime::from_hms_opt(2, 0, 0).unwrap() {
                            local.date().pred_opt().unwrap_or(local.date())
                        } else {
                            local.date()
                        };
                        let key = format!(
                            "{}:{}:{}",
                            event.event_id.unwrap_or_default(),
                            event.timestamp,
                            event.kind
                        );
                        if let Ok(json) = serde_json::to_string(&event) {
                            let inserted = db.execute(
                                "INSERT OR IGNORE INTO raw_events(event_key,day,data) VALUES(?1,?2,?3)",
                                params![key, day.to_string(), json],
                            ).unwrap_or(0) == 1;
                            if inserted
                                && allow_first_unlock_notification
                                && first_unlock.as_ref().is_some_and(|first| {
                                    first.timestamp == event.timestamp && first.kind == event.kind
                                })
                            {
                                first_unlock_to_notify = Some(event.clone());
                            }
                        }
                    }
                }
                for day in [from, to] {
                    let _ = recalculate_day(&db, day, now);
                }
            }
            if let Ok(mut notified) = state.notified.lock() {
                notified.remove(&format!("windows-events-error:{}", now.date()));
            };
            if let Some(event) = first_unlock_to_notify {
                notify_first_unlock(app, &event);
            }
            wake_notification_scheduler(app);
            let _ = app.emit("attendance-events-updated", ());
        }
        Err(error) => {
            let state = app.state::<AppState>();
            if let Ok(mut notified) = state.notified.lock() {
                let key = format!("windows-events-error:{}", now.date());
                if notified.insert(key) {
                    notify(
                        app,
                        "读取系统事件失败",
                        &format!("{error}。应用不会请求管理员权限；打开对应月份时会自动重试，也可手动补录。"),
                    );
                }
            };
        }
    }
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        let _ = window.emit("tauri://focus-today", ());
    }
}

fn tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开考勤管理", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
    TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("考勤管理")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name("考勤管理")
                .build(),
        )
        .setup(|app| {
            let path = data_file(&app.handle())?;
            let is_first_open = !path.exists();
            let first_open_date = Local::now().date_naive();
            let db = open_db(&path)?;
            let stored_first_open: Option<String> = db
                .query_row(
                    "SELECT value FROM metadata WHERE key='first_open_date'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let first_open_date = stored_first_open
                .as_deref()
                .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
                .unwrap_or(first_open_date);
            if stored_first_open.is_none() {
                db.execute(
                    "INSERT INTO metadata(key,value) VALUES('first_open_date',?1)",
                    [first_open_date.to_string()],
                )?;
            }
            app.manage(AppState {
                db: Mutex::new(db),
                notified: Mutex::new(HashSet::new()),
                first_open_date,
            });
            #[cfg(windows)]
            if let Some(window) = app.get_webview_window("main") {
                let theme = app
                    .state::<AppState>()
                    .db
                    .lock()
                    .ok()
                    .and_then(|db| {
                        db.query_row(
                            "SELECT value FROM metadata WHERE key='theme_mode'",
                            [],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()
                        .ok()
                        .flatten()
                    })
                    .unwrap_or_else(|| "auto".to_string());
                let _ = set_native_titlebar_theme(&window, &theme);
            }
            tray(app)?;
            let handle = app.handle().clone();
            #[cfg(windows)]
            {
                let unlock_handle = handle.clone();
                thread::spawn(move || {
                    if let Err(error) = listen_for_session_unlock(unlock_handle.clone()) {
                        notify(
                            &unlock_handle,
                            "解锁通知监听失败",
                            &format!(
                                "无法订阅 Windows 解锁事件：{error}。将使用后台事件同步作为补偿。"
                            ),
                        );
                    }
                });
            }
            if is_first_open {
                let history_handle = handle.clone();
                thread::spawn(move || sync_initial_history(&history_handle, first_open_date));
            }
            let (sync_sender, sync_requests) = mpsc::channel::<bool>();
            app.manage(sync_sender);
            let (notification_sender, notification_requests) = mpsc::channel::<()>();
            app.manage(notification_sender);
            let event_sync_handle = handle.clone();
            thread::spawn(move || {
                let _ = sync_recent_events(&event_sync_handle, false);
                let mut foreground = false;
                let mut next_sync = std::time::Instant::now() + StdDuration::from_secs(2 * 60 * 60);
                loop {
                    let timeout = next_sync.saturating_duration_since(std::time::Instant::now());
                    match sync_requests.recv_timeout(timeout) {
                        Ok(is_focused) => {
                            foreground = is_focused;
                            if foreground {
                                let _ = sync_recent_events(&event_sync_handle, false);
                            }
                            next_sync = std::time::Instant::now()
                                + if foreground {
                                    StdDuration::from_secs(60)
                                } else {
                                    StdDuration::from_secs(2 * 60 * 60)
                                };
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let _ = sync_recent_events(&event_sync_handle, false);
                            next_sync = std::time::Instant::now()
                                + if foreground {
                                    StdDuration::from_secs(60)
                                } else {
                                    StdDuration::from_secs(2 * 60 * 60)
                                };
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            let notification_handle = handle.clone();
            thread::spawn(move || {
                notification_scheduler(notification_handle, notification_requests);
            });
            let calendar_handle = handle.clone();
            thread::spawn(move || loop {
                sync_calendar_if_due(&calendar_handle);
                let now = Local::now();
                let tomorrow = now
                    .date_naive()
                    .succ_opt()
                    .and_then(|day| day.and_hms_opt(0, 0, 0))
                    .and_then(|midnight| midnight.and_local_timezone(Local).earliest())
                    .unwrap_or_else(|| now + Duration::hours(24));
                let wait = (tomorrow - now)
                    .to_std()
                    .unwrap_or(StdDuration::from_secs(60))
                    .max(StdDuration::from_secs(60));
                thread::sleep(wait);
            });
            Ok(())
        })
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            tauri::WindowEvent::Focused(focused) => {
                if let Some(sender) = window.app_handle().try_state::<mpsc::Sender<bool>>() {
                    let _ = sender.send(*focused);
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_record,
            get_month,
            save_record,
            set_day_type,
            sync_month_events,
            set_company_workday,
            get_calendar_info,
            export_backup,
            import_backup,
            export_csv,
            get_settings,
            set_theme_mode,
            save_attendance_settings,
            set_notification_settings,
            check_for_updates,
            download_update,
            install_update,
            should_check_updates,
            mark_updates_checked,
            open_release_page,
            complete_intro,
            set_autostart
        ]);
    builder
        .run(tauri::generate_context!())
        .expect("启动考勤管理程序失败");
}

fn sync_initial_history(app: &AppHandle, first_open_date: NaiveDate) {
    let current_month =
        NaiveDate::from_ymd_opt(first_open_date.year(), first_open_date.month(), 1).unwrap();
    let mut month_start = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
    while month_start < current_month {
        let next_month = if month_start.month() == 12 {
            NaiveDate::from_ymd_opt(month_start.year() + 1, 1, 1).unwrap()
        } else {
            NaiveDate::from_ymd_opt(month_start.year(), month_start.month() + 1, 1).unwrap()
        };
        let month_end = next_month.pred_opt().unwrap();
        match query_windows_events(month_start, month_end) {
            Ok(events) => {
                if let Some(state) = app.try_state::<AppState>() {
                    if let Ok(db) = state.db.lock() {
                        if persist_synced_events(&db, &events, month_start, month_end).is_ok() {
                            let _ = mark_month_events_synced(
                                &db,
                                month_start.year(),
                                month_start.month(),
                            );
                        }
                    }
                }
            }
            Err(error) => {
                notify(
                    app,
                    "历史考勤同步受限",
                    &format!(
                        "{} 年 {} 月未能读取：{error}",
                        month_start.year(),
                        month_start.month()
                    ),
                );
            }
        }
        month_start = next_month;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_html_time_values_with_or_without_seconds() {
        assert_eq!(
            parse_clock_time("05:00").unwrap(),
            NaiveTime::from_hms_opt(5, 0, 0).unwrap()
        );
        assert_eq!(
            parse_clock_time("18:00").unwrap(),
            NaiveTime::from_hms_opt(18, 0, 0).unwrap()
        );
        assert_eq!(
            parse_clock_time("18:00:00").unwrap(),
            NaiveTime::from_hms_opt(18, 0, 0).unwrap()
        );
        assert!(parse_clock_time("").unwrap_err().contains("收到“”"));
    }

    #[test]
    fn unlock_notification_marks_late_arrival_and_carries_expected_end() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 13).unwrap();
        let settings = AttendanceSettings::default();
        let on_time = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
        let late = NaiveTime::from_hms_opt(9, 31, 0).unwrap();

        assert!(!arrival_is_late(on_time, &settings));
        assert!(arrival_is_late(late, &settings));
        assert!(first_unlock_message(
            late,
            expected_end_for_start(day, late, &settings),
            &settings
        )
        .contains("请注意迟到异常"));
        assert!(first_unlock_message(
            on_time,
            expected_end_for_start(day, on_time, &settings),
            &settings
        )
        .contains("预计下班时间：18:30"));

        let record = DailyRecord {
            day: day.to_string(),
            day_type: "workday".into(),
            calendar_name: None,
            calendar_marker: None,
            start_time: Some("09:31".into()),
            expected_end: Some("18:30".into()),
            end_time: None,
            overtime_minutes: 0,
            status: "迟到".into(),
            manual: false,
            note: String::new(),
        };
        let end_message = end_notification_message(&record, &settings);
        assert!(end_message.starts_with("今天预计下班时间是 18:30。"));
        assert!(end_message.contains("早上迟到异常"));
    }

    fn calendar_test_db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE calendar_days(day TEXT PRIMARY KEY, day_type TEXT NOT NULL, name TEXT NOT NULL, manual INTEGER NOT NULL DEFAULT 0); CREATE TABLE company_workdays(month TEXT PRIMARY KEY, day TEXT NOT NULL UNIQUE); CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();
        db
    }

    #[test]
    fn notification_settings_default_on_and_persist_independently() {
        let db = calendar_test_db();
        let defaults = read_attendance_settings(&db).unwrap();
        assert!(defaults.notify_at_end);
        assert!(defaults.notify_first_unlock);

        db.execute(
            "INSERT INTO metadata(key,value) VALUES('notify_at_end','0'),('notify_first_unlock','1')",
            [],
        )
        .unwrap();
        let settings = read_attendance_settings(&db).unwrap();
        assert!(!settings.notify_at_end);
        assert!(settings.notify_first_unlock);
    }

    #[test]
    fn synced_month_marker_is_idempotent_and_month_specific() {
        let db = calendar_test_db();
        mark_month_events_synced(&db, 2026, 11).unwrap();
        mark_month_events_synced(&db, 2026, 11).unwrap();

        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM metadata WHERE key='events_month_synced:2026-11'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let next_month: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='events_month_synced:2026-12')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!next_month);
    }

    #[test]
    fn empty_calendar_response_is_never_cached() {
        let db = calendar_test_db();
        let result = persist_calendar(
            &db,
            2027,
            CalendarApi {
                code: 0,
                holiday: std::collections::HashMap::new(),
            },
        );

        assert!(result.is_err());
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM calendar_days", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    fn event(timestamp: &str, kind: &str) -> RawEvent {
        RawEvent {
            timestamp: timestamp.into(),
            kind: kind.into(),
            source: "test".into(),
            event_id: None,
        }
    }

    #[test]
    fn company_saturday_skips_holiday_and_makeup_saturday_even_for_past_months() {
        let db = calendar_test_db();
        db.execute(
            "INSERT INTO calendar_days(day,day_type,name,manual) VALUES('2026-10-03','restday','国庆节',0)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO calendar_days(day,day_type,name,manual) VALUES('2026-10-10','workday','国庆节调休',0)",
            [],
        )
        .unwrap();
        let first_saturday = NaiveDate::from_ymd_opt(2026, 10, 3).unwrap();
        let makeup_saturday = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        let company_saturday = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        let queried_day = NaiveDate::from_ymd_opt(2026, 10, 22).unwrap();

        assert_eq!(
            company_workday_date(&db, queried_day).unwrap(),
            company_saturday
        );
        assert_eq!(day_type(&db, first_saturday).unwrap(), "restday");
        assert_eq!(day_type(&db, makeup_saturday).unwrap(), "workday");
        assert_eq!(day_type(&db, company_saturday).unwrap(), "workday");
        assert_eq!(
            calendar_metadata(&db, first_saturday).unwrap().1.as_deref(),
            Some("休")
        );
        assert_eq!(
            calendar_metadata(&db, makeup_saturday)
                .unwrap()
                .1
                .as_deref(),
            Some("班")
        );
        assert_eq!(
            calendar_metadata(&db, company_saturday)
                .unwrap()
                .1
                .as_deref(),
            Some("公司")
        );
    }

    #[test]
    fn manually_selected_company_saturday_overrides_estimate() {
        let db = calendar_test_db();
        db.execute(
            "INSERT INTO calendar_days(day,day_type,name,manual) VALUES('2026-10-03','restday','国庆节',0)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO company_workdays(month,day) VALUES('2026-10','2026-10-24')",
            [],
        )
        .unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 10, 24).unwrap();
        assert_eq!(company_workday_date(&db, date).unwrap(), date);
        assert_eq!(day_type(&db, date).unwrap(), "workday");
        assert_eq!(
            calendar_metadata(&db, date).unwrap().0.as_deref(),
            Some("手动指定公司上班日")
        );
    }

    #[test]
    fn workday_expected_end_and_threshold_round_down() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        let events = vec![
            event("2026-11-10T09:20:00", "unlock"),
            event("2026-11-10T20:40:00", "lock"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(21, 0, 0).unwrap());
        assert_eq!(record.expected_end.as_deref(), Some("18:20"));
        assert_eq!(record.overtime_minutes, 120);
        let events = vec![
            event("2026-11-10T09:20:00", "unlock"),
            event("2026-11-10T20:50:00", "lock"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(21, 0, 0).unwrap());
        assert_eq!(record.overtime_minutes, 150);
        let events = vec![
            event("2026-11-10T09:20:00", "unlock"),
            event("2026-11-10T21:00:21", "lock"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(21, 1, 0).unwrap());
        assert_eq!(record.overtime_minutes, 150);
        let events = vec![
            event("2026-11-10T09:30:41", "unlock"),
            event("2026-11-10T21:00:21", "lock"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(21, 1, 0).unwrap());
        assert_eq!(record.expected_end.as_deref(), Some("18:30"));
        assert_eq!(record.overtime_minutes, 150);
        let events = vec![
            event("2026-11-10T09:20:00", "unlock"),
            event("2026-11-10T21:23:01", "lock"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(21, 24, 0).unwrap());
        assert_eq!(record.overtime_minutes, 180);
    }

    #[test]
    fn rest_day_counts_from_first_unlock_without_two_hour_gate() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 8).unwrap();
        let events = vec![
            event("2026-11-08T09:10:00", "unlock"),
            event("2026-11-08T09:40:00", "lock"),
        ];
        let record = calculate_day(day, "restday", &events, day.and_hms_opt(10, 0, 0).unwrap());
        assert_eq!(record.overtime_minutes, 30);
        assert_eq!(record.expected_end, None);
        assert_eq!(record.status, "正常");
    }

    #[test]
    fn overtime_sessions_shorter_than_thirty_minutes_are_ignored() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 8).unwrap();
        let events = vec![
            event("2026-11-08T09:10:00", "unlock"),
            event("2026-11-08T09:39:59", "lock"),
        ];
        let record = calculate_day(day, "restday", &events, day.and_hms_opt(10, 0, 0).unwrap());
        assert_eq!(record.end_time.as_deref(), Some("09:39"));
        assert_eq!(record.overtime_minutes, 0);
    }

    #[test]
    fn short_remote_session_does_not_extend_prior_qualified_overtime() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        let events = vec![
            event("2026-11-09T09:00:00", "unlock"),
            event("2026-11-09T20:00:00", "lock"),
            event("2026-11-09T22:00:00", "unlock"),
            event("2026-11-09T22:20:00", "lock"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(23, 0, 0).unwrap());
        assert_eq!(record.end_time.as_deref(), Some("22:20"));
        assert_eq!(record.overtime_minutes, 120);
    }

    #[test]
    fn voluntary_rest_day_has_no_expected_end_or_early_departure_anomaly() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
        let events = vec![
            event("2026-11-14T11:32:37", "unlock"),
            event("2026-11-14T12:42:23", "lock"),
        ];
        let record = calculate_day(day, "restday", &events, day.and_hms_opt(13, 0, 0).unwrap());
        assert_eq!(record.start_time.as_deref(), Some("11:32"));
        assert_eq!(record.end_time.as_deref(), Some("12:42"));
        assert_eq!(record.expected_end, None);
        assert_eq!(record.status, "正常");
        assert_eq!(record.overtime_minutes, 60);
    }

    #[test]
    fn startup_and_shutdown_are_valid_attendance_events() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        let events = vec![
            event("2026-11-09T08:55:00", "startup"),
            event("2026-11-09T18:05:00", "shutdown"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(19, 0, 0).unwrap());
        assert_eq!(record.start_time.as_deref(), Some("08:55"));
        assert_eq!(record.end_time.as_deref(), Some("18:05"));
        assert_eq!(record.status, "正常");
    }

    #[test]
    fn early_departure_grace_requires_after_six_pm() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        let within_grace = vec![
            event("2026-11-09T09:10:00", "unlock"),
            event("2026-11-09T18:05:00", "lock"),
        ];
        let record = calculate_day(
            day,
            "workday",
            &within_grace,
            day.and_hms_opt(19, 0, 0).unwrap(),
        );
        assert_eq!(record.expected_end.as_deref(), Some("18:10"));
        assert_eq!(record.status, "正常");

        let at_six = vec![
            event("2026-11-09T09:10:00", "unlock"),
            event("2026-11-09T18:00:00", "lock"),
        ];
        let record = calculate_day(day, "workday", &at_six, day.and_hms_opt(19, 0, 0).unwrap());
        assert_eq!(record.status, "提前下班");

        let outside_grace = vec![
            event("2026-11-09T09:10:00", "unlock"),
            event("2026-11-09T17:59:59", "lock"),
        ];
        let record = calculate_day(
            day,
            "workday",
            &outside_grace,
            day.and_hms_opt(19, 0, 0).unwrap(),
        );
        assert_eq!(record.status, "提前下班");
    }

    #[test]
    fn configured_schedule_keeps_early_unlock_from_advancing_expected_end() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        let events = vec![
            event("2026-11-09T08:50:00", "unlock"),
            event("2026-11-09T18:05:00", "lock"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(19, 0, 0).unwrap());
        assert_eq!(record.expected_end.as_deref(), Some("18:00"));
        assert_eq!(record.status, "正常");
    }

    #[test]
    fn flexible_arrival_within_window_delays_expected_end_by_actual_lateness() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        let events = vec![
            event("2026-11-09T09:28:00", "unlock"),
            event("2026-11-09T18:28:00", "lock"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(19, 0, 0).unwrap());
        assert_eq!(record.expected_end.as_deref(), Some("18:28"));
        assert_eq!(record.status, "正常");
    }

    #[test]
    fn disabled_flex_schedule_flags_lateness_beyond_grace_window() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        let events = vec![
            event("2026-11-09T09:31:00", "unlock"),
            event("2026-11-09T18:05:00", "lock"),
        ];
        let settings = AttendanceSettings {
            flexible_enabled: false,
            ..AttendanceSettings::default()
        };
        let record = calculate_day_with_settings(
            day,
            "workday",
            &events,
            day.and_hms_opt(19, 0, 0).unwrap(),
            &settings,
        );
        assert_eq!(record.expected_end.as_deref(), Some("18:00"));
        assert_eq!(record.status, "迟到");
    }

    #[test]
    fn open_session_is_capped_at_next_day_two() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        let events = vec![event("2026-11-10T09:20:00", "unlock")];
        let record = calculate_day(
            day,
            "workday",
            &events,
            day.succ_opt().unwrap().and_hms_opt(2, 0, 0).unwrap(),
        );
        assert_eq!(record.end_time, None);
        assert_eq!(record.status, "整夜未锁");
        assert_eq!(record.overtime_minutes, 450);
    }

    #[test]
    fn actual_end_is_unknown_when_a_later_unlock_keeps_day_open() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        let events = vec![
            event("2026-11-10T09:09:00", "unlock"),
            event("2026-11-10T19:56:03", "lock"),
            event("2026-11-10T19:56:05", "unlock"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(20, 0, 0).unwrap());
        assert_eq!(record.end_time, None);
        assert_eq!(record.status, "进行中");
        assert_eq!(record.status, "进行中");
        assert_eq!(record.overtime_minutes, 0);
    }
}
