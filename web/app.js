
document.getElementById('luser').addEventListener('keydown', e => {
 if (e.key === 'Enter') document.getElementById('lpass').focus();
});
document.getElementById('lpass').addEventListener('keydown', e => {
 if (e.key === 'Enter') doLogin();
});

let token='',timer=null,statusData=null,logFiles=[],currentLogFile=null;
let svcTimer=null,starTimer=null,trafficTimer=null;
let histData=null,histDays=30,histSite='',histStamp=''; // 流量历史（每日归档）
let cmdHistory=JSON.parse(sessionStorage.getItem('agent_cmd_history')||'[]');
const $=id=>document.getElementById(id);

async function doLogin(){
 const u=$('luser').value,p=$('lpass').value,e=$('lerr');
 e.classList.remove('show');
 if(!u&&!p){e.textContent='请输入用户名和密码';e.classList.add('show');return}
 if(!u){e.textContent='请输入用户名';e.classList.add('show');return}
 if(!p){e.textContent='请输入密码';e.classList.add('show');return}
 try{
  const r=await fetch('/api/login',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({user:u,password:p})});
  const j=await r.json();
  if(j.code===429){e.textContent='登录失败次数过多，请稍后再试';e.classList.add('show');return}
  if(j.code!==0){
   if(j.message&&j.message.includes('credentials'))e.textContent='用户名或密码错误';
   else if(j.message&&j.message.includes('configured'))e.textContent='服务未配置登录凭据';
   else e.textContent=j.message||'登录失败';
   e.classList.add('show');return
  }
  token=j.data.token;sessionStorage.setItem('agent_token',token);showApp();
 }catch(ex){e.textContent='网络错误，请检查服务是否正常运行';e.classList.add('show')}
}

async function logout(){
 try{await fetch('/api/logout',{method:'POST',headers:{'Authorization':'Bearer '+token,'Content-Type':'application/json'}})}catch(e){}
 sessionStorage.removeItem('agent_token');token='';
 clearInterval(timer);if(svcTimer)clearInterval(svcTimer);if(starTimer)clearInterval(starTimer);
 if(trafficTimer){clearInterval(trafficTimer);trafficTimer=null}
 $('login').style.display='';$('app').style.display='none';$('luser').focus()
}

function showApp(){
 $('login').style.display='none';$('app').style.display='block';
 loadMe().then(()=>{
  // 按权限启动轮询（无权限的页签不发起请求）：仪表盘 / 子服务
  if(canView('dashboard')){loadDashboard();timer=setInterval(loadDashboard,3000);}
  if(canView('services')||canView('watchdog')){loadServices();svcTimer=setInterval(loadServices,3000);}
  // 刷新后仍停留在当前页签（由 URL hash 记忆；缺省/非法/无权限时回退到首个可用页）
  restorePanelFromHash();
 });
}

// 页面重新可见时立即补刷当前数据（浏览器后台标签各轮询已暂停——恢复即刷新，稳态流量不增加）
document.addEventListener('visibilitychange',()=>{
 if(document.hidden||$('login').style.display!=='none')return;
 if(canView('dashboard'))loadDashboard();
 if(canView('services')||canView('watchdog'))loadServices();
 updateControlStatus();
 if(trafficTimer)loadTraffic();
});

// 当前登录主体（/api/me：用户名/是否内置管理员/菜单权限）；加载失败时降级为不限制
let me=null;
async function loadMe(){
 try{const j=await api('/api/me');me=(j&&j.code===0)?j.data:null;}catch(e){me=null}
 renderNav();
}
function renderNav(){
 document.querySelectorAll('#nav-bar a[data-panel]').forEach(a=>{
  const k=a.dataset.panel;
  let show=true;
  if(k==='users')show=!!(me&&me.isAdmin);
  else if(k==='logs')show=canView('logs')||canView('audit'); // 日志页含“操作日志”子页签（任一权限可见）
  else if(k==='config')show=canView('config')||canView('starconfig'); // 配置页含“星尘设置”子页签（任一权限可见）
  else if(k==='services')show=canView('services')||canView('watchdog'); // 子服务页含“看门狗”卡片（任一权限可见）
  else if(k==='security')show=canView('control'); // 安全页（WAF 在线管理）随「控制」权限
  else if(me&&!me.isAdmin)show=(me.perms||[]).includes(k);
  a.style.display=show?'':'none';
 });
}
function canView(key){
 return !me||me.isAdmin||(me.perms||[]).includes(key);
}

async function api(url,opt={}){
 const h=opt.headers||{};
 if(token)h['Authorization']='Bearer '+token;
 const r=await fetch(url,{...opt,headers:{...h,'Content-Type':'application/json'}});
 const _el=r.headers.get('X-Elapsed-Ms');if(_el)setSrvElapsed(_el);
 const j=await r.json();
 if(j.code===401||r.status===401){logout();showToast('登录已过期，请重新登录','error');throw new Error('Unauthorized')}
 return j;
}

function showToast(msg,type='info'){
 const t=$('toast');t.textContent=msg;t.className='toast '+type+' show';
 clearTimeout(t._tid);t._tid=setTimeout(()=>t.classList.remove('show'),3500);
}

let _modalCb=null;
function showModal(title,body,confirmText,cb){
 $('modal-title').textContent=title;
 $('modal-body').innerHTML=body;
 $('modal-confirm-btn').textContent=confirmText||'确认';
 $('modal-overlay').classList.remove('hidden');_modalCb=cb;
}
function closeModal(){$('modal-overlay').classList.add('hidden');_modalCb=null}
function confirmModal(){const cb=_modalCb;closeModal();if(cb)cb()}

function switchPanel(name){
 document.querySelectorAll('nav a').forEach(a=>a.classList.remove('active'));
 const link=document.querySelector(`nav a[data-panel="${name}"]`);if(link)link.classList.add('active');
 document.querySelectorAll('.panel').forEach(p=>p.classList.remove('active'));
 const panel=$('panel-'+name);if(panel)panel.classList.add('active');
 // 流量页签：进入时 3 秒轮询、离开即停止（其它页签不产生额外请求）
 if(trafficTimer){clearInterval(trafficTimer);trafficTimer=null}
 if(!panel)return;
 // 切换 tab 时立即加载对应数据
 const loaders={
  dashboard:loadDashboard,
  services:loadServices,
  traffic:loadTraffic,
  control:updateControlStatus,
  security:loadSecurity,
  config:loadConfig,
  logs:loadLogs,
  database:loadDatabase,
  fileman:loadFileman,
  plugins:loadPlugins,
  ai:loadAi,
  terminal:loadTerminal,
  users:loadUsers
 };
 if(loaders[name])loaders[name]();
 if(name==='traffic'){applyTrafficSub(trafficSubFromHash());trafficTimer=setInterval(loadTraffic,3000);}
 // 记忆当前页签：刷新后仍停留在本页（replaceState 不产生浏览器历史记录；流量/日志/配置页含子页后缀）
 let want='#'+name;
 if(name==='traffic')want=trafficSubFromHash()==='ports'?'#traffic.ports':'#traffic';
 if(name==='logs')want=logsSubFromHash()==='audit'?'#logs.audit':'#logs';
 if(name==='config')want=configSubFromHash()==='star'?'#config.star':'#config';
 if(location.hash!==want)history.replaceState(null,'',want);
}

