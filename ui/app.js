const tauriInvoke = window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
const tauriListen = window.__TAURI__?.event?.listen;
const invoke = (command, args = {}) => {
  if (!tauriInvoke) {
    return Promise.reject(new Error('当前页面未连接到 Tauri 后端。请使用 npm run dev 启动桌面窗口，不要直接打开 ui/index.html。'));
  }
  return tauriInvoke(command, args);
};
const $ = (id) => document.getElementById(id);
const APP_VERSION = '1.0.4';
const pad = (n) => String(n).padStart(2, '0');
function isoDate(d) { return `${d.getFullYear()}-${pad(d.getMonth()+1)}-${pad(d.getDate())}`; }
const today = new Date();
let selected = isoDate(today);
let viewMonth = new Date(today.getFullYear(), today.getMonth(), 1);
let monthRecords = [];
let currentRecord = null;
let toastTimer;
let firstOpenDate = isoDate(today);
let liveOvertimeExpectedEnd = null;
let liveOvertimeBaseMinutes = 0;

function setToast(message, type = 'success') {
  const el = $('toast'); el.textContent = message; el.className = `toast show ${type}`;
  clearTimeout(toastTimer); toastTimer = setTimeout(() => el.className = 'toast', 4500);
}
function prettyDate(day) {
  const d = new Date(`${day}T00:00:00`);
  return new Intl.DateTimeFormat('zh-CN', {year:'numeric',month:'long',day:'numeric',weekday:'long'}).format(d);
}
function hourText(minutes) { return `${Math.floor(minutes/60)}小时${minutes%60 ? ` ${minutes%60}分` : ''}`; }
function escapeHtml(value) { return String(value).replace(/[&<>"']/g, char => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[char])); }
function statusClass(status) {
  if (status === '正常') return 'status-ok';
  if (status === '进行中' || status === '待打卡') return 'status-muted';
  if (status === '待补录') return 'status-muted';
  if (status === '人工修改') return 'status-warn';
  return 'status-error';
}
function showView(name) {
  document.querySelectorAll('.view').forEach(el => el.classList.remove('active-view'));
  $(`view-${name}`).classList.add('active-view');
  document.querySelectorAll('.nav-item').forEach(el => el.classList.toggle('active', el.dataset.view === name));
  const labels = {today:['考勤工作台','今日概览'],calendar:['每日记录','月度日历'],stats:['统计分析','统计报表'],settings:['偏好设置','设置']};
  $('top-eyebrow').textContent = labels[name][0]; $('page-title').textContent = labels[name][1];
  $('top-actions').classList.toggle('hidden', name !== 'today');
  if (name === 'calendar' || name === 'stats') loadMonth();
  if (name === 'settings') loadSettings();
}
function focusToday() {
  selected = isoDate(new Date());
  $('selected-date').value = selected;
  updateSyncAvailability();
  showView('today');
  loadRecord();
}
async function loadRecord() {
  $('selected-date').value = selected;
  $('today-long-date').textContent = prettyDate(selected);
  $('banner-title').textContent = selected === isoDate(today) ? '今天的考勤' : `${selected} 的考勤`;
  try {
    const result = await invoke('get_record', {day:selected});
    currentRecord = result.record;
    renderRecord(result.record, result.events || []);
  } catch (e) { setToast(String(e), 'error'); }
}
function renderRecord(r, events) {
  $('metric-start').textContent = r.start_time || '--:--';
  $('metric-expected').textContent = r.expected_end || '--:--';
  $('metric-end').textContent = r.end_time || '--:--';
  $('metric-overtime').innerHTML = `${(r.overtime_minutes/60).toFixed(r.overtime_minutes%60 ? 1 : 0)}<span>小时</span>`;
  const todayRecordIsOpen = selected === isoDate(new Date()) && r.status === '进行中' && !r.manual;
  liveOvertimeExpectedEnd = r.day_type === 'workday' && todayRecordIsOpen && r.expected_end
    ? new Date(`${selected}T${r.expected_end}:00`)
    : null;
  liveOvertimeBaseMinutes = r.overtime_minutes;
  updateLiveOvertime();
  const subtitles = {
    '正常':'已根据本机锁屏、解锁及开关机事件完成计算',
    '进行中':'当天记录仍在进行，后续锁屏或关机时会补记下班时间',
    '待打卡':'当天尚未检测到上班事件',
    '无打卡记录':'工作日未检测到上班或启动记录，请核对并手动补录',
    '迟到':'到岗时间超出设置的弹性范围，请核对打卡记录',
    '提前下班':'结束时间早于预计下班时间，请核对记录',
    '人工修改':'此日期包含手动修正',
    '整夜未锁':'未检测到结束锁屏或关机，已按 02:00 截止',
  };
  $('banner-subtitle').textContent = subtitles[r.status] || '记录存在缺失，可同步事件或手动补充';
  $('record-status').textContent = r.status; $('record-status').className = `status-pill ${statusClass(r.status)}`;
  $('day-type').value = r.manual ? r.day_type : (r.day_type || 'workday');
  $('edit-start').value = r.start_time || ''; $('edit-expected').value = r.expected_end || ''; $('edit-end').value = r.end_time || '';
  $('edit-overtime').value = (r.overtime_minutes / 60) || 0; $('edit-note').value = r.note || '';
  $('source-note').textContent = r.manual ? '数据来源：人工修改' : '数据来源：系统事件 / 规则计算';
  $('event-count').textContent = `${events.length} 条`;
  $('event-list').innerHTML = events.length ? [...events].sort((a,b)=>Date.parse(a.timestamp)-Date.parse(b.timestamp)).map(e => {
    const t = new Date(e.timestamp).toLocaleTimeString('zh-CN',{hour:'2-digit',minute:'2-digit',second:'2-digit',hour12:false});
    const lock = e.kind === 'lock' || e.kind === 'shutdown';
    const label = e.kind === 'shutdown' ? '系统关机时间' : e.kind === 'startup' ? '系统开机时间' : lock ? '锁定电脑' : '解锁电脑';
    return `<div class="event-row ${lock?'lock':''}"><i class="event-dot"></i><span class="event-kind">${label}${e.source === 'manual'?' · 手动':''}</span><time class="event-time">${t}</time></div>`;
  }).join('') : '<div class="empty-state">尚无锁屏、解锁或开关机事件<br>可同步系统记录，或直接填写当日时间</div>';
}
function updateLiveOvertime() {
  if (!liveOvertimeExpectedEnd || Number.isNaN(liveOvertimeExpectedEnd.getTime())) return;
  const now = new Date();
  if (isoDate(now) !== selected) { liveOvertimeExpectedEnd = null; return; }
  const elapsedSeconds = Math.floor((now - liveOvertimeExpectedEnd) / 1000);
  if (elapsedSeconds < 2 * 60 * 60) return;
  const intervalSeconds = 30 * 60;
  const intervals = Math.floor(elapsedSeconds / intervalSeconds);
  const countableMinutes = Math.max(liveOvertimeBaseMinutes, intervals * 30);
  $('metric-overtime').innerHTML = `${(countableMinutes / 60).toFixed(countableMinutes % 60 ? 1 : 0)}<span>小时</span>`;
  const hours = Math.floor(elapsedSeconds / 3600);
  const minutes = Math.floor((elapsedSeconds % 3600) / 60);
  const seconds = elapsedSeconds % 60;
  $('overtime-detail').textContent = `实时加班 ${pad(hours)}:${pad(minutes)}:${pad(seconds)}`;
}
async function loadMonth() {
  const year = viewMonth.getFullYear(), month = viewMonth.getMonth()+1;
  $('calendar-month-label').textContent = `${year} 年 ${month} 月`;
  $('company-workday-date').min = `${year}-${pad(month)}-01`;
  $('company-workday-date').max = isoDate(new Date(year, month, 0));
  $('company-workday-date').value = `${year}-${pad(month)}-01`;
  $('sync-month-events').disabled = false;
  try { monthRecords = await invoke('get_month',{year,month}); renderMonth(); renderStats(); }
  catch(e) { setToast(String(e),'error'); }
}
async function syncMonthEvents() {
  try {
    const result=await invoke('sync_month_events',{year:viewMonth.getFullYear(),month:viewMonth.getMonth()+1});
    setToast(`本月同步完成：发现 ${result.found} 条事件，新增 ${result.added} 条`);
    await loadMonth();
    if (selected.startsWith(`${viewMonth.getFullYear()}-${pad(viewMonth.getMonth()+1)}`)) await loadRecord();
  } catch(e) { setToast(String(e),'error'); }
}
async function setCompanyWorkday() {
  const day=$('company-workday-date').value;
  if (!day) { setToast('请先选择日期','error'); return; }
  try {
    await invoke('set_company_workday',{day});
    setToast(`${day} 已设为本月公司上班日`);
    await loadMonth();
    if (selected===day) await loadRecord();
  } catch(e) { setToast(String(e),'error'); }
}
function renderMonth() {
  const first = new Date(viewMonth.getFullYear(),viewMonth.getMonth(),1);
  const offset = (first.getDay()+6)%7;
  const gridStart = new Date(first); gridStart.setDate(1-offset);
  const byDay = new Map(monthRecords.map(r=>[r.day,r]));
  const cells=[];
  for(let i=0;i<42;i++) {
    const d=new Date(gridStart); d.setDate(gridStart.getDate()+i); const key=isoDate(d); const r=byDay.get(key);
    const outside=d.getMonth()!==viewMonth.getMonth();
    const alert=r && isAnomaly(r.status);
    const weekend=d.getDay()===0||d.getDay()===6;
    const marker=r?.calendar_marker||'';
    const markerClass=marker==='休'?'holiday':marker==='班'?'makeup':marker==='公司'?'company':'manual-day';
    const endLabel=r?.status==='整夜未锁'||!r?.end_time?'未锁':r.end_time;
    const timeLabel=r?.start_time?`${r.start_time} → ${endLabel}`:(r?.status==='整夜未锁'?'未锁':'');
    const holidayName=r?.calendar_name?`<span class="cell-name">${escapeHtml(r.calendar_name)}</span>`:'';
    const title=[r?.calendar_name,marker==='公司'?'公司固定上班':marker==='调'?'手动调整':null,r?.status].filter(Boolean).join(' · ');
    cells.push(`<button class="calendar-cell ${outside?'outside':''} ${weekend?'weekend':''} ${key===selected?'selected':''} ${key===isoDate(new Date())?'today':''} ${alert?'alert':''} ${marker?`marked ${markerClass}`:''}" data-day="${key}" title="${escapeHtml(title)}"><span class="cell-head"><span class="cell-day">${d.getDate()}</span>${marker?`<span class="cell-marker ${markerClass}">${escapeHtml(marker)}</span>`:''}</span>${holidayName}${timeLabel?`<span class="cell-time">${escapeHtml(timeLabel)}</span>`:''}${r?.overtime_minutes?`<span class="cell-ot">加班 ${r.overtime_minutes/60}h</span>`:''}${alert?`<span class="cell-status">${escapeHtml(r.status)}</span>`:''}</button>`);
  }
  $('month-grid').innerHTML=cells.join('');
  $('month-grid').querySelectorAll('[data-day]').forEach(el=>el.addEventListener('click',()=>{selected=el.dataset.day;loadRecord();renderMonth();showView('today');}));
  const total=monthRecords.reduce((n,r)=>n+r.overtime_minutes,0);
  $('month-overtime').textContent=`${(total/60).toFixed(total%60?1:0)} 小时`;
  $('month-anomalies').textContent=`${monthRecords.filter(r=>isAnomaly(r.status)).length} 天`;
  $('month-workdays').textContent=`${monthRecords.filter(r=>r.day_type==='workday'&&r.start_time).length} 天`;
}
function renderStats() {
  const total=monthRecords.reduce((n,r)=>n+r.overtime_minutes,0);
  $('stats-hours').textContent=`${(total/60).toFixed(total%60?1:0)} 小时`;
  $('stats-days').textContent=`${monthRecords.filter(r=>r.overtime_minutes>0).length} 个加班日`;
  $('stats-anomalies').textContent=`${monthRecords.filter(r=>isAnomaly(r.status)).length} 天`;
  $('stats-manual').textContent=`${monthRecords.filter(r=>r.manual).length} 天`;
  $('stats-table').innerHTML=monthRecords.map(r=>`<tr><td>${r.day}</td><td>${r.day_type==='workday'?'工作日':'休息日'}</td><td>${r.start_time||'—'}</td><td>${r.expected_end||'—'}</td><td>${r.end_time||'—'}</td><td>${r.overtime_minutes?hourText(r.overtime_minutes):'—'}</td><td><span class="table-status">${r.status}</span></td></tr>`).join('');
}
function isAnomaly(status) {
  return ['无打卡记录','迟到','提前下班','整夜未锁','事件不完整'].includes(status);
}
async function syncRange() {
  if (selected < firstOpenDate) {
    setToast(`按日同步仅允许 ${firstOpenDate}（含）之后的日期；更早月份由首次启动的历史同步处理。`, 'error');
    return;
  }
  try { const result=await invoke('sync_events',{start:selected,end:selected}); setToast(`同步完成：发现 ${result.found} 条事件，新增 ${result.added} 条`); await loadRecord(); await loadMonth(); }
  catch(e) { setToast(String(e),'error'); }
}
async function syncCalendar(year=new Date().getFullYear()) {
  try { const info=await invoke('sync_calendar',{year}); setToast(`${year} 年法定工作日/节假日日历已同步`); await loadSettings(); await loadMonth(); await loadRecord(); }
  catch(e) { setToast(String(e),'error'); }
}
async function loadSettings() {
  try {
    const s=await invoke('get_settings'); firstOpenDate=s.first_open_date||firstOpenDate; $('autostart').checked=s.autostart; $('data-path').textContent=s.data_path; updateSyncAvailability();
    $('work-start').value=s.attendance.work_start;
    $('work-end').value=s.attendance.work_end;
    $('flexible-enabled').checked=s.attendance.flexible_enabled;
    $('flexible-minutes').value=s.attendance.flexible_minutes;
    $('flexible-minutes').disabled=!s.attendance.flexible_enabled;
    $('calendar-status').textContent=s.calendar.last_sync?`上次同步：${new Date(s.calendar.last_sync).toLocaleString('zh-CN')}`:'尚无同步信息';
    $('calendar-years').textContent=s.calendar.cached_years.length?`已缓存年份：${s.calendar.cached_years.join('、')}`:'尚无本地日历缓存';
    return s;
  } catch(e) { setToast(String(e),'error'); return null; }
}
async function checkForUpdates(silent = false) {
  const status = $('update-status');
  const notes = $('update-notes');
  const open = $('update-open');
  const install = $('update-install');
  const progress = $('update-progress');
  if (!silent) $('update-dialog').classList.remove('hidden');
  status.textContent = '正在连接 GitHub 检查最新版本…';
  notes.classList.add('hidden');
  open.classList.add('hidden');
  install.classList.add('hidden');
  progress.classList.add('hidden');
  try {
    await invoke('mark_updates_checked');
    const release = await invoke('check_for_updates');
    if (!release.update_available) {
      if (!silent) status.textContent = `当前版本 ${release.current_version} 已是最新版本。`;
      return;
    }
    $('update-dialog').classList.remove('hidden');
    status.textContent = `发现新版本 ${release.latest_version}（当前版本 ${release.current_version}）。`;
    if (release.notes) {
      notes.textContent = release.notes;
      notes.classList.remove('hidden');
    }
    open.dataset.url = release.html_url;
    open.dataset.downloadUrl = release.download_url;
    open.dataset.digest = release.digest || '';
    open.classList.remove('hidden');
  } catch (error) {
    if (!silent) status.textContent = `检查失败：${String(error)}`;
  }
}
async function downloadUpdate() {
  const open = $('update-open');
  const install = $('update-install');
  const progress = $('update-progress');
  const status = $('update-status');
  open.disabled = true;
  progress.value = 0;
  progress.classList.remove('hidden');
  status.textContent = '正在下载更新安装包…';
  try {
    const path = await invoke('download_update', {downloadUrl:open.dataset.downloadUrl, expectedDigest:open.dataset.digest || null});
    install.dataset.path = path;
    status.textContent = '更新已下载完成，点击“确认升级”后将关闭软件并启动安装程序。';
    open.classList.add('hidden');
    install.classList.remove('hidden');
  } catch (error) {
    status.textContent = `下载失败：${String(error)}`;
    open.disabled = false;
  }
}
async function checkForUpdatesDaily() {
  try {
    if (await invoke('should_check_updates')) await checkForUpdates(true);
  } catch (_) { /* Daily checks are silent; manual checks show errors. */ }
}
function updateSyncAvailability() {
  const button=$('sync-now');
  const allowed=selected>=firstOpenDate;
  button.disabled=!allowed;
  button.title=allowed?'同步所选日期的系统锁定/解锁事件':`首次打开日期为 ${firstOpenDate}，更早日期不能按日同步`;
}
function downloadFile(filename,content,type) {
  const blob=new Blob([content],{type}); const a=document.createElement('a'); a.href=URL.createObjectURL(blob); a.download=filename; a.click(); URL.revokeObjectURL(a.href);
}

document.querySelectorAll('.nav-item').forEach(el=>el.addEventListener('click',()=>showView(el.dataset.view)));
$('selected-date').addEventListener('change',e=>{selected=e.target.value;updateSyncAvailability();loadRecord();});
$('sync-now').addEventListener('click',syncRange);
$('month-prev').addEventListener('click',()=>{viewMonth.setMonth(viewMonth.getMonth()-1);loadMonth();});
$('month-next').addEventListener('click',()=>{viewMonth.setMonth(viewMonth.getMonth()+1);loadMonth();});
$('calendar-sync').addEventListener('click',()=>syncCalendar(viewMonth.getFullYear()));
$('sync-month-events').addEventListener('click',syncMonthEvents);
$('set-company-workday').addEventListener('click',setCompanyWorkday);
$('calendar-sync-settings').addEventListener('click',()=>syncCalendar(new Date().getFullYear()));
$('day-type').addEventListener('change',async e=>{try{await invoke('set_day_type',{day:selected,dayType:e.target.value});await loadRecord();await loadMonth();setToast('日期类型已更新');}catch(err){setToast(String(err),'error');}});
$('record-form').addEventListener('submit',async e=>{
  e.preventDefault(); const overtimeHours=Number($('edit-overtime').value||0); const record={...(currentRecord||{}),day:selected,day_type:$('day-type').value==='auto'?(currentRecord?.day_type||'workday'):$('day-type').value,start_time:$('edit-start').value||null,expected_end:$('edit-expected').value||null,end_time:$('edit-end').value||null,overtime_minutes:Math.round(overtimeHours*2)*30,status:'人工修改',manual:true,note:$('edit-note').value};
  try{await invoke('save_record',{record});await loadRecord();await loadMonth();setToast('当日记录已保存');}catch(err){setToast(String(err),'error');}
});
$('autostart').addEventListener('change',async e=>{try{await invoke('set_autostart',{enabled:e.target.checked});setToast(e.target.checked?'已启用开机自启':'已关闭开机自启');}catch(err){e.target.checked=!e.target.checked;setToast(String(err),'error');}});
$('flexible-enabled').addEventListener('change',e=>{$('flexible-minutes').disabled=!e.target.checked;});
$('save-attendance-settings').addEventListener('click',async()=>{
  const button=$('save-attendance-settings');
  button.disabled=true;
  try {
    await invoke('save_attendance_settings',{
      workStart:$('work-start').value,
      workEnd:$('work-end').value,
      flexibleEnabled:$('flexible-enabled').checked,
      flexibleMinutes:Number($('flexible-minutes').value)
    });
    await loadRecord();
    await loadMonth();
    setToast('考勤时间已保存，自动记录已按新规则重算');
  } catch(error) { setToast(String(error),'error'); }
  finally { button.disabled=false; }
});
$('export-csv').addEventListener('click',async()=>{try{const csv=await invoke('export_csv',{year:viewMonth.getFullYear(),month:viewMonth.getMonth()+1});downloadFile(`考勤记录-${viewMonth.getFullYear()}-${pad(viewMonth.getMonth()+1)}.csv`,'\ufeff'+csv,'text/csv;charset=utf-8');}catch(e){setToast(String(e),'error');}});
$('backup-export').addEventListener('click',async()=>{try{const json=await invoke('export_backup');downloadFile(`考勤备份-${isoDate(new Date())}.json`,json,'application/json;charset=utf-8');setToast('备份文件已导出');}catch(e){setToast(String(e),'error');}});
$('backup-import').addEventListener('change',async e=>{const file=e.target.files?.[0];if(!file)return;try{await invoke('import_backup',{data:await file.text()});setToast('备份已导入');await loadRecord();await loadMonth();}catch(err){setToast(String(err),'error');}finally{e.target.value='';}});
$('intro-confirm').addEventListener('click',async()=>{
  try { await invoke('complete_intro'); $('intro-dialog').classList.add('hidden'); }
  catch (error) { setToast(String(error),'error'); }
});
$('check-updates').addEventListener('click',()=>checkForUpdates(false));
$('update-close').addEventListener('click',()=>$('update-dialog').classList.add('hidden'));
$('update-open').addEventListener('click',downloadUpdate);
$('update-install').addEventListener('click',async()=>{
  try { await invoke('install_update',{installerPath:$('update-install').dataset.path}); }
  catch (error) { setToast(String(error),'error'); }
});
function renderDownloadProgress(event){
  const detail=event.detail||{};
  if (detail.total>0) $('update-progress').value=Math.min(100,detail.downloaded/detail.total*100);
  $('update-status').textContent=detail.total>0
    ? `正在下载更新：${Math.round(detail.downloaded/detail.total*100)}%`
    : `正在下载更新：${Math.round(detail.downloaded/1024/1024)} MB`;
}
window.addEventListener('tauri://focus-today',focusToday);
window.addEventListener('DOMContentLoaded',async()=>{
  if (tauriListen) await tauriListen('update-download-progress', renderDownloadProgress);
  setInterval(updateLiveOvertime, 1000);
  setInterval(checkForUpdatesDaily, 60 * 60 * 1000);
  $('selected-date').value=selected;
  $('sync-now').disabled=true;
  if (!tauriInvoke) setToast('当前是界面预览模式；数据功能需要在 Tauri 桌面窗口中运行。','error');
  await loadRecord(); await loadMonth();
  const settings=await loadSettings();
  $('app-version').textContent=`版本 ${settings?.version||APP_VERSION}`;
  if (settings?.intro_pending) $('intro-dialog').classList.remove('hidden');
  await checkForUpdatesDaily();
});

