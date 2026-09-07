# ADR 0001：Relay 传输是显式能力

- 状态：Accepted
- 日期：2026-09-04

## 决策

Relay 默认使用 HTTP Responses。Responses WebSocket 只在用户明确选择时写入 `supports_websockets = true`。HTTPS scheme、服务名或“OpenAI-compatible”描述均不能推导 WSS 能力。

## 原因

大量中转站只实现 HTTP/SSE；强制 WSS 会造成客户端反复重连，同时把服务端能力错误建模成运行模式固有属性。

## 后果

旧槽位缺少传输字段时迁移为 HTTP；用户需要 WSS 时重新保存显式选择。保存/切换仍不执行网络探测。
