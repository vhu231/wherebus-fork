/* WhereBus 管理面板：Telegram Mini App 前端。
   身份由服务端校验 initData 签名后签发会话令牌，这里只负责展示与调用接口。 */
const tg = window.Telegram && window.Telegram.WebApp;
const $ = id => document.getElementById(id);
const state = { token: null, me: null, isAdmin: false };

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
  const minutes = Math.max(0, Math.round((Date.now() / 1000 - unix) / 60));
  if (minutes < 60) return `${minutes} 分钟前`;
  if (minutes < 60 * 24) return `${Math.round(minutes / 60)} 小时前`;
  return `${Math.round(minutes / 1440)} 天前`;
}
function duration(secs) {
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
  if (!response.ok) throw new Error(body.error || `请求失败（${response.status}）`);
  return body;
}

/* ─── 我的 ─── */

function renderMe(me) {
  state.me = me;
  const view = $('view-me');
  view.replaceChildren();

  const profile = el('div', null, 'card');
  profile.append(
    el('div', `${me.name || '未命名'}${me.username ? ' @' + me.username : ''}`),
    el('div', `城市：${me.city || '未选择'} · 累计查询 ${me.queries} 次 · 收藏 ${me.favorites.length} 条`, 'muted'),
    el('div', `最近活跃：${since(me.last_seen)}`, 'muted'),
  );
  view.append(profile);

  // 盯车
  const watchCard = el('div', null, 'card');
  watchCard.append(el('h2', '盯车'));
  if (me.watch) {
    const row = el('div', null, 'row');
    row.append(el('div', `${me.watch.label}（已盯 ${duration(Math.max(0, Date.now() / 1000 - me.watch.started_at))}）`));
    const stop = el('button', '停止', 'danger');
    stop.onclick = async () => {
      stop.disabled = true;
      try { await api('/api/me/watch/stop', { method: 'POST', body: '{}' }); await loadMe(); status('盯车已停止。'); }
      catch (error) { status(error.message, true); stop.disabled = false; }
    };
    row.append(stop);
    watchCard.append(row);
  } else {
    watchCard.append(el('p', '当前没有进行中的盯车。在机器人里打开线路的实时到站页，点「🔔 盯这趟车」即可开始。', 'muted'));
  }
  view.append(watchCard);

  // 提醒设置
  const settings = el('div', null, 'card');
  settings.append(el('h2', '提醒设置'));
  settings.append(el('p', me.alerts.using_defaults ? '当前跟随全局默认值，改动任一项后变为个人设置。' : '当前使用个人设置。', 'muted'));
  me.alerts.fields.forEach(field => {
    const row = el('div', null, 'row');
    row.append(el('div', field.label));
    const stepper = el('div', null, 'stepper');
    const minus = el('button', '➖', 'ghost');
    const value = el('output', `${me.alerts.values[field.key]} ${field.unit}`);
    const plus = el('button', '➕', 'ghost');
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

  // 收藏
  const favorites = el('div', null, 'card');
  favorites.append(el('h2', `收藏车次（${me.favorites.length}）`));
  if (!me.favorites.length) favorites.append(el('p', '还没有收藏。', 'muted'));
  me.favorites.forEach(favorite => {
    const row = el('div', null, 'row');
    row.append(el('div', `${favorite.line_name} @ ${favorite.station_name}\n${favorite.city_label} · 查过 ${favorite.hits} 次`));
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
    row.append(remove);
    favorites.append(row);
  });
  view.append(favorites);

  // 习惯
  const habits = el('div', null, 'card');
  habits.append(el('h2', '乘车习惯'));
  const max = Math.max(1, ...me.hour_histogram);
  const blocks = '▁▂▃▄▅▆▇█';
  habits.append(el('div', me.hour_histogram.map(v => (v ? blocks[Math.round((v / max) * 7)] : '·')).join(''), 'bars'));
  habits.append(el('div', '0     6     12    18   23', 'bars muted'));
  me.habits.slice(0, 5).forEach((habit, index) => habits.append(el('div', `${index + 1}. ${habit.label} — ${habit.total} 次`, 'muted')));
  if (!me.habits.length) habits.append(el('p', '还没有查询记录。', 'muted'));
  view.append(habits);

  // 删除我的数据
  const danger = el('div', null, 'card');
  danger.append(el('h2', '数据'));
  danger.append(el('p', '删除后，你的城市选择、收藏、习惯统计与盯车任务都会被清空，且无法恢复。', 'muted'));
  const forget = el('button', '删除我的全部数据', 'danger');
  forget.onclick = async () => {
    const ok = await confirmDialog('确认删除你的全部数据？此操作不可撤销。');
    if (!ok) return;
    try { await api('/api/me', { method: 'DELETE' }); status('数据已删除，可以关闭面板。'); $('view-me').replaceChildren(el('p', '数据已删除。', 'muted')); }
    catch (error) { status(error.message, true); }
  };
  danger.append(forget);
  view.append(danger);
}

function confirmDialog(message) {
  return new Promise(resolve => {
    if (tg && tg.showConfirm) tg.showConfirm(message, resolve);
    else resolve(window.confirm(message));
  });
}

async function loadMe() {
  renderMe(await api('/api/me'));
}

/* ─── Bot 管理 ─── */

async function loadAdmin() {
  const view = $('view-admin');
  view.replaceChildren(el('p', '正在加载…', 'muted'));
  try {
    const [overview, users] = await Promise.all([api('/api/admin/overview'), api('/api/admin/users')]);
    view.replaceChildren();

    const stats = el('div', null, 'card');
    stats.append(el('h2', `@${overview.bot} 运行状态`));
    [
      ['已运行', duration(overview.uptime_secs)],
      ['用户 / 已停用', `${overview.users} / ${overview.banned}`],
      ['收藏 / 累计查询', `${overview.favorites} / ${overview.queries}`],
      ['进行中的盯车', String(overview.watches_active)],
      ['活跃会话 / 管理员', `${overview.sessions_active} / ${overview.admins}`],
      ['时区', `UTC${overview.tz_offset >= 0 ? '+' : ''}${overview.tz_offset}`],
      ['数据文件', overview.data_file],
    ].forEach(([label, value]) => {
      const row = el('div', null, 'row');
      row.append(el('div', label, 'muted'), el('div', value));
      stats.append(row);
    });
    view.append(stats);

    const globals = el('div', null, 'card');
    globals.append(el('h2', '全局默认提醒设置'), el('p', '新用户以及没有个人设置的用户使用这组默认值。', 'muted'));
    overview.settings.defaults.fields.forEach(field => {
      const row = el('div', null, 'row');
      row.append(el('div', field.label));
      const stepper = el('div', null, 'stepper');
      const minus = el('button', '➖', 'ghost');
      const value = el('output', `${overview.settings.defaults.values[field.key]} ${field.unit}`);
      const plus = el('button', '➕', 'ghost');
      const change = delta => async () => {
        const next = Math.min(field.max, Math.max(field.min, overview.settings.defaults.values[field.key] + delta * field.step));
        minus.disabled = plus.disabled = true;
        try { await api('/api/admin/settings', { method: 'POST', body: JSON.stringify({ values: { [field.key]: next } }) }); await loadAdmin(); }
        catch (error) { status(error.message, true); minus.disabled = plus.disabled = false; }
      };
      minus.onclick = change(-1);
      plus.onclick = change(1);
      stepper.append(minus, value, plus);
      row.append(stepper);
      globals.append(row);
    });
    const allowRow = el('div', null, 'row');
    allowRow.append(el('div', '接纳新用户'));
    const toggle = el('button', overview.settings.allow_new_users ? '开启中' : '已关闭', overview.settings.allow_new_users ? '' : 'ghost');
    toggle.onclick = async () => {
      toggle.disabled = true;
      try { await api('/api/admin/settings', { method: 'POST', body: JSON.stringify({ allow_new_users: !overview.settings.allow_new_users }) }); await loadAdmin(); }
      catch (error) { status(error.message, true); toggle.disabled = false; }
    };
    allowRow.append(toggle);
    globals.append(allowRow);
    view.append(globals);

    const table = el('div', null, 'card');
    table.append(el('h2', `用户（${users.users.length}）`));
    const grid = el('table');
    const head = el('tr');
    ['用户', '城市 / 盯车', '收藏·查询', '操作'].forEach(title => head.append(el('th', title)));
    grid.append(head);
    users.users.forEach(user => {
      const row = el('tr');
      const who = el('td');
      who.append(el('div', `${user.name || '未命名'}${user.username ? ' @' + user.username : ''}`));
      who.append(el('div', `${user.id} · ${since(user.last_seen)}`, 'muted'));
      if (user.is_admin) who.append(el('span', '管理员', 'pill'));
      if (user.banned) who.append(el('span', '已停用', 'pill off'));
      row.append(who);
      row.append(el('td', `${user.city || '未选择'}${user.watch ? '\n🔔 ' + user.watch : ''}`));
      row.append(el('td', `${user.favorites} · ${user.queries}`));

      const actions = el('td');
      if (user.watch) {
        const stop = el('button', '停盯车', 'ghost');
        stop.onclick = () => act(user.id, 'stop_watch');
        actions.append(stop);
      }
      if (!user.is_admin) {
        const ban = el('button', user.banned ? '解除停用' : '停用', user.banned ? 'ghost' : 'danger');
        ban.onclick = () => act(user.id, user.banned ? 'unban' : 'ban');
        actions.append(ban);
      }
      row.append(actions);
      grid.append(row);
    });
    table.append(grid);
    view.append(table);
  } catch (error) {
    view.replaceChildren(el('p', error.message, 'error'));
  }
}

async function act(userId, action) {
  const labels = { ban: '停用该用户？', unban: '解除停用？', stop_watch: '停止该用户的盯车？', delete: '删除该用户全部数据？' };
  if (!(await confirmDialog(labels[action] || '确认操作？'))) return;
  try {
    await api('/api/admin/users/action', { method: 'POST', body: JSON.stringify({ user_id: userId, action }) });
    await loadAdmin();
    status('操作完成。');
  } catch (error) { status(error.message, true); }
}

/* ─── 启动 ─── */

function showTab(which) {
  $('tab-me').setAttribute('aria-selected', String(which === 'me'));
  $('tab-admin').setAttribute('aria-selected', String(which === 'admin'));
  $('view-me').hidden = which !== 'me';
  $('view-admin').hidden = which !== 'admin';
  if (which === 'admin') loadAdmin();
}

(async () => {
  if (tg) { tg.ready(); tg.expand(); }
  const initData = tg && tg.initData;
  if (!initData) {
    status('请在 Telegram 里通过机器人的「管理面板」按钮打开本页面。', true);
    return;
  }
  try {
    const session = await api('/api/auth/telegram', { method: 'POST', body: JSON.stringify({ init_data: initData }) });
    state.token = session.token;
    state.isAdmin = session.is_admin;
    status(`已登录：${session.user.name}${session.is_admin ? '（管理员）' : ''}`);
    $('tabs').hidden = false;
    $('tab-admin').hidden = !session.is_admin;
    $('tab-me').onclick = () => showTab('me');
    $('tab-admin').onclick = () => showTab('admin');
    await loadMe();
    showTab('me');
  } catch (error) {
    status(error.message, true);
  }
})();