// 从 URL hash 解析页签名（非法/缺省/无权限返回空串；与导航实际可见页签比对，避免选择器注入）
// 支持子页后缀：traffic.ports → 返回主页名 traffic（子页由 trafficSubFromHash 解析）；兼容早期独立页签书签 #ports / #audit / #starconfig / #cleanup / #watchdog
function panelFromHash(){
 const h=(location.hash||'').replace(/^#\/?/,'');
 if(!h)return '';
 if(h==='ports')return 'traffic';
 if(h==='audit')return 'logs';
 if(h==='starconfig')return 'config'; // 兼容早期独立页签书签
 if(h==='cleanup')return 'plugins'; // “日志清理”已迁移为插件（🧩 插件 → 日志清理）
 if(h==='watchdog')return 'services'; // “看门狗”已并入“子服务”页
 const name=h.split('.')[0];
 return [...document.querySelectorAll('nav a[data-panel]')].some(a=>a.dataset.panel===name&&a.style.display!=='none')?name:'';
}
// 流量页子页签（site 网站流量 / ports 端口流量）
function trafficSubFromHash(){
 const h=(location.hash||'').replace(/^#\/?/,'');
 return (h==='traffic.ports'||h==='ports')?'ports':'site';
}
// 恢复上次所在页签；缺省/状态页时规范化 hash；无权限的页签回退到首个可用页
function restorePanelFromHash(){
 const h=panelFromHash();
 if(h&&h!=='dashboard'){switchPanel(h);return;}
 if(canView('dashboard')){
  if(location.hash!=='#dashboard'){history.replaceState(null,'','#dashboard');}
  return;
 }
 // 无“状态”权限：切到第一个可见页签
 const first=[...document.querySelectorAll('#nav-bar a[data-panel]')].find(a=>a.style.display!=='none');
 if(first)switchPanel(first.dataset.panel);
}

document.querySelector('#nav-bar').addEventListener('click',e=>{
 const a=e.target.closest('a[data-panel]');if(!a)return;switchPanel(a.dataset.panel);
});

// 手动修改 hash（或历史前进/后退）时同步切换页签
window.addEventListener('hashchange',()=>{
 const h=panelFromHash();
 if(!h)return;
 const link=[...document.querySelectorAll('nav a[data-panel]')].find(a=>a.dataset.panel===h);
 if(link&&!link.classList.contains('active'))switchPanel(h);
 else if(h==='traffic'&&$('traffic-sub-site'))applyTrafficSub(trafficSubFromHash()); // 流量页内子页签同步
 else if(h==='logs'&&$('logs-sub-tabs'))applyLogsSub(logsSubFromHash()); // 日志页内子页签同步
 else if(h==='config'&&$('config-sub-tabs'))applyConfigSub(configSubFromHash()); // 配置页内子页签同步
});

/* ── 安全（WAF 在线开关 + 参考配置；库 Pek.RWaf / config 落盘 Config/Waf.json） ── */
let wafCfg=null;
const WAF_ACTS=[['off','关闭'],['monitor','仅记录'],['block','拦截']];
function wafActSel(id,v){return '<select id="'+id+'" class="waf-sel">'+WAF_ACTS.map(a=>'<option value="'+a[0]+'"'+(v===a[0]?' selected':'')+'>'+a[1]+'</option>').join('')+'</select>'}
function wafList(v){return (v||[]).join(', ')}
function wafParseList(t){return String(t||'').split(/[,，;；\s]+/).filter(Boolean)}
async function loadSecurity(){
 try{
  const j=await api('/star/waf');
  if(j.code!==0){$('panel-security').innerHTML='<div class="card"><h3>🛡 WAF</h3><p>加载失败：'+esc(j.message||'')+'</p></div>';return}
  wafCfg=j.data;renderSecurity();
 }catch(e){}
}
function renderSecurity(){
 const c=wafCfg||{};
 $('panel-security').innerHTML=`
<div class="card">
 <h3>🛡 Web 应用防火墙（Pek.RWaf）<span class="text-muted" style="font-size:12px;margin-left:8px;font-weight:400">配置：${escAttr(c.configFile||'Config/Waf.json')}</span></h3>
 <p class="text-muted" style="font-size:12px;margin:2px 0 10px">爬虫分级（放行正常搜索引擎/GEO-AI，拦恶意爬虫与扫描器）+ SQL 注入/路径探测防护 + CC 限速；保存后立即生效，并写入配置文件。</p>
 <div class="waf-row">
  <label><input type="checkbox" id="waf-enable" ${c.enable?'checked':''}> <b>启用 WAF</b>（关闭后请求全放行）</label>
  <label><input type="checkbox" id="waf-block-mal" ${c.blockMaliciousBots?'checked':''}> 拦截恶意爬虫/扫描器</label>
 </div>
 <div class="waf-row">
  <label>模式：<select id="waf-mode"><option value="adminApi" ${c.mode==='adminApi'?'selected':''}>管理端（不放行任何机器人）</option><option value="publicSite" ${c.mode==='publicSite'?'selected':''}>前台站点（放行搜索引擎/GEO-AI）</option></select></label>
  <label>CC 限速：<input id="waf-rate" type="number" min="0" style="width:90px" value="${Number(c.rateLimitPerMin||0)}"> 次/分钟/IP（0=关闭）</label>
 </div>
 <div class="waf-row">
  <label><input type="checkbox" id="waf-search" ${c.allowSearchBots?'checked':''}> 放行搜索引擎</label>
  <label><input type="checkbox" id="waf-aibots" ${c.allowAiBots?'checked':''}> 放行 GEO/AI 抓取</label>
  <label><input type="checkbox" id="waf-social" ${c.allowSocialPreviews?'checked':''}> 放行社交预览</label>
 </div>
 <div class="waf-row">攻击防护动作：SQL 注入 ${wafActSel('waf-sqli',c.sqliAction)} 路径穿透 ${wafActSel('waf-path',c.pathAction)} XSS ${wafActSel('waf-xss',c.xssAction)} 命令注入 ${wafActSel('waf-cmd',c.cmdAction)}</div>
 <div class="waf-row">
  <label>敏感前缀（禁一切机器人）<input id="waf-sensitive" value="${escAttr(wafList(c.sensitivePrefixes))}"></label>
  <label>IP 白名单<input id="waf-wl" value="${escAttr(wafList(c.ipWhitelist))}"></label>
  <label>IP 黑名单<input id="waf-bl" value="${escAttr(wafList(c.ipBlacklist))}"></label>
 </div>
 <div class="waf-row">
  <label><input type="checkbox" id="waf-emptyua" ${c.blockEmptyUa?'checked':''}> 拦截空 UA</label>
  <label><input type="checkbox" id="waf-loopback" ${c.loopbackBypass?'checked':''}> 回环地址豁免（本地工具）</label>
  <label><input type="checkbox" id="waf-verbose" ${c.verbose?'checked':''}> 详细日志</label>
 </div>
 <div style="margin-top:12px"><button class="btn btn-primary" onclick="wafSave()">💾 保存并立即生效</button></div>
</div>
<div class="card">
 <h3>📖 参考配置（完整 JSON）</h3>
 <p class="text-muted" style="font-size:12px;margin:2px 0 8px">高级项（如规则集 <code>ruleset</code>、跳过检测前缀 <code>skipAttackPrefixes</code>）可直接编辑 JSON 保存；也可直接编辑服务器配置文件（修改后 10 秒内热生效）。</p>
 <textarea id="waf-json" class="waf-json" spellcheck="false">${JSON.stringify(c,null,2).replace(/</g,'\\u003c')}</textarea>
 <div style="margin-top:10px"><button class="btn btn-primary" onclick="wafSaveJson()">💾 保存 JSON</button></div>
</div>`;
}
function wafCollect(){
 const c=Object.assign({},wafCfg,{
  enable:$('waf-enable').checked,
  mode:$('waf-mode').value,
  rateLimitPerMin:Math.max(0,parseInt($('waf-rate').value||'0',10)||0),
  allowSearchBots:$('waf-search').checked,
  allowAiBots:$('waf-aibots').checked,
  allowSocialPreviews:$('waf-social').checked,
  blockMaliciousBots:$('waf-block-mal').checked,
  blockEmptyUa:$('waf-emptyua').checked,
  loopbackBypass:$('waf-loopback').checked,
  verbose:$('waf-verbose').checked,
  sqliAction:$('waf-sqli').value,
  xssAction:$('waf-xss').value,
  pathAction:$('waf-path').value,
  cmdAction:$('waf-cmd').value,
  sensitivePrefixes:wafParseList($('waf-sensitive').value),
  ipWhitelist:wafParseList($('waf-wl').value),
  ipBlacklist:wafParseList($('waf-bl').value),
 });
 delete c.configFile;
 return c;
}
async function wafPost(c){
 try{
  const j=await api('/star/wafSave',{method:'POST',body:JSON.stringify(c)});
  if(j.code===0){showToast('WAF 配置已保存并生效','success');wafCfg=null;loadSecurity();}
  else showToast(j.message||'保存失败','error');
 }catch(e){}
}
function wafSave(){if(!$('waf-enable'))return;wafPost(wafCollect())}
function wafSaveJson(){
 try{const c=JSON.parse($('waf-json').value);wafPost(c)}catch(e){showToast('JSON 解析失败：'+e.message,'error')}
}

/* ── 页脚：服务端处理耗时（dhrust 为每个响应附加 X-Elapsed-Ms） ── */
function setSrvElapsed(v){
 const f=$('srv-foot');if(!f)return;
 const t=new Date().toLocaleTimeString('zh-CN',{hour12:false});
 f.textContent='服务端处理耗时：'+v+' ms（最近一次请求 · '+t+'）';
}

/* ── Dashboard ── */
let _dashboardBuilt=false;
let _diskSig=''; // 磁盘列表签名：磁盘增减时重建仪表盘
let _machineInfoBuilt=false;

async function loadDashboard(){
 if(document.hidden)return; // 后台标签暂停轮询（恢复可见时由 visibilitychange 立即补刷）
 try{
  // 单请求：health 指标已合并进 /api/status（后端复用同一批系统采集，省去第二次请求与重复采样）
  const s=await api('/api/status');
  if(s.code!==0)return;statusData=s.data;
  const d=s.data;const h={data:d.health||null};
  const title='StarAgent';
  document.title=title+' 管理面板';
  $('header-title').textContent=title+' 管理面板';
  $('header-meta').textContent='PID '+d.processId;
  const dot=$('status-dot-header');
  dot.className='dot status-dot '+(d.running?'on':'off');
  updateSecBanner(d);

  const diskSig=(d.disks||[]).map(x=>x.name).join(',');
  if(diskSig!==_diskSig){_diskSig=diskSig;_dashboardBuilt=false}
  if(!_dashboardBuilt){
   _dashboardBuilt=true;
   buildDashboard(d,h.data);
  }else{
   updateDashboard(d,h.data);
  }
  pushHistory(d);
  renderChart(d);
 }catch(e){}
}

// 默认密码安全横幅：仍为 admin/admin 时常显，改密后随 3 秒状态刷新自动消失
// 改密入口：内置管理员在【用户】页（admin 行·编辑）管理用户名/密码（配置页不再维护该凭据）
function updateSecBanner(d){
 const el=$('sec-banner');if(!el)return;
 if(d.defaultPassword){
  const hint=(me&&me.isAdmin)?'请尽快在【用户】页修改密码！':'请提醒管理员尽快修改密码！';
  el.textContent='⚠️ 安全提示：Web 面板仍在使用默认密码 admin/admin'+(d.remoteAccess?'，且已允许远程访问（LocalOnly=false），存在被自动化扫描利用的风险':'')+'，'+hint;
  el.classList.remove('hidden');
 }else{
  el.classList.add('hidden');
 }
}
function buildDashboard(d,health){
 const running=d.running;
 let html='';
 // 服务状态卡片
 html+=`<div class="glass-card card"><div class="flex-between mb-16"><h2>📊 服务状态</h2><span id="svc-indicator" class="status-indicator ${running?'running':'stopped'}"><span class="status-dot ${running?'on':'off'}"></span>${running?'运行中':'已停止'}</span></div>`;
 html+=`<div class="stats-grid">`;
 html+=statCard('服务名',d.displayName||d.serviceName,'','svc-name');
 html+=statCard('运行时长',d.uptime,'','svc-uptime');
 html+=statCard('进程 CPU',(d.processCpuRate||'0.0')+'%','font-mono','svc-cpu');
 html+=statCard('进程内存',d.memoryMB+' MB','','svc-mem',d.memoryMB>500?'text-yellow':'');
 html+=statCard('线程数',d.threadCount||0,'','svc-thr');
 html+=statCard('句柄数',d.handleCount||0,'','svc-hnd');
 html+=statCard('进程 ID',d.processId,'font-mono','svc-pid');
 html+=statCard('监听端口',d.port,'font-mono','svc-port');
 html+=statCard('启动时间',d.startTime,'text-secondary','svc-start');
 html+=statCard('连接数',d.tcpConnections||0,'','conn-val',d.tcpConnections>1000?'text-red':(d.tcpConnections>100?'text-yellow':'text-green'));
 html+=statCard('磁盘 IO',formatIops(d.diskIops),'font-mono','disk-iops');
 html+=`</div></div>`;

 // 资源监控（整机视角，与宝塔一致：CPU / 内存 / 磁盘×N / 负载）
 const cpuPct = d.cpuRateValue>0 ? Math.min(100, d.cpuRateValue) : (d.cpuRate?Math.min(100, parseFloat(d.cpuRate)):0);
 const memUsedMB = d.memoryUsedMB||0, memTotalMB = d.memoryTotalMB||0;
 const memPct = memTotalMB>0 ? pct(memUsedMB, memTotalMB) : 0;
 const hasLoad = d.load1!=null;
 // 宝塔口径：负载百分比 = 1 分钟均值 /（CPU 逻辑核数 × 2）（与宝塔 load.max 一致）
 const loadPct = hasLoad && d.cpuCount>0 ? Math.min(100, Math.round(d.load1/(d.cpuCount*2)*100)) : 0;

 html+=`<div class="glass-card card"><h2>💚 资源监控</h2><div class="gauge-row">`;
 // 顺序与宝塔一致：负载 → CPU → 内存 → 磁盘（负载仅 Linux 显示）
 if(hasLoad)html+=ringGauge('负载', d.load1.toFixed(2), d.load5.toFixed(2)+' / '+d.load15.toFixed(2), loadPct, 'gauge-load', '平均负载');
 html+=ringGauge('CPU 使用率', (d.cpuCount||0)+' 核', '', cpuPct, 'gauge-cpu');
 html+=ringGauge('内存', fmtMem(memUsedMB)+' / '+fmtMem(memTotalMB), '', memPct, 'gauge-mem');
 (d.disks||[]).forEach((x,i)=>{
  html+=ringGauge('磁盘 '+x.name, fmtMem(x.usedMB)+' / '+fmtMem(x.totalMB), '', pct(x.usedMB,x.totalMB), 'gauge-disk-'+i);
 });
 html+=`</div>`;
 if(health){
  html+=`<div class="mt-12"><div class="collapse-header" onclick="this.classList.toggle('open');document.getElementById('gc-body').classList.toggle('open')"><span class="arrow">▶</span> GC 与 CPU 统计</div>`;
  html+=`<div class="collapse-body" id="gc-body"><div class="gc-stats">`;
  html+=gcStat('Gen 0',health.gcCollections.gen0,'gc-gen0')+gcStat('Gen 1',health.gcCollections.gen1,'gc-gen1')+gcStat('Gen 2',health.gcCollections.gen2,'gc-gen2');
  html+=gcStat('GC 内存',health.gcTotalMemory+' MB','gc-mem')+gcStat('CPU 时间',health.totalProcessorTime+'s','gc-cpu');
  html+=`</div><div style="margin-top:12px;text-align:right"><button class="btn btn-ghost btn-sm" onclick="freeMemory()">🗑 释放内存</button></div></div></div>`;
 }
 html+=`</div>`;

 // 流量 / 磁盘 IO 趋势图（独立卡片，对齐宝塔：下划线页签 + 统计块 + 平滑双曲线）
 html+=`<div class="glass-card card"><div class="chart-head"><div class="chart-tabs"><span class="chart-tab active" id="tab-net" onclick="switchChart('net')">流量</span><span class="chart-tab" id="tab-disk" onclick="switchChart('disk')">磁盘 IO</span></div><span class="chart-hint">近 30 秒 · 每 3 秒采样</span></div>`;
 html+=`<div class="chart-stats" id="chart-stats"></div>`;
 html+=`<div class="chart-wrap"><svg id="trend-svg" viewBox="0 0 600 150"></svg></div></div>`;

 // 系统信息
 html+=`<div class="glass-card card"><h2>🖥 系统信息</h2><table class="info-table">`;
 html+=infoRow('操作系统',d.platform+(d.osVersion?'  '+d.osVersion:''));
 html+=infoRow('主机名',d.hostMachine);
 html+=infoRow('CPU',(d.cpuName||'-')+(d.cpuCount?' — '+d.cpuCount+' 核':''));
 if(d.cpuRate)html+=infoRow('CPU 使用率',d.cpuRate+'%',parseFloat(d.cpuRate)>80?'text-yellow':'');
 if(d.totalMemory)html+=infoRow('物理内存',d.totalMemory+(d.availableMemory?' — 可用 '+d.availableMemory+(d.freeMemory?' / 空闲 '+d.freeMemory:''):''));
 if(d.board)html+=infoRow('主板',d.board);
 if(d.machineGuid)html+=infoRow('设备 ID',d.machineGuid,'font-mono');
 if(d.localTime)html+=`<tr><td>服务器时间</td><td><span id="svc-time" class="font-mono">${esc(d.localTime)}</span><button class="btn btn-ghost btn-sm" style="margin-left:10px;padding:2px 10px;font-size:12px" onclick="syncServerTime(this)" title="将本机（浏览器）时间同步到服务器（需管理员/root 权限）">⟳ 同步时间</button></td></tr>`;
 html+=`</table></div>`;

 // 本机信息（折叠区块）
 html+=`<div class="glass-card card"><div class="collapse-header open" onclick="this.classList.toggle('open');document.getElementById('machine-body').classList.toggle('open')"><span class="arrow">▶</span> 本机详情</div>`;
 html+=`<div class="collapse-body open" id="machine-body"><div id="machine-detail" style="padding:8px 0"><p class="text-muted" style="text-align:center;padding:20px">加载中...</p></div></div></div>`;

 $('panel-dashboard').innerHTML=html;

 // 异步加载本机详情
 loadMachineDetail();
}

function updateDashboard(d,health){
 setText('svc-name',d.displayName||d.serviceName);
 setText('svc-uptime',d.uptime);
 setText('svc-pid',d.processId);
 setText('svc-port',d.port);
 setText('svc-cpu',(d.processCpuRate||'0.0')+'%');
 setText('svc-mem',d.memoryMB+' MB');
 setText('svc-thr',d.threadCount||0);
 setText('svc-hnd',d.handleCount||0);
 setText('svc-start',d.startTime);
 const ind=$('svc-indicator');
 if(ind){
  ind.className='status-indicator '+(d.running?'running':'stopped');
  ind.innerHTML=`<span class="status-dot ${d.running?'on':'off'}"></span>${d.running?'运行中':'已停止'}`;
 }
 const cpuPct = d.cpuRateValue>0 ? Math.min(100, d.cpuRateValue) : (d.cpuRate?Math.min(100, parseFloat(d.cpuRate)):0);
 const memUsedMB = d.memoryUsedMB||0, memTotalMB = d.memoryTotalMB||0;
 const memPct = memTotalMB>0 ? pct(memUsedMB, memTotalMB) : 0;
 const hasLoad = d.load1!=null;
 // 宝塔口径：负载百分比 = 1 分钟均值 /（CPU 逻辑核数 × 2）（与宝塔 load.max 一致）
 const loadPct = hasLoad && d.cpuCount>0 ? Math.min(100, Math.round(d.load1/(d.cpuCount*2)*100)) : 0;
 updateGauge('gauge-cpu', cpuPct, (d.cpuCount||0)+' 核', '');
 updateGauge('gauge-mem', memPct, fmtMem(memUsedMB)+' / '+fmtMem(memTotalMB), '');
 (d.disks||[]).forEach((x,i)=>{
  updateGauge('gauge-disk-'+i, pct(x.usedMB,x.totalMB), fmtMem(x.usedMB)+' / '+fmtMem(x.totalMB), '');
 });
 if(hasLoad)updateGauge('gauge-load', loadPct, d.load1.toFixed(2), d.load5.toFixed(2)+' / '+d.load15.toFixed(2));
 setText('conn-val', d.tcpConnections||0);
 setConnColor('conn-val', d.tcpConnections||0);
 setText('disk-iops', formatIops(d.diskIops));
 setText('svc-time', d.localTime||'');
 if(health){
  if(health.gcCollections){
   setText('gc-gen0',health.gcCollections.gen0);
   setText('gc-gen1',health.gcCollections.gen1);
   setText('gc-gen2',health.gcCollections.gen2);
   setText('gc-mem',health.gcTotalMemory+' MB');
   setText('gc-cpu',health.totalProcessorTime+'s');
  }
 }
 // 每30秒刷新本机详情
 if(!_machineInfoBuilt){_machineInfoBuilt=true;setInterval(loadMachineDetail,30000)}
}

async function loadMachineDetail(){
 if(document.hidden)return;
 const el=$('machine-detail');if(!el)return;
 try{
  const j=await api('/star/machine');
  if(j.code!==0){el.innerHTML=`<p class="text-muted">加载失败</p>`;return}
  const d=j.data;
  let html='';

  // 系统概览
  html+=`<div class="section-title">系统概览</div><table class="info-table">`;
  html+=infoRow('操作系统',d.os.os);
  html+=infoRow('主机名',d.os.hostName);
  html+=infoRow('运行时',d.os.runtime+' ('+d.os.processArch+')');
  html+=infoRow('系统架构',d.os.systemArch);
  html+=infoRow('处理器数',d.os.processorCount+' 核');
  html+=infoRow('运行时长',d.os.hostUptime);
  html+=`</table>`;

  // CPU 信息
  html+=`<div class="section-title mt-16">CPU</div><table class="info-table">`;
  html+=infoRow('型号',d.cpu.cpuName);
  html+=infoRow('逻辑核数',d.cpu.cpuCount);
  html+=infoRow('使用率',d.cpu.cpuRatePercent,parseFloat(d.cpu.cpuRate)>0.8?'text-yellow':'');
  html+=`</table>`;

  // 内存信息
  html+=`<div class="section-title mt-16">内存</div><table class="info-table">`;
  html+=infoRow('总量',d.memory.totalMemory);
  html+=infoRow('已用',d.memory.usedMemory,parseFloat(d.memory.memoryRate)>0.8?'text-yellow':'');
  html+=infoRow('可用',d.memory.availableMemory,'text-green');
  html+=infoRow('使用率',d.memory.memoryRatePercent,parseFloat(d.memory.memoryRate)>0.8?'text-yellow':'');
  html+=`</table>`;

  // 磁盘分区
  if(d.drives&&d.drives.length>0){
   html+=`<div class="section-title mt-16">磁盘分区</div><table class="info-table">`;
   html+=`<tr><td style="font-size:11px;color:var(--text-tertiary)">分区</td><td style="font-size:11px;color:var(--text-tertiary)">总量</td><td style="font-size:11px;color:var(--text-tertiary)">已用</td><td style="font-size:11px;color:var(--text-tertiary)">可用</td><td style="font-size:11px;color:var(--text-tertiary)">使用率</td></tr>`;
   d.drives.forEach(dr=>{
    const pct=parseFloat(dr.usedPercent);
    html+=`<tr><td style="font-family:var(--font-mono);font-size:12px">${esc(dr.name)} ${dr.label?esc('('+dr.label+')'):''}</td><td>${esc(dr.totalSize)}</td><td${pct>80?' class="text-yellow"':''}>${esc(dr.usedSize)}</td><td class="text-green">${esc(dr.freeSize)}</td><td>${esc(dr.usedPercent)}</td></tr>`;
   });
   html+=`</table>`;
  }

  // 网络接口
  if(d.nics&&d.nics.length>0){
   html+=`<div class="section-title mt-16">网络接口</div><table class="info-table">`;
   d.nics.forEach(n=>{
    html+=infoRow(n.name||n.description,`IP: ${n.ip}  MAC: ${n.mac}  速率: ${n.speed}  收: ${n.bytesReceived}  发: ${n.bytesSent}`);
   });
   html+=`</table>`;
  }

  // 进程 Top
  if(d.processes&&d.processes.length>0){
   html+=`<div class="section-title mt-16">进程 Top 15（按内存排序）</div><table class="info-table">`;
   html+=`<tr><td style="font-size:11px;color:var(--text-tertiary)">进程名</td><td style="font-size:11px;color:var(--text-tertiary)">PID</td><td style="font-size:11px;color:var(--text-tertiary)">内存</td><td style="font-size:11px;color:var(--text-tertiary)">线程</td><td style="font-size:11px;color:var(--text-tertiary)">CPU时间</td></tr>`;
   d.processes.forEach(p=>{
    html+=`<tr><td style="font-family:var(--font-mono);font-size:12px">${esc(p.name)}</td><td>${p.pid}</td><td>${p.memoryMB} MB</td><td>${p.threadCount}</td><td>${p.cpuTime}</td></tr>`;
   });
   html+=`</table>`;
  }

  // GPU 信息
  if(d.gpu&&d.gpu.length>0){
   html+=`<div class="section-title mt-16">GPU</div><table class="info-table">`;
   d.gpu.forEach(g=>{
    html+=infoRow(g.name||'显卡',`显存: ${g.ram}  驱动: ${g.driverVersion}`);
   });
   html+=`</table>`;
  }

  el.innerHTML=html;
 }catch(e){el.innerHTML=`<p class="text-muted">加载失败</p>`}
}

/* ── 流量统计（网站访问日志 + 端口计数） ── */
const TH='font-size:11px;color:var(--text-tertiary)'; // 流量表头样式
function fmtBytes(v){
 v=Number(v)||0;
 if(v>=1099511627776)return (v/1099511627776).toFixed(2)+' TB';
 if(v>=1073741824)return (v/1073741824).toFixed(2)+' GB';
 if(v>=1048576)return (v/1048576).toFixed(2)+' MB';
 if(v>=1024)return (v/1024).toFixed(2)+' KB';
 return v+' B';
}
function fmtRate(v){return v==null?'—':fmtBytes(v)+'/s'}
function ensureTrafficShell(){
 if($('web-traffic-body'))return;
 $('panel-traffic').innerHTML=`
  <div class="chart-tabs" id="traffic-sub-tabs" style="margin-bottom:14px">
   <span class="chart-tab active" data-sub="site" onclick="trafficSubSwitch('site')">📈 网站流量</span>
   <span class="chart-tab" data-sub="ports" onclick="trafficSubSwitch('ports')">🔌 端口流量</span>
  </div>
  <div id="traffic-sub-site">
   <div class="glass-card card">
    <h2>📈 网站流量 <span style="font-size:11px;font-weight:400;color:var(--text-tertiary)">解析访问日志（nginx / Apache / Caddy）· 今日与累计</span></h2>
    <div id="web-traffic-body"><p class="text-muted">加载中…</p></div>
   </div>
   <div class="glass-card card">
    <h2>🗓 历史数据 <span style="font-size:11px;font-weight:400;color:var(--text-tertiary)">每日归档（跨天自动保存）· 保留 <span id="hist-retention">—</span> 天</span></h2>
    <div style="display:flex;align-items:center;gap:18px;flex-wrap:wrap;margin-bottom:12px">
     <span class="chart-tabs" id="hist-ranges"></span>
     <select id="hist-site" onchange="histSite=this.value;renderHistChart();renderHistTable();" style="max-width:240px"></select>
     <span style="flex:1"></span>
     <button class="btn btn-ghost btn-sm" onclick="loadTrafficHistory(true)">🔄 刷新</button>
    </div>
    <div id="hist-chart"><p class="text-muted">加载中…</p></div>
    <div id="traffic-history-body" class="mt-16"></div>
   </div>
  </div>
  <div id="traffic-sub-ports" style="display:none">
   <div class="glass-card card">
    <h2>🔌 端口流量 <span id="port-traffic-mode" style="font-size:11px;font-weight:400;color:var(--text-tertiary)"></span></h2>
    <div id="port-traffic-body"><p class="text-muted">加载中…</p></div>
   </div>
   <div class="glass-card card">
    <h2>🧭 端口每日数据 <span style="font-size:11px;font-weight:400;color:var(--text-tertiary)">按端口查看每日收发（nftables 计数归档 · 点击端口行钻取）</span></h2>
    <div style="display:flex;align-items:center;gap:14px;flex-wrap:wrap;margin-bottom:12px">
     <span class="chart-tabs" id="port-ranges"></span>
     <input id="port-filter" placeholder="筛选端口（如 22、tcp）" oninput="portKeyword=this.value;renderPortSummary()" style="max-width:200px">
     <span style="flex:1"></span>
     <span class="text-muted" style="font-size:12px" id="port-page-note"></span>
     <button class="btn btn-ghost btn-sm" onclick="loadTrafficHistory(true)">🔄 刷新</button>
    </div>
    <div id="port-summary-body"><p class="text-muted">加载中…</p></div>
   </div>
   <div class="glass-card card" id="port-detail-card" style="display:none">
    <h2 id="port-detail-title"></h2>
    <div id="port-detail-body"></div>
   </div>
   <div class="glass-card card">
    <h2>🗓 端口每日流量（按日期） <span style="font-size:11px;font-weight:400;color:var(--text-tertiary)">每日总量与端口明细（Linux nftables 计数 · 仅含有流量的端口）</span></h2>
    <div id="port-history-body"><p class="text-muted">加载中…</p></div>
   </div>
  </div>`;
}
// 流量页内子页签切换（site 网站流量 / ports 端口流量）
function trafficSubSwitch(sub){
 applyTrafficSub(sub);
 const h=sub==='ports'?'#traffic.ports':'#traffic';
 if(location.hash!==h)history.replaceState(null,'',h);
}
function applyTrafficSub(sub){
 const site=$('traffic-sub-site'),ports=$('traffic-sub-ports');if(!site||!ports)return;
 site.style.display=sub==='ports'?'none':'';
 ports.style.display=sub==='ports'?'':'none';
 document.querySelectorAll('#traffic-sub-tabs .chart-tab').forEach(el=>el.classList.toggle('active',el.dataset.sub===sub));
}
async function loadTraffic(){
 if(document.hidden)return;
 ensureTrafficShell();
 try{
  const [w,p]=await Promise.all([api('/star/webTraffic'),api('/star/portTraffic')]);
  renderWebTraffic(w.code===0?w.data:null);
  renderPortTraffic(p.code===0?p.data:null);
  // 历史数据：首次进入拉取；跨天（本地日期变化）自动刷新；其余轮询不重复请求
  const stamp=new Date().toDateString();
  if(stamp!==histStamp){histStamp=stamp;loadTrafficHistory(true)}
  else loadTrafficHistory(false);
 }catch(e){}
}
function renderWebTraffic(d){
 const el=$('web-traffic-body');if(!el)return;
 if(!d){el.innerHTML='<p class="text-muted">加载失败</p>';return}
 if(!d.enabled){el.innerHTML=`<p class="text-muted">${esc(d.message||'网站流量统计未启用（配置 WebTraffic=true 开启）')}</p>`;return}
 const sites=d.sites||[];
 if(!sites.length){el.innerHTML=`<p class="text-muted">${esc(d.message||'未发现网站日志')}</p>`;return}
 let html=`<table class="info-table"><tr>`;
 html+=['站点','今日流量','请求','UV','2xx / 4xx / 5xx','速率','累计流量'].map(h=>`<td style="${TH}">${h}</td>`).join('');
 html+=`</tr>`;
 sites.forEach(s=>{
  const logs=(s.logs||[]).join('\n');
  html+=`<tr>
   <td style="font-family:var(--font-mono);font-size:12px;word-break:break-all" title="${esc(logs)}">${esc(s.name)}</td>
   <td>${fmtBytes(s.todayBytes)}</td>
   <td>${s.todayRequests||0}</td>
   <td>${s.uv||0}</td>
   <td><span class="text-green">${s.s2xx||0}</span> / <span class="${s.s4xx?'text-yellow':'text-muted'}">${s.s4xx||0}</span> / <span class="${s.s5xx?'text-red':'text-muted'}">${s.s5xx||0}</span></td>
   <td>${fmtRate(s.rateBps)}</td>
   <td>${fmtBytes(s.totalBytes)}</td>
  </tr>`;
 });
 el.innerHTML=html+`</table>`;
}
function renderPortTraffic(d){
 const el=$('port-traffic-body');if(!el)return;
 const modeEl=$('port-traffic-mode');
 if(!d){el.innerHTML='<p class="text-muted">加载失败</p>';return}
 if(modeEl)modeEl.textContent=d.mode==='nft'?'（nftables 计数 · 5 秒采样）':(d.mode==='view'?'（连接视图 · 无字节计数）':'');
 if(!d.enabled){el.innerHTML=`<p class="text-muted">端口流量统计未启用（在配置页把 PortTraffic 设为 true；Linux 需 root 与 nft 命令，Windows 为连接视图）</p>`;return}
 const rows=d.ports||[];
 const note=d.message?`<p class="text-muted" style="margin-bottom:10px">${esc(d.message)}</p>`:'';
 if(!rows.length){el.innerHTML=note+'<p class="text-muted">暂无数据（采样中…）</p>';return}
 let html=note+`<table class="info-table"><tr>`;
 html+=['端口','协议','状态','连接数','接收','发送','↓ 速率','↑ 速率'].map(h=>`<td style="${TH}">${h}</td>`).join('');
 html+=`</tr>`;
 rows.forEach(r=>{
  const st=r.listen?'<span class="text-green">监听中</span>':'—';
  html+=`<tr>
   <td style="font-family:var(--font-mono);font-size:12px">${r.port}</td>
   <td>${esc((r.proto||'').toUpperCase())}</td>
   <td>${st}</td>
   <td>${r.conns||0}</td>
   <td>${r.rxBytes==null?'—':fmtBytes(r.rxBytes)}</td>
   <td>${r.txBytes==null?'—':fmtBytes(r.txBytes)}</td>
   <td>${fmtRate(r.rxBps)}</td>
   <td>${fmtRate(r.txBps)}</td>
  </tr>`;
 });
 el.innerHTML=html+`</table>`;
}

/* ── 流量历史（每日归档） ── */
async function loadTrafficHistory(force){
 if(histData&&!force)return;
 try{
  const r=await api('/star/trafficHistory?days=90');
  if(r.code===0){histData=r.data||{days:[]};renderTrafficHistory()}
 }catch(e){}
}
function renderTrafficHistory(){
 const ret=$('hist-retention');if(ret)ret.textContent=(histData&&histData.retentionDays!=null)?histData.retentionDays:'—';
 renderHistRanges();renderHistSites();renderHistChart();renderHistTable();renderPortHistory();
 if($('port-summary-body'))renderPortPage(); // 端口流量子页面已打开时同步刷新
}
function histSetDays(n){histDays=n;renderHistRanges();renderHistChart();renderHistTable()}
function renderHistRanges(){
 const el=$('hist-ranges');if(!el)return;
 el.innerHTML=[7,30,90].map(n=>`<span class="chart-tab${n===histDays?' active':''}" onclick="histSetDays(${n})">最近 ${n} 天</span>`).join('');
}
function histAllDays(){return (histData&&histData.days)||[]}
function histViewDays(){return histAllDays().slice(-histDays)}
function histDayAgg(d){ // 按站点过滤聚合当日数据（全部站点时 UV 为各站之和，可能有重复）
 const sites=(d&&d.web&&d.web.sites)||{};
 let a={hits:0,bytes:0,uv:0,s2xx:0,s3xx:0,s4xx:0,s5xx:0};
 if(histSite){const s=sites[histSite];if(!s)return a;return {hits:s.hits||0,bytes:s.bytes||0,uv:s.uv||0,s2xx:s.s2xx||0,s3xx:s.s3xx||0,s4xx:s.s4xx||0,s5xx:s.s5xx||0}}
 for(const k in sites){const s=sites[k];a.hits+=s.hits||0;a.bytes+=s.bytes||0;a.uv+=s.uv||0;a.s2xx+=s.s2xx||0;a.s3xx+=s.s3xx||0;a.s4xx+=s.s4xx||0;a.s5xx+=s.s5xx||0}
 return a;
}
function renderHistSites(){
 const el=$('hist-site');if(!el)return;
 const names=new Set();
 histAllDays().forEach(d=>{const s=(d.web&&d.web.sites)||{};Object.keys(s).forEach(k=>names.add(k))});
 const list=[...names].sort();
 if(histSite&&!names.has(histSite))histSite='';
 el.innerHTML=`<option value="">全部站点</option>`+list.map(n=>`<option value="${escAttr(n)}"${n===histSite?' selected':''}>${esc(n)}</option>`).join('');
}
function renderHistChart(){
 const el=$('hist-chart');if(!el)return;
 if(!histData){el.innerHTML='<p class="text-muted">加载中…</p>';return}
 const days=histViewDays().filter(d=>{const a=histDayAgg(d);return a.hits>0||a.bytes>0});
 if(!days.length){el.innerHTML=`<p class="text-muted">暂无历史数据（跨天后自动归档；刚启用时需运行至次日零点）</p>`;return}
 const vals=days.map(d=>histDayAgg(d).bytes);
 const max=Math.max(...vals,1);
 const W=720,H=190,padL=58,padR=10,padT=12,padB=26;
 const innerW=W-padL-padR,innerH=H-padT-padB,step=innerW/days.length;
 const bw=Math.max(2,Math.min(26,step-3));
 let bars='';
 days.forEach((d,i)=>{
  const a=histDayAgg(d);
  const h=Math.max(1,Math.round(innerH*(a.bytes/max)));
  const x=padL+i*step+(step-bw)/2,y=H-padB-h;
  bars+=`<rect x="${x.toFixed(1)}" y="${y}" width="${bw.toFixed(1)}" height="${h}" rx="2" fill="url(#histGrad)"><title>${esc(d.date)}：流量 ${fmtBytes(a.bytes)} · 请求 ${a.hits} · 独立IP ${a.uv}</title></rect>`;
 });
 let grid='';
 [0,0.5,1].forEach(f=>{const y=H-padB-innerH*f;grid+=`<line x1="${padL}" y1="${y}" x2="${W-padR}" y2="${y}" stroke="rgba(128,128,128,0.25)" stroke-width="1"${f===0?'':' stroke-dasharray="3,3"'}/><text x="${padL-8}" y="${y+3}" text-anchor="end" font-size="10" fill="var(--text-tertiary)">${fmtBytes(Math.round(max*f))}</text>`});
 let xs='';const skip=Math.max(1,Math.ceil(days.length/7));
 days.forEach((d,i)=>{if(i%skip===0||i===days.length-1){const x=padL+i*step+step/2;xs+=`<text x="${x.toFixed(1)}" y="${H-8}" text-anchor="middle" font-size="10" fill="var(--text-tertiary)">${esc(d.date.slice(5))}</text>`}});
 el.innerHTML=`<svg viewBox="0 0 ${W} ${H}" style="width:100%;height:auto"><defs><linearGradient id="histGrad" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#3b82f6"/><stop offset="1" stop-color="#3b82f6" stop-opacity="0.35"/></linearGradient></defs>${grid}${bars}${xs}</svg>`;
}
function renderHistTable(){
 const el=$('traffic-history-body');if(!el)return;
 if(!histData){el.innerHTML='';return}
 const days=histViewDays().slice().reverse();
 if(!days.length){el.innerHTML='';return}
 let sum={hits:0,bytes:0,uv:0,s4xx:0,s5xx:0};
 let html=`<table class="info-table"><tr>`+['日期','请求','流量','独立IP','4xx / 5xx'].map(h=>`<td style="${TH}">${h}</td>`).join('')+`</tr>`;
 days.forEach(d=>{
  const a=histDayAgg(d);sum.hits+=a.hits;sum.bytes+=a.bytes;sum.uv+=a.uv;sum.s4xx+=a.s4xx;sum.s5xx+=a.s5xx;
  html+=`<tr><td style="font-family:var(--font-mono);font-size:12px">${esc(d.date)}</td><td>${a.hits}</td><td>${fmtBytes(a.bytes)}</td><td>${a.uv}</td><td><span class="${a.s4xx?'text-yellow':'text-muted'}">${a.s4xx}</span> / <span class="${a.s5xx?'text-red':'text-muted'}">${a.s5xx}</span></td></tr>`;
 });
 html+=`<tr><td style="${TH}">合计（${days.length} 天）</td><td style="${TH}">${sum.hits}</td><td style="${TH}">${fmtBytes(sum.bytes)}</td><td style="${TH}">≈${sum.uv}</td><td style="${TH}">${sum.s4xx} / ${sum.s5xx}</td></tr>`;
 el.innerHTML=html+`</table>`;
}
function renderPortHistory(){
 const el=$('port-history-body');if(!el)return;
 if(!histData){el.innerHTML='<p class="text-muted">加载中…</p>';return}
 const days=histViewDays().slice().reverse();
 // 仅统计有流量的端口（零流量端口不入档、也不展示），按总流量降序
 const dayKeys=d=>{const ports=d.ports||{};return Object.keys(ports).filter(k=>(ports[k].rx||0)+(ports[k].tx||0)>0).sort((a,b)=>((ports[b].rx||0)+(ports[b].tx||0))-((ports[a].rx||0)+(ports[a].tx||0)))};
 const any=days.some(d=>dayKeys(d).length);
 if(!any){el.innerHTML='<p class="text-muted">暂无端口历史（仅 Linux nftables 计数模式按天归档；连接视图/Windows 无字节数）</p>';return}
 let html=`<table class="info-table"><tr>`+['日期','总接收','总发送','端口明细（接收 / 发送，按流量排序）'].map(h=>`<td style="${TH}">${h}</td>`).join('')+`</tr>`;
 days.forEach(d=>{
  const ports=d.ports||{},keys=dayKeys(d);
  let rx=0,tx=0,text='—';
  keys.forEach(k=>{rx+=ports[k].rx||0;tx+=ports[k].tx||0});
  if(keys.length){
   const shown=keys.slice(0,6);
   text=shown.map(k=>`<span style="font-family:var(--font-mono)">${esc(k)}</span> <span class="text-muted">收</span> ${fmtBytes(ports[k].rx||0)} <span class="text-muted">发</span> ${fmtBytes(ports[k].tx||0)}`).join('&emsp;');
   if(keys.length>shown.length){
    const rest=keys.slice(shown.length).map(k=>`${k}  收 ${fmtBytes(ports[k].rx||0)}  发 ${fmtBytes(ports[k].tx||0)}`).join('\n');
    text+=`&emsp;<span class="text-muted" style="cursor:help;border-bottom:1px dashed currentColor" title="${escAttr(rest).replace(/\n/g,'&#10;')}">…等 ${keys.length} 个端口</span>`;
   }
  }
  html+=`<tr><td style="font-family:var(--font-mono);font-size:12px">${esc(d.date)}</td><td>${fmtBytes(rx)}</td><td>${fmtBytes(tx)}</td><td style="font-size:12px">${text}</td></tr>`;
 });
 el.innerHTML=html+`</table>`;
}

/* ── 端口流量子页面（每端口每日数据；位于“流量”页签内的子页） ── */
let portDays=30,portSel='',portKeyword='';
function portViewDays(){return ((histData&&histData.days)||[]).slice(-portDays)}
function portDayKeys(d){const ports=d.ports||{};return Object.keys(ports).filter(k=>(ports[k].rx||0)+(ports[k].tx||0)>0)}
function portAggregate(){
 const map=new Map();
 portViewDays().forEach(d=>{
  portDayKeys(d).forEach(k=>{
   const c=d.ports[k];let a=map.get(k);
   if(!a){a={rx:0,tx:0,days:0,last:''};map.set(k,a)}
   a.rx+=c.rx||0;a.tx+=c.tx||0;a.days++;if(d.date>a.last)a.last=d.date;
  });
 });
 return map;
}
function portSetDays(n){portDays=n;renderPortPage()}
function portSelect(k){portSel=k;renderPortSummary();renderPortDetail()}
function renderPortPage(){
 const el=$('port-summary-body');if(!el)return;
 if(!histData){el.innerHTML='<p class="text-muted">加载中…</p>';return}
 const ranges=$('port-ranges');
 if(ranges)ranges.innerHTML=[7,30,90].map(n=>`<span class="chart-tab${n===portDays?' active':''}" onclick="portSetDays(${n})">最近 ${n} 天</span>`).join('');
 // 默认选中总流量最大的端口（保持已选）
 const agg=portAggregate();
 if(!agg.has(portSel)){portSel=agg.size?[...agg.entries()].sort((a,b)=>(b[1].rx+b[1].tx)-(a[1].rx+a[1].tx))[0][0]:''}
 renderPortSummary();renderPortDetail();
}
function renderPortSummary(){
 const el=$('port-summary-body');if(!el)return;
 const agg=portAggregate();
 const days=portViewDays().length;
 const totalRx=[...agg.values()].reduce((s,a)=>s+a.rx,0),totalTx=[...agg.values()].reduce((s,a)=>s+a.tx,0);
 const note=$('port-page-note');
 if(note)note.textContent=agg.size?`共 ${agg.size} 个端口 · 总接收 ${fmtBytes(totalRx)} · 总发送 ${fmtBytes(totalTx)}`:(histData?'无数据':'');
 if(!agg.size){el.innerHTML='<p class="text-muted">暂无端口历史（仅 Linux nftables 计数模式按天归档；连接视图/Windows 无字节数）</p>';return}
 const kw=(portKeyword||'').trim().toLowerCase();
 const rows=[...agg.entries()].filter(([k])=>!kw||k.toLowerCase().includes(kw)).sort((a,b)=>(b[1].rx+b[1].tx)-(a[1].rx+a[1].tx));
 if(!rows.length){el.innerHTML='<p class="text-muted">无匹配端口（关键词：'+esc(portKeyword)+'）</p>';return}
 const grand=Math.max(1,totalRx+totalTx),dayCnt=Math.max(1,days);
 let html=`<table class="info-table"><tr>`+['端口','总接收','总发送','合计','日均','活跃天数','最近活跃','占比'].map(h=>`<td style="${TH}">${h}</td>`).join('')+`</tr>`;
 rows.forEach(([k,a])=>{
  const sum=a.rx+a.tx;
  html+=`<tr style="cursor:pointer${k===portSel?';background:rgba(59,130,246,0.10)':''}" onclick="portSelect('${escAttr(k)}')">
   <td style="font-family:var(--font-mono);font-size:12px">${esc(k)}</td>
   <td>${fmtBytes(a.rx)}</td>
   <td>${fmtBytes(a.tx)}</td>
   <td><b>${fmtBytes(sum)}</b></td>
   <td>${fmtBytes(Math.round(sum/dayCnt))}</td>
   <td>${a.days} / ${days}</td>
   <td style="font-family:var(--font-mono);font-size:12px">${esc(a.last)}</td>
   <td>${(sum/grand*100).toFixed(1)}%</td>
  </tr>`;
 });
 el.innerHTML=html+`</table>`;
}
function renderPortDetail(){
 const card=$('port-detail-card');if(!card)return;
 const agg=portAggregate();
 const a=agg.get(portSel);
 if(!agg.size||!a){card.style.display='none';return}
 const title=$('port-detail-title');
 if(title)title.innerHTML=`🧭 <span style="font-family:var(--font-mono)">${esc(portSel)}</span> 每日数据 <span style="font-size:11px;font-weight:400;color:var(--text-tertiary)">最近 ${portDays} 天 · 共接收 ${fmtBytes(a.rx)} / 发送 ${fmtBytes(a.tx)}</span>`;
 const body=$('port-detail-body');if(!body)return;
 body.innerHTML=portDetailChart()+portDetailTable();
 card.style.display='';
}
function portDetailChart(){
 const days=portViewDays();
 if(!days.length)return '';
 const rows=days.map(d=>{const c=(d.ports||{})[portSel]||{};return{date:d.date,rx:c.rx||0,tx:c.tx||0}});
 const max=Math.max(...rows.map(r=>r.rx+r.tx),1);
 const W=720,H=190,padL=58,padR=10,padT=12,padB=26;
 const innerW=W-padL-padR,innerH=H-padT-padB,step=innerW/days.length;
 const bw=Math.max(2,Math.min(26,step-3));
 let bars='';
 rows.forEach((r,i)=>{
  const sum=r.rx+r.tx;if(sum<=0)return;
  const hTotal=Math.max(2,Math.round(innerH*(sum/max)));
  const hRx=Math.round(hTotal*(r.rx/sum));
  const x=padL+i*step+(step-bw)/2,y=H-padB-hTotal;
  const tip=`${r.date}  收 ${fmtBytes(r.rx)}  发 ${fmtBytes(r.tx)}  合计 ${fmtBytes(sum)}`;
  bars+=`<g><title>${esc(tip)}</title>`;
  if(hRx>0)bars+=`<rect x="${x.toFixed(1)}" y="${(y+hTotal-hRx)}" width="${bw.toFixed(1)}" height="${hRx}" rx="1.5" fill="#3b82f6" opacity="0.85"/>`;
  if(hTotal-hRx>0)bars+=`<rect x="${x.toFixed(1)}" y="${y}" width="${bw.toFixed(1)}" height="${hTotal-hRx}" rx="1.5" fill="#f59e0b" opacity="0.85"/>`;
  bars+=`</g>`;
 });
 let grid='';[0,0.5,1].forEach(f=>{const y=H-padB-innerH*f;grid+=`<line x1="${padL}" y1="${y}" x2="${W-padR}" y2="${y}" stroke="rgba(128,128,128,0.25)" stroke-width="1"${f===0?'':' stroke-dasharray="3,3"'}/><text x="${padL-8}" y="${y+3}" text-anchor="end" font-size="10" fill="var(--text-tertiary)">${fmtBytes(Math.round(max*f))}</text>`});
 let xs='';const skip=Math.max(1,Math.ceil(days.length/7));
 days.forEach((d,i)=>{if(i%skip===0||i===days.length-1){const x=padL+i*step+step/2;xs+=`<text x="${x.toFixed(1)}" y="${H-8}" text-anchor="middle" font-size="10" fill="var(--text-tertiary)">${esc(d.date.slice(5))}</text>`}});
 return `<svg viewBox="0 0 ${W} ${H}" style="width:100%;height:auto">${grid}${bars}${xs}</svg>
  <div class="text-muted" style="font-size:11px;margin:2px 0 12px"><span style="color:#3b82f6">■</span> 接收　<span style="color:#f59e0b">■</span> 发送　（堆叠柱，悬停查看当日明细）</div>`;
}
function portDetailTable(){
 const days=portViewDays().slice().reverse();
 let rx=0,tx=0,cnt=0;
 let html=`<table class="info-table"><tr>`+['日期','接收','发送','合计'].map(h=>`<td style="${TH}">${h}</td>`).join('')+`</tr>`;
 days.forEach(d=>{
  const c=(d.ports||{})[portSel]||{};const r=c.rx||0,t=c.tx||0,sum=r+t;
  if(sum>0){rx+=r;tx+=t;cnt++}
  html+=`<tr><td style="font-family:var(--font-mono);font-size:12px">${esc(d.date)}</td><td>${sum>0?fmtBytes(r):'—'}</td><td>${sum>0?fmtBytes(t):'—'}</td><td>${sum>0?'<b>'+fmtBytes(sum)+'</b>':'—'}</td></tr>`;
 });
 html+=`<tr><td style="${TH}">合计（${cnt} 个活跃日）</td><td style="${TH}">${fmtBytes(rx)}</td><td style="${TH}">${fmtBytes(tx)}</td><td style="${TH}">${fmtBytes(rx+tx)}</td></tr>`;
 return html+`</table>`;
}

/* ── 数据库管理（只读 SQL 查询 / 备份 / 还原） ── */
function ensureDbShell(){
 if($('db-info-body'))return;
 $('panel-database').innerHTML=`
  <div class="glass-card card">
   <h2>🗄 数据库 <span style="font-size:11px;font-weight:400;color:var(--text-tertiary)">流量历史库（SQLite · Pek.RCode/XCode 模型管理）</span></h2>
   <div id="db-info-body"><p class="text-muted">加载中…</p></div>
  </div>
  <div class="glass-card card">
   <h2>🔍 SQL 查询 <span style="font-size:11px;font-weight:400;color:var(--text-tertiary)">仅允许单条 SELECT（不开放增删改；最多显示 500 行）</span></h2>
   <textarea id="db-sql" spellcheck="false" placeholder="SELECT * FROM Agent_PortTrafficDaily ORDER BY StatDate DESC LIMIT 20" style="width:100%;min-height:88px;box-sizing:border-box;font-family:var(--font-mono);font-size:12px;background:transparent;color:inherit;border:1px solid #8883;border-radius:6px;padding:8px"></textarea>
   <div style="display:flex;gap:10px;align-items:center;margin-top:10px;flex-wrap:wrap">
    <button class="btn btn-primary btn-sm" onclick="runDbQuery()">▶ 执行查询</button>
    <button class="btn btn-ghost btn-sm" onclick="dbSample('web')">示例：网站表</button>
    <button class="btn btn-ghost btn-sm" onclick="dbSample('ports')">示例：端口表</button>
    <span id="db-query-meta" class="text-muted" style="font-size:12px"></span>
   </div>
   <div id="db-query-result" class="mt-16"></div>
  </div>
  <div class="glass-card card">
   <h2>💾 备份与还原 <span style="font-size:11px;font-weight:400;color:var(--text-tertiary)">备份文件保存在服务器 Data/Backup 目录（XCode DbTable 包，与 C# 生态互通）</span></h2>
   <div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap">
    <button class="btn btn-primary btn-sm" onclick="createDbBackup()">➕ 立即备份</button>
    <label class="btn btn-ghost btn-sm" style="cursor:pointer;display:inline-block">📤 上传还原<input type="file" id="db-restore-file" accept=".zip" style="display:none" onchange="uploadDbRestore(this)"></label>
    <span class="text-muted" style="font-size:12px">还原会先清空两张流量表再导入备份数据（覆盖操作，请谨慎）</span>
   </div>
   <div id="db-backup-list" class="mt-16"><p class="text-muted">加载中…</p></div>
  </div>`;
}
async function loadDatabase(){
 ensureDbShell();
 await loadDbInfo();
 await loadDbBackups();
}
async function loadDbInfo(){
 const el=$('db-info-body');if(!el)return;
 try{
  const r=await api('/star/dbInfo');
  if(r.code!==0){el.innerHTML=`<p class="text-muted">${esc(r.message||'读取失败')}</p>`;return}
  const d=r.data||{};
  let html=`<table class="info-table">`;
  html+=`<tr><td style="${TH}">数据库</td><td style="font-family:var(--font-mono);font-size:12px">${esc(d.path||'—')}</td></tr>`;
  html+=`<tr><td style="${TH}">类型 / 大小</td><td>${esc(d.provider||'')} · ${fmtBytes(d.sizeBytes||0)}</td></tr>`;
  (d.tables||[]).forEach(t=>{html+=`<tr><td style="${TH}">${esc(t.name)}</td><td>${t.rows==null?'—':t.rows+' 行'}</td></tr>`});
  html+=`</table>`;
  if(d.error)html+=`<p class="text-muted" style="margin-top:8px">⚠ ${esc(d.error)}</p>`;
  el.innerHTML=html;
 }catch(e){el.innerHTML='<p class="text-muted">加载失败</p>'}
}
function dbSample(which){
 const el=$('db-sql');if(!el)return;
 el.value=which==='ports'
  ?'SELECT * FROM Agent_PortTrafficDaily ORDER BY StatDate DESC, Rx DESC LIMIT 20'
  :'SELECT * FROM Agent_WebTrafficDaily ORDER BY StatDate DESC LIMIT 20';
 el.focus();
}
async function runDbQuery(){
 const sql=($('db-sql')||{}).value||'';
 const meta=$('db-query-meta'),out=$('db-query-result');
 if(!sql.trim()){showToast('请输入 SQL','error');return}
 if(sql.includes(';')){showToast('不允许分号（仅支持单条 SELECT）','error');return}
 if(meta)meta.textContent='执行中…';
 try{
  const r=await api('/star/dbQuery',{method:'POST',body:JSON.stringify({sql})});
  if(r.code!==0){if(meta)meta.textContent='';if(out)out.innerHTML=`<p class="text-muted">${esc(r.message||'执行失败')}</p>`;return}
  const d=r.data||{};
  if(meta)meta.textContent=`${d.rowCount||0} 行${d.truncated?'（已截断，仅显示前 500 行）':''} · ${d.elapsedMs||0} ms`;
  renderDbResult(d);
 }catch(e){if(meta)meta.textContent=''}
}
function renderDbResult(d){
 const el=$('db-query-result');if(!el)return;
 const cols=d.columns||[],rows=d.rows||[];
 if(!cols.length){el.innerHTML='<p class="text-muted">（无结果集）</p>';return}
 let html=`<table class="info-table"><tr>`+cols.map(c=>`<td style="${TH}">${esc(c)}</td>`).join('')+`</tr>`;
 rows.forEach(r=>{
  html+='<tr>'+r.map(v=>{
   if(v==null)return '<td class="text-muted">NULL</td>';
   if(typeof v==='object')v=JSON.stringify(v);
   return `<td style="font-size:12px;word-break:break-all;max-width:420px">${esc(String(v))}</td>`;
  }).join('')+'</tr>';
 });
 el.innerHTML=html+'</table>';
}
async function createDbBackup(){
 try{
  showToast('正在创建备份…');
  const r=await api('/star/dbCreateBackup',{method:'POST'});
  if(r.code===0){showToast('备份完成：'+((r.data||{}).name||''),'success');loadDbBackups();}
  else showToast('备份失败：'+(r.message||''),'error');
 }catch(e){showToast('备份失败：'+e.message,'error')}
}
async function loadDbBackups(){
 const el=$('db-backup-list');if(!el)return;
 try{
  const r=await api('/star/dbListBackups');
  if(r.code!==0){el.innerHTML=`<p class="text-muted">${esc(r.message||'读取失败')}</p>`;return}
  const d=r.data||{},items=d.items||[];
  let html=`<p class="text-muted" style="font-size:12px;margin:0 0 6px">备份目录：${esc(d.dir||'')}${items.length?'（'+items.length+' 个）':''}</p>`;
  if(!items.length){el.innerHTML=html+'<p class="text-muted" style="font-size:12px">暂无备份文件，点击“立即备份”创建。</p>';return}
  html+=`<table class="info-table"><tr><td style="${TH}">文件</td><td style="${TH}">大小</td><td style="${TH}">时间</td><td style="${TH}">操作</td></tr>`;
  items.forEach(it=>{
   const n=it.name;
   html+=`<tr><td style="font-family:var(--font-mono);font-size:12px">${esc(n)}</td><td>${fmtBytes(it.sizeBytes||0)}</td><td style="font-size:12px">${fmtDbTime(it.created)}</td><td style="white-space:nowrap">`+
    `<button class="btn btn-ghost btn-sm" onclick="downloadDbBackupFile('${n}')">下载</button>`+
    `<button class="btn btn-ghost btn-sm" onclick="restoreDbBackupFile('${n}')">还原</button>`+
    `<button class="btn btn-ghost btn-sm" onclick="deleteDbBackupFile('${n}')">删除</button>`+
    `</td></tr>`;
  });
  el.innerHTML=html+`</table>`;
 }catch(e){el.innerHTML='<p class="text-muted">加载失败</p>'}
}
function fmtDbTime(sec){
 if(!sec)return '—';
 const d=new Date(sec*1000),p=n=>String(n).padStart(2,'0');
 return `${d.getFullYear()}-${p(d.getMonth()+1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}
async function downloadDbBackupFile(name){
 try{
  const r=await fetch('/star/dbDownloadBackup?name='+encodeURIComponent(name),{headers:token?{'Authorization':'Bearer '+token}:{}});
  if(!r.ok){showToast('下载失败（HTTP '+r.status+'）','error');return}
  const blob=await r.blob();
  const url=URL.createObjectURL(blob);
  const a=document.createElement('a');a.href=url;a.download=name;document.body.appendChild(a);a.click();
  setTimeout(()=>{URL.revokeObjectURL(url);a.remove()},1000);
  showToast('已开始下载：'+name,'success');
 }catch(e){showToast('下载失败：'+e.message,'error')}
}
function restoreDbBackupFile(name){
 showModal('从备份还原',
  `将使用服务器备份 <b>${esc(name)}</b> <b>覆盖</b>当前两张流量表：<br>清空现有数据后导入备份内容，此操作不可撤销。确定继续？`,
  '确定还原', async()=>{
   try{
    showToast('正在还原…');
    const r=await api('/star/dbRestoreBackup',{method:'POST',body:JSON.stringify({name})});
    if(r.code===0){showToast('还原完成','success');loadDbInfo();loadDbBackups();}
    else showToast('还原失败：'+(r.message||''),'error');
   }catch(e){showToast('还原失败：'+e.message,'error')}
  });
}
function deleteDbBackupFile(name){
 showModal('删除备份',`确定删除服务器上的备份文件 <b>${esc(name)}</b>？删除后无法恢复。`,'确定删除', async()=>{
  try{
   const r=await api('/star/dbDeleteBackup',{method:'POST',body:JSON.stringify({name})});
   if(r.code===0){showToast('已删除','success');loadDbBackups();}
   else showToast('删除失败：'+(r.message||''),'error');
  }catch(e){showToast('删除失败：'+e.message,'error')}
 });
}
function uploadDbRestore(input){
 const file=input.files&&input.files[0];input.value='';
 if(!file)return;
 if(!/\.zip$/i.test(file.name)){showToast('请选择备份 zip 包','error');return}
 if(file.size>64*1024*1024){showToast('文件过大（上限 64MB）','error');return}
 showModal('数据库还原',
  `将上传 <b>${esc(file.name)}</b>（${fmtBytes(file.size)}）并<b>覆盖</b>当前两张流量表：<br>清空现有数据后导入备份内容，此操作不可撤销。确定继续？`,
  '确定还原', async()=>{
   try{
    showToast('正在上传并还原…');
    const r=await fetch('/star/dbRestore',{method:'POST',headers:Object.assign({'Content-Type':'application/zip'},token?{'Authorization':'Bearer '+token}:{}),body:file});
    const j=await r.json();
    if(j.code===0){showToast('还原完成','success');loadDbInfo();}
    else showToast('还原失败：'+(j.message||''),'error');
   }catch(e){showToast('还原失败：'+e.message,'error')}
  });
}

function formatIops(v){
 v=parseInt(v)||0;
 if(v>=1000)return (v/1000).toFixed(1)+' K ops/s';
 return v+' ops/s';
}
function setConnColor(id,val){
 const el=document.getElementById(id);if(!el)return;
 el.className='stat-val '+(val>1000?'text-red':val>100?'text-yellow':'text-green');
}
function setText(id,val){
 const el=document.getElementById(id);if(el)el.textContent=val;
}
function updateGauge(id,pctVal,val,max){
 const pctEl=document.getElementById(id+'-pct');
 const valEl=document.getElementById(id+'-val');
 const maxEl=document.getElementById(id+'-max');
 const fillEl=document.getElementById(id+'-fill');
 if(pctEl)pctEl.textContent=pctVal+'%';
 if(valEl)valEl.textContent=val;
 if(maxEl)maxEl.textContent=max;
 if(fillEl){
  const r=48,c=2*Math.PI*r,offset=c-(pctVal/100)*c;
  fillEl.setAttribute('stroke-dashoffset',offset);
  fillEl.setAttribute('stroke',pctVal>80?'#ef4444':pctVal>50?'#f59e0b':'url(#gaugeGrad)');
 }
}
function statCard(lbl,val,cls='',id='',valCls=''){
 return `<div class="stat"${id?` id="${id}-card"`:''}><div class="stat-val ${valCls||cls}"${id?` id="${id}"`:''}>${esc(val)}</div><div class="stat-lbl">${lbl}</div></div>`;
}
function infoRow(lbl,val,cls='',id=''){return `<tr><td>${esc(lbl)}</td><td><span${id?` id="${id}"`:''}${cls?` class="${cls}"`:''}>${esc(val)}</span></td></tr>`}
function gcStat(lbl,val,id=''){return `<div class="gc-stat"><div class="gc-val"${id?` id="${id}"`:''}>${esc(val)}</div><div class="gc-lbl">${lbl}</div></div>`}
function pct(v,max){return max>0?Math.min(100,Math.round(v/max*100)):0}
function fmtMem(mb){return mb>=1024?(mb/1024).toFixed(1)+' GB':Math.round(mb)+' MB'}
function ringGauge(label,val,max,pctVal,id,unit='已使用'){
 const r=48,c=2*Math.PI*r,offset=c-(pctVal/100)*c;
 const cls=pctVal>80?'#ef4444':pctVal>50?'#f59e0b':'url(#gaugeGrad)';
 return `<div class="gauge-card glass-card"${id?` id="${id}-card"`:''}><div class="gauge-label">${label}</div>
 <div class="ring-wrap">
 <svg width="120" height="120" viewBox="0 0 120 120"><defs><linearGradient id="gaugeGrad" x1="0%" y1="0%" x2="100%" y2="0%"><stop offset="0%" stop-color="#5b5bff"/><stop offset="100%" stop-color="#8b5cf6"/></linearGradient></defs>
 <circle class="ring-bg" cx="60" cy="60" r="${r}"/><circle class="ring-fill" cx="60" cy="60" r="${r}" stroke="${cls}" stroke-dasharray="${c}" stroke-dashoffset="${offset}"${id?` id="${id}-fill"`:''}/></svg>
 <div class="ring-text"><span class="pct"${id?` id="${id}-pct"`:''}>${pctVal}%</span><span class="unit">${unit}</span></div></div>
 <div class="gauge-detail">${id?`<span id="${id}-val">${esc(val)}</span>`:esc(val)}${max?` / ${id?`<span id="${id}-max">${esc(max)}</span>`:esc(max)}`:''}</div></div>`;
}

/* ── 流量 / 磁盘 IO 趋势图（SVG 手绘，对齐宝塔：双曲线 + 渐变面积 + 页签） ── */
let _chartTab='net';
const _hist={up:[],down:[],rd:[],wr:[]};
const _histTs=[];
const HIST_MAX=10; // 滚动窗口：10 个采样点（30 秒），与宝塔一致

function pushHistory(d){
 _hist.up.push(d.uplinkBps||0);
 _hist.down.push(d.downlinkBps||0);
 _hist.rd.push(d.diskReadBps||0);
 _hist.wr.push(d.diskWriteBps||0);
 _histTs.push(new Date());
 for(const k in _hist){while(_hist[k].length>HIST_MAX)_hist[k].shift()}
 while(_histTs.length>HIST_MAX)_histTs.shift();
}

function switchChart(tab){
 _chartTab=tab;
 const n=$('tab-net'),dk=$('tab-disk');
 if(n)n.classList.toggle('active',tab==='net');
 if(dk)dk.classList.toggle('active',tab==='disk');
 renderChart(statusData);
}

function fmtRate(bps){
 if(bps>=1048576)return (bps/1048576).toFixed(2)+' MB/s';
 if(bps>=1024)return (bps/1024).toFixed(2)+' KB/s';
 return Math.round(bps)+' B/s';
}
function fmtBytes(n){
 if(n>=1099511627776)return (n/1099511627776).toFixed(2)+' TB';
 if(n>=1073741824)return (n/1073741824).toFixed(2)+' GB';
 if(n>=1048576)return (n/1048576).toFixed(2)+' MB';
 if(n>=1024)return (n/1024).toFixed(2)+' KB';
 return Math.round(n)+' B';
}
function fmtAxis(v){
 if(v>=1048576)return (v/1048576).toFixed(1)+'M';
 if(v>=1024)return (v/1024).toFixed(0)+'K';
 return Math.round(v)+'';
}
function chartStat(color,label,val){
 const dot=color?`<span class="dotc" style="background:${color}"></span>`:'';
 return `<div class="chart-stat"><span class="l">${dot}${esc(label)}</span><span class="v">${esc(val)}</span></div>`;
}

function renderChart(d){
 const stats=$('chart-stats'); if(!stats)return;
 const dd=d||statusData||{};
 if(_chartTab==='net'){
  stats.innerHTML=
   chartStat('#22c55e','上行',fmtRate(dd.uplinkBps||0))+
   chartStat('#f59e0b','下行',fmtRate(dd.downlinkBps||0))+
   chartStat('','总发送',fmtBytes(dd.netTxBytes||0))+
   chartStat('','总接收',fmtBytes(dd.netRxBytes||0));
 }else{
  const lat=dd.diskLatencyMs!=null?dd.diskLatencyMs.toFixed(1):'0.0';
  stats.innerHTML=
   chartStat('#f43f5e','读取',fmtRate(dd.diskReadBps||0))+
   chartStat('#14b8a6','写入',fmtRate(dd.diskWriteBps||0))+
   chartStat('','每秒读写',(dd.diskIops||0)+' 次')+
   chartStat('','IO 延迟',lat+' ms');
 }
 drawTrend();
}

function smoothPath(pts,top,bottom){
 if(pts.length<2)return '';
 const clamp=v=>Math.max(top,Math.min(bottom,v));
 let d=`M ${pts[0][0].toFixed(1)} ${pts[0][1].toFixed(1)}`;
 for(let i=0;i<pts.length-1;i++){
  const p0=pts[i-1]||pts[i],p1=pts[i],p2=pts[i+1],p3=pts[i+2]||p2;
  const c1x=p1[0]+(p2[0]-p0[0])/6, c1y=clamp(p1[1]+(p2[1]-p0[1])/6);
  const c2x=p2[0]-(p3[0]-p1[0])/6, c2y=clamp(p2[1]-(p3[1]-p1[1])/6);
  d+=` C ${c1x.toFixed(1)} ${c1y.toFixed(1)}, ${c2x.toFixed(1)} ${c2y.toFixed(1)}, ${p2[0].toFixed(1)} ${p2[1].toFixed(1)}`;
 }
 return d;
}

function drawTrend(){
 const svg=$('trend-svg'); if(!svg)return;
 const W=600,H=150,PL=52,PR=12,PT=12,PB=24;
 const iw=W-PL-PR, ih=H-PT-PB, top=PT, bottom=PT+ih;
 const series=_chartTab==='net'?[_hist.up,_hist.down]:[_hist.rd,_hist.wr];
 const colors=_chartTab==='net'?['#22c55e','#f59e0b']:['#f43f5e','#14b8a6'];
 const all=series[0].concat(series[1]);
 let maxV=1024;
 for(const v of all)if(v>maxV)maxV=v;
 const nice=Math.ceil(maxV*1.15);
 const n=_histTs.length;
 const xAt=i=>PL+iw*i/Math.max(1,n-1); // 按已有点数铺满全宽（与宝塔一致）
 const parts=[];
 for(let g=0;g<=6;g++){
  const y=top+ih-(ih*g/6);
  parts.push(`<line x1="${PL}" y1="${y.toFixed(1)}" x2="${W-PR}" y2="${y.toFixed(1)}" stroke="var(--border-default)" stroke-dasharray="3 4"/>`);
  parts.push(`<text x="${PL-6}" y="${(y+3.5).toFixed(1)}" text-anchor="end" font-size="10" fill="var(--text-tertiary)">${fmtAxis(nice*g/6)}</text>`);
 }
 if(n>=2){
  for(let s=0;s<2;s++){
   const data=series[s];
   const pts=[];
   for(let i=0;i<data.length;i++){
    pts.push([xAt(i), top+ih-(ih*Math.min(nice,data[i])/nice)]);
   }
   const line=smoothPath(pts,top,bottom);
   const lastX=pts[pts.length-1][0];
   parts.push(`<path d="${line} L ${lastX.toFixed(1)} ${bottom} L ${PL} ${bottom} Z" fill="${colors[s]}" opacity="0.13"/>`);
   parts.push(`<path d="${line}" fill="none" stroke="${colors[s]}" stroke-width="2" stroke-linecap="round"/>`);
   for(const p of pts)parts.push(`<circle cx="${p[0].toFixed(1)}" cy="${p[1].toFixed(1)}" r="2.6" fill="${colors[s]}"/>`);
  }
  const tf=t=>`${String(t.getHours()).padStart(2,'0')}:${String(t.getMinutes()).padStart(2,'0')}:${String(t.getSeconds()).padStart(2,'0')}`;
  const step=Math.max(1,Math.ceil(n/10));
  const xs=[];
  for(let i=0;i<n;i+=step)xs.push(i);
  if(xs[xs.length-1]!==n-1)xs.push(n-1);
  for(const i of xs){
   const x=xAt(i);
   const anchor=i===0?'start':(x>W-56?'end':'middle');
   parts.push(`<text x="${x.toFixed(1)}" y="${H-7}" text-anchor="${anchor}" font-size="9.5" fill="var(--text-tertiary)">${tf(_histTs[i])}</text>`);
  }
 }else{
  parts.push(`<text x="${W/2}" y="${H/2}" text-anchor="middle" font-size="11" fill="var(--text-tertiary)">正在采集数据…</text>`);
 }
 svg.innerHTML=parts.join('');
}

/* ── 子服务管理（含“看门狗”卡片：原独立页签已并入本页） ── */
let _servicesData=null;
let _watchdogData=null; // 看门狗服务列表；null=未加载/无权限/失败

async function loadServices(){
 // 非当前页签 / 浏览器后台标签不轮询（带宽治理；进入页签与恢复可见时立即刷新）
 if(document.hidden||!document.querySelector('#panel-services.active'))return;
 try{
  // 子服务与看门狗并行拉取（仅请求有权限的数据）
  const [sj,wj]=await Promise.all([
   canView('services')?api('/star/services'):Promise.resolve(null),
   canView('watchdog')?api('/api/watchdog'):Promise.resolve(null),
  ]);
  if(sj&&sj.code===0)_servicesData=sj.data;
  if(wj&&wj.code===0)_watchdogData=(wj.data&&wj.data.services)||[];
  renderServices();
 }catch(e){}
}

function renderServices(){
 let html='';
 const d=_servicesData;
 if(canView('services')&&d){
 const svcs=d.services||[];
 html+=`<div class="glass-card card"><div class="flex-between mb-16"><h2>📦 子服务管理</h2><span style="font-size:12px;color:var(--text-tertiary)">运行 ${d.running||0} / 总计 ${d.total||0}</span></div>`;
 html+=`<div class="mb-12 form-inline"><button class="btn btn-primary btn-sm" onclick="showAddService()">➕ 添加服务</button></div>`;
 if(svcs.length===0){
  html+=`<div class="empty-state"><div class="empty-icon">📦</div><p>暂无子服务</p><p style="font-size:11px;margin-top:4px">点击"添加服务"按钮创建第一个子服务</p></div>`;
 }else{
  html+=`<table class="svc-table"><thead><tr><th>名称</th><th>文件</th><th>启用</th><th>状态</th><th>进程</th><th>资源</th><th style="min-width:180px">操作</th></tr></thead><tbody>`;
  svcs.forEach((s,idx)=>{
   const run=s.Running;
   html+=`<tr><td><div class="svc-name">${esc(s.Name)}</div></td>`;
   html+=`<td><div class="svc-file">${esc(s.FileName||'')}${s.Arguments?' '+esc(s.Arguments):''}</div></td>`;
   html+=`<td><span class="badge ${s.Enable?'badge-green':'badge-gray'}">${s.Enable?'启用':'禁用'}</span></td>`;
   html+=`<td><span class="badge ${run?'badge-green':'badge-red'}">${run?'运行中':'已停止'}</span></td>`;
   html+=`<td style="font-family:var(--font-mono);font-size:12px">${run?(s.ProcessId||'')+(s.ProcessName?' '+esc(s.ProcessName):''):'-'}</td>`;
   html+=`<td style="font-size:12px;white-space:nowrap">${run?(s.CpuRate!=null?s.CpuRate.toFixed(1)+'%':'—')+(s.MemoryMB!=null?' · '+fmtMem(s.MemoryMB):''):'-'}</td>`;
   html+=`<td><div class="svc-actions">`;
   html+=`<button class="btn btn-primary btn-sm" onclick="doSvcAction('start','${escAttr(s.Name)}')" ${run?'disabled':''}>▶ 启动</button>`;
   html+=`<button class="btn btn-danger btn-sm" onclick="doSvcAction('stop','${escAttr(s.Name)}')" ${!run?'disabled':''}>⏹ 停止</button>`;
   html+=`<button class="btn btn-sm" style="background:var(--orange);color:#fff" onclick="doSvcAction('restart','${escAttr(s.Name)}')" ${!run?'disabled':''}>↻ 重启</button>`;
   html+=`<button class="btn btn-secondary btn-sm" onclick="editServiceByIdx(${idx})"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/></svg></button>`;
   html+=`<button class="btn btn-ghost btn-sm" onclick="confirmRemoveService('${escAttr(s.Name)}')" style="color:var(--red)"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><line x1="10" y1="11" x2="10" y2="17"/><line x1="14" y1="11" x2="14" y2="17"/></svg></button>`;
   html+=`</div></td></tr>`;
  });
  html+=`</tbody></table>`;
 }
 html+=`</div>`;
 }
 if(canView('watchdog')&&_watchdogData!==null){
  html+=`<div class="glass-card card" style="margin-top:16px"><h2>🐕 看门狗</h2>`;
  if(_watchdogData.length===0){
   html+=`<div class="empty-state"><div class="empty-icon">🐕</div><p>未配置看门狗服务</p><p style="font-size:11px;margin-top:4px">在配置中设置 WatchDog 字段，多个服务名用逗号分隔</p></div>`;
  }else{
   html+=`<div class="stats-grid">`;
   _watchdogData.forEach(s=>{
    const cls=s.running===true?'text-green':s.running===false?'text-red':'text-yellow';
    const txt=s.running===true?'运行中':s.running===false?'已停止':'状态未知';
    html+=`<div class="stat"><div class="stat-val ${cls}">${txt}</div><div class="stat-lbl">${esc(s.name)}</div></div>`;
   });
   html+=`</div>`;
  }
  html+=`</div>`;
 }
 if(!html)return;
 $('panel-services').innerHTML=html;
}

function editServiceByIdx(idx){
 const svc=_servicesData?.services?.[idx];
 if(!svc)return;
 showServiceForm(svc,'编辑子服务');
}

async function doSvcAction(action,name){
 const labels={start:'启动',stop:'停止',restart:'重启'};
 const endpoints={start:'startService',stop:'stopService',restart:'restartService'};
 try{
  const j=await api(`/star/${endpoints[action]}`,{method:'POST',body:JSON.stringify({serviceName:name})});
  showToast(j.message||(labels[action]+'操作完成'),j.code?'error':'success');
  setTimeout(loadServices,1000);
 }catch(e){showToast('操作失败','error')}
}

function showAddService(){showServiceForm(null,'添加子服务')}

function showServiceForm(svc,title){
 const isEdit=svc!==null;
 // 兼容 C# PascalCase 和 JS camelCase 的属性名
 const s=svc||{};
 const name=s.Name||s.name||'';
 const fileName=s.FileName||s.fileName||'';
 const args=s.Arguments||s.arguments||'';
 const cwd=s.WorkingDirectory||s.workingDirectory||'';
 const enable=s.Enable??s.enable??true;
 const mode=s.Mode||s.mode||'Standard';
 const maxMem=s.MaxMemory??s.maxMemory??0;
 const health=s.HealthCheck||s.healthCheck||'';
 const env=s.Environments||s.environments||'';
 const autoStop=s.AutoStop??s.autoStop??false;
 const reload=s.ReloadOnChange??s.reloadOnChange??!isEdit;
 const multi=s.AllowMultiple??s.allowMultiple??false;
 const html=`<div class="form-row">
  <div class="form-group"><label>服务名称 *</label><input type="text" id="sf-name" value="${esc(name)}" placeholder="全局唯一标识"></div>
  <div class="form-group"><label>文件名 *</label><input type="text" id="sf-file" value="${esc(fileName)}" placeholder="启动进程时使用的可执行文件或zip包"></div>
 </div>
 <div class="form-group"><label>参数</label><input type="text" id="sf-args" value="${esc(args)}" placeholder="启动参数"></div>
 <div class="form-group"><label>工作目录</label><input type="text" id="sf-cwd" value="${esc(cwd)}" placeholder="留空使用默认目录"></div>
 <div class="form-row">
  <div class="form-group"><label>部署模式</label><select id="sf-mode"><option value="Standard" ${mode==='Standard'?'selected':''}>Standard</option><option value="Shadow" ${mode==='Shadow'?'selected':''}>Shadow</option><option value="Hosted" ${mode==='Hosted'?'selected':''}>Hosted</option><option value="Task" ${mode==='Task'?'selected':''}>Task</option></select></div>
  <div class="form-group"><label>最大内存 (MB)</label><input type="number" id="sf-maxmem" value="${maxMem}" placeholder="0 不限制"></div>
 </div>
 <div class="form-group"><label>健康检查地址</label><input type="text" id="sf-health" value="${esc(health)}" placeholder="http://localhost:port/health"></div>
 <div class="form-group"><label>环境变量</label><input type="text" id="sf-env" value="${esc(env)}" placeholder="KEY=VALUE;KEY2=VALUE2"></div>
 <div class="form-row">
  <div class="form-group"><label class="toggle"><input type="checkbox" id="sf-enable" ${enable?'checked':''}><span class="track"><span class="thumb"></span></span></label><div style="font-size:12px;color:var(--text-tertiary)">启用</div></div>
  <div class="form-group"><label class="toggle"><input type="checkbox" id="sf-autostop" ${autoStop?'checked':''}><span class="track"><span class="thumb"></span></span></label><div style="font-size:12px;color:var(--text-tertiary)">自动停止（随宿主退出）</div></div>
 </div>
 <div class="form-row">
  <div class="form-group"><label class="toggle"><input type="checkbox" id="sf-reload" ${reload?'checked':''}><span class="track"><span class="thumb"></span></span></label><div style="font-size:12px;color:var(--text-tertiary)">文件变动自动重启</div></div>
  <div class="form-group"><label class="toggle"><input type="checkbox" id="sf-multi" ${multi?'checked':''}><span class="track"><span class="thumb"></span></span></label><div style="font-size:12px;color:var(--text-tertiary)">允许多实例</div></div>
 </div>`;

 showModal(title,html,isEdit?'保存':'添加',()=>{
  const info={
   name:$('sf-name').value.trim(),
   fileName:$('sf-file').value.trim(),
   arguments:$('sf-args').value.trim()||null,
   workingDirectory:$('sf-cwd').value.trim()||null,
   mode:$('sf-mode').value,
   maxMemory:parseInt($('sf-maxmem').value)||0,
   healthCheck:$('sf-health').value.trim()||null,
   environments:$('sf-env').value.trim()||null,
   enable:$('sf-enable').checked,
   autoStop:$('sf-autostop').checked,
   reloadOnChange:$('sf-reload').checked,
   allowMultiple:$('sf-multi').checked,
  };
  if(!info.name){showToast('服务名称不能为空','error');return}
  if(!info.fileName){showToast('文件名不能为空','error');return}
  saveService(info);
 });
}

async function saveService(info){
 try{
  const j=await api('/star/addService',{method:'POST',body:JSON.stringify(info)});
  showToast(j.message||'保存成功',j.code?'error':'success');
  if(j.code===0){loadServices();closeModal()}
 }catch(e){showToast('保存失败','error')}
}

function confirmRemoveService(name){
 showModal('确认删除',`确定要删除子服务 <strong>${esc(name)}</strong> 吗？<br><span style="color:var(--text-tertiary)">如果服务正在运行，会先停止再删除。</span>`,'删除',async()=>{
  try{
   const j=await api('/star/removeService',{method:'POST',body:JSON.stringify({serviceName:name})});
   showToast(j.message||'删除成功',j.code?'error':'success');
   if(j.code===0){loadServices();closeModal()}
  }catch(e){showToast('删除失败','error')}
 });
}

/* ── Control ── */
let _freeMemRunning=false;

async function freeMemory(){
 if(_freeMemRunning)return;_freeMemRunning=true;
 const btn=$('btn-free-mem');if(btn)btn.disabled=true;
 showToast('正在释放内存...','info');
 try{
  const j=await api('/api/freeMemory');
  if(j.code===0){
   showToast(`✅ ${j.message}`,'success');
  }else{
   showToast('❌ '+(j.message||'释放失败'),'error');
  }
  setTimeout(()=>{loadDashboard();},1000);
 }catch(e){showToast('❌ 请求失败','error')}
 finally{if(btn)btn.disabled=false;_freeMemRunning=false}
}

let _syncingTime=false;

async function syncServerTime(btn){
 if(_syncingTime)return;_syncingTime=true;if(btn)btn.disabled=true;
 showToast('正在同步系统时间...','info');
 try{
  const j=await api('/api/syncTime',{method:'POST',body:JSON.stringify({timeMs:Date.now()})});
  if(j.code===0){showToast('✅ '+(j.message||'系统时间已同步'),'success');loadDashboard()}
  else showToast('❌ '+(j.message||'同步失败（需管理员/root 权限）'),'error');
 }catch(e){showToast('❌ 同步失败','error')}
 finally{_syncingTime=false;if(btn)btn.disabled=false}
}

async function changePassword(){
 const oldPwd=$('pwd-old').value;
 const newPwd=$('pwd-new').value;
 const confirmPwd=$('pwd-confirm').value;
 const msg=$('pwd-msg');

 if(!oldPwd){msg.textContent='请输入旧密码';msg.style.color='var(--red)';return}
 if(!newPwd){msg.textContent='请输入新密码';msg.style.color='var(--red)';return}
 if(newPwd.length<4){msg.textContent='新密码至少 4 位';msg.style.color='var(--red)';return}
 if(newPwd!==confirmPwd){msg.textContent='两次输入的新密码不一致';msg.style.color='var(--red)';return}

 msg.textContent='修改中...';msg.style.color='var(--text-muted)';
 try{
  const j=await api('/api/changePassword',{
   method:'POST',
   body:JSON.stringify({oldPassword:oldPwd,newPassword:newPwd})
  });
  if(j.code===0){
   msg.textContent='✅ 密码已修改，下次登录请使用新密码';msg.style.color='var(--green)';
   $('pwd-old').value='';$('pwd-new').value='';$('pwd-confirm').value='';
   showToast('密码已修改','success');
  }else{
   msg.textContent='❌ '+(j.message||'修改失败');msg.style.color='var(--red)';
  }
 }catch(e){
  msg.textContent='❌ 网络错误';msg.style.color='var(--red)';
 }
}

(function initControl(){
 let html=`<div class="glass-card card"><h2>⚡ 服务控制</h2>`;
 html+=`<div id="ctrl-status" class="mb-16" style="font-size:14px;color:var(--text-secondary)">加载中...</div>`;
 html+=`<div style="display:flex;gap:12px;flex-wrap:wrap;margin-bottom:20px">
 <button class="btn btn-primary btn-lg" onclick="doControl('start')" id="btn-start">▶ 启动服务</button>
 <button class="btn btn-danger btn-lg" onclick="doControl('stop')" id="btn-stop">⏹ 停止服务</button>
 <button class="btn btn-lg" style="background:var(--orange);color:#fff;box-shadow:0 2px 8px rgba(249,115,22,0.3)" onclick="doControl('restart')" id="btn-restart">↻ 重启服务</button>
 <button class="btn btn-lg" style="background:var(--gradient-brand);color:#fff;box-shadow:0 2px 8px rgba(91,91,255,0.3)" onclick="freeMemory()" id="btn-free-mem">🗑 释放内存</button>
 </div>`;
 html+=`<div class="section-title">操作记录</div><div style="font-size:11px;color:var(--text-muted);margin-bottom:8px">通过本面板发起的启停操作，保存在浏览器本地存储</div><div class="cmd-history" id="cmd-history">`;
 if(cmdHistory.length===0)html+=`<div class="text-muted" style="font-size:12px">暂无操作记录</div>`;
 else cmdHistory.slice(-8).reverse().forEach(h=>{
  html+=`<div class="cmd-item"><span class="cmd-time">${esc(h.time)}</span><span class="cmd-action">${esc(h.action)}</span><span class="cmd-result">${esc(h.result)}</span></div>`;
 });
 html+=`</div></div>`;
 html+=`<div class="glass-card card" style="margin-top:16px"><h2>🛡 DHDeploy 访问控制</h2><div id="dhdeploy-panel-box"><div class="text-muted" style="font-size:12px">检测中...</div></div></div>`;
 html+=`<div class="glass-card card" style="margin-top:16px"><h2>⬆️ 自动升级 / 平台通道</h2><div id="upgrade-box"><div class="text-muted" style="font-size:12px">加载中...</div></div></div>`;
 $('panel-control').innerHTML=html;
 // 进入控制页时由 switchPanel 的 loaders 立即拉一次状态；停留期间按 3 秒轮询（仅控制页激活时请求）
 setInterval(updateControlStatus,3000);updateControlStatus();
})();

// DHDeploy Agent（Rust）访问控制：本机 8282 控制接口（仅回环）；节流探测，切换后延时刷新
// 两个独立开关：服务访问（整个监听 0.0.0.0 ⇄ 127.0.0.1，热重绑）/ 管理面板访问（off/local/always）
let _dhdeployLast=0;
function renderDHDeployBox(st){
 const box=$('dhdeploy-panel-box');if(!box)return;
 if(!st){box.innerHTML='<div class="text-muted" style="font-size:12px">检测中...</div>';return}
 if(!st.detected){
  box.innerHTML=`<div class="text-muted" style="font-size:12px">未检测到 DHDeploy Agent（Rust）：${esc(st.reason||'')}</div>`;
  return;
 }
 const svc=(st.service||'').toLowerCase();
 const svcBtns=[['remote','允许远程（默认）'],['local','仅本机']].map(m=>`<button class="btn btn-sm ${svc===m[0]?'btn-primary':'btn-ghost'}" onclick="setDHDeploy('service','${m[0]}')">${m[1]}</button>`).join(' ');
 const modes=[['local','仅本机（推荐）'],['always','允许远程'],['off','关闭面板']];
 const pnlBtns=modes.map(m=>`<button class="btn btn-sm ${st.mode===m[0]?'btn-primary':'btn-ghost'}" onclick="setDHDeploy('mode','${m[0]}')">${m[1]}</button>`).join(' ');
 box.innerHTML=`<div class="font-mono" style="font-size:12px;color:var(--text-muted);margin-bottom:12px">http://127.0.0.1:8282/panel</div>
  <div style="font-size:13px;margin-bottom:6px">服务远程访问（业务接口 8282/8283）· 当前：<b>${esc(st.serviceText||'未知')}</b></div>
  <div style="display:flex;gap:8px;flex-wrap:wrap">${svcBtns}</div>
  <div style="font-size:12px;color:var(--text-muted);margin:6px 0 14px">仅本机 = 监听只绑 127.0.0.1，外部完全无法连接（适合平台走 WS 中继的节点）；直连部署/传输将不可用。</div>
  <div style="font-size:13px;margin-bottom:6px">管理面板访问 · 当前：<b>${esc(st.modeText||'未知')}</b></div>
  <div style="display:flex;gap:8px;flex-wrap:wrap">${pnlBtns}</div>
  <div style="font-size:12px;color:var(--text-muted);margin-top:8px">面板与业务接口独立：面板默认「仅本机」（远程请求 403），避免默认账号被扫描登录。</div>`;
}
async function loadDHDeployPanelStatus(force){
 if(!$('dhdeploy-panel-box'))return;
 if(!force&&Date.now()-_dhdeployLast<10000)return;
 _dhdeployLast=Date.now();
 try{const j=await api('/api/dhdeployPanel');renderDHDeployBox(j.data||null)}catch(e){}
}
async function setDHDeploy(kind,value){
 try{
  const body={};body[kind]=value;
  const j=await api('/api/dhdeployPanel',{method:'POST',body:JSON.stringify(body)});
  if(j.code===0){showToast('✅ '+(j.message||'已切换'),'success')}else{showToast('❌ '+(j.message||'切换失败'),'error')}
  _dhdeployLast=0;renderDHDeployBox(j.data&&j.data.detected?j.data:null);
  setTimeout(()=>loadDHDeployPanelStatus(true),1300); // 监听重绑约 1 秒，延时再刷新一次
 }catch(e){showToast('❌ 切换失败','error')}
}

async function updateControlStatus(){
 if(document.hidden||!document.querySelector('#panel-control.active'))return;
 try{
  const j=await api('/api/status');if(j.code!==0)return;const d=j.data;
  const el=$('ctrl-status');if(!el)return;
  el.innerHTML=`当前状态：<span class="status-indicator ${d.running?'running':'stopped'}"><span class="status-dot ${d.running?'on':'off'}"></span>${d.running?'运行中':'已停止'}</span> &nbsp; 运行时长：${esc(d.uptime)}`;
  $('btn-start').disabled=d.running;$('btn-stop').disabled=!d.running;$('btn-restart').disabled=!d.running;
  renderUpgradeBox(d);
  loadDHDeployPanelStatus(false);
 }catch(e){}
}

// 自动升级 / 平台实时通道卡片（数据来自 /api/status 的 selfUpgrade + panelWs）
function renderUpgradeBox(d){
 const box=$('upgrade-box');if(!box||!d)return;
 const su=d.selfUpgrade||{};
 const pw=d.panelWs||{};
 const chk=su.checkedAt?('最近检查：'+esc(su.checkedAt)):'';
 const info=su.message?esc(su.message):(su.latest?('最新版本 v'+esc(su.latest)):'');
 const pwText=pw.connected?'<span style="color:var(--green)">已连接</span>':(pw.enabled?'<span style="color:var(--yellow,#eab308)">连接中/离线</span>':'未配置');
 box.innerHTML=`
  <div style="display:flex;gap:28px;flex-wrap:wrap;margin-bottom:10px">
   <div><div style="font-size:12px;color:var(--text-muted)">当前版本</div><div style="font-size:16px;font-weight:600">v${esc(su.current||'')}</div></div>
   <div><div style="font-size:12px;color:var(--text-muted)">自动升级</div><div style="font-size:16px;font-weight:600">${su.enabled?'<span style="color:var(--green)">已启用</span>':'未配置'}</div></div>
   <div><div style="font-size:12px;color:var(--text-muted)">平台实时通道</div><div style="font-size:16px;font-weight:600">${pwText}</div></div>
  </div>
  <div style="font-size:12px;color:var(--text-muted);margin-bottom:10px">${chk}${info?' · '+info:''}<br>升级源：${esc(su.url||'（未配置）')}${su.intervalMinutes?(' · 检查间隔 '+su.intervalMinutes+' 分钟'):''}</div>
  <button class="btn btn-sm btn-primary" onclick="checkUpgradeNow()">⬆️ 立即检查升级</button>
  <span style="font-size:12px;color:var(--text-muted);margin-left:8px">发现新版本将自动下载（优先增量补丁）并替换重启，无需手工操作</span>`;
}
async function checkUpgradeNow(){
 try{
  const j=await api('/api/selfUpgradeCheck',{method:'POST',body:'{}'});
  showToast(j.code===0?('✅ '+(j.message||'已开始检查')):('❌ '+(j.message||'检查失败')),j.code===0?'success':'error');
  setTimeout(updateControlStatus,1500);
 }catch(e){showToast('❌ 检查失败','error')}
}

async function doControl(action){
 const labels={stop:'停止',start:'启动',restart:'重启'};
 showModal(`${labels[action]}服务`,`确定要${labels[action]}服务吗？${action==='restart'?'重启后服务将自动恢复。':''}`,labels[action],async()=>{
  try{
   const j=await api('/api/control',{method:'POST',body:JSON.stringify({action})});
   cmdHistory.push({time:new Date().toLocaleString(),action:labels[action],result:j.message||'完成'});
   sessionStorage.setItem('agent_cmd_history',JSON.stringify(cmdHistory));
   showToast(j.message||'操作已提交',j.code?'error':'success');
   setTimeout(()=>{updateControlStatus();loadDashboard();},2000);
  }catch(e){showToast('操作失败','error')}
 });
}

/* ── Config（Agent 配置 / 星尘设置 子页签） ── */
function ensureConfigShell(){
 if($('config-sub-tabs'))return;
 $('panel-config').innerHTML=`
  <div class="chart-tabs" id="config-sub-tabs" style="margin-bottom:14px">
   <span class="chart-tab active" data-sub="agent" id="config-tab-agent" onclick="configSubSwitch('agent')">⚙ Agent 配置</span>
   <span class="chart-tab" data-sub="star" id="config-tab-star" onclick="configSubSwitch('star')">🌐 星尘设置</span>
  </div>
  <div id="config-sub-agent"></div>
  <div id="config-sub-star" style="display:none"></div>`;
}
// 配置页子页签（agent Agent 配置 / star 星尘设置；兼容旧 #starconfig 独立页书签）
function configSubFromHash(){
 const h=(location.hash||'').replace(/^#\/?/,'');
 return (h==='config.star'||h==='starconfig')?'star':'agent';
}
function configSubSwitch(sub){
 applyConfigSub(sub);
 const h=sub==='star'?'#config.star':'#config';
 if(location.hash!==h)history.replaceState(null,'',h);
}
// 应用子页签：按权限显隐标签与内容（仅有 starconfig 权限时自动落到星尘设置子页）
function applyConfigSub(sub){
 const agent=$('config-sub-agent'),star=$('config-sub-star');if(!agent||!star)return;
 const canAgent=canView('config'),canStar=canView('starconfig');
 const tabAgent=$('config-tab-agent'),tabStar=$('config-tab-star');
 if(tabAgent)tabAgent.style.display=canAgent?'':'none';
 if(tabStar)tabStar.style.display=canStar?'':'none';
 if(sub==='star'&&!canStar)sub='agent';
 if(sub==='agent'&&!canAgent)sub='star';
 agent.style.display=sub==='star'?'none':'';
 star.style.display=sub==='star'?'':'none';
 document.querySelectorAll('#config-sub-tabs .chart-tab').forEach(el=>el.classList.toggle('active',el.dataset.sub===sub));
 if(sub==='star')loadStarConfig();else loadAgentConfig();
}
async function loadConfig(){ensureConfigShell();applyConfigSub(configSubFromHash())}

async function loadAgentConfig(){
 try{
  const j=await api('/api/configMetadata');if(j.code!==0)return;const items=j.data.items||[];
  let html=`<div class="glass-card card"><h2>⚙ Agent 配置</h2><table class="config-table"><tbody>`;
  items.forEach(item=>{
   html+=`<tr><td>${esc(item.displayName)}</td><td>${renderConfigInput(item)}</td><td>${esc(item.description)}</td></tr>`;
  });
  html+=`</tbody></table><div class="mt-16"><button class="btn btn-primary" onclick="saveConfig()">💾 保存配置</button> <span style="font-size:12px;color:var(--text-muted);margin-left:12px">部分配置需重启服务后生效</span></div></div>`;

  // 修改密码卡片：内置管理员在「用户」页管理凭据（用户名/密码），此处仅为数据库用户保留本人改密
  if(!(me&&me.isAdmin)){
   html+=`<div class="glass-card card" style="margin-top:16px"><h2>🔑 修改密码</h2>`;
   html+=`<div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:12px;align-items:end">`;
   html+=`<div><label style="display:block;font-size:12px;font-weight:600;color:var(--text-secondary);margin-bottom:6px">旧密码</label><input type="password" id="pwd-old" placeholder="请输入旧密码" autocomplete="off"></div>`;
   html+=`<div><label style="display:block;font-size:12px;font-weight:600;color:var(--text-secondary);margin-bottom:6px">新密码</label><input type="password" id="pwd-new" placeholder="请输入新密码" autocomplete="new-password"></div>`;
   html+=`<div><label style="display:block;font-size:12px;font-weight:600;color:var(--text-secondary);margin-bottom:6px">确认新密码</label><input type="password" id="pwd-confirm" placeholder="再次输入新密码" autocomplete="new-password"></div>`;
   html+=`</div>`;
   html+=`<div style="margin-top:14px;display:flex;align-items:center;gap:12px">`;
   html+=`<button class="btn btn-primary" onclick="changePassword()">修改密码</button>`;
   html+=`<span id="pwd-msg" style="font-size:12px;color:var(--text-muted)"></span></div></div>`;
  }

  // 程序升级卡片
  html+=`<div class="glass-card card" style="margin-top:16px"><h2>🚀 程序升级</h2>`;
  html+=`<p style="font-size:12px;color:var(--text-muted);margin-bottom:10px">选择新版本程序文件（Linux：pek-ragent / Windows：pek-ragent.exe），上传后服务端自动校验（影子自检）、替换程序文件并重启服务，无需手动改名或停服。</p>`;
  html+=`<div style="display:flex;gap:12px;align-items:center;flex-wrap:wrap">`;
  html+=`<input type="file" id="upgrade-file" style="font-size:12px">`;
  html+=`<button class="btn btn-primary" onclick="doUpgrade()">⬆ 上传并升级</button>`;
  html+=`<span id="upgrade-msg" style="font-size:12px;color:var(--text-muted)"></span></div></div>`;

  $('config-sub-agent').innerHTML=html;
 }catch(e){$('config-sub-agent').innerHTML=`<div class="glass-card card"><h2>⚙ Agent 配置</h2><p class="text-muted">加载失败</p></div>`}
}

