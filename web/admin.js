/* WhereBus 管理控制台：首次进入设置口令，之后凭口令登录。
   令牌只放在 sessionStorage，关掉标签页即失效。 */
const $ = id => document.getElementById(id);
const state = { token: sessionStorage.getItem('wherebus-console') || null, overview: null };

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
  const minutes = Math.max(0, Math.round(Date.now() / 1000 - unix) / 60);
  if (minutes < 60) return `${Math.round(minutes)} 分钟前`;
  if (minutes < 1440) return `${Math.round(minutes / 60)} 小时前`;
  return `${Math.round(minutes / 1440)} 天前`;
}
function duration(secs) {
  if (secs == null) return '—';
  const hours = Math.floor(secs / 3600), minutes = Math.floor((secs % 3600) / 60);
  return hours ? `${hours} 小时 ${minutes} 分钟` : `${minutes} 分钟`;
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
  if (response.status === 401 && state.token) {
    signOut('登录已过期，请重新登录。');
    throw new Error(body.error || '登录已过期');
  }
  if (!response.ok) throw new Error(body.error || `请求失败（${response.status}）`);
  return body;
}

function signOut(message) {
  state.token = null;
  sessionStorage.removeItem('wherebus-console');
  status(message || '已退出登录。');
  start();
}

/* ─── 首次设置 / 登录 ─── */

function renderGate(info) {
  const view = $('view');
  view.replaceChildren();
  const card = el('div', null, 'card');
  const first = !info.initialized;
  card.append(el('h2', first ? '首次进入：设置管理口令' : '登录管理控制台'));
  card.append(el('p', first
    ? `这是本站第一次打开管理控制台，请设置管理口令（至少 ${info.min_password_len} 位）。口令只保存哈希值，忘记后需要清空数据库里的 admin_password 记录才能重设。`
    : '输入管理口令继续。控制台包含全部用户数据，请只在可信网络或 HTTPS 下访问。', 'muted'));

  const form = el('form', null, 'stack');
  const password = el('input');
  password.type = 'password';
  password.autocomplete = first ? 'new-password' : 'current-password';
  password.required = true;
  const passwordField = el('div');
  passwordField.append(el('label', '管理口令'), password);
  form.append(passwordField);

  let repeat = null;
  if (first) {
    repeat = el('input');
    repeat.type = 'password';
    repeat.autocomplete = 'new-password';
    repeat.required = true;
    const repeatField = el('div');
    repeatField.append(el('label', '再输入一次'), repeat);
    form.append(repeatField);
  }

  const submit = el('button', first ? '设置口令并进入' : '登录');
  form.append(submit);
  form.onsubmit = async event => {
    event.preventDefault();
    if (first && password.value !== repeat.value) return status('两次输入的口令不一致。', true);
    submit.disabled = true;
    try {
      const result = await api(first ? '/api/console/setup' : '/api/console/login', {
        method: 'POST',
        body: JSON.stringify({ password: password.value }),
      });
      state.token = result.token;
      sessionStorage.setItem('wherebus-console', result.token);
      status('已登录。');
      await loadConsole();
    } catch (error) {
      status(error.message, true);
      submit.disabled = false;
    }
  };
  card.append(form);
  view.append(card);
}

/* ─── 控制台 ─── */

