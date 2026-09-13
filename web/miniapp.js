/* WhereBus 用户面板（Telegram Mini App）：管理个人偏好、收藏与车次监控。
   身份由服务端校验 initData 签名后签发会话令牌；站点管理在网页端 /admin。 */
const tg = window.Telegram && window.Telegram.WebApp;
const $ = id => document.getElementById(id);
const state = { token: null, me: null, pick: { service: null, line: null, stops: [], order: null } };

function el(tag, text, className) {
  const node = document.createElement(tag);
  if (text != null) node.textContent = text;
  if (className) node.className = className;
  return node;
}
function status(text, isError = false) {
  $('status').textContent = text;
  $('status').classList.toggle('error', isError);
}
function since(unix) {
  if (!unix) return '—';
  const minutes = Math.max(0, (Date.now() / 1000 - unix) / 60);
  if (minutes < 60) return `${Math.round(minutes)} 分钟前`;
  if (minutes < 1440) return `${Math.round(minutes / 60)} 小时前`;
  return `${Math.round(minutes / 1440)} 天前`;
}
function duration(secs) {
  const hours = Math.floor(secs / 3600), minutes = Math.floor((secs % 3600) / 60);
  return hours ? `${hours} 小时 ${minutes} 分钟` : `${minutes} 分钟`;
}
function confirmDialog(message) {
  return new Promise(resolve => {
    if (tg && tg.showConfirm) tg.showConfirm(message, resolve);
    else resolve(window.confirm(message));
  });
}

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: {
      'Content-Type': 'application/json',
      ...(state.token ? { Authorization: `Bearer ${state.token}` } : {}),
      ...(options.headers || {}),
    },
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || `请求失败（${response.status}）`);
  return body;
}
/* 公交数据走公开的查询接口，与网页版同源同一套 */
async function bus(path, params) {
  const response = await fetch(`/api/${path}?` + new URLSearchParams(params), { cache: 'no-store' });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || `请求失败（${response.status}）`);
  return body.data;
}

/* ─── 我的 ─── */

function renderMe(me) {
  state.me = me;
  const view = $('view-me');
  view.replaceChildren();

  const profile = el('div', null, 'card');
  profile.append(el('h2', '个人资料'));
  profile.append(el('div', `${me.name || '未命名'}${me.username ? ' @' + me.username : ''}`));
  profile.append(el('div', `累计查询 ${me.queries} 次 · 收藏 ${me.favorites.length} 条 · 最近活跃 ${since(me.last_seen)}`, 'muted'));
  const cityRow = el('div', null, 'row');
  cityRow.append(el('div', `城市：${me.city || '未选择'}`));
  const change = el('button', '切换城市', 'ghost');
  change.onclick = () => pickCity(profile, change);
  cityRow.append(change);
  profile.append(cityRow);
  view.append(profile);

  const settings = el('div', null, 'card');
  settings.append(el('h2', '提醒偏好'));
  settings.append(el('p', me.alerts.using_defaults ? '当前跟随全局默认值，改动任一项后变为个人设置。' : '当前使用个人设置。', 'muted'));
  me.alerts.fields.forEach(field => {
    const row = el('div', null, 'row');
    row.append(el('div', field.label));
    const stepper = el('div', null, 'stepper');
    const minus = el('button', '−', 'ghost');
    const value = el('output', `${me.alerts.values[field.key]} ${field.unit}`);
    const plus = el('button', '+', 'ghost');
    const change = delta => async () => {
      const next = Math.min(field.max, Math.max(field.min, me.alerts.values[field.key] + delta * field.step));
      minus.disabled = plus.disabled = true;
      try { renderMe(await api('/api/me/settings', { method: 'POST', body: JSON.stringify({ values: { [field.key]: next } }) })); }
      catch (error) { status(error.message, true); minus.disabled = plus.disabled = false; }
    };
    minus.onclick = change(-1);
    plus.onclick = change(1);
    stepper.append(minus, value, plus);
    row.append(stepper);
    settings.append(row);
  });
  const reset = el('button', '恢复默认', 'ghost');
  reset.onclick = async () => {
    try { renderMe(await api('/api/me/settings', { method: 'POST', body: JSON.stringify({ reset: true }) })); status('已恢复为全局默认值。'); }
    catch (error) { status(error.message, true); }
  };
  settings.append(reset);
  view.append(settings);

  const favorites = el('div', null, 'card');
  favorites.append(el('h2', `收藏车次（${me.favorites.length}）`));
  if (!me.favorites.length) favorites.append(el('p', '还没有收藏。在机器人里查看线路到站后点「⭐ 收藏这一趟」。', 'muted'));
  me.favorites.forEach(favorite => {
    const row = el('div', null, 'row');
    const info = el('div');
    info.append(el('div', `${favorite.line_name} @ ${favorite.station_name}`));
    info.append(el('div', `${favorite.city_label} · 查过 ${favorite.hits} 次`, 'muted'));
    row.append(info);
    const buttons = el('div', null, 'stepper');
    const watch = el('button', '监控', 'ghost');
    watch.onclick = () => startWatch({ service: favorite.service, direction: favorite.direction, order: favorite.order });
    const remove = el('button', '删除', 'ghost');
    remove.onclick = async () => {
      remove.disabled = true;
      try {
        renderMe(await api('/api/me/favorites/delete', {
          method: 'POST',
          body: JSON.stringify({ service: favorite.service, direction: favorite.direction, order: favorite.order }),
        }));
      } catch (error) { status(error.message, true); remove.disabled = false; }
    };
    buttons.append(watch, remove);
    row.append(buttons);
    favorites.append(row);
  });
  view.append(favorites);

  const habits = el('div', null, 'card');
  habits.append(el('h2', '乘车习惯'));
  const max = Math.max(1, ...me.hour_histogram);
  const blocks = '▁▂▃▄▅▆▇█';
  habits.append(el('div', me.hour_histogram.map(v => (v ? blocks[Math.round((v / max) * 7)] : '·')).join(''), 'bars'));
  habits.append(el('div', '0     6     12    18   23', 'bars muted'));
  me.habits.slice(0, 5).forEach((habit, index) => habits.append(el('div', `${index + 1}. ${habit.label} — ${habit.total} 次`, 'muted')));
  if (!me.habits.length) habits.append(el('p', '还没有查询记录。', 'muted'));
  view.append(habits);

  const danger = el('div', null, 'card');
  danger.append(el('h2', '我的数据'));
  danger.append(el('p', '删除后，你的城市选择、收藏、习惯统计与监控任务都会被清空，且无法恢复。', 'muted'));
  const forget = el('button', '删除我的全部数据', 'danger');
  forget.onclick = async () => {
    if (!(await confirmDialog('确认删除你的全部数据？此操作不可撤销。'))) return;
    try {
      await api('/api/me', { method: 'DELETE' });
      status('数据已删除，可以关闭面板。');
      $('view-me').replaceChildren(el('p', '数据已删除。', 'muted'));
      $('view-watch').replaceChildren();
    } catch (error) { status(error.message, true); }
  };
  danger.append(forget);
  view.append(danger);
}