function renderConfigInput(item){
 const v=item.value,n=item.name;
 if(item.type==='Boolean')
  return `<label class="toggle"><input type="checkbox" id="cfg-${n}" ${v?'checked':''} onchange="markDirty('${n}')"><span class="track"><span class="thumb"></span></span></label>`;
 if(item.type==='Int32')
  return `<input type="number" id="cfg-${n}" value="${v??0}" onchange="markDirty('${n}')">`;
 if(item.type==='Password')
  return `<input type="password" id="cfg-${n}" value="${esc(v??'')}" autocomplete="off" onchange="markDirty('${n}')">`;
 return `<input type="text" id="cfg-${n}" value="${esc(v??'')}" onchange="markDirty('${n}')">`;
}
let dirtyFields=new Set();
function markDirty(name){dirtyFields.add(name)}
async function saveConfig(){
 if(dirtyFields.size===0){showToast('没有需要保存的修改','info');return}
 const updates={};
 dirtyFields.forEach(n=>{
  const el=$('cfg-'+n);if(!el)return;let v;
  if(el.type==='checkbox')v=el.checked;
  else if(el.type==='number')v=parseInt(el.value)||0;
  else v=el.value;updates[n]=v;
 });
 try{
  const j=await api('/api/updateConfig',{method:'POST',body:JSON.stringify(updates)});
  if(j.code===0){dirtyFields.clear();showToast(j.message,'success');loadAgentConfig()}
  else showToast(j.message,'error');
 }catch(e){showToast('保存失败','error')}
}

