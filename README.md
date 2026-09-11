<div align="center">

# WhereBus

开源实时公交到站查询

[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Android](https://img.shields.io/badge/Android-7.0%2B-green.svg)](https://developer.android.com)
[![Rust](https://img.shields.io/badge/Rust-2024-orange.svg)](https://www.rust-lang.org)

</div>

## 功能

- 附近站点自动发现，按距离排序
- 线路实时到站信息（到站时间、距离、站数）
- 线路详情与全程站点时间轴
- 实时车辆位置追踪与拥堵状态显示
- 多信息源、多提供者，可扩展接入不同城市数据源
- 多城市切换
- 离线缓存，弱网可用

## 截图

<!-- TODO: 添加截图 -->

## 系统要求

- Android 7.0 (API 24) 及以上

## 架构

- `core/` — Rust 共享核心（Crux 架构），包含业务逻辑、数据缓存、provider 抽象
- `android/` — Android 客户端（Jetpack Compose）

核心通过 UniFFI 桥接暴露给 Android 端，UI 层负责渲染和事件转发。

## 构建

### Core

```
cargo build
```

### Android

用 Android Studio 打开 `android/` 目录，或：

```
cd android && ./gradlew assembleDebug
```

需要 NDK 和 Rust Android targets（`aarch64-linux-android` 等）。

## 支持的数据源

- 掌上公交 (mygolbs)
- 车来了 (chelaile)

通过 provider trait 抽象，可扩展其他城市数据源。

## 致谢

本项目的开发离不开以下优秀的开源项目，在此向它们的开发者和社区表示衷心的感谢！

- [Crux](https://github.com/redbadger/crux) — 跨平台应用架构框架

## License

AGPL-3.0

## API 查询网页版

网页版使用原生 HTML/CSS/JavaScript，通过同源 HTTP JSON API 查询现有公交 provider；不依赖 Android、模拟器、浏览器自动化或本地数据库。保留 Android 客户端，两个入口可以独立使用。网页没有模拟公交数据，数据源出错会明确提示。

### 启动

安装 Rust 工具链和平台 C/C++ 编译工具后，在仓库根目录运行：

```sh
cargo run -p wherebus --features web --bin wherebus-web
```

浏览器访问 http://127.0.0.1:8080 。前端资源编译内嵌于服务二进制，修改网页后需要重启编译。无需 Node.js 或前端依赖安装。

生产构建：

```sh
cargo build --release -p wherebus --features web --bin wherebus-web
```

环境变量 `WHEREBUS_BIND` 控制监听地址，默认 `127.0.0.1:8080`。对外提供服务时，通过 HTTPS 反向代理部署，并在代理层配置访问限流。浏览器定位需要 HTTPS 或 localhost；也支持手动输入 WGS84 经纬度。城市选择不会自动修改输入的坐标。

### HTTP API

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

### 验证

```sh
cargo test -p wherebus --features web web::tests
node --check web/app.js
```

人工检查：选择城市 → 搜索线路 → 打开详情 → 点击上车站 → 换向；输入经纬度 → 附近站点 → 经过线路；验证无结果、定位拒绝、数据源失败及手机窄屏布局。实际公交数据的可用性取决于上游服务，健康检查成功不表示上游可用。