async function pickCity(card, button) {
  button.disabled = true;
  try {
    const services = await bus('services', {});
    const box = el('div', null, 'stack');
    const select = el('select');
    services.forEach(service => {
      const option = el('option', `${service.province} · ${service.city} / ${service.provider}`);
      option.value = service.id;
      if (service.id === state.me.service) option.selected = true;
      select.append(option);
    });
    const save = el('button', '保存城市');
    save.onclick = async () => {
      save.disabled = true;
      try {
        renderMe(await api('/api/me/city', { method: 'POST', body: JSON.stringify({ service: select.value }) }));
        status('城市已更新。');
      } catch (error) { status(error.message, true); save.disabled = false; }
    };
    box.append(select, save);
    card.append(box);
  } catch (error) { status(error.message, true); }
  button.disabled = false;
}

/* ─── 车次监控 ─── */

function renderWatch() {
  const me = state.me;
  const view = $('view-watch');
  view.replaceChildren();

  const current = el('div', null, 'card');
  current.append(el('h2', '当前监控'));
  if (me.watch) {
    current.append(el('div', me.watch.label));
    current.append(el('div', `已监控 ${duration(Math.max(0, Date.now() / 1000 - me.watch.started_at))} · 提醒发送到你与机器人的聊天里`, 'muted'));
    const stop = el('button', '停止监控', 'danger');
    stop.onclick = async () => {
      stop.disabled = true;
      try { const result = await api('/api/me/watch/stop', { method: 'POST', body: '{}' }); renderMe(result.me); renderWatch(); status('监控已停止。'); }
      catch (error) { status(error.message, true); stop.disabled = false; }
    };
    current.append(stop);
  } else {
    current.append(el('p', '没有进行中的监控。选好线路与上车站即可开始，同一时间只能监控一趟车。', 'muted'));
  }
  view.append(current);

  if (me.favorites.length) {
    const quick = el('div', null, 'card');
    quick.append(el('h2', '从收藏开始'));
    const list = el('div', null, 'list');
    me.favorites.forEach(favorite => {
      const button = el('button', `${favorite.line_name} @ ${favorite.station_name}`, 'ghost wide');
      button.onclick = () => startWatch({ service: favorite.service, direction: favorite.direction, order: favorite.order });
      list.append(button);
    });
    quick.append(list);
    view.append(quick);
  }

  const search = el('div', null, 'card');
  search.append(el('h2', '搜索线路'));
  if (!me.service) {
    search.append(el('p', '先在「我的」里选择城市。', 'muted'));
    view.append(search);
    return;
  }
  const form = el('form', null, 'stack');
  const keyword = el('input');
  keyword.placeholder = '线路名，例如 1路 / K155';
  const submit = el('button', '搜索');
  form.append(keyword, submit);
  const results = el('div', null, 'list');
  form.onsubmit = async event => {
    event.preventDefault();
    if (!keyword.value.trim()) return;
    submit.disabled = true;
    results.replaceChildren(el('p', '正在查询…', 'muted'));
    try {
      const lines = await bus('lines', { service: me.service, q: keyword.value.trim() });
      results.replaceChildren();
      if (!lines.length) results.append(el('p', '没有找到匹配的线路。', 'muted'));
      lines.slice(0, 30).forEach(line => {
        const button = el('button', `${line.name} → ${line.endpoints.terminus || '终点待更新'}`, 'ghost wide');
        button.onclick = () => pickStop(line, results);
        results.append(button);
      });
    } catch (error) { results.replaceChildren(el('p', error.message, 'error')); }
    submit.disabled = false;
  };
  search.append(form, results);
  view.append(search);
}