/* ── 程序升级 ── */
async function doUpgrade(){
 const input=$('upgrade-file');
 const f=input&&input.files?input.files[0]:null;
 if(!f){showToast('请先选择升级文件','error');return}
 const mb=(f.size/1048576).toFixed(2);
 showModal('程序升级',`确定上传「${esc(f.name)}」（${mb} MB）并升级？<br><span style="font-size:12px;color:var(--text-muted)">服务端会先执行影子自检，通过后自动替换程序文件并重启服务；失败时保持当前程序不变。</span>`,'确认升级',async()=>{
  const msg=$('upgrade-msg');
  try{
   if(msg)msg.textContent='上传中…';
   const r=await fetch('/api/upgrade',{method:'POST',headers:{'Authorization':'Bearer '+token,'Content-Type':'application/octet-stream'},body:f});
   const j=await r.json();
   if(msg)msg.textContent=j.message||'';
   showToast(j.message||(j.code===0?'升级完成':'升级失败'),j.code===0?'success':'error');
   if(j.code===0){
    if(msg)msg.textContent='升级完成，服务重启中…';
    setTimeout(()=>location.reload(),8000);
   }
  }catch(e){
   if(msg)msg.textContent='上传失败：'+(e.message||e);
   showToast('上传失败（若服务正在重启，属正常现象）','error');
  }
 });
}