async function loadConsole() {
  const view = $('view');
  view.replaceChildren(el('p', '正在加载…', 'muted'));
  const [overview, users] = await Promise.all([
    api('/api/console/overview'),
    api('/api/console/users'),
  ]);
  state.overview = overview;
  view.replaceChildren();

  // 运行状态
  const stats = el('div', null, 'card');
  const head = el('div', null, 'row');
  head.append(el('h2', overview.bot_running ? `机器人 @${overview.bot} 运行中` : '机器人未启动'));
  const out = el('button', '退出登录', 'ghost');
  out.onclick = () => signOut();
  head.append(out);
  stats.append(head);
  if (!overview.bot_running) {
    stats.append(el('p', '没有设置 TELEGRAM_BOT_TOKEN，本次只运行网页版；用户数据仍可在这里查看和管理。', 'muted'));
  }
  const grid = el('div', null, 'grid');
  [
    ['用户', overview.users],
    ['已停用', overview.banned],
    ['收藏车次', overview.favorites],
    ['累计查询', overview.queries],
    ['进行中的盯车', overview.watches_active],
    ['已运行', duration(overview.uptime_secs)],
  ].forEach(([label, value]) => {
    const cell = el('div', null, 'stat');
    cell.append(el('b', String(value)), el('span', label, 'muted'));
    grid.append(cell);
  });
  stats.append(grid);
  stats.append(el('p', `数据库：${overview.database}`, 'muted'));
  view.append(stats);

  // 全局默认设置
  const settings = el('div', null, 'card');
  settings.append(el('h2', '全局默认提醒设置'));
  settings.append(el('p', '新用户，以及没有单独设置过的用户，使用这组默认值。', 'muted'));
  overview.settings.defaults.fields.forEach(field => {
    const row = el('div', null, 'row');
    row.append(el('div', field.label));
    const stepper = el('div', null, 'stepper');
    const minus = el('button', '−', 'ghost');
    const value = el('output', `${overview.settings.defaults.values[field.key]} ${field.unit}`);
    const plus = el('button', '+', 'ghost');
    const change = delta => async () => {
      const next = Math.min(field.max, Math.max(field.min,
        overview.settings.defaults.values[field.key] + delta * field.step));
      minus.disabled = plus.disabled = true;
      try { await api('/api/console/settings', { method: 'POST', body: JSON.stringify({ values: { [field.key]: next } }) }); await loadConsole(); }
      catch (error) { status(error.message, true); minus.disabled = plus.disabled = false; }
    };
    minus.onclick = change(-1);
    plus.onclick = change(1);
    stepper.append(minus, value, plus);
    row.append(stepper);
    settings.append(row);
  });
  const allowRow = el('div', null, 'row');
  allowRow.append(el('div', '接纳新用户'));
  const toggle = el('button', overview.settings.allow_new_users ? '开启中' : '已关闭',
    overview.settings.allow_new_users ? '' : 'ghost');
  toggle.onclick = async () => {
    toggle.disabled = true;
    try { await api('/api/console/settings', { method: 'POST', body: JSON.stringify({ allow_new_users: !overview.settings.allow_new_users }) }); await loadConsole(); }
    catch (error) { status(error.message, true); toggle.disabled = false; }
  };
  allowRow.append(toggle);
  settings.append(allowRow);
  view.append(settings);

  // 用户列表
  const table = el('div', null, 'card');
  const tableHead = el('div', null, 'row');
  tableHead.append(el('h2', `用户（${users.users.length}）`));
  const search = el('input');
  search.type = 'search';
  search.placeholder = '搜索昵称 / 用户名 / ID / 城市 / 线路';
  search.style.maxWidth = '260px';
  tableHead.append(search);
  table.append(tableHead);

  if (!users.users.length) table.append(el('p', '还没有用户。', 'muted'));
  const scroll = el('div', null, 'scroll');
  const userTable = el('table');
  const header = el('tr');
  ['用户', '城市', '车次监控', '收藏 · 查询', '最近活跃', '操作'].forEach(title => header.append(el('th', title)));
  userTable.append(header);

  const render = keyword => {
    [...userTable.querySelectorAll('tr.user, tr.detail')].forEach(row => row.remove());
    const needle = keyword.trim().toLowerCase();
    const matched = users.users.filter(user => !needle || [
      user.name, user.username, String(user.id), user.city,
      user.watch && user.watch.label, ...(user.favorites_detail || []).map(f => `${f.line_name} ${f.station_name}`),
    ].some(field => (field || '').toLowerCase().includes(needle)));

    if (!matched.length && users.users.length) {
      const empty = el('tr', null, 'user');
      const cell = el('td', '没有匹配的用户。', 'muted');
      cell.colSpan = 6;
      empty.append(cell);
      userTable.append(empty);
      return;
    }

    matched.forEach(user => {
      const row = el('tr', null, 'user');

      const who = el('td');
      const name = el('button', `${user.name || '未命名'}${user.username ? ' @' + user.username : ''}`, 'linky');
      who.append(name);
      who.append(el('div', `ID ${user.id} · 加入 ${since(user.first_seen)}`, 'muted'));
      if (user.banned) who.append(el('span', '已停用', 'pill off'));
      row.append(who);

      const where = el('td');
      where.append(el('div', user.city || '未选择'));
      if (user.service) where.append(el('div', user.service, 'muted'));
      if (user.has_location) where.append(el('div', '有定位记录', 'muted'));
      row.append(where);

      const watching = el('td');
      if (user.watch) {
        watching.append(el('div', `🔔 ${user.watch.line_name} @ ${user.watch.station_name}`));
        watching.append(el('div', `${user.watch.target_bus ? '车辆 ' + user.watch.target_bus : '最近的一班'} · 第 ${user.watch.order} 站`, 'muted'));
        watching.append(el('div', `已监控 ${duration(Math.max(0, Date.now() / 1000 - user.watch.started_at))}`, 'muted'));
      } else {
        watching.append(el('div', '—', 'muted'));
      }
      row.append(watching);

      const counts = el('td');
      counts.append(el('div', `${user.favorites} 收藏 · ${user.queries} 次查询`));
      counts.append(el('div', user.peak_hour == null
        ? `记录线路 ${user.habit_lines} 条`
        : `记录线路 ${user.habit_lines} 条 · 高峰 ${String(user.peak_hour).padStart(2, '0')}:00`, 'muted'));
      row.append(counts);

      row.append(el('td', since(user.last_seen)));

      const actions = el('td');
      const box = el('div', null, 'actions');
      if (user.watch) box.append(action('停监控', 'ghost', user.id, 'stop_watch'));
      box.append(action(user.banned ? '解除停用' : '停用', user.banned ? 'ghost' : 'danger', user.id, user.banned ? 'unban' : 'ban'));
      box.append(action('删除数据', 'danger', user.id, 'delete'));
      actions.append(box);
      row.append(actions);
      userTable.append(row);

      // 展开行：提醒设置、收藏明细、常用线路
      const detail = el('tr', null, 'detail');
      detail.hidden = true;
      const cell = el('td');
      cell.colSpan = 6;
      cell.append(detailPanel(user));
      detail.append(cell);
      userTable.append(detail);
      name.onclick = () => { detail.hidden = !detail.hidden; };
    });
  };

  search.oninput = () => render(search.value);
  render('');
  scroll.append(userTable);
  table.append(scroll);
  view.append(table);

  // 修改口令
  const security = el('div', null, 'card');
  security.append(el('h2', '修改管理口令'));
  const form = el('form', null, 'stack');
  const current = el('input'); current.type = 'password'; current.required = true;
  const next = el('input'); next.type = 'password'; next.required = true;
  const currentField = el('div'); currentField.append(el('label', '当前口令'), current);
  const nextField = el('div'); nextField.append(el('label', '新口令'), next);
  const save = el('button', '保存新口令');
  form.append(currentField, nextField, save);
  form.onsubmit = async event => {
    event.preventDefault();
    save.disabled = true;
    try {
      const result = await api('/api/console/password', {
        method: 'POST',
        body: JSON.stringify({ current: current.value, new_password: next.value }),
      });
      state.token = result.token;
      sessionStorage.setItem('wherebus-console', result.token);
      current.value = next.value = '';
      status('口令已更新，其他设备上的登录已失效。');
    } catch (error) { status(error.message, true); }
    save.disabled = false;
  };
  security.append(form);
  view.append(security);
}

