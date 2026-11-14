# 第三方组件与授权

本项目的源码以 MIT License 发布（见 `LICENSE`）。仓库与二进制中包含的第三方内容如下。

## 仓库内直接分发的文件

| 组件 | 授权 | 位置 | 说明 |
|---|---|---|---|
| **KaTeX**（`katex.min.css` / `katex.min.js` / 20 个数学字体 woff2）| MIT | `assets/katex/` | 公式渲染，构建时内嵌进 exe |

KaTeX 的完整授权文本见 <https://github.com/KaTeX/KaTeX/blob/main/LICENSE>，
其要求随分发保留版权声明与许可声明；本项目在此声明：

> KaTeX — Copyright (c) 2013-2020 Khan Academy and other contributors.
> Released under the MIT License.

## 通过 crate 链接进二进制的组件

| crate | 授权 | 用途 |
|---|---|---|
| `comrak` | BSD-2-Clause | Markdown 解析（CommonMark + GFM）|
| `syntect` | MIT | 代码高亮引擎 |
| `two-face`（bat 0.26 资产）| MIT / Apache-2.0 | 扩展语法集（200+ 语言）|
| `once_cell` | MIT / Apache-2.0 | 惰性静态初始化 |
| `windows-sys` | MIT / Apache-2.0 | Win32 API 绑定 |

各 crate 的完整授权文本随 crate 源码分发，可用 `cargo metadata` / `cargo deny` 导出。

## 图标与截图

- `assets/mdv.ico`：本项目自制（以程序生成），随源码以 MIT 分发。
- `assets/demo.svg`：本项目自制，仅用于 `spec.md` 的图片渲染测试。
- `assets/mdv-dark.tmTheme`：本项目自制的高亮主题（配色取自公开的 GitHub Dark 调色板，颜色值本身不受版权保护）。