/* ── 星尘设置 ── */
let _starDirtyFields=new Set();

async function loadStarConfig(){
 try{
  const j=await api('/star/getStarConfig');
  if(j.code!==0){$('config-sub-star').innerHTML=`<div class="glass-card card"><h2>🌐 星尘设置</h2><p class="text-muted">加载失败</p></div>`;return}
  const groups=j.data.groups||[];
  let html=`<div class="glass-card card"><h2>🌐 星尘设置</h2>`;
  groups.forEach(g=>{
   html+=`<div class="section-title" style="margin-top:${groups.indexOf(g)>0?'20px':'0'}">${esc(g.group)}</div>`;
   html+=`<table class="config-table"><tbody>`;
   g.items.forEach(item=>{
    html+=`<tr><td>${esc(item.displayName)}</td><td>${renderStarConfigInput(item)}</td><td>${esc(item.description)}</td></tr>`;
   });
   html+=`</tbody></table>`;
  });
  html+=`<div class="mt-16"><button class="btn btn-primary" onclick="saveStarConfig()">💾 保存星尘配置</button> <span style="font-size:12px;color:var(--text-muted);margin-left:12px">修改 StarServer 地址后需重启 StarAgent 生效</span></div></div>`;
  $('config-sub-star').innerHTML=html;
 }catch(e){$('config-sub-star').innerHTML=`<div class="glass-card card"><h2>🌐 星尘设置</h2><p class="text-muted">加载失败</p></div>`}
}

