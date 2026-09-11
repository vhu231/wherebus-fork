const {test} = require('node:test');
const assert = require('node:assert/strict');
const {describeBus} = require('../bus-view.js');
const stops = [{order:1,name:'起点站'},{order:3,name:'中心站'},{order:5,name:'终点站'}];
test('maps real stop orders rather than array offsets, with bus identity',()=>{
 const v=describeBus({bus_id:'闽C12345',station_index:3,is_arriving:false,travel_time_secs:125},0,stops,5);
 assert.equal(v.identity,'车辆 闽C12345');assert.equal(v.location,'已离开：中心站 → 开往 终点站');assert.match(v.estimate,/约 3 分钟/);assert.equal(v.order,3);
});
test('arrival at boarding stop is explicit',()=>{
 const v=describeBus({bus_id:'B',station_index:3,is_arriving:true},0,stops,3);
 assert.equal(v.atTarget,true);assert.equal(v.estimate,'正在进入你的上车站');
});
test('departed target is passed, never an upcoming ETA',()=>{
 const v=describeBus({station_index:3,is_arriving:false,travel_time_secs:40},0,stops,3);
 assert.equal(v.passed,true);assert.equal(v.estimate,'已驶过你的上车站');
});
test('unknown station stays unmatched rather than assigned to first station',()=>{
 const v=describeBus({station_index:999,is_arriving:true},0,stops,3);
 assert.equal(v.order,undefined);assert.equal(v.location,'当前站点未匹配');assert.match(v.identity,/未提供编号/);assert.equal(v.estimate,'暂无该车到站预估');
});
test('switching boarding stop changes relation to same vehicle',()=>{
 const bus={station_index:3,is_arriving:true};assert.equal(describeBus(bus,0,stops,1).passed,true);assert.equal(describeBus(bus,0,stops,5).passed,false);
});