function detailPanel(user) {
  const box = el('div', null, 'detail-grid');

  const alerts = el('div');
  alerts.append(el('h3', '提醒设置'));
  alerts.append(el('div', user.alerts.using_defaults ? '跟随全局默认值' : '用户自定义', 'muted'));
  user.alerts.fields.forEach(field => {
    alerts.append(el('div', `${field.label}：${user.alerts.values[field.key]} ${field.unit}`, 'muted'));
  });
  box.append(alerts);

  const favorites = el('div');
  favorites.append(el('h3', `收藏车次（${user.favorites}）`));
  if (!user.favorites_detail.length) favorites.append(el('div', '无', 'muted'));
  user.favorites_detail.forEach(favorite => {
    favorites.append(el('div', `${favorite.line_name} @ ${favorite.station_name}（第 ${favorite.order} 站）· 查过 ${favorite.hits} 次`, 'muted'));
  });
  box.append(favorites);

  const habits = el('div');
  habits.append(el('h3', '常用线路'));
  if (!user.habits.length) habits.append(el('div', '还没有查询记录', 'muted'));
  user.habits.forEach((habit, index) => {
    habits.append(el('div', `${index + 1}. ${habit.label} — ${habit.total} 次（${since(habit.last_at)}）`, 'muted'));
  });
  const max = Math.max(1, ...user.hour_histogram);
  const blocks = '▁▂▃▄▅▆▇█';
  habits.append(el('div', user.hour_histogram.map(v => (v ? blocks[Math.round((v / max) * 7)] : '·')).join(''), 'bars'));
  habits.append(el('div', '0     6     12    18   23', 'bars muted'));
  box.append(habits);

  return box;
}

function action(label, className, userId, name) {
  const button = el('button', label, className);
  button.onclick = async () => {
    const prompts = { ban: '停用该用户？', unban: '解除停用？', stop_watch: '停止该用户的盯车？', delete: '删除该用户的全部数据？此操作不可撤销。' };
    if (!window.confirm(prompts[name])) return;
    button.disabled = true;
    try { await api('/api/console/users/action', { method: 'POST', body: JSON.stringify({ user_id: userId, action: name }) }); await loadConsole(); status('操作完成。'); }
    catch (error) { status(error.message, true); button.disabled = false; }
  };
  return button;
}

/* ─── 启动 ─── */

async function start() {
  try {
    const info = await api('/api/console/status');
    if (!state.token) return renderGate(info);
    try {
      await loadConsole();
      status('已登录。');
    } catch (error) {
      if (state.token) { status(error.message, true); renderGate(info); }
    }
  } catch (error) {
    status(error.message, true);
  }
}

start();