function renderStarConfigInput(item){
 const v=item.value,n=item.name;
 if(item.type==='Boolean')
  return `<label class="toggle"><input type="checkbox" id="scfg-${n}" ${v?'checked':''} onchange="markStarDirty('${n}')"><span class="track"><span class="thumb"></span></span></label>`;
 if(item.type==='Int32')
  return `<input type="number" id="scfg-${n}" value="${v??0}" onchange="markStarDirty('${n}')">`;
 if(item.type==='Password')
  return `<input type="password" id="scfg-${n}" placeholder="输入新值以修改" onchange="markStarDirty('${n}')">`;
 return `<input type="text" id="scfg-${n}" value="${esc(v??'')}" onchange="markStarDirty('${n}')">`;
}
function markStarDirty(name){_starDirtyFields.add(name)}
async function saveStarConfig(){
 if(_starDirtyFields.size===0){showToast('没有需要保存的修改','info');return}
 const updates={};
 _starDirtyFields.forEach(n=>{
  const el=$('scfg-'+n);if(!el)return;let v;
  if(el.type==='checkbox')v=el.checked;
  else if(el.type==='number')v=parseInt(el.value)||0;
  else if(el.type==='password'){
   if(el.value)v=el.value;else return;
  }
  else v=el.value;updates[n]=v;
 });
 if(Object.keys(updates).length===0){showToast('没有需要保存的修改','info');return}
 try{
  const j=await api('/star/updateStarConfig',{method:'POST',body:JSON.stringify(updates)});
  if(j.code===0){_starDirtyFields.clear();showToast(j.message,'success');loadStarConfig()}
  else showToast(j.message,'error');
 }catch(e){showToast('保存失败','error')}
}

