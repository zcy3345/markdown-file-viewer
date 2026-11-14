
# 显示能力验收文档

这篇文档用来逐条确认**代码块**、**公式块**、以及其他渲染能力。每一节对应一项要求。

## 1. 代码块 · 多语言 + 行号 + 复制

```rust
use std::path::{Path, PathBuf};

/// 渲染一篇 Markdown，返回 HTML 片段。
pub fn render(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let source = std::fs::read_to_string(path)?;
    let mut html = String::with_capacity(source.len() * 2);
    comrak::markdown_to_html(&source, &comrak::Options::default(), &mut html);
    Ok(html)
}
```

```javascript
// 长行测试：这一行故意很长，用来验证横向滚动而不是折行
const options = { extensions: ["md", "markdown"], theme: "paper", width: 46, highlight: true, math: "katex" };
```

```python
def summarize(paths):
    return {p: sum(1 for _ in open(p, encoding="utf-8")) for p in paths}
```

```bash
cargo run --release -- --theme=paper --width=46 doc.md
```

## 2. 公式块 · 居中显示

行内公式：质能等价 $E = mc^2$，以及欧拉恒等式 $e^{i\pi} + 1 = 0$。

块级、居中：

$$ax^2 + bx + c = 0 \quad \Rightarrow \quad x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}$$

$$\sum_{k=1}^{n} \frac{1}{k^2} = \frac{\pi^2}{6}$$

矩阵与分段函数：

$$
A = \begin{pmatrix} a & b \\ c & d \end{pmatrix}, \qquad
f(x) = \begin{cases} x^2 & x \ge 0 \\ -x & x < 0 \end{cases}
$$

$$\int_{-\infty}^{\infty} e^{-x^2}\,dx = \sqrt{\pi}$$

## 3. 表格

| 能力 | 状态 | 备注 |
|---|---|---|
| 代码块高亮 | 已确认 | syntect + two-face 语法集，token 颜色由 CSS 类控制 |
| 行号 + 复制 | 已确认 | 独立 gutter 列 + ~1KB JS |
| 行内公式 | 已确认 | KaTeX（浏览器端渲染）|
| 块级公式居中 | 已确认 | KaTeX display 块，不加框 |
| 表格 | 已确认 | GFM + 不拉伸 + 单方向细线 |

## 4. 引用、任务列表、脚注

> 引用块的左侧色条与行距。
> 第二行。

- [x] 已完成的任务
- [ ] 未完成的任务
- 普通列表项，含行内代码 `cargo check`

脚注测试：mdv 没有缓存[^cache]，每次打开都重新渲染。

[^cache]: 只往 `TEMP%\mdv\` 写一个临时 HTML，下次运行前清理；样式改动立刻生效。


## 5. 图片（相对路径重写）

![示意图](assets/demo.svg)

---

**加粗**、*斜体*、~~删除线~~、[外链](https://example.com)。

## 6. 边界用例（回归）

这一节是踩过的坑，改管线后必须仍然通过。

**6.1 代码块里的 `$` 不能被当成公式。** 下面这段应当原样显示 `$HOME`、`$1`、`$(pwd)`：

````bash
echo "$HOME and $1 and $(pwd)"   # 这里的 $x$ 不该被渲染成公式
awk '{print $2}' file.txt
````

**6.2 行内代码里的 `$` 与 `__` 同样安全。** 比如 `$5`、`$x$`、`__init__`、`*args, **kwargs` 都不该被公式/强调吃掉。

**6.3 原始 HTML 默认被转义。** 下面这行应当以源码形式显示，而不是被执行：

```
<script>alert("raw HTML was escaped")</script>
```

**6.4 公式与代码混排。** 行内：`$x^2$` 是代码，$x^2$ 才是公式；未闭合的 `$` 单个出现也不能炸。