async function pickStop(line, container) {
  container.replaceChildren(el('p', '正在加载站点…', 'muted'));
  try {
    const detail = await bus('line', { service: state.me.service, direction: line.direction_id });
    container.replaceChildren();
    container.append(el('div', `${detail.name}：选择上车站`, 'muted'));
    detail.topology.stations.forEach(stop => {
      const button = el('button', `${stop.order}. ${stop.name}`, 'ghost wide');
      button.onclick = () => pickBus(detail, stop, container);
      container.append(button);
    });
  } catch (error) { container.replaceChildren(el('p', error.message, 'error')); }
}

async function pickBus(detail, stop, container) {
  container.replaceChildren(el('p', '正在查询在途车辆…', 'muted'));
  const base = { service: state.me.service, direction: detail.direction_id, order: stop.order };
  try {
    const realtime = await bus('realtime', base);
    container.replaceChildren();
    container.append(el('div', `${detail.name} @ ${stop.name}：选择要等的车`, 'muted'));
    const auto = el('button', '⚡ 最近的一班（自动跟随）', 'wide');
    auto.onclick = () => startWatch(base);
    container.append(auto);
    realtime.buses.filter(item => item.bus_id).forEach(item => {
      const button = el('button', `🚌 车辆 ${item.bus_id}（第 ${item.station_index} 站）`, 'ghost wide');
      button.onclick = () => startWatch({ ...base, bus: item.bus_id });
      container.append(button);
    });
    if (!realtime.buses.length) {
      container.append(el('p', '上游当前没有在途车辆数据，可以先用「最近的一班」。', 'muted'));
    }
  } catch (error) {
    container.replaceChildren(el('p', error.message, 'error'));
    const auto = el('button', '⚡ 仍然监控最近的一班', 'wide');
    auto.onclick = () => startWatch(base);
    container.append(auto);
  }
}

async function startWatch(body) {
  status('正在启动监控…');
  try {
    const me = await api('/api/me/watch/start', { method: 'POST', body: JSON.stringify(body) });
    renderMe(me);
    renderWatch();
    showTab('watch');
    status('监控已启动，提醒会发到机器人聊天里。');
  } catch (error) { status(error.message, true); }
}

/* ─── 启动 ─── */

function showTab(which) {
  $('tab-me').setAttribute('aria-selected', String(which === 'me'));
  $('tab-watch').setAttribute('aria-selected', String(which === 'watch'));
  $('view-me').hidden = which !== 'me';
  $('view-watch').hidden = which !== 'watch';
  if (which === 'watch') renderWatch();
}

(async () => {
  if (tg) { tg.ready(); tg.expand(); }
  const initData = tg && tg.initData;
  if (!initData) {
    status('请在 Telegram 里通过机器人的「我的面板」按钮打开本页面。', true);
    return;
  }
  try {
    const session = await api('/api/auth/telegram', { method: 'POST', body: JSON.stringify({ init_data: initData }) });
    state.token = session.token;
    status(`已登录：${session.user.name}`);
    $('tabs').hidden = false;
    $('tab-me').onclick = () => showTab('me');
    $('tab-watch').onclick = () => showTab('watch');
    renderMe(await api('/api/me'));
    showTab('me');
  } catch (error) {
    status(error.message, true);
  }
})();