/* ── 日志（系统日志 / 操作日志 子页签） ── */
function ensureLogsShell(){
 if($('logs-sub-tabs'))return;
 $('panel-logs').innerHTML=`
  <div class="chart-tabs" id="logs-sub-tabs" style="margin-bottom:14px">
   <span class="chart-tab active" data-sub="sys" id="logs-tab-sys" onclick="logsSubSwitch('sys')">📋 系统日志</span>
   <span class="chart-tab" data-sub="audit" id="logs-tab-audit" onclick="logsSubSwitch('audit')">📜 操作日志</span>
  </div>
  <div id="log-sub-sys"></div>
  <div id="log-sub-audit" style="display:none"></div>`;
}
// 日志页子页签（sys 系统日志 / audit 操作日志；兼容旧 #audit 独立页书签）
function logsSubFromHash(){
 const h=(location.hash||'').replace(/^#\/?/,'');
 return (h==='logs.audit'||h==='audit')?'audit':'sys';
}
function logsSubSwitch(sub){
 applyLogsSub(sub);
 const h=sub==='audit'?'#logs.audit':'#logs';
 if(location.hash!==h)history.replaceState(null,'',h);
}
// 应用子页签：按权限显隐标签与内容（仅有 audit 权限时自动落到操作日志子页）
function applyLogsSub(sub){
 const sys=$('log-sub-sys'),audit=$('log-sub-audit');if(!sys||!audit)return;
 const canSys=canView('logs'),canAudit=canView('audit');
 const tabSys=$('logs-tab-sys'),tabAudit=$('logs-tab-audit');
 if(tabSys)tabSys.style.display=canSys?'':'none';
 if(tabAudit)tabAudit.style.display=canAudit?'':'none';
 if(sub==='audit'&&!canAudit)sub='sys';
 if(sub==='sys'&&!canSys)sub='audit';
 sys.style.display=sub==='audit'?'none':'';
 audit.style.display=sub==='audit'?'':'none';
 document.querySelectorAll('#logs-sub-tabs .chart-tab').forEach(el=>el.classList.toggle('active',el.dataset.sub===sub));
 if(sub==='audit')loadAudit();else loadLogsSys();
}
async function loadLogs(){ensureLogsShell();applyLogsSub(logsSubFromHash())}
async function loadLogsSys(){await loadLogFiles();renderLogPanel();if(currentLogFile||logFiles.length>0)await fetchLogContent();else if($('log-content'))$('log-content').innerHTML=`<div class="log-empty">📂 未找到日志文件<br><span style="font-size:11px;color:var(--text-muted)">请确保 Log 目录中存在 .log 文件</span></div>`}
async function loadLogFiles(){
 try{const j=await api('/api/logFiles');if(j.code===0)logFiles=j.data.files||[]}catch(e){logFiles=[]}
 if(!currentLogFile&&logFiles.length>0)currentLogFile=logFiles[0].name;
}
function renderLogPanel(){
 let html=`<div class="glass-card card"><h2>📋 系统日志</h2>`;
 html+=`<div class="log-layout"><div class="log-files" id="log-file-list">`;
 if(logFiles.length===0)html+=`<div class="text-muted" style="font-size:11px;padding:8px">无日志文件</div>`;
 else logFiles.forEach(f=>{
  const active=currentLogFile===f.name;
  html+=`<div class="log-file-item${active?' active':''}" onclick="selectLogFile('${escAttr(f.name)}')"><div class="fn">${esc(f.name)}</div><div class="fm">${esc(f.sizeDisplay)} · ${esc(f.lastModified)}</div></div>`;
 });
 html+=`</div><div><div class="log-toolbar">`;
 html+=`<span style="flex:1"></span><button class="btn btn-ghost btn-sm" onclick="refreshLogs()">🔄 刷新</button></div>`;
 html+=`<div class="log-content" id="log-content"><div class="log-empty">加载中...</div></div></div></div></div>`;
 $('log-sub-sys').innerHTML=html;
}
async function selectLogFile(name){currentLogFile=name;renderLogPanel();await fetchLogContent()}
async function refreshLogs(){await loadLogFiles();renderLogPanel();await fetchLogContent()}
async function fetchLogContent(){
 const el=$('log-content');if(!el)return;el.innerHTML=`<div class="log-empty">加载中...</div>`;
 try{
  let url=`/api/logs?count=500`;
  if(currentLogFile)url+=`&file=${encodeURIComponent(currentLogFile)}`;
  const j=await api(url);
  if(j.code!==0){el.innerHTML=`<div class="log-empty">❌ ${esc(j.message||'加载失败')}</div>`;return}
  const lines=j.data.lines||[];
  if(lines.length===0){el.innerHTML=`<div class="log-empty">📄 无日志内容</div>`;return}
  el.innerHTML=lines.map(l=>{
   let cls='';
   if(l.includes('ERROR')||l.includes('FATAL'))cls='error';
   else if(l.includes('WARN'))cls='warn';
   else if(l.includes('DEBUG'))cls='debug';
   return `<div class="log-line ${cls}">${esc(l)}</div>`;
  }).join('');el.scrollTop=el.scrollHeight;
 }catch(e){el.innerHTML=`<div class="log-empty">❌ 加载失败</div>`}
}

/* ── 文件管理 ── */
let fmData=null,fmPath='',fmSelected=new Set();
let fmDirSizes=new Map(); // 目录大小（手动计算缓存：完整路径 → 字节数）

function fmJoin(name){
 const p=(fmData&&fmData.path)?fmData.path:'';
 if(!p)return /^[A-Za-z]:$/.test(name)?name+'\\':'/'+name;
 if(p.endsWith('/')||p.endsWith('\\'))return p+name;
 const sep=(p.includes('\\')&&!p.includes('/'))?'\\':'/';
 return p+sep+name;
}
function fmParentText(){
 const p=(fmData&&fmData.path)?fmData.path:'';
 if(!p)return '';
 return fmData.parent||'';
}
function fmReady(){if(fmData&&fmData.path)return true;showToast('请先进入具体目录','error');return false}
function fmNavigate(path){fmPath=path;loadFileman()}

async function loadFileman(){
 try{
  const j=await api('/star/fileList?path='+encodeURIComponent(fmPath||''));
  if(j.code!==0){showToast(j.message||'加载失败','error');return}
  fmData=j.data;fmSelected=new Set();
  renderFileman();
 }catch(e){}
}

function renderFileman(){
 const d=fmData||{items:[]};
 const isRoot=!d.path;
 let rows='';
 if(!(d.items||[]).length){rows=`<tr><td colspan="6" class="fm-empty">${isRoot?'':'此目录为空'}</td></tr>`}
 else{(d.items||[]).forEach(it=>{
  const name=it.name;
  const checked=fmSelected.has(name)?'checked':'';
  const icon=it.isDir?(it.isLink?'🔗':'📁'):'📄';
  rows+=`<tr>
   <td style="width:34px"><input type="checkbox" ${checked} onchange="fmToggle('${escAttr(name)}',this.checked)"></td>
   <td><a class="fm-name" onclick="fmOpen('${escAttr(name)}',${it.isDir?1:0})" title="${escAttr(name)}">${icon} ${esc(name)}</a></td>
   <td style="width:92px" class="text-secondary">${it.isDir?fmSizeCell(name):fmtBytes(it.size)}</td>
   <td style="width:152px" class="text-muted">${esc(it.mtime||'')}</td>
   <td style="width:72px" class="text-muted">${esc(it.perm||'—')}</td>
   <td class="fm-row-actions" style="width:118px">${isRoot?'':`
    ${it.isDir?'':`<button class="fm-icon-btn" title="下载" onclick="fmDownload(fmJoin('${escAttr(name)}'))">⬇</button>`}
    <button class="fm-icon-btn" title="重命名" onclick="fmRename('${escAttr(name)}')">✏️</button>
    <button class="fm-icon-btn" title="删除" onclick="fmDelete(['${escAttr(name)}'])">🗑️</button>`}
   </td>
  </tr>`;
 });}
 const sel=fmSelected.size;
 const canUp=!!(fmParentText()||d.path);
 let html=`<div class="glass-card card">
  <div class="flex-between mb-16" style="flex-wrap:wrap;gap:8px">
   <h2>🗂 文件管理</h2>
   <div style="display:flex;gap:8px;flex-wrap:wrap">
    <button class="btn btn-secondary btn-sm" onclick="$('fm-upload-input').click()">⬆ 上传</button>
    <input type="file" id="fm-upload-input" multiple style="display:none" onchange="fmUpload(this)">
    <button class="btn btn-secondary btn-sm" onclick="fmMkdir()">📁 新建文件夹</button>
    <button class="btn btn-secondary btn-sm" onclick="fmNewFile()">📄 新建文件</button>
    <button class="btn btn-secondary btn-sm" onclick="fmSearchPrompt()">🔍 搜索</button>
    <button class="btn btn-secondary btn-sm" onclick="loadFileman()">⟳ 刷新</button>
   </div>
  </div>
  <div class="fm-toolbar">
   <button class="btn btn-ghost btn-sm" onclick="fmNavigate('')">🖥 根目录</button>
   <button class="btn btn-ghost btn-sm" ${canUp?'':'disabled'} onclick="fmNavigate(fmParentText())">⬆ 上一级</button>
   <span class="fm-path" title="${escAttr(d.path||'计算机')}">${esc(d.path||'计算机')}</span>
   <span style="flex:1"></span>
   <button class="btn btn-secondary btn-sm" ${sel?'':'disabled'} onclick="fmCompress()">🗜 压缩</button>
   <button class="btn btn-secondary btn-sm" ${sel===1?'':'disabled'} onclick="fmExtract()">📤 解压</button>
   <button class="btn btn-secondary btn-sm" ${sel?'':'disabled'} onclick="fmTransfer(false)">📋 复制</button>
   <button class="btn btn-secondary btn-sm" ${sel?'':'disabled'} onclick="fmTransfer(true)">✂ 移动</button>
   <button class="btn btn-secondary btn-sm" ${sel===1?'':'disabled'} onclick="fmChmod()">🔒 权限</button>
   <button class="btn btn-danger btn-sm" ${sel?'':'disabled'} onclick="fmDelete([...fmSelected])">🗑 删除</button>
  </div>
  <div style="overflow-x:auto"><table class="svc-table">
   <thead><tr><th style="width:34px"></th><th>名称</th><th>大小</th><th>修改时间</th><th>权限</th><th style="text-align:right">操作</th></tr></thead>
   <tbody>${rows}</tbody>
  </table></div>
  <div class="text-muted" style="margin-top:10px;font-size:12px">共 ${(d.items||[]).length} 项${sel?`，选中 ${sel} 项`:''} · 单击文件夹进入、单击文件在线编辑 · 文件夹大小点“计算”按需统计 · 上传/编辑上限 64MB/2MB</div>
 </div>`;
 $('panel-fileman').innerHTML=html;
}

function fmToggle(name,checked){checked?fmSelected.add(name):fmSelected.delete(name);renderFileman()}
function fmOpen(name,isDir){
 if(isDir){fmNavigate(fmJoin(name));return}
 fmEdit(fmJoin(name));
}

// 目录大小单元格：未计算显示“计算”，已计算显示大小（点击可重新计算）
function fmSizeCell(name){
 const full=fmJoin(name);
 if(fmDirSizes.has(full))
  return `<a class="fm-name" style="cursor:pointer" title="点击重新计算" onclick="fmDirSize('${escAttr(name)}',this)">${fmtBytes(fmDirSizes.get(full))}</a>`;
 return `<a class="fm-name" style="cursor:pointer" title="计算文件夹大小（递归统计，可手动触发）" onclick="fmDirSize('${escAttr(name)}',this)">计算</a>`;
}
async function fmDirSize(name,el){
 const full=fmJoin(name);
 if(el)el.textContent='计算中…';
 try{
  const j=await api('/star/fileSize',{method:'POST',body:JSON.stringify({path:full})});
  if(j.code!==0){showToast(j.message||'计算失败','error');if(el)el.textContent='计算';return}
  fmDirSizes.set(full,j.data.bytes);
  if(el){
   el.textContent=fmtBytes(j.data.bytes);
   el.title=`${j.data.files} 个文件 · ${j.data.dirs} 个子目录${j.data.partial?' · 已截断（目录过大）':''}（点击重新计算）`;
  }
  showToast(`文件夹大小：${fmtBytes(j.data.bytes)}（${j.data.files} 文件 / ${j.data.dirs} 目录）`,j.data.partial?'info':'success');
 }catch(e){if(el)el.textContent='计算'}
}

function fmPrompt(title,labelHtml,value,okText,cb){
 showModal(title,`${labelHtml}<input type="text" id="fm-prompt-input" value="${escAttr(value||'')}" style="width:100%;margin-top:10px">`,okText,()=>{
  const el=$('fm-prompt-input');
  cb(el?el.value:'');
 });
 setTimeout(()=>{const el=$('fm-prompt-input');if(el){el.focus();el.select()}},50);
}

function fmUpload(input){
 const files=[...input.files];input.value='';
 if(!files.length)return;
 if(!fmReady())return;
 const dir=fmData.path;let i=0,ok=0,fail=0;
 const next=()=>{
  if(i>=files.length){
   showToast(`上传完成：成功 ${ok} 个${fail?`，失败 ${fail} 个`:''}`,fail?'error':'success');
   loadFileman();return;
  }
  const f=files[i++];
  showToast(`正在上传 ${i}/${files.length}：${f.name}`,'info');
  const xhr=new XMLHttpRequest();
  xhr.open('POST','/star/fileUpload?path='+encodeURIComponent(dir)+'&name='+encodeURIComponent(f.name));
  if(token)xhr.setRequestHeader('Authorization','Bearer '+token);
  xhr.upload.onprogress=e=>{if(e.lengthComputable)showToast(`正在上传 ${i}/${files.length}：${f.name} ${Math.round(e.loaded/e.total*100)}%`,'info')};
  xhr.onload=()=>{
   let msg='';
   try{const j=JSON.parse(xhr.responseText);if(j.code===0)ok++;else{fail++;msg=j.message||''}}catch(e){fail++}
   if(msg)console.warn('上传失败 '+f.name+'：'+msg);
   next();
  };
  xhr.onerror=()=>{fail++;next()};
  xhr.send(f);
 };
 next();
}

async function fmDownload(path){
 try{
  const r=await fetch('/star/fileDownload?path='+encodeURIComponent(path),{headers:token?{'Authorization':'Bearer '+token}:{}});
  const ct=r.headers.get('content-type')||'';
  if(!r.ok||ct.includes('application/json')){
   let msg='下载失败';
   try{const j=await r.json();msg=j.message||msg}catch(e){}
   showToast(msg,'error');return;
  }
  const blob=await r.blob();
  const name=path.split(/[\\/]/).pop()||'download';
  const a=document.createElement('a');a.href=URL.createObjectURL(blob);a.download=name;
  document.body.appendChild(a);a.click();
  setTimeout(()=>{URL.revokeObjectURL(a.href);a.remove()},1000);
 }catch(e){showToast('下载失败','error')}
}

function fmDelete(names){
 if(!(names&&names.length))return;
 showModal('删除确认',`确定删除选中的 <b>${names.length}</b> 项？<br><span class="text-muted">文件夹将连同内容递归删除，此操作不可恢复</span>`,'删除',async()=>{
  const paths=names.map(n=>fmJoin(n));
  const j=await api('/star/fileDelete',{method:'POST',body:JSON.stringify({paths})});
  if(j.code!==0){showToast(j.message||'删除失败','error');return}
  showToast(`已删除 ${j.data.deleted} 项（释放 ${fmtBytes(j.data.freedBytes)}）`,'success');
  loadFileman();
 });
}

function fmRename(name){
 fmPrompt('重命名','新名称',name,'重命名',async v=>{
  v=v.trim();if(!v||v===name)return;
  const j=await api('/star/fileRename',{method:'POST',body:JSON.stringify({path:fmJoin(name),newName:v})});
  if(j.code!==0){showToast(j.message||'重命名失败','error');return}
  showToast('已重命名','success');loadFileman();
 });
}

function fmMkdir(){
 if(!fmReady())return;
 fmPrompt('新建文件夹','文件夹名称','','创建',async v=>{
  v=v.trim();if(!v)return;
  const j=await api('/star/fileMkdir',{method:'POST',body:JSON.stringify({path:fmData.path,name:v})});
  if(j.code!==0){showToast(j.message||'创建失败','error');return}
  showToast('已创建','success');loadFileman();
 });
}

function fmNewFile(){
 if(!fmReady())return;
 fmPrompt('新建文件','文件名称（含扩展名，如 config.json）','','创建',async v=>{
  v=v.trim();if(!v)return;
  const j=await api('/star/fileNewFile',{method:'POST',body:JSON.stringify({path:fmData.path,name:v})});
  if(j.code!==0){showToast(j.message||'创建失败','error');return}
  showToast('已创建','success');loadFileman();
 });
}

function fmCompress(){
 const names=[...fmSelected];if(!names.length||!fmReady())return;
 const def=((names.length===1?names[0]:'archive').replace(/[\\/]+$/,''))+'.zip';
 fmPrompt('压缩为 ZIP',`将选中的 <b>${names.length}</b> 项压缩到当前目录：`,def,'压缩',async v=>{
  v=v.trim();if(!v)return;
  const paths=names.map(n=>fmJoin(n));
  const j=await api('/star/fileCompress',{method:'POST',body:JSON.stringify({paths,target:fmData.path,name:v})});
  if(j.code!==0){showToast(j.message||'压缩失败','error');return}
  showToast(`压缩完成（${j.data.fileCount} 个文件，${fmtBytes(j.data.size)}）`,'success');loadFileman();
 });
}

function fmExtract(){
 const names=[...fmSelected];if(names.length!==1||!fmReady())return;
 const name=names[0];
 showModal('解压 ZIP',`将 <b>${esc(name)}</b> 解压到所在目录（保留内部目录结构）？`,'解压',async()=>{
  const j=await api('/star/fileExtract',{method:'POST',body:JSON.stringify({path:fmJoin(name)})});
  if(j.code!==0){showToast(j.message||'解压失败','error');return}
  showToast(`解压完成：${j.data.fileCount} 个文件`,'success');loadFileman();
 });
}

function fmTransfer(isMove){
 const names=[...fmSelected];if(!names.length||!fmReady())return;
 fmPrompt(isMove?'移动到…':'复制到…','目标目录（绝对路径）：',fmData.path,isMove?'移动':'复制',async v=>{
  v=v.trim();if(!v)return;
  const paths=names.map(n=>fmJoin(n));
  const j=await api(isMove?'/star/fileMove':'/star/fileCopy',{method:'POST',body:JSON.stringify({paths,target:v})});
  if(j.code!==0){showToast(j.message||'操作失败','error');return}
  showToast(isMove?'已移动':'已复制','success');loadFileman();
 });
}

function fmChmod(){
 const names=[...fmSelected];if(names.length!==1)return;
 fmPrompt('修改权限','八进制权限（如 755、644；仅 Linux/macOS）：','755','应用',async v=>{
  v=v.trim();if(!v)return;
  const j=await api('/star/fileChmod',{method:'POST',body:JSON.stringify({path:fmJoin(names[0]),mode:v})});
  if(j.code!==0){showToast(j.message||'修改失败','error');return}
  showToast('权限已修改','success');loadFileman();
 });
}

function fmSearchPrompt(){
 if(!fmReady())return;
 fmPrompt('搜索文件','名称关键词（不区分大小写，递归当前目录）：','','搜索',async v=>{
  v=v.trim();if(!v)return;
  const j=await api('/star/fileSearch?path='+encodeURIComponent(fmData.path)+'&q='+encodeURIComponent(v));
  if(j.code!==0){showToast(j.message||'搜索失败','error');return}
  const items=j.data.items||[];
  let body;
  if(!items.length){body='<div class="text-muted">无匹配结果</div>'}
  else{
   body=`<div class="fm-search-list">`+items.map(it=>
    `<div class="fm-search-item"><span>${it.isDir?(it.isLink?'🔗':'📁'):'📄'}</span><span class="p" title="${escAttr(it.path)}">${esc(it.path)}</span>${it.isDir?`<button class="fm-icon-btn" title="打开目录" onclick="fmReveal('${escAttr(it.path)}')">打开</button>`:''}</div>`).join('')+`</div>`;
   if(j.data.truncated)body+=`<div class="text-muted" style="font-size:11px;margin-top:6px">结果已截断（最多 200 条结果 / 扫描 20000 项）</div>`;
  }
  showModal(`搜索结果（${items.length}）`,body,'关闭',null);
 });
}

function fmReveal(path){closeModal();fmNavigate(path)}

async function fmEdit(path){
 try{
  const j=await api('/star/fileRead?path='+encodeURIComponent(path));
  if(j.code!==0){showToast(j.message||'读取失败','error');return}
  const shortName=path.split(/[\\/]/).pop()||path;
  showModal('编辑：'+shortName,
   `<div class="text-muted" style="margin-bottom:8px">${esc(j.data.path)} · ${fmtBytes(j.data.size)}</div><textarea id="fm-editor" class="fm-editor" spellcheck="false"></textarea>`,
   '保存',async()=>{
    const content=$('fm-editor')?$('fm-editor').value:'';
    const r=await api('/star/fileWrite',{method:'POST',body:JSON.stringify({path,content})});
    if(r.code!==0){showToast(r.message||'保存失败','error');return}
    showToast('已保存','success');loadFileman();
   });
  const el=$('fm-editor');if(el)el.value=j.data.content;
 }catch(e){}
}

/* ── 日志清理：已迁移为插件（🧩 插件 → 日志清理；后端接口 logCleanScan/logCleanRun/logCleanConfig 保留在核心） ── */
let pluginData=null,pluginStore=null,pluginSvc={};
let pluginSearch=''; // 插件搜索关键词

async function loadPlugins(){
 try{
  const j=await api('/star/pluginList');
  if(j.code!==0){showToast(j.message||'加载失败','error');return}
  pluginData=j.data;pluginSvc={};
  const apps=(pluginData.plugins||[]).map(p=>p.app).filter(a=>a);
  if(apps.length&&canView('services')){
   try{const s=await api('/star/services');if(s.code===0)(s.data.services||[]).forEach(x=>{if(x.name)pluginSvc[x.name]=x});}catch(e){}
  }
  try{const st=await api('/star/pluginStore');pluginStore=(st.code===0)?st.data:null;}catch(e){pluginStore=null;}
  renderPlugins();
 }catch(e){}
}

// 打开插件：以悬浮弹窗（iframe）呈现，不替换面板内容；关闭后仍停留在插件列表
function pluginOpen(id){
 const d=pluginData||{plugins:[]};
 const p=(d.plugins||[]).find(x=>x.id===id);
 if(!p)return;
 const entry=String(p.entry||'index.html').split('/').map(encodeURIComponent).join('/');
 const src='/plugins/'+encodeURIComponent(p.id)+'/'+entry;
 const svc=p.app?pluginSvc[p.app]:null;
 $('plugin-dialog-title').innerHTML=`${esc(p.icon||'🧩')} ${esc(p.name)}${p.version?` <span class="text-muted" style="font-size:12px;font-weight:400">v${esc(p.version)}</span>`:''}`;
 $('plugin-dialog-sub').textContent=p.app?`关联子服务：${p.app} ${svc?(svc.running?'· 🟢 运行中':'· 🔴 已停止'):''}`:'';
 $('plugin-dialog-body').innerHTML=`<iframe class="plugin-frame" src="${escAttr(src)}" title="${escAttr(p.name)}"></iframe>`;
 $('plugin-overlay').classList.remove('hidden');
}
function pluginClose(){
 $('plugin-overlay').classList.add('hidden');
 $('plugin-dialog-body').innerHTML='';
}

function renderPlugins(){
 const d=pluginData||{plugins:[],invalid:[],dir:''};
 $('panel-plugins').innerHTML=pluginShellHtml(d);
}

// 插件页外壳（标题/搜索/上传按钮；表格由 pluginTableHtml 单独渲染，搜索时只重建表格、不重建输入框）
function pluginShellHtml(d){
 return `<div class="glass-card card">
  <div class="flex-between mb-16" style="flex-wrap:wrap;gap:8px">
   <h2>🧩 插件</h2>
   <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
    <input type="text" id="plugin-search" value="${escAttr(pluginSearch)}" oninput="onPluginSearch()" placeholder="🔍 搜索插件（名称/ID/说明/开发商）" style="width:260px">
    <label class="btn btn-secondary btn-sm" style="cursor:pointer;display:inline-block;margin:0">📦 选择 zip<input type="file" id="plugin-file" accept=".zip" style="display:none" onchange="pluginFilePicked(this)"></label>
    <span id="plugin-file-name" class="text-muted" style="font-size:12px;max-width:150px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap"></span>
    <button class="btn btn-primary btn-sm" onclick="pluginInstall()">⬆ 上传安装</button>
   </div>
  </div>
  <div id="plugin-table-wrap">${pluginTableHtml()}</div>
  <div class="text-muted" style="margin-top:10px;font-size:12px">插件目录：<code>${esc(d.dir||'')}</code>；压缩包内需含 <code>plugin.json</code>（可置于包根或唯一子目录）；插件页面以同源 iframe 运行，可复用当前登录态调用面板接口（受当前用户菜单权限约束并记入操作日志）。在线安装仅走 HTTPS 并强制 SHA-256 校验（配置公钥后另行强制 Ed25519 验签），校验失败一律拒绝，插件包不会执行任何安装脚本。</div>
 </div>`;
}
function onPluginSearch(){ pluginSearch=$('plugin-search').value; $('plugin-table-wrap').innerHTML=pluginTableHtml(); }

// 插件表（合并展示：在线插件源条目 + 本地安装（不在源中）的插件 —— 每个插件仅一行；按搜索词过滤）
function pluginTableHtml(){
 const d=pluginData||{plugins:[],invalid:[],dir:''};
 const q=pluginSearch.trim().toLowerCase();
 const hit=(...fields)=>!q||fields.some(f=>String(f||'').toLowerCase().includes(q));
 const storeReady=!!(pluginStore&&pluginStore.configured&&!pluginStore.error);
 const entries=(storeReady?(pluginStore.plugins||[]):[]).filter(e=>hit(e.id,e.name,e.description,e.author));
 const storeIds=new Set(entries.map(e=>e.id));
 let rows='';
 entries.forEach(e=>{
  const svc=e.app?pluginSvc[e.app]:null;
  const appTxt=e.app?`<span class="text-muted" style="font-size:11px"> · 关联 ${esc(e.app)}${svc?(svc.running?' 🟢':' 🔴'):''}</span>`:'';
  const svcBtns=(e.app&&canView('services'))?`<button class="btn btn-secondary btn-sm" onclick="doSvcAction('start','${escAttr(e.app)}')">启动</button><button class="btn btn-secondary btn-sm" onclick="doSvcAction('stop','${escAttr(e.app)}')">停止</button>`:'';
  let ops='';
  if(e.installed){
   ops+=svcBtns+`<button class="btn btn-secondary btn-sm" onclick="pluginOpen('${escAttr(e.id)}')">打开</button>`;
   ops+=e.upToDate
    ?'<button class="btn btn-secondary btn-sm" disabled>已是最新</button>'
    :`<button class="btn btn-primary btn-sm" onclick="pluginStoreInstall('${escAttr(e.id)}','${escAttr(e.name)}')">更新到 v${esc(e.version)}</button>`;
   ops+=`<button class="btn btn-danger btn-sm" onclick="pluginDelete('${escAttr(e.id)}')">删除</button>`;
  }else{
   ops+=`<button class="btn btn-primary btn-sm" onclick="pluginStoreInstall('${escAttr(e.id)}','${escAttr(e.name)}')">安装</button>`;
  }
  const status=e.installed?('已安装'+(e.installedVersion?` v${esc(e.installedVersion)}`:'')+(e.upToDate?'':' · <span class="text-red">可更新</span>')):'未安装';
  rows+=`<tr>
   <td><b>${esc(e.icon||'🧩')} ${esc(e.name)}</b> <span class="text-muted" style="font-size:11px">v${esc(e.version)}</span>${appTxt}</td>
   <td style="font-size:12px">${e.author?esc(e.author):'<span class="text-muted">—</span>'}</td>
   <td class="text-muted" style="font-size:12px">${e.description?esc(e.description):'—'}</td>
   <td style="font-size:12px">${status}</td>
   <td class="fm-row-actions">${ops}</td>
  </tr>`;
 });
 (d.plugins||[]).forEach(p=>{
  if(storeIds.has(p.id)||!hit(p.id,p.name,p.description)) return;
  const svc=p.app?pluginSvc[p.app]:null;
  const appTxt=p.app?`<span class="text-muted" style="font-size:11px"> · 关联 ${esc(p.app)}${svc?(svc.running?' 🟢':' 🔴'):''}</span>`:'';
  const svcBtns=(p.app&&canView('services'))?`<button class="btn btn-secondary btn-sm" onclick="doSvcAction('start','${escAttr(p.app)}')">启动</button><button class="btn btn-secondary btn-sm" onclick="doSvcAction('stop','${escAttr(p.app)}')">停止</button>`:'';
  rows+=`<tr>
   <td><b>${esc(p.icon||'🧩')} ${esc(p.name)}</b>${p.version?` <span class="text-muted" style="font-size:11px">v${esc(p.version)}</span>`:''} <span class="text-muted" style="font-size:11px">· ${esc(p.id)}</span>${appTxt}</td>
   <td style="font-size:12px"><span class="text-muted">—</span></td>
   <td class="text-muted" style="font-size:12px">${p.description?esc(p.description):'—'}</td>
   <td style="font-size:12px">已安装（本地）</td>
   <td class="fm-row-actions">${svcBtns}<button class="btn btn-secondary btn-sm" onclick="pluginOpen('${escAttr(p.id)}')">打开</button><button class="btn btn-danger btn-sm" onclick="pluginDelete('${escAttr(p.id)}')">删除</button></td>
  </tr>`;
 });
 let invalidHtml='';
 if((d.invalid||[]).length){
  invalidHtml=`<div style="margin-top:12px"><div class="text-muted" style="font-size:12px;margin-bottom:6px">⚠ 以下目录不构成有效插件（缺少/损坏 plugin.json）：</div>`+
   (d.invalid||[]).map(x=>`<div class="clean-target"><span class="p">${esc(x.id)}</span><span class="text-muted">${esc(x.error)}</span><button class="btn btn-danger btn-sm" onclick="pluginDelete('${escAttr(x.id)}')">删除</button></div>`).join('')+`</div>`;
 }
 let storeNote='';
 if(pluginStore&&pluginStore.configured===false){
  storeNote=`<div class="text-muted" style="font-size:12px;margin-bottom:8px">🌐 在线插件源未配置（「配置」页 →「插件源地址」填写 catalog.json 的 HTTPS 地址后，可在此一键安装/更新）。</div>`;
 }else if(pluginStore&&pluginStore.error){
  storeNote=`<div class="text-red" style="font-size:12px;margin-bottom:8px">🌐 在线插件源加载失败 —— ${esc(pluginStore.error)}</div>`;
 }else if(storeReady){
  storeNote=`<div class="text-muted" style="font-size:12px;margin-bottom:8px">🌐 在线插件源已连接${pluginStore.signature?' · 🔏 强制验签已启用':''} · HTTPS + SHA-256 校验</div>`;
 }
 return `${storeNote}
  <div style="overflow-x:auto"><table class="svc-table">
   <thead><tr><th>插件</th><th>开发商</th><th>说明</th><th>状态</th><th style="text-align:right">操作</th></tr></thead>
   <tbody>${rows||'<tr><td colspan="5" class="fm-empty">'+(q?'未找到匹配“'+esc(pluginSearch.trim())+'”的插件':'暂无插件（上传 zip，或在平台审核上架后从插件源安装）')+'</td></tr>'}</tbody>
  </table></div>
  ${invalidHtml}`;
}

function pluginDelete(id){
 showModal('删除插件',`确定删除插件 <b>${esc(id)}</b>？<br><span class="text-muted">将删除其整个目录，不可恢复</span>`,'删除',async()=>{
  const j=await api('/star/pluginDelete',{method:'POST',body:JSON.stringify({id})});
  if(j.code!==0){showToast(j.message||'删除失败','error');return}
  showToast('已删除','success');loadPlugins();
 });
}

async function pluginInstall(){
 const el=$('plugin-file');const f=el&&el.files?el.files[0]:null;
 if(!f){showToast('请选择插件压缩包（.zip）','error');return}
 try{
  const r=await fetch('/star/pluginInstall?name='+encodeURIComponent(f.name),{method:'POST',headers:Object.assign({'Content-Type':'application/zip'},token?{'Authorization':'Bearer '+token}:{}),body:f});
  const j=await r.json();
  if(j.code!==0){showToast(j.message||'安装失败','error');return}
  showToast(j.message||'安装完成','success');loadPlugins();
 }catch(e){showToast('安装失败','error')}
}

// 选择 zip 后回显文件名（原生文件控件已隐藏，用按钮式 label 触发）
function pluginFilePicked(el){
 const f=el&&el.files?el.files[0]:null;
 const n=$('plugin-file-name');if(n)n.textContent=f?f.name:'';
}

function pluginStoreInstall(id,name){
 showModal('安装/更新插件',`从在线插件源获取 <b>${esc(name||id)}</b> 并安装？<br><span class="text-muted">服务端经 HTTPS 下载并校验 SHA-256（启用公钥时强制验签），校验失败拒绝安装</span>`,'安装',async()=>{
  showToast('正在下载安装…','info');
  const j=await api('/star/pluginStoreInstall',{method:'POST',body:JSON.stringify({id})});
  if(j.code!==0){showToast(j.message||'安装失败','error');return}
  showToast(j.message||'安装完成','success');loadPlugins();
 });
}

/* ── AI 助手（服务器问题分析；模型接口/Key 可在配置页自定义，默认 DeepSeek） ── */
let aiMessages=[]; // {role:'user'|'assistant', content, usage?}
let aiMeta=null,aiBusy=false,aiPending=false,aiUseContext=true;
const AI_QUICKS=['分析当前服务器状态','检查异常与风险点','分析最近的错误日志','磁盘空间还够用吗？'];
try{aiMessages=JSON.parse(sessionStorage.getItem('ai_chat')||'[]')||[];aiUseContext=sessionStorage.getItem('ai_chat_ctx')!=='0'}catch(e){aiMessages=[]}

function aiPersist(){
 try{sessionStorage.setItem('ai_chat',JSON.stringify(aiMessages.slice(-60)));sessionStorage.setItem('ai_chat_ctx',aiUseContext?'1':'0')}catch(e){}
}
// 轻量渲染：```代码块→<pre>、`行内代码`→<code>、换行保留；其余转义防注入
function aiRender(content){
 const esc2=s=>s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
 const parts=String(content||'').split('```');
 let html='';
 parts.forEach((p,i)=>{
  if(i%2===1){const body=p.replace(/^[a-zA-Z0-9_-]*\r?\n/,'');html+=`<pre>${esc2(body)}</pre>`}
  else{html+=esc2(p).replace(/`([^`\n]+)`/g,'<code>$1</code>').replace(/\n/g,'<br>')}
 });
 return html;
}
async function loadAi(){
 $('panel-ai').innerHTML=`<div class="glass-card card">
  <div class="flex-between mb-16" style="flex-wrap:wrap;gap:8px">
   <h2>🤖 AI 助手 <span id="ai-model-tag" class="text-muted" style="font-size:12px;font-weight:400"></span></h2>
   <div style="display:flex;gap:10px;align-items:center">
    <label class="perm-item" style="font-size:12px"><input type="checkbox" id="ai-context" ${aiUseContext?'checked':''} onchange="aiCtxToggle(this)"> 附服务器实况快照</label>
    <button class="btn btn-secondary btn-sm" onclick="aiClear()">🗑 清空</button>
   </div>
  </div>
  <div id="ai-status" class="text-muted" style="font-size:12px;margin-bottom:10px">加载中…</div>
  <div id="ai-msgs" class="ai-msgs"></div>
  <div class="ai-quick">${AI_QUICKS.map(q=>`<span class="ai-chip" onclick="aiQuick('${escAttr(q)}')">${esc(q)}</span>`).join('')}</div>
  <div class="ai-input-row">
   <textarea id="ai-input" placeholder="描述服务器问题，如：磁盘快满了吗？/ 为什么服务反复重启？（Enter 发送 · Shift+Enter 换行）" onkeydown="aiKey(event)"></textarea>
   <button class="btn btn-primary" id="ai-send" onclick="aiSend()">发送</button>
  </div>
  <div class="text-muted" style="margin-top:10px;font-size:12px">对话由服务端转发至配置的模型接口（默认 DeepSeek）；仅发送文字与所选快照（不含密码等敏感配置）；模型答复仅供参考，涉及高风险操作请先备份。</div>
 </div>`;
 renderAiMessages();
 try{const j=await api('/star/aiStatus');aiMeta=(j&&j.code===0)?j.data:null}catch(e){aiMeta=null}
 updateAiStatusLine();
}
function updateAiStatusLine(){
 const el=$('ai-status');if(!el)return;
 const tag=$('ai-model-tag');
 if(!aiMeta){el.textContent='⚠ 无法获取 AI 配置状态';return}
 if(tag)tag.textContent=aiMeta.model?('· '+aiMeta.model):'';
 if(!aiMeta.enabled){el.innerHTML='⚠ AI 助手未启用 —— 请到「⚙ 配置」页开启「AI 助手」开关';return}
 if(!aiMeta.hasKey){el.innerHTML=`⚠ 尚未配置 AI API Key —— 请到「⚙ 配置」页填写「AI API Key」（当前接口：${esc(aiMeta.baseUrl)} · 模型：${esc(aiMeta.model)}）`;return}
 el.innerHTML=`模型：<b>${esc(aiMeta.model)}</b> · 接口：${esc(aiMeta.baseUrl)} · 附服务器实况快照：${aiUseContext?'开':'关'}`;
}
function aiCtxToggle(el){aiUseContext=!!el.checked;aiPersist();updateAiStatusLine()}
function aiQuick(t){const i=$('ai-input');if(!i)return;i.value=t;aiSend()}
function aiKey(e){if(e.key==='Enter'&&!e.shiftKey){e.preventDefault();aiSend()}}
function aiClear(){aiMessages=[];aiPersist();renderAiMessages()}
function renderAiMessages(){
 const box=$('ai-msgs');if(!box)return;
 let html='';
 if(!aiMessages.length&&!aiPending){
  html='<div class="text-muted" style="font-size:13px;text-align:center;padding:34px 0">向 AI 描述服务器问题即可开始分析（可点下方快捷问题直接提问）</div>';
 }else{
  aiMessages.forEach(m=>{
   const cls=m.role==='user'?'user':'assistant';
   const usage=m.usage&&m.usage.total_tokens?`<div class="ai-usage">tokens ${m.usage.prompt_tokens||0}+${m.usage.completion_tokens||0}=${m.usage.total_tokens}</div>`:'';
   html+=`<div class="ai-msg ${cls}">${aiRender(m.content)}${usage}</div>`;
  });
 }
 if(aiPending)html+='<div class="ai-msg assistant ai-thinking">正在分析，请稍候…</div>';
 box.innerHTML=html;box.scrollTop=box.scrollHeight;
 const send=$('ai-send');if(send)send.disabled=aiBusy;
}
async function aiSend(){
 if(aiBusy)return;
 const input=$('ai-input');if(!input)return;
 const text=(input.value||'').trim();
 if(!text)return;
 input.value='';
 aiMessages.push({role:'user',content:text});
 aiPersist();aiBusy=true;aiPending=true;renderAiMessages();
 try{
  const j=await api('/star/aiChat',{method:'POST',body:JSON.stringify({messages:aiMessages.slice(-30),useContext:aiUseContext})});
  if(j.code!==0)aiMessages.push({role:'assistant',content:'⚠ '+(j.message||'AI 对话失败')});
  else aiMessages.push({role:'assistant',content:j.data.reply,usage:j.data.usage});
 }catch(e){aiMessages.push({role:'assistant',content:'⚠ 请求失败：'+(e&&e.message||e)})}
 aiBusy=false;aiPending=false;
 aiPersist();renderAiMessages();
}

/* ── 在线终端（真 PTY + xterm.js；WebSocket 流式，单区域、光标闪烁、命令回显） ── */
let termSid=sessionStorage.getItem('term_sid')||'';
if(!termSid){termSid=('t'+Date.now().toString(36)+Math.random().toString(36).slice(2)+Math.random().toString(36).slice(2)).slice(0,40);sessionStorage.setItem('term_sid',termSid)}
let termInst=null,termFit=null,termWs=null,termReconnectTimer=null;

// 资源按需加载（xterm.js 仅在进入终端页时拉取）
function termEnsureAssets(cb){
 if(window.Terminal)return cb();
 if(!document.getElementById('xterm-css')){
  const css=document.createElement('link');css.id='xterm-css';css.rel='stylesheet';css.href='/assets/xterm.css';document.head.appendChild(css);
 }
 const s1=document.createElement('script');
 s1.src='/assets/xterm.js';
 s1.onload=()=>{const s2=document.createElement('script');s2.src='/assets/addon-fit.js';s2.onload=cb;s2.onerror=()=>cb();document.head.appendChild(s2)};
 s1.onerror=()=>{const box=$('term-box');if(box)box.innerHTML='<div style="color:#f87171;padding:10px;font-size:13px">终端组件加载失败（/assets/xterm.js）——请确认面板版本已升级</div>'};
 document.head.appendChild(s1);
}
function loadTerminal(){
 $('panel-terminal').innerHTML=`<div class="glass-card card">
  <div class="flex-between mb-16" style="flex-wrap:wrap;gap:8px">
   <h2>🖥 在线终端 <span id="term-state" class="term-state">连接中…</span></h2>
   <div style="display:flex;gap:8px;align-items:center">
    <button class="btn btn-secondary btn-sm" onclick="termReset()">⟳ 重置会话</button>
   </div>
  </div>
  <div id="term-box" class="term-box"></div>
  <div class="text-muted" style="margin-top:8px;font-size:12px">真终端（PTY）：支持 vim/top 等交互式程序与完整键盘操作；命令以服务账户权限运行（Linux 通常 root）；会话在标签页内保持、空闲 30 分钟自动回收；刷新页面自动重连并回放最近输出。</div>
 </div>`;
 termEnsureAssets(()=>termInit());
}
function termInit(){
 const box=$('term-box');if(!box)return;
 if(!termInst){
  termInst=new Terminal({cursorBlink:true,fontSize:13,fontFamily:'Consolas,"Cascadia Mono",Menlo,"Courier New",monospace',theme:{background:'#0b1220',foreground:'#cbd5e1',cursor:'#60a5fa',selectionBackground:'#334155'},scrollback:5000});
  if(window.FitAddon){termFit=new FitAddon.FitAddon();termInst.loadAddon(termFit)}
  termInst.onData(d=>{if(termWs&&termWs.readyState===1)termWs.send(JSON.stringify({t:'i',d}))});
  termInst.onResize(({cols,rows})=>{if(termWs&&termWs.readyState===1)termWs.send(JSON.stringify({t:'r',c:cols,r:rows}))});
 }
 if(termInst.element){box.appendChild(termInst.element)}else{termInst.open(box)}
 try{termFit&&termFit.fit()}catch(e){}
 // 已有活连接（面板未离开过）则不重连，避免重复回放；仅同步状态文字
 if(termWs&&termWs.readyState===1){termSetState('已连接','ok');termInst.focus();return}
 termConnect();
 termInst.focus();
}
function termConnect(){
 if(termWs){try{termWs.close()}catch(e){}termWs=null}
 clearTimeout(termReconnectTimer);
 termSetState('连接中…','warn');
 const proto=location.protocol==='https:'?'wss://':'ws://';
 let url=proto+location.host+'/star/termWs?sid='+encodeURIComponent(termSid)+'&cols='+(termInst?termInst.cols:120)+'&rows='+(termInst?termInst.rows:30);
 if(token)url+='&token='+encodeURIComponent(token);
 let ws;try{ws=new WebSocket(url)}catch(e){termSetState('连接失败','err');return}
 termWs=ws;
 ws.onopen=()=>{if(termWs!==ws)return;termSetState('已连接','ok');if(termInst){try{termInst.reset()}catch(e){}}try{termFit&&termFit.fit()}catch(e){}};
 ws.onmessage=ev=>{if(termInst&&typeof ev.data==='string')termInst.write(ev.data)};
 ws.onclose=()=>{if(termWs!==ws)return;termWs=null;termSetState('已断开（3 秒后自动重连）','err');termReconnectTimer=setTimeout(()=>{const el=$('panel-terminal');if(el&&el.classList.contains('active'))termConnect()},3000)};
 ws.onerror=()=>{};
}
function termSetState(text,kind){const el=$('term-state');if(el){el.textContent=text;el.className='term-state '+(kind||'')}}
async function termReset(){
 try{await api('/star/termReset',{method:'POST',body:JSON.stringify({sid:termSid})})}catch(e){}
 if(termInst){try{termInst.reset()}catch(e){}}
 termConnect();
}
window.addEventListener('resize',()=>{const el=$('panel-terminal');if(el&&el.classList.contains('active')&&termFit){try{termFit.fit()}catch(e){}}});

/* ── 操作日志（审计） ── */
let auditData=null,auditPage=1,auditFilters={q:'',user:'',success:''};

async function loadAudit(){
 try{
  const params=new URLSearchParams({page:String(auditPage),size:'50'});
  if(auditFilters.q)params.set('q',auditFilters.q);
  if(auditFilters.user)params.set('user',auditFilters.user);
  if(auditFilters.success)params.set('success',auditFilters.success);
  const j=await api('/star/auditLogs?'+params.toString());
  if(j.code!==0){$('log-sub-audit').innerHTML=`<div class="glass-card card"><h2>📜 操作日志</h2><p class="text-muted">${esc(j.message||'加载失败')}</p></div>`;return}
  auditData=j.data;renderAudit();
 }catch(e){}
}

function renderAudit(){
 const d=auditData||{items:[],total:0,page:1,size:50};
 let rows='';
 (d.items||[]).forEach(it=>{
  rows+=`<tr>
   <td class="text-muted" style="white-space:nowrap">${esc(it.time)}</td>
   <td style="white-space:nowrap">${esc(it.userName)}<div class="text-muted" style="font-size:11px">${esc(it.ip)}</div></td>
   <td style="white-space:nowrap">${esc(it.title)}<div class="text-muted" style="font-size:11px">${esc(it.action)} ${esc(it.method)}</div></td>
   <td class="text-secondary" style="font-size:12px;word-break:break-all;max-width:380px">${esc(it.detail||'')}</td>
   <td style="white-space:nowrap">${it.success?'<span class="text-green">✓ 成功</span>':'<span class="text-red">✗ 失败</span>'}<div class="text-muted" style="font-size:11px">${esc(it.success?'':(it.message||('code '+it.code)))}</div></td>
   <td class="text-muted" style="white-space:nowrap">${it.elapsedMs}ms</td>
  </tr>`;
 });
 const totalPages=Math.max(1,Math.ceil(d.total/(d.size||50)));
 let html=`<div class="glass-card card">
  <div class="flex-between mb-16" style="flex-wrap:wrap;gap:8px">
   <h2>📜 操作日志</h2>
   <button class="btn btn-secondary btn-sm" onclick="loadAudit()">⟳ 刷新</button>
  </div>
  <div class="fm-toolbar">
   <input type="text" id="audit-q" placeholder="关键词（动作/路径/详情）" value="${escAttr(auditFilters.q)}" style="max-width:240px" onkeydown="if(event.key==='Enter')auditFilter()">
   <input type="text" id="audit-user" placeholder="操作者" value="${escAttr(auditFilters.user)}" style="max-width:140px" onkeydown="if(event.key==='Enter')auditFilter()">
   <select id="audit-success" style="max-width:120px">
    <option value="">全部结果</option>
    <option value="true" ${auditFilters.success==='true'?'selected':''}>仅成功</option>
    <option value="false" ${auditFilters.success==='false'?'selected':''}>仅失败</option>
   </select>
   <button class="btn btn-primary btn-sm" onclick="auditFilter()">🔍 筛选</button>
  </div>
  <div style="overflow-x:auto"><table class="svc-table">
   <thead><tr><th>时间</th><th>操作者</th><th>动作</th><th>详情</th><th>结果</th><th>耗时</th></tr></thead>
   <tbody>${rows||'<tr><td colspan="6" class="fm-empty">暂无操作记录</td></tr>'}</tbody>
  </table></div>
  <div class="fm-toolbar" style="justify-content:flex-end;margin-top:10px;margin-bottom:0">
   <span class="text-muted" style="font-size:12px">共 ${d.total} 条${d.truncated?'（仅扫描最近 2 万条）':''}</span>
   <button class="btn btn-secondary btn-sm" ${d.page<=1?'disabled':''} onclick="auditGo(${d.page-1})">上一页</button>
   <span class="text-secondary" style="font-size:12px">第 ${d.page} / ${totalPages} 页</span>
   <button class="btn btn-secondary btn-sm" ${d.page>=totalPages?'disabled':''} onclick="auditGo(${d.page+1})">下一页</button>
  </div>
  <div class="text-muted" style="margin-top:10px;font-size:12px">登录、配置修改、服务控制、文件管理、日志清理等全部变更类操作自动落库（密码等敏感字段已脱敏）；本页查询本身受“操作日志”菜单权限控制。</div>
 </div>`;
 $('log-sub-audit').innerHTML=html;
}

function auditFilter(){
 auditFilters.q=$('audit-q')?$('audit-q').value.trim():'';
 auditFilters.user=$('audit-user')?$('audit-user').value.trim():'';
 auditFilters.success=$('audit-success')?$('audit-success').value:'';
 auditPage=1;loadAudit();
}
function auditGo(p){auditPage=Math.max(1,p);loadAudit()}

/* ── 用户管理（仅内置管理员） ── */
let usersData=null;

async function loadUsers(){
 try{
  const j=await api('/star/userList');
  if(j.code!==0){$('panel-users').innerHTML=`<div class="glass-card card"><h2>👥 用户管理</h2><p class="text-muted">${esc(j.message||'加载失败')}</p></div>`;return}
  usersData=j.data;renderUsers();
 }catch(e){}
}

function renderUsers(){
 const d=usersData||{users:[],permissions:[]};
 let rows='';
 (d.users||[]).forEach(u=>{
  const builtin=!!u.isBuiltin;
  const head=builtin?`👑 ${esc(u.userName)} <span class="text-muted" style="font-size:11px">(内置管理员)</span>`:`👤 ${esc(u.userName)}`;
  const permCell=builtin
   ? '<span class="text-secondary">全部权限</span>'
   : (u.permissionNames&&u.permissionNames.length?esc(u.permissionNames.join('、')):'<span class="text-muted">无</span>');
  const actions=builtin
   ? `<button class="btn btn-secondary btn-sm" onclick="userAdminEdit('${escAttr(u.userName)}')">编辑</button>`
   : `<button class="btn btn-secondary btn-sm" onclick="userEdit('${escAttr(u.userName)}')">编辑</button>
      <button class="btn btn-danger btn-sm" onclick="userDelete('${escAttr(u.userName)}')">删除</button>`;
  rows+=`<tr>
   <td>${head}${u.remark?` <span class="text-muted" style="font-size:12px">（${esc(u.remark)}）</span>`:''}</td>
   <td style="font-size:12px">${permCell}</td>
   <td>${u.enabled?'<span class="text-green">✅ 启用</span>':'<span class="text-red">⛔ 禁用</span>'}</td>
   <td class="fm-row-actions">${actions}</td>
  </tr>`;
 });
 let html=`<div class="glass-card card">
  <div class="flex-between mb-16" style="flex-wrap:wrap;gap:8px">
   <h2>👥 用户管理</h2>
   <button class="btn btn-primary btn-sm" onclick="userEdit('')">➕ 新增用户</button>
  </div>
  <div style="overflow-x:auto"><table class="svc-table">
   <thead><tr><th>用户名</th><th>菜单权限</th><th>状态</th><th style="text-align:right">操作</th></tr></thead>
   <tbody>${rows||'<tr><td colspan="4" class="fm-empty">暂无用户</td></tr>'}</tbody>
  </table></div>
  <div class="text-muted" style="margin-top:10px;font-size:12px">内置管理员（配置文件凭据）拥有全部权限且不可删除（可在此编辑用户名/密码）；其余用户登录后仅能看到被勾选的菜单，越权请求会被服务端拒绝并记入操作日志。</div>
 </div>`;
 $('panel-users').innerHTML=html;
}

// 编辑内置管理员（用户名/密码，均写入配置文件凭据）
function userAdminEdit(name){
 showModal('编辑内置管理员',
  `<div class="text-muted" style="margin-bottom:8px">内置管理员拥有全部权限（配置文件凭据）</div>
   <span class="text-secondary" style="font-size:12px">用户名</span>
   <input type="text" id="admin-name" value="${escAttr(name)}" style="width:100%;margin-top:4px">
   <span class="text-secondary" style="font-size:12px;display:block;margin-top:10px">新密码（留空则不修改）</span>
   <input type="password" id="admin-pass" style="width:100%;margin-top:4px" autocomplete="new-password">`,
  '保存',async()=>{
   const nm=$('admin-name')?$('admin-name').value.trim():'';
   const pw=$('admin-pass')?$('admin-pass').value:'';
   if(!nm){showToast('用户名不能为空','error');return}
   if(nm.toLowerCase()===name.toLowerCase()&&!pw){showToast('未修改任何内容','error');return}
   const payload={name,newName:nm};
   if(pw)payload.password=pw;
   const j=await api('/star/userSave',{method:'POST',body:JSON.stringify(payload)});
   if(j.code!==0){showToast(j.message||'保存失败','error');return}
   showToast('内置管理员已更新','success');loadUsers();
  });
}

function userEdit(name){
 const d=usersData||{users:[],permissions:[]};
 const u=(d.users||[]).find(x=>x.userName===name);
 const perms=new Set(u?(u.permissions||[]):[]);
 const boxes=(d.permissions||[]).map(p=>`<label class="perm-item"><input type="checkbox" value="${escAttr(p.key)}" ${perms.has(p.key)?'checked':''}> ${esc(p.name)}</label>`).join('');
 const body=`
  <div style="margin-bottom:10px"><span class="text-secondary" style="font-size:12px">用户名</span>
   <input type="text" id="user-name" value="${escAttr(name)}" ${u?'disabled':''} style="width:100%;margin-top:4px" placeholder="登录用户名"></div>
  <div style="margin-bottom:10px"><span class="text-secondary" style="font-size:12px">密码${u?'（留空则不修改）':'（必填）'}</span>
   <input type="password" id="user-pass" style="width:100%;margin-top:4px" placeholder="${u?'••••••':'设置初始密码'}" autocomplete="new-password"></div>
  <div style="margin-bottom:6px"><span class="text-secondary" style="font-size:12px">菜单权限</span></div>
  <div class="perm-grid">${boxes}</div>
  <div style="margin:8px 0 10px"><label class="perm-item" style="font-size:13px"><input type="checkbox" id="user-enabled" ${!u||u.enabled?'checked':''}> 启用该用户</label></div>
  <div><span class="text-secondary" style="font-size:12px">备注</span>
   <input type="text" id="user-remark" value="${escAttr(u?u.remark:'')}" style="width:100%;margin-top:4px" placeholder="可选"></div>`;
 showModal(u?'编辑用户：'+u.userName:'新增用户',body,'保存',async()=>{
  const nmEl=$('user-name');
  const nameVal=nmEl?nmEl.value.trim():name;
  const pass=$('user-pass')?$('user-pass').value:'';
  const enabled=$('user-enabled')?$('user-enabled').checked:true;
  const remark=$('user-remark')?$('user-remark').value.trim():'';
  const permissions=[...document.querySelectorAll('.perm-grid input:checked')].map(c=>c.value);
  if(!nameVal){showToast('用户名不能为空','error');return}
  const builtin=(usersData&&usersData.users||[]).find(x=>x.isBuiltin);
  if(builtin&&nameVal.toLowerCase()===String(builtin.userName||'').toLowerCase()){showToast('该名称属于内置管理员，请另选名称','error');return}
  const payload={name:nameVal,enabled,remark,permissions};
  if(pass)payload.password=pass;
  const j=await api('/star/userSave',{method:'POST',body:JSON.stringify(payload)});
  if(j.code!==0){showToast(j.message||'保存失败','error');return}
  showToast('已保存','success');loadUsers();
 });
}

function userDelete(name){
 showModal('删除用户',`确定删除用户 <b>${esc(name)}</b>？<br><span class="text-muted">该用户将立即无法登录</span>`,'删除',async()=>{
  const j=await api('/star/userDelete',{method:'POST',body:JSON.stringify({name})});
  if(j.code!==0){showToast(j.message||'删除失败','error');return}
  showToast('已删除','success');loadUsers();
 });
}



function esc(s){return String(s??'').replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;')}
function escAttr(s){return String(s??'').replace(/&/g,'&amp;').replace(/"/g,'&quot;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/'/g,'&#39;')}

/* ── Theme ── */
function getTheme(){return localStorage.getItem('agent_theme')||'auto'}
function applyTheme(theme){
 if(theme==='auto')theme=window.matchMedia('(prefers-color-scheme:dark)').matches?'dark':'light';
 document.documentElement.classList.toggle('light',theme==='light');
 localStorage.setItem('agent_theme',getTheme());
}
function toggleTheme(){
 const cur=document.documentElement.classList.contains('light')?'light':'dark';
 const next=cur==='light'?'dark':'light';
 applyTheme(next);
 localStorage.setItem('agent_theme',next);
}
applyTheme(getTheme());
window.matchMedia('(prefers-color-scheme:dark)').addEventListener('change',()=>{if(getTheme()==='auto')applyTheme('auto')});

// ————— 拖拽上传（全局委托：任意「含 file input 的 .card/.card2」即拖放目标）—————
// 拖动开始即高亮全部候选卡（解决“不知道能拖到哪”）；拖入卡片强化高亮 → 松开填充
// input.files → 派发 change（复用既有上传流程；每卡片取首个 file input）。
// 常驻小提示自动注入（.drop-hint）。document 级委托，动态渲染的卡片自动生效。
function installDropUpload(){
 if(document._dropUploadInstalled) return;
 document._dropUploadInstalled = true;
 const cards = () => Array.from(document.querySelectorAll('.card, .card2')).filter(c => c.querySelector('input[type=file]'));
 const cardOf = e => (e.target && e.target.closest) ? e.target.closest('.card, .card2') : null;
 const inputOf = c => c ? c.querySelector('input[type=file]') : null;
 const clearCandidates = () => document.querySelectorAll('.drop-candidate').forEach(c => c.classList.remove('drop-candidate'));
 const ensureHints = () => cards().forEach(c => {
  if(!c.querySelector('.drop-hint')){
   const h = document.createElement('div');
   h.className = 'drop-hint';
   h.textContent = '⬆ 支持拖拽上传：把文件拖到本卡片即可';
   c.appendChild(h);
  }
 });
 ensureHints();
 document.addEventListener('dragenter', e => {
  const hasFile = !!(e.dataTransfer && Array.from(e.dataTransfer.types || []).includes('Files'));
  if(hasFile){ ensureHints(); cards().forEach(c => c.classList.add('drop-candidate')); }
  const c = cardOf(e); if(c && inputOf(c)) c.classList.add('drop-active');
 }, true);
 document.addEventListener('dragover', e => { const c = cardOf(e); if(c && inputOf(c)){ e.preventDefault(); e.stopPropagation(); if(e.dataTransfer) e.dataTransfer.dropEffect = 'copy'; } }, true);
 document.addEventListener('dragleave', e => {
  if(e.relatedTarget === null){ clearCandidates(); return; }
  const c = cardOf(e); if(c && !c.contains(e.relatedTarget)) c.classList.remove('drop-active');
 }, true);
 document.addEventListener('drop', e => {
  clearCandidates();
  const c = cardOf(e); if(!c) return;
  const input = inputOf(c); if(!input) return;
  e.preventDefault(); e.stopPropagation();
  c.classList.remove('drop-active');
  const fs = e.dataTransfer && e.dataTransfer.files;
  if(!fs || !fs.length) return;
  const dt = new DataTransfer();
  if(input.multiple){ for(const f of fs) dt.items.add(f); } else { dt.items.add(fs[0]); }
  input.files = dt.files;
  input.dispatchEvent(new Event('change', { bubbles: true }));
 }, true);
}
installDropUpload();

token=sessionStorage.getItem('agent_token')||'';
if(token)showApp();
else probeAuthlessAccess();

// 免鉴权探测：WebAuthLevel=LocalOnly 时本机（回环）免登录，None 时全部免登录；
// 匿名访问成功则直接进入面板，失败（401/网络错误）停留登录页。
async function probeAuthlessAccess(){
 try{
  const r=await fetch('/api/status');
  if(!r.ok)return;
  const j=await r.json();
  if(j&&j.code===0)showApp();
 }catch(e){}
}
