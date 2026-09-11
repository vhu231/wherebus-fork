const $ = id => document.getElementById(id);
let generation = 0, detailGeneration = 0, current = null, timer = null;
function el(tag, text, className) { const node = document.createElement(tag); if (text != null) node.textContent = text; if (className) node.className = className; return node; }
function status(text, error = false) { $('status').textContent = text; $('status').classList.toggle('error', error); }
async function api(path, params = {}) {
  const query = new URLSearchParams({service: $('service').value, ...params});
  const response = await fetch(`/api/${path}?${query}`, {signal: AbortSignal.timeout(25000), cache: 'no-store'});
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || `请求失败（${response.status}），请检查参数后重试`);
  return body.data;
}
function resetDetail() { detailGeneration++; clearTimeout(timer); current = null; $('detail').replaceChildren(el('h2', '你的下一程'), el('p', '选择一条线路，查看沿途站点和到站信息。', 'muted')); }
function arrival(value) { if(value === 'Arriving') return '即将到站'; if(value === 'NoService') return '暂无班次'; if(value?.Approaching) { const a=value.Approaching; return `${a.stations_away} 站${a.minutes_away == null ? '' : ` · 约 ${a.minutes_away} 分钟`}`; } return '暂无到站预估'; }
function renderLines(rows) {
  $('list').replaceChildren(); $('count').textContent = `${rows.length} 条`;
  rows.forEach(row => { const card=el('button', null, 'card'); card.append(el('strong',row.name),el('small',`${row.endpoints.origin || '起点待更新'} → ${row.endpoints.terminus || '终点待更新'}`)); if(row.arrival) card.append(el('em',arrival(row.arrival))); card.onclick=()=>{ document.querySelectorAll('.card.selected').forEach(n=>n.classList.remove('selected'));card.classList.add('selected');openLine(row.direction_id,row.station_order); }; $('list').append(card); });
}
async function search(path, params, title, render) {
  const token=++generation; resetDetail(); $('result-title').textContent=title; $('count').textContent=''; $('list').replaceChildren(); status('正在查询…');
  try { const rows=await api(path,params); if(token!==generation)return; render(rows);status(rows.length ? '选择结果查看详情。' : '没有找到结果，请更换关键词、坐标或数据源。'); }
  catch(error) { if(token===generation)status(error.message,true); }
}
$('search').onsubmit=event=>{event.preventDefault();search('lines',{q:$('keyword').value},'线路查询',renderLines);};
$('nearby').onsubmit=event=>{event.preventDefault(); const coords={lat:$('lat').value,lng:$('lng').value}; search('nearby',coords,'附近站点',rows=>{ $('count').textContent=`${rows.length} 站`;rows.sort((a,b)=>a.distance_m-b.distance_m).forEach(row=>{const card=el('button',null,'card');card.append(el('strong',row.name),el('small',`距离约 ${row.distance_m} 米`));card.onclick=()=>search('station-lines',{...coords,station:row.name},row.name,renderLines);$('list').append(card);});});};
$('locate').onclick=()=>{if(!navigator.geolocation){status('浏览器不支持定位，请手动输入坐标。',true);return;} const token=generation; $('locate').disabled=true;status('正在获取位置…');navigator.geolocation.getCurrentPosition(position=>{$('locate').disabled=false;if(token!==generation)return;$('lat').value=position.coords.latitude;$('lng').value=position.coords.longitude;$('nearby').requestSubmit();},()=>{$('locate').disabled=false;if(token===generation)status('定位失败，请允许位置权限，或手动输入坐标。',true);},{timeout:12000,maximumAge:60000});};
function time(value){if(!value)return '—';const m=value.minutes_since_midnight;return `${String(Math.floor(m/60)).padStart(2,'0')}:${String(m%60).padStart(2,'0')}`;}
async function openLine(direction, order) {
  resetDetail();const token=detailGeneration; $('detail').replaceChildren(el('p','正在加载线路…','muted'));
  try {const line=await api('line',{direction});if(token!==detailGeneration)return;
    current={direction,order:line.topology.stations.some(s=>s.order===order) ? order : line.topology.stations[0]?.order,token,stops:line.topology.stations};
    const head=el('div',null,'detail-head');head.append(el('h2',line.name));if(line.reverse_id){const reverse=el('button','换向','secondary');reverse.onclick=()=>openLine(line.reverse_id);head.append(reverse);}
    const meta=el('p',`首班 ${time(line.meta.first_service)} · 末班 ${time(line.meta.last_service)} · 票价 ${line.meta.fare?.Text || '暂无信息'}`,'muted');
    const live=el('div',null,'live');live.id='live';live.setAttribute('aria-live','polite');
    const refresh=el('button','刷新到站','secondary');refresh.id='refresh';refresh.onclick=()=>refreshLive();
    const stops=el('ol',null,'stops');stops.id='timeline';line.topology.stations.forEach(stop=>{const li=el('li'),button=el('button',null,'stop');button.dataset.order=stop.order;button.append(el('span',stop.order),document.createTextNode(stop.name));button.onclick=()=>{current.order=stop.order;refreshLive();};const vehicles=el('div',null,'station-vehicles');vehicles.dataset.station=stop.order;li.append(button,vehicles);stops.append(li);});
    const target=el('div',null,'boarding');target.id='boarding';
    const unmatched=el('div');unmatched.id='unmatched';
    $('detail').replaceChildren(head,meta,target,refresh,el('h3','车辆 · 站点位置'),el('p','点击站名设置上车站。车辆显示在对应站点下方，离站车辆标明下一站。每 20 秒更新。','muted'),stops,unmatched,el('h3','车辆到站详情'),live);await refreshLive();
  }catch(error){if(token===detailGeneration)$('detail').replaceChildren(el('p',error.message,'error'));}
}
let realtimeGeneration=0;
async function refreshLive(){
  clearTimeout(timer);if(!current?.order)return;const selection={...current},request=++realtimeGeneration;
  const live=$('live');live.textContent='正在更新到站信息…';$('refresh').disabled=true;
  const target=selection.stops.find(s=>s.order===selection.order);
  $('boarding').replaceChildren(el('span','你的上车站'),el('strong',target?.name || '未选择'));
  document.querySelectorAll('.station-vehicles').forEach(n=>n.replaceChildren());$('unmatched').replaceChildren();
  document.querySelectorAll('.stop').forEach(n=>{const active=Number(n.dataset.order)===selection.order;n.classList.toggle('active',active);n.setAttribute('aria-pressed',String(active));n.querySelector('.boarding-label')?.remove();if(active)n.append(el('b','上车站','boarding-label'));});
  try{const data=await api('realtime',{direction:selection.direction,order:selection.order});if(selection.token!==detailGeneration||request!==realtimeGeneration)return;
    const states={Running:'运营中',NotOperating:'尚未运营',Stopped:'已停运',NoRealtime:'暂无实时数据'};
    live.replaceChildren(el('strong',states[data.run_state] || '状态未知'));
    if(data.plan_time)live.append(el('div',`计划发车 ${data.plan_time}`));
    live.append(el('div',`上游返回 ${data.buses.length} 辆车 · 到站目标：${target?.name || '未选择'}`,'muted'));
    data.buses.forEach((bus,index)=>{
      const view=WhereBusView.describeBus(bus,index,selection.stops,selection.order);
      const card=el('article',null,`vehicle-card${view.passed?' passed':''}${view.atTarget?' arriving':''}`);
      card.append(el('strong',view.identity),el('div',view.location),el('div',`到「${view.targetName}」：${view.estimate}`,'vehicle-eta'));
      if(view.description)card.append(el('small',`上游提示：${view.description}`,'muted'));
      live.append(card);
      const marker=el('article',null,`vehicle-marker${bus.is_arriving?' entering':' leaving'}${view.passed?' passed':''}`);
      marker.append(el('strong',view.identity),el('div',view.location),el('small',view.estimate));
      const container=[...document.querySelectorAll('.station-vehicles')].find(n=>Number(n.dataset.station)===view.order);
      (container || $('unmatched')).append(marker);
    });
    if(data.arrival_estimates.length){
      const estimates=el('details',null,'anonymous-estimates');estimates.append(el('summary',`到「${target?.name || '上车站'}」的综合预估`),el('p','这些预估未关联车辆编号，单独显示，不与上面的车辆强行配对。','muted'));
      data.arrival_estimates.forEach(a=>estimates.append(el('div',`${a.stations_away} 站 · 约 ${a.minutes_away} 分钟 · ${a.distance_m} 米`)));live.append(estimates);
    }
    if(!data.buses.length&&!data.arrival_estimates.length)live.append(el('div','当前暂无车辆到站预估'));
    live.append(el('div',`更新于 ${new Date().toLocaleTimeString('zh-CN')}`,'muted'));
  }catch(error){if(selection.token===detailGeneration&&request===realtimeGeneration)live.replaceChildren(el('p',error.message,'error'));}
  finally{if(selection.token===detailGeneration&&request===realtimeGeneration){$('refresh').disabled=false;if(!document.hidden)timer=setTimeout(refreshLive,20000);}}
}
document.addEventListener('visibilitychange',()=>{clearTimeout(timer);if(!document.hidden&&current)refreshLive();});
$('service').onchange=()=>{generation++;resetDetail();$('list').replaceChildren();$('count').textContent='';status('城市已切换，请重新查询。');};
(async()=>{try{const services=await api('services');$('service').replaceChildren(...services.map(s=>{const option=el('option',`${s.province} · ${s.city} / ${s.provider}`);option.value=s.id;return option;}));$('service').disabled=false;}catch(error){$('service').replaceChildren(el('option','城市加载失败，请刷新页面'));status(error.message,true);}})();

