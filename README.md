<div align="center">

# WhereBus (Web)

开源实时公交到站查询 · 网页版

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

## 架构

- `core/` — Rust 服务端：`web` 模块提供 HTTP JSON API 与静态资源，`provider` 抽象接入不同城市数据源，`domain` 为共享数据模型
- `web/` — 前端资源（原生 HTML/CSS/JavaScript），编译时内嵌进服务二进制

## 系统要求

- Rust 工具链（edition 2024）
- 平台 C/C++ 编译工具

## 构建与启动

在仓库根目录运行：

```sh
cargo run --bin wherebus-web
```

浏览器访问 <http://127.0.0.1:8080> 。前端资源编译内嵌于服务二进制，修改网页后需要重启编译。无需 Node.js 或前端依赖安装。

生产构建：

```sh
cargo build --release --bin wherebus-web
```

环境变量 `WHEREBUS_BIND` 控制监听地址，默认 `127.0.0.1:8080`。对外提供服务时，通过 HTTPS 反向代理部署，并在代理层配置访问限流。浏览器定位需要 HTTPS 或 localhost；也支持手动输入 WGS84 经纬度。城市选择不会自动修改输入的坐标。

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

## 支持的数据源

- 掌上公交 (mygolbs)
- 车来了 (chelaile)

通过 provider trait 抽象，可扩展其他城市数据源。

## 验证

```sh
cargo test -p wherebus web::tests
node --check web/app.js
node --check web/bus-view.js
```

人工检查：选择城市 → 搜索线路 → 打开详情 → 点击上车站 → 换向；输入经纬度 → 附近站点 → 经过线路；验证无结果、定位拒绝、数据源失败及手机窄屏布局。实际公交数据的可用性取决于上游服务，健康检查成功不表示上游可用。

## 致谢

本项目的开发离不开以下优秀的开源项目，在此向它们的开发者和社区表示衷心的感谢！

- [noctiro/wherebus](https://github.com/noctiro/wherebus) — 本项目的上游来源

## License

AGPL-3.0，与上游 [noctiro/wherebus](https://github.com/noctiro/wherebus) 保持一致。完整条款见 [LICENSE](LICENSE)。

本仓库是上游的修改版本（移除 Android 客户端，仅保留网页版）。根据 AGPL-3.0：

- 修改内容已在本文件中明确标注；
- 原始版权与许可声明予以保留；
- 若你通过网络向用户提供本软件（含修改版），必须依据 AGPL-3.0 第 13 条向这些用户提供对应的完整源代码。
