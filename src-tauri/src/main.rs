#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet, fs, path::PathBuf, process::Command, sync::Mutex, thread,
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
const EARLY_DEPARTURE_GRACE_MINUTES: i64 = 10;
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_REPOSITORY: &str = "Siman8/CloudroomRecord";

struct AppState {
    db: Mutex<Connection>,
    notified: Mutex<HashSet<String>>,
    first_open_date: NaiveDate,
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

#[derive(Serialize, Deserialize)]
struct BackupFile {
    schema_version: u32,
    exported_at: String,
    records: Vec<DailyRecord>,
    events: Vec<RawEvent>,
    calendar: Vec<(String, String, String, bool)>,
    #[serde(default)]
    company_workdays: Vec<(String, String)>,
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

fn calculate_day(
    day: NaiveDate,
    kind: &str,
    events: &[RawEvent],
    now: NaiveDateTime,
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
    let expected = start.map(|time| time + Duration::hours(9));
    let day_is_complete = day < now.date() || reached_cutoff;
    let status = if start.is_none() {
        if workday && day_is_complete {
            "无打卡记录"
        } else if day == now.date() {
            "待打卡"
        } else {
            "待补录"
        }
    } else if completed_end.is_some() {
        if workday
            && completed_end.zip(expected).is_some_and(|(end, expected)| {
                let expected = expected.time();
                let end = end.time();
                end <= NaiveTime::from_hms_opt(18, 0, 0).unwrap()
                    || (end < expected
                        && (expected - end).num_minutes() > EARLY_DEPARTURE_GRACE_MINUTES)
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
    let calculation_end = completed_end.or_else(|| reached_cutoff.then(|| cutoff(day)));
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
        end_time: end.map(|v| v.format("%H:%M").to_string()),
        overtime_minutes,
        status,
        manual: false,
        note: String::new(),
    }
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
    let mut record = calculate_day(day, &kind, &events, now);
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
fn save_record(state: State<'_, AppState>, mut record: DailyRecord) -> Result<(), String> {
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
    Ok(())
}

#[tauri::command]
fn set_day_type(
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
    Ok(())
}

fn query_windows_events(start: NaiveDate, end: NaiveDate) -> Result<Vec<RawEvent>, String> {
    let start_stamp = start.format("%Y-%m-%dT00:00:00").to_string();
    let end_stamp = end
        .succ_opt()
        .unwrap_or(end)
        .format("%Y-%m-%dT02:00:00")
        .to_string();
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$from = [datetime]::Parse('{start_stamp}')
$to = [datetime]::Parse('{end_stamp}')
$items = @(Get-WinEvent -FilterHashtable @{{LogName='Microsoft-Windows-Winlogon/Operational'; Id=811; StartTime=$from; EndTime=$to}} -ErrorAction SilentlyContinue | ForEach-Object {{
  $xml = [xml]$_.ToXml()
  $eventCode = ($xml.Event.EventData.Data | Where-Object {{ $_.Name -eq 'Event' }}).'#text'
  $subscriber = ($xml.Event.EventData.Data | Where-Object {{ $_.Name -eq 'SubscriberName' }}).'#text'
  $eventSid = $xml.Event.System.Security.UserID
  if ($eventSid -eq $sid -and $subscriber -eq 'TermSrv' -and ($eventCode -eq '4' -or $eventCode -eq '5')) {{
    [pscustomobject]@{{ timestamp=$_.TimeCreated.ToString('o'); kind=$(if ($eventCode -eq '5') {{'unlock'}} else {{'lock'}}); source='windows-winlogon-log'; event_id=$_.RecordId }}
  }}
}})
$items += @(Get-WinEvent -FilterHashtable @{{LogName='System'; Id=1074,6005,6006,12,13; StartTime=$from; EndTime=$to}} -ErrorAction SilentlyContinue | ForEach-Object {{
  $kind = $null
  if ($_.Id -eq 1074 -or $_.Id -eq 6006 -or ($_.Id -eq 13 -and $_.ProviderName -eq 'Microsoft-Windows-Kernel-General')) {{ $kind = 'shutdown' }}
  elseif ($_.Id -eq 6005 -or ($_.Id -eq 12 -and $_.ProviderName -eq 'Microsoft-Windows-Kernel-General')) {{ $kind = 'startup' }}
  if ($kind) {{ [pscustomobject]@{{ timestamp=$_.TimeCreated.ToString('o'); kind=$kind; source='windows-system-log'; event_id=$_.RecordId }} }}
}})
ConvertTo-Json -InputObject @($items) -Compress
"#
    );
    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &script,
    ]);
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);
    let output = command
        .output()
        .map_err(|e| format!("无法启动 PowerShell：{e}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("读取 Windows 会话事件日志失败：{error}"));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 Windows 事件结果失败：{e}"))?;
    let values = value.as_array().cloned().unwrap_or_else(|| vec![value]);
    values
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
        .collect()
}

#[tauri::command]
fn sync_events(
    state: State<'_, AppState>,
    start: String,
    end: String,
) -> Result<serde_json::Value, String> {
    let first = parse_date(&start)?;
    let last = parse_date(&end)?;
    if last < first || (last - first).num_days() > 366 {
        return Err("日期范围无效（最多 367 天）".into());
    }
    if first < state.first_open_date || last < state.first_open_date {
        return Err(format!(
            "按日同步仅允许同步首次打开日期 {}（含）之后的数据；此前月份由首次启动的月度同步处理。",
            state.first_open_date
        ));
    }
    let events = query_windows_events(first, last)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let added = persist_synced_events(&db, &events, first, last)?;
    Ok(serde_json::json!({"found": events.len(), "added": added}))
}

#[tauri::command]
fn sync_month_events(
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
    Ok(serde_json::json!({"found": events.len(), "added": added}))
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

#[tauri::command]
fn sync_calendar(
    app: AppHandle,
    state: State<'_, AppState>,
    year: i32,
) -> Result<CalendarInfo, String> {
    if !(2020..=2035).contains(&year) {
        return Err("日历同步年份应在 2020–2035 之间".into());
    }
    let response = match fetch_calendar(year) {
        Ok(value) => value,
        Err(error) => {
            notify(&app, "节假日日历同步失败", &error);
            return Err(error);
        }
    };
    let db = state.db.lock().map_err(|e| e.to_string())?;
    persist_calendar(&db, year, response)?;
    calendar_info(&db, None)
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
fn export_backup(state: State<'_, AppState>) -> Result<String, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let records = {
        let mut stmt = db
            .prepare("SELECT data FROM daily_records ORDER BY day")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .map(|r| {
                serde_json::from_str(&r.map_err(|e| e.to_string())?).map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<DailyRecord>, String>>()?;
        rows
    };
    let events = {
        let mut stmt = db
            .prepare("SELECT data FROM raw_events ORDER BY day,event_key")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .map(|r| {
                serde_json::from_str(&r.map_err(|e| e.to_string())?).map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<RawEvent>, String>>()?;
        rows
    };
    let calendar = {
        let mut stmt = db
            .prepare("SELECT day,day_type,name,manual FROM calendar_days ORDER BY day")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                ))
            })
            .map_err(|e| e.to_string())?
            .map(|r| r.map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, String>>()?;
        rows
    };
    let company_workdays = {
        let mut stmt = db
            .prepare("SELECT month,day FROM company_workdays ORDER BY month")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| e.to_string())?
            .map(|row| row.map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, String>>()?;
        rows
    };
    serde_json::to_string_pretty(&BackupFile {
        schema_version: 1,
        exported_at: Local::now().to_rfc3339(),
        records,
        events,
        calendar,
        company_workdays,
    })
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn import_backup(state: State<'_, AppState>, data: String) -> Result<(), String> {
    let backup: BackupFile =
        serde_json::from_str(&data).map_err(|e| format!("备份文件无效：{e}"))?;
    if backup.schema_version != 1 {
        return Err("该备份文件版本不受支持".into());
    }
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    for record in backup.records {
        let json = serde_json::to_string(&record).map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO daily_records(day,data) VALUES(?1,?2) ON CONFLICT(day) DO UPDATE SET data=excluded.data", params![record.day,json]).map_err(|e| e.to_string())?;
    }
    for event in backup.events {
        let dt = chrono::DateTime::parse_from_rfc3339(&event.timestamp)
            .map_err(|e| e.to_string())?
            .naive_local();
        let day = if dt.time() <= NaiveTime::from_hms_opt(2, 0, 0).unwrap() {
            dt.date().pred_opt().unwrap_or(dt.date())
        } else {
            dt.date()
        };
        let key = format!("backup:{}:{}", event.timestamp, event.kind);
        let json = serde_json::to_string(&event).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR IGNORE INTO raw_events(event_key,day,data) VALUES(?1,?2,?3)",
            params![key, day.to_string(), json],
        )
        .map_err(|e| e.to_string())?;
    }
    for (day, kind, name, manual) in backup.calendar {
        tx.execute("INSERT INTO calendar_days(day,day_type,name,manual) VALUES(?1,?2,?3,?4) ON CONFLICT(day) DO UPDATE SET day_type=excluded.day_type,name=excluded.name,manual=excluded.manual",params![day,kind,name,manual as i32]).map_err(|e|e.to_string())?;
    }
    for (month, day) in backup.company_workdays {
        tx.execute(
            "INSERT INTO company_workdays(month,day) VALUES(?1,?2) ON CONFLICT(month) DO UPDATE SET day=excluded.day",
            params![month, day],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
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
    Ok(
        serde_json::json!({"autostart":autostart,"data_path":path.to_string_lossy(),"calendar":info,"first_open_date":state.first_open_date,"intro_pending":intro_shown.as_deref()!=Some("1"),"version":APP_VERSION}),
    )
}

#[tauri::command]
fn check_for_updates() -> Result<serde_json::Value, String> {
    let response: serde_json::Value = ureq::AgentBuilder::new()
        .timeout_connect(StdDuration::from_secs(5))
        .timeout_read(StdDuration::from_secs(10))
        .build()
        .get(&format!(
            "https://api.github.com/repos/{GITHUB_REPOSITORY}/releases/latest"
        ))
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "CloudroomRecord")
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(status, response) => {
                format!("GitHub API 返回 HTTP {status}：{}", response.status_text())
            }
            other => format!("无法连接 GitHub：{other}"),
        })?
        .into_json()
        .map_err(|error| format!("无法解析 GitHub Release 信息：{error}"))?;

    let tag = response["tag_name"]
        .as_str()
        .ok_or("GitHub Release 缺少版本标签")?;
    let latest_version = tag.strip_prefix('v').unwrap_or(tag);
    let html_url = response["html_url"]
        .as_str()
        .ok_or("GitHub Release 缺少下载地址")?;
    Ok(serde_json::json!({
        "current_version": APP_VERSION,
        "latest_version": latest_version,
        "update_available": version_is_newer(latest_version, APP_VERSION),
        "html_url": html_url,
        "notes": response["body"].as_str().unwrap_or("")
    }))
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

fn poll_and_notify(app: &AppHandle) {
    let state = app.state::<AppState>();
    let now = Local::now().naive_local();
    let year = now.date().year();
    let calendar_due = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            let last_year: Option<String> = db
                .query_row(
                    "SELECT value FROM metadata WHERE key='calendar_last_year'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .ok()
                .flatten();
            let last_sync: Option<String> = db
                .query_row(
                    "SELECT value FROM metadata WHERE key='calendar_last_sync'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .ok()
                .flatten();
            let last_attempt: Option<String> = db
                .query_row(
                    "SELECT value FROM metadata WHERE key='calendar_last_attempt'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .ok()
                .flatten();
            let attempted_recently = last_attempt
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
                .map(|value| {
                    Local::now().signed_duration_since(value.with_timezone(&Local))
                        < Duration::hours(6)
                })
                .unwrap_or(false);
            let recent = last_sync
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
                .map(|value| {
                    value.with_timezone(&Local).date_naive()
                        >= Local::now().date_naive() - Duration::days(30)
                })
                .unwrap_or(false);
            Some(
                !attempted_recently && (last_year.as_deref() != Some(&year.to_string()) || !recent),
            )
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
                    let _ = persist_calendar(&db, year, calendar);
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
    let from = now.date().pred_opt().unwrap_or(now.date());
    let to = now.date();
    if let Ok(events) = query_windows_events(from, to) {
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
                        let _ = db.execute(
                            "INSERT OR IGNORE INTO raw_events(event_key,day,data) VALUES(?1,?2,?3)",
                            params![key, day.to_string(), json],
                        );
                    }
                }
            }
            for day in [from, to] {
                if let Ok(record) = recalculate_day(&db, day, now) {
                    if let Some(expected) = record
                        .expected_end
                        .as_deref()
                        .and_then(|t| NaiveTime::parse_from_str(t, "%H:%M").ok())
                        .map(|t| day.and_time(t))
                    {
                        let key = format!("{}:end", day);
                        if day == now.date()
                            && now >= expected
                            && now < expected + Duration::minutes(5)
                            && claim_notification(&db, &key)
                        {
                            notify(
                                app,
                                "下班时间到了",
                                &format!(
                                    "今天预计下班时间是 {}。",
                                    record.expected_end.unwrap_or_default()
                                ),
                            );
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
    let open = MenuItem::with_id(app, "open", "打开考勤记录", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
    TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("考勤记录")
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
                .app_name("考勤记录")
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
            tray(app)?;
            let handle = app.handle().clone();
            if is_first_open {
                let history_handle = handle.clone();
                thread::spawn(move || sync_initial_history(&history_handle, first_open_date));
            }
            thread::spawn(move || loop {
                thread::sleep(StdDuration::from_secs(60));
                poll_and_notify(&handle);
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_record,
            get_month,
            save_record,
            set_day_type,
            sync_events,
            sync_month_events,
            set_company_workday,
            sync_calendar,
            get_calendar_info,
            export_backup,
            import_backup,
            export_csv,
            get_settings,
            check_for_updates,
            should_check_updates,
            mark_updates_checked,
            open_release_page,
            complete_intro,
            set_autostart
        ]);
    builder
        .run(tauri::generate_context!())
        .expect("启动考勤记录程序失败");
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
                        let _ = persist_synced_events(&db, &events, month_start, month_end);
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

    fn calendar_test_db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE calendar_days(day TEXT PRIMARY KEY, day_type TEXT NOT NULL, name TEXT NOT NULL, manual INTEGER NOT NULL DEFAULT 0); CREATE TABLE company_workdays(month TEXT PRIMARY KEY, day TEXT NOT NULL UNIQUE);",
        )
        .unwrap();
        db
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
    fn latest_lock_remains_visible_when_followed_by_unlock() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        let events = vec![
            event("2026-11-10T09:09:00", "unlock"),
            event("2026-11-10T19:56:03", "lock"),
            event("2026-11-10T19:56:05", "unlock"),
        ];
        let record = calculate_day(day, "workday", &events, day.and_hms_opt(20, 0, 0).unwrap());
        assert_eq!(record.end_time.as_deref(), Some("19:56"));
        assert_eq!(record.status, "进行中");
        assert_eq!(record.overtime_minutes, 0);
    }
}
