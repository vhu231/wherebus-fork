/* Arrival estimates without a bus ID must never be assigned by array position. */
(function(root) {
  function describeBus(bus, index, stops, targetOrder) {
    const position = stops.findIndex(s => s.order === bus.station_index);
    const targetPosition = stops.findIndex(s => s.order === targetOrder);
    const stop = stops[position], target = stops[targetPosition], next = stops[position + 1];
    const passed = position >= 0 && targetPosition >= 0 && (position > targetPosition || (position === targetPosition && !bus.is_arriving));
    const atTarget = position >= 0 && position === targetPosition && bus.is_arriving;
    const identity = bus.bus_id ? `车辆 ${bus.bus_id}` : `车辆 ${index + 1}（上游未提供编号）`;
    const location = !stop ? '当前站点未匹配' : bus.is_arriving ? `正在进站：${stop.name}` : `已离开：${stop.name}${next ? ` → 开往 ${next.name}` : '（末站）'}`;
    let estimate = '暂无该车到站预估';
    if(passed) estimate = '已驶过你的上车站';
    else if(atTarget) estimate = '正在进入你的上车站';
    else if(target) {
      const parts = [];
      if(position >= 0 && targetPosition > position) parts.push(`站序相差 ${targetPosition - position} 站`);
      if(Number.isFinite(bus.travel_time_secs) && bus.travel_time_secs >= 0) parts.push(bus.travel_time_secs === 0 ? '即将到站' : `约 ${Math.ceil(bus.travel_time_secs / 60)} 分钟`);
      if(parts.length) estimate = parts.join(' · ');
    }
    return {identity,location,estimate,passed,atTarget,order:stop?.order,targetName:target?.name || '未选择',description:bus.state_description || ''};
  }
  root.WhereBusView = {describeBus};
  if(typeof module !== 'undefined') module.exports = root.WhereBusView;
})(typeof window === 'undefined' ? globalThis : window);
