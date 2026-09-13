<div align="center">

# WhereBus (Web)

开源实时公交到站查询 · 网页版 + Telegram 机器人

[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Rust](https://img.shields.io/badge/Rust-2024-orange.svg)](https://www.rust-lang.org)

</div>

## 关于本项目

这是 [noctiro/wherebus](https://github.com/noctiro/wherebus) 的修改版分支：**移除了 Android 客户端，只保留网页版**。业务逻辑、provider 抽象与数据源接入沿用上游实现。

上游项目地址（源码来源）：<https://github.com/noctiro/wherebus>

本项目基于 AGPL-3.0 许可证发布，与上游保持一致。作为修改版，我们在此明确标注了对上游的改动，并保留原始版权与许可声明。详见下方 [License](#license)。

## 功能

- 城市与数据源选择，多城市切换
- 线路搜索（按线路名称、起点、终点筛选）
- 附近站点查询，按距离排序
- 线路详情与全程站点时间轴
- 实时到站信息（到站时间、距离、站数）
- 网页可见时定时刷新，隐藏时暂停

网页使用原生 HTML/CSS/JavaScript，通过同源 HTTP JSON API 查询公交 provider；不依赖 Android、模拟器、浏览器自动化或本地数据库。网页没有模拟公交数据，数据源出错会明确提示。

同一个进程还提供（见 [Telegram 机器人](#telegram-机器人)）：

- **Telegram 机器人**：交互式查询、收藏车次、记录并统计乘车习惯
- **车次监控与主动推送**：指定在某线路某站台等哪辆车，按设定间隔轮询并原地更新卡片消息，车到前一站、进入设定距离时主动提醒
- **我的面板**（Telegram Mini App）：用户自己管理城市、提醒偏好、收藏，并直接开始/停止车次监控
- **管理控制台**（网页端 `/admin`）：首次进入设置管理口令，之后查看运行状态、全局默认值与用户列表

## 架构

- `core/` — Rust 服务端，只有一个二进制 `wherebus`
  - `runtime` — 单进程启动：网页版、管理控制台与机器人共用一个端口、一个 SQLite 库
  - `web` — HTTP JSON API 与静态资源
  - `bot` — Telegram 机器人：`telegram` 最小 Bot API 客户端、`app` 交互逻辑、`watch` 车次监控与推送、`store` + `db` 用户数据（SQLite）、`auth` 登录校验与口令哈希、`miniapp` 用户面板接口、`console` 管理控制台接口、`render` 消息排版
  - `provider` — 抽象接入不同城市数据源；`domain` — 共享数据模型
- `web/` — 前端资源（原生 HTML/CSS/JavaScript），编译时内嵌进服务二进制：网页版、`miniapp.*`（Mini App 用户面板）、`admin.*`（管理控制台）

网页与机器人共用同一套 provider：机器人直接调用 provider，不经过 HTTP 接口；三者同进程运行，共享同一个 SQLite 数据库。

## 系统要求

- Rust 工具链（edition 2024）
- 平台 C/C++ 编译工具（SQLite 随 `rusqlite` 的 bundled 特性一起编译，无需另外安装）

## 构建与启动

在仓库根目录运行：

```sh
cargo run
```

一条命令同时启动：网页版 <http://127.0.0.1:8080>、管理控制台 <http://127.0.0.1:8080/admin>，以及（配置了令牌时）Telegram 机器人与它的 Mini App 用户面板。前端资源编译内嵌于服务二进制，修改网页后需要重启编译。无需 Node.js 或前端依赖安装。

配置放在仓库根目录的 `.env` 里（[`.env.example`](.env.example) 是模板）：

```sh
cp .env.example .env   # 填入 TELEGRAM_BOT_TOKEN 等
cargo run
```

启动时会先读取 `.env`，**真实环境变量优先**，因此也可以临时覆盖：

```sh
TELEGRAM_BOT_TOKEN=你的机器人令牌 cargo run
```

没设置 `TELEGRAM_BOT_TOKEN` 时不启动机器人，网页版与管理控制台照常可用。`.env` 已在 `.gitignore` 中，不要提交。

生产构建：

```sh
cargo build --release
```

环境变量 `WHEREBUS_BIND` 控制监听地址，默认 `127.0.0.1:8080`。对外提供服务时，通过 HTTPS 反向代理部署，并在代理层配置访问限流；管理控制台会暴露全部用户数据，务必只在 HTTPS 或受信网络下开放。浏览器定位需要 HTTPS 或 localhost；也支持手动输入 WGS84 经纬度。城市选择不会自动修改输入的坐标。

## HTTP API

所有查询均为 GET。业务成功响应为 `{"data": ...}`；业务错误为 `{"error":"说明"}`，HTTP 状态为 400（参数错误）、502（数据源错误）、504（超时）。框架级缺少参数/类型错误可能返回纯文本 400，客户端兼容此情形。

| 路径 | 参数 | 用途 |
| --- | --- | --- |
| `/api/health` | 无 | 服务健康检查，返回 `{"status":"ok"}` |
| `/api/services` | 无 | 城市与数据源列表，使用返回的 `id` 作为 service |
| `/api/lines` | service, q（可选） | 按线路名称、起点、终点筛选；省略 q 返回全部线路 |
| `/api/nearby` | service, lat, lng | 附近站点；输入 WGS84，服务端转换为 GCJ02 |
| `/api/station-lines` | service, station, lat, lng | 站点经过线路；坐标与附近查询相同 |
| `/api/line` | service, direction | 线路详情、换向 ID、全部站点 |
| `/api/realtime` | service, direction, order | 指定方向、上车站序的实时到站数据 |

`direction` 使用线路数据返回的 `direction_id`，`order` 使用站点的 `order`（从 1 开始）。数据字段保持 Rust domain 模型的 Serde JSON 格式。每个请求单独选择 service，不写入共享城市配置。API 不接受自定义上游地址。当前直接调用上游，不使用过期本地缓存；网页可见时每 20 秒刷新所选线路到站信息，隐藏页面暂停定时刷新。

## Telegram 机器人

与网页版同一个进程启动，共享同一个 SQLite 数据库。交互式查询实时公交，把「线路 + 上车站」收藏为车次，监控某一趟车并在到站前主动提醒；同时记录查询习惯，在主菜单按当前时段推荐常查车次。

### 环境变量

全部配置都可以写进 `.env`（见 [`.env.example`](.env.example)），也可以直接用环境变量；同名时环境变量优先。

| 变量 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- |
| `TELEGRAM_BOT_TOKEN` | 否 | — | 向 [@BotFather](https://t.me/BotFather) 申请的机器人令牌；不设置就不启动机器人 |
| `WHEREBUS_BIND` | 否 | `127.0.0.1:8080` | 网页版 / 管理控制台 / Mini App 的监听地址 |
| `WHEREBUS_DB` | 否 | `wherebus.db` | SQLite 数据库路径 |
| `WHEREBUS_ENV_FILE` | 否 | `.env` | 配置文件路径 |
| `WHEREBUS_BOT_MINIAPP_URL` | 否 | — | 「我的面板」的公网 HTTPS 地址（`https://你的域名/miniapp`），不设置则机器人里不显示入口 |
| `WHEREBUS_BOT_TZ` | 否 | `8` | 统计习惯、显示时间所用的时区偏移（小时） |
| `WHEREBUS_BOT_API` | 否 | `https://api.telegram.org` | 自建 Bot API 服务地址，本地联调也用它 |
| `WHEREBUS_BOT_DATA` | 否 | `wherebus-bot-data.json` | 旧版本的 JSON 数据文件；库里还没有用户时自动导入一次，原文件保留 |

### 命令与交互

| 命令 | 用途 |
| --- | --- |
| `/start` | 主菜单：当前城市、收藏数、按时段推荐的车次 |
| `/city [城市]` | 选择城市与数据源，选择结果会被记住 |
| `/line [关键词]` | 搜索线路；直接发送线路名（如「1路」）等价于此命令 |
| `/nearby` | 发送位置后列出附近站点及各线路到站情况 |
| `/fav` | 我的收藏车次，一键刷新到站；可删除 |
| `/watch` | 车次监控：查看当前监控状态，或从收藏里选一个开始 |
| `/settings` | 提醒设置（刷新间隔、提前几站、距离阈值、重复间隔、最长监控） |
| `/habits` | 常用线路、24 小时查询分布、当前时段推荐 |
| `/app` | 打开「我的面板」（需配置 `WHEREBUS_BOT_MINIAPP_URL`） |
| `/help`、`/cancel` | 使用说明；取消当前输入 |

交互路径：选城市 → 搜线路（或附近站点）→ 选上车站 → 实时到站页「🔄 刷新 / ⭐ 收藏这一趟 / 🔔 盯这趟车 / 🚏 换上车站」。收藏保存的是「线路 + 方向 + 上车站」的组合，之后从 `/fav` 一键查看该站到站情况。

### 车次监控与主动推送

在实时到站页点「🔔 盯这趟车」（或在「我的面板」里选线路与上车站），选择要等的具体车辆，或选「⚡ 最近的一班」自动跟随离站最近的一辆（上游没给车辆编号时只能用自动模式，卡片会注明预估未关联车辆）。

机器人随后会：

- 按 `刷新间隔`（默认 10 秒）轮询实时数据，**原地更新同一条卡片消息**，不刷屏；
- 目标车还差 `提前提醒` 站（默认 1 站，即到前一站）时，单独推送一条带通知的提醒，只发一次；
- 目标车进入 `距离提醒` 阈值（默认 500 米）后，按 `重复间隔`（默认 60 秒）反复提醒，直到上车；
- 车正在进上车站或已驶过时推送最后一条提醒并结束监控；
- 超过 `最长监控` 时间（默认 60 分钟）或连续 6 次取不到实时数据时自动结束；
- 随时可以点卡片上的「⏹ 停止盯车」，或在「我的面板」/管理控制台里停止。

阈值全部可调：机器人里 `/settings` 用 ➖ ➕ 调整，或在「我的面板」里改；管理员可在控制台设置全局默认值，供没有个人设置的用户使用。每一轮轮询都会重新读取设置，改完立即生效。`距离` 取自上游返回的「车辆距上车站距离」，数据源不提供时该条提醒不会触发。

同一用户同时只有一个监控任务，开始新的会替换旧的。任务落库，重启后自动恢复并继续更新原来的卡片消息。

### 我的面板（Telegram Mini App）

给**用户自己**用的面板，地址 `/miniapp`，只能在 Telegram 里打开：

- 个人资料与乘车习惯统计
- 切换城市与数据源
- 提醒偏好（刷新间隔、提前几站、距离阈值、重复间隔、最长监控），可恢复为全局默认
- 收藏车次：查看、删除、一键开始监控
- 车次监控：搜索线路 → 选上车站 → 选车辆或「最近的一班」→ 开始；也能随时停止
- 删除自己的全部数据

登录方式：面板只接受 Telegram Mini App 的 `initData`，服务端按官方算法用 bot token 校验 HMAC-SHA256 签名与时效（24 小时），通过后签发 12 小时有效的会话令牌。前端传来的用户 ID 一律不作为身份依据；伪造或篡改的 `initData` 会被拒绝。

配置：把服务通过 HTTPS 暴露出去，然后设置 `WHEREBUS_BOT_MINIAPP_URL=https://你的域名/miniapp`。机器人会把聊天窗口的菜单按钮指向它，主菜单也会出现「🧭 我的面板」。

接口（除登录外都需要 `Authorization: Bearer <会话令牌>`）：

| 方法与路径 | 用途 |
| --- | --- |
| `POST /api/auth/telegram` | 用 `initData` 登录，返回会话令牌 |
| `GET /api/me` | 我的资料、偏好、收藏、习惯、监控状态 |
| `POST /api/me/settings` | 修改提醒偏好（`values`）或恢复默认（`reset`） |
| `POST /api/me/city` | 切换城市与数据源 |
| `POST /api/me/favorites/delete` | 删除一条收藏 |
| `POST /api/me/watch/start` | 开始车次监控（`direction`、`order`，可选 `service`、`bus`） |
| `POST /api/me/watch/stop` | 停止车次监控 |
| `DELETE /api/me` | 删除我的全部数据 |

### 管理控制台（网页端）

地址 `/admin`，与 Telegram 身份无关：

- **首次进入时设置管理口令**（至少 8 位）。口令用 PBKDF2-HMAC-SHA256（20 万次迭代、随机盐）哈希后存进数据库的 `meta` 表，只保存哈希值。
- 之后凭口令登录，会话令牌只存在内存（进程重启即失效），保存在浏览器的 `sessionStorage` 里。
- 忘记口令：删掉数据库里 `meta` 表的 `admin_password` 记录，下次打开会重新进入首次设置流程。
- 改口令会作废其他设备上的登录。

功能：运行状态（用户数、收藏数、累计查询、进行中的监控、运行时长、数据库路径）、全局默认提醒设置、是否接纳新用户、用户列表（停用 / 解除停用 / 停止其监控 / 删除其数据）。机器人没启动时控制台照常可用，只是不显示机器人相关状态。

| 方法与路径 | 用途 |
| --- | --- |
| `GET /api/console/status` | 是否已设置口令、机器人是否运行（公开） |
| `POST /api/console/setup` | 首次设置口令（已设置过则拒绝） |
| `POST /api/console/login`、`POST /api/console/logout` | 登录 / 退出 |
| `POST /api/console/password` | 修改口令（需当前口令） |
| `GET /api/console/overview`、`GET /api/console/users` | 运行状态、用户列表 |
| `POST /api/console/users/action` | `ban` / `unban` / `stop_watch` / `delete` |
| `POST /api/console/settings` | 全局默认提醒设置、是否接纳新用户 |

越界的设置值会被夹到合法区间（例如刷新间隔不低于 5 秒），未知设置项返回 400。

### 数据存储

SQLite（默认 `wherebus.db`，WAL 模式），表结构：

| 表 | 内容 |
| --- | --- |
| `users` | 城市选择、昵称、查询次数、最近定位（已转换为 GCJ-02）、是否停用、个人提醒设置（全为 NULL 表示跟随全局默认） |
| `favorites` | 收藏的「线路 + 方向 + 上车站」 |
| `habits` | 每条线路按小时的查询次数分布 |
| `watches` | 进行中的车次监控，重启后据此恢复 |
| `meta` | 全局默认设置、是否接纳新用户、管理口令哈希 |

每次改动立即写库（一个用户的完整状态在一次事务里写完），进程被杀不会丢数据。首次启动时，如果库里还没有用户而旧的 `wherebus-bot-data.json` 存在，会自动导入一次，原文件保留作备份。

用户可在「我的面板」里删除自己的全部数据；管理员可在控制台删除任一用户的数据。

按钮参数存在内存中（callback_data 限长 64 字节），机器人重启后旧消息上的按钮会失效并提示重新查询；收藏、习惯与监控任务不受影响。

## 支持的数据源

- 掌上公交 (mygolbs)
- 车来了 (chelaile)

通过 provider trait 抽象，可扩展其他城市数据源。

## 验证

```sh
cargo test
node --check web/app.js
node --check web/bus-view.js
node --check web/miniapp.js
node --check web/admin.js
```

机器人人工检查：`/city` 选城市 → 发送线路名 → 选上车站 → 收藏 → `/fav` 刷新 → `/habits` 查看统计；「🔔 盯这趟车」→ 选车 → 观察卡片按设定间隔更新、到前一站与进入距离阈值时收到推送 → 「⏹ 停止盯车」；`/settings` 改阈值后确认下一轮立即生效；重启进程确认监控自动恢复。此外还应覆盖未选城市、关键词无结果、上游报错、按钮过期（重启后点旧按钮）。

我的面板人工检查：从机器人菜单按钮打开 → 切换城市 → 改提醒偏好 → 搜索线路并开始监控 → 停止监控 → 删除收藏；直接用浏览器打开 `/miniapp`（没有 Telegram 的 initData）应提示只能在 Telegram 内使用。

管理控制台人工检查：用空数据库打开 `/admin` 应进入首次设置流程 → 设置口令后看到运行状态与用户列表 → 改全局默认值 → 停用/解除停用某个用户 → 修改口令后确认其他会话失效。

本地联调机器人不需要真实令牌：把 `WHEREBUS_BOT_API` 指向一个本地假 Bot API 服务即可驱动整条链路；调试构建还内置了 `debug_beijing` 模拟数据源，可用于验证监控提醒。

网页人工检查：选择城市 → 搜索线路 → 打开详情 → 点击上车站 → 换向；输入经纬度 → 附近站点 → 经过线路；验证无结果、定位拒绝、数据源失败及手机窄屏布局。实际公交数据的可用性取决于上游服务，健康检查成功不表示上游可用。

## 致谢

本项目的开发离不开以下优秀的开源项目，在此向它们的开发者和社区表示衷心的感谢！

- [noctiro/wherebus](https://github.com/noctiro/wherebus) — 本项目的上游来源

## License

AGPL-3.0，与上游 [noctiro/wherebus](https://github.com/noctiro/wherebus) 保持一致。完整条款见 [LICENSE](LICENSE)。

本仓库是上游的修改版本（移除 Android 客户端，仅保留网页版）。根据 AGPL-3.0：

- 修改内容已在本文件中明确标注；
- 原始版权与许可声明予以保留；
- 若你通过网络向用户提供本软件（含修改版），必须依据 AGPL-3.0 第 13 条向这些用户提供对应的完整源代码。
