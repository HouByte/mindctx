# polyglot 项目笔记（非代码语料，验证 search 覆盖 docs）

- 重试上限默认 3 次；退避用 exponential backoff，第 n 次延迟 = base * 2^(n-1) ms。
- full jitter 与 equal jitter 的取舍：对冲惊群优先 full；保底延迟优先 equal。
- `Retry-After` 响应头优先于本地退避曲线；解析失败回退本地策略。
- 约定：所有对外 HTTP 客户端必须经过 Transport 抽象，便于注入 fake。
- 约定：单元测试不访问网络；用 fake transport 固定响应序列。
