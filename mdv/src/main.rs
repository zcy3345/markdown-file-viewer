//! mdv — Markdown -> styled static HTML -> Edge `--app` window.
//!
//! 子系统是 GUI（不是 console）：否则双击 .md 时 Windows 会为 mdv.exe 弹一个黑框。
//! 从终端运行时用 AttachConsole(父进程) 挂回控制台，输出照常可见。
//!
//! 无缓存：每次打开都重新渲染，只写一个临时 HTML 到 %TEMP%\mdv\，下次运行清理。
//!
//! Math: comrak leaves $...$ / $$...$$ verbatim; KaTeX + auto-render are inlined into
//! the page and render formulas browser-side, so no math engine is ported to Rust.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use once_cell::sync::Lazy;
use std::io::Cursor;
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

/// 支持 --app 的 Chromium 系浏览器，按优先级找。
const BROWSER_EXES: [&str; 6] = [
    "msedge.exe",
    "chrome.exe",
    "brave.exe",
    "vivaldi.exe",
    "chromium.exe",
    "opera.exe",
];

/// 注册表里查不到时的兜底路径（Edge 也可能装在 C 盘以外）。
const BROWSER_PATHS: [&str; 4] = [
    "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
    "C:/Program Files/Microsoft/Edge/Application/msedge.exe",
    "C:/Program Files/Google/Chrome/Application/chrome.exe",
    "C:/Program Files (x86)/Google/Chrome/Application/chrome.exe",
];

/// Compile-time copy of the reference stylesheet; runtime lookup below prefers the
/// live file so that CSS edits invalidate the cache without a rebuild.
const EMBEDDED_CSS: &str = include_str!("../../mdv.css");
const EMBEDDED_ICON: &[u8] = include_bytes!("../../assets/mdv.ico");

/// 从 Explorer 双击启动时没有控制台，stdout/stderr 是无效句柄；
/// 此时 println! 会 panic，所以所有输出都走这两个宏。
static QUIET: AtomicBool = AtomicBool::new(false);

macro_rules! say {
    ($($t:tt)*) => {
        if !QUIET.load(Ordering::Relaxed) {
            println!($($t)*);
        }
    };
}

macro_rules! say_err {
    ($($t:tt)*) => {
        if !QUIET.load(Ordering::Relaxed) {
            eprintln!($($t)*);
        }
    };
}

/// 文件关联用的 ProgId
const PROGID: &str = "mdv.Document";
const KEY_PROGID: &str = r"HKCU\Software\Classes\mdv.Document";
const KEY_APP: &str = r"HKCU\Software\Classes\Applications\mdv.exe";
const KEY_MD: &str = r"HKCU\Software\Classes\.md";
const KEY_STATE: &str = r"HKCU\Software\mdv";

/// 语法集来自 two-face（bat 打包的那套，200+ 语言），比 syntect 默认的 75 种全得多。
static SYNTAXES: Lazy<SyntaxSet> = Lazy::new(two_face::syntax::extra_newlines);

/// 挂到父进程的控制台（从 cmd/PowerShell 启动时），这样 GUI 子系统也能输出。
/// 从 Explorer 启动时没有父控制台 → 返回 false，此时所有输出静默丢弃。
#[cfg(windows)]
fn attach_parent_console() -> bool {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 }
}

#[cfg(not(windows))]
fn attach_parent_console() -> bool {
    true
}

/// stdout 是否可写（GUI 子系统下从 Explorer 启动时是无效句柄，直接 println! 会 panic）。
#[cfg(windows)]
fn stdout_usable() -> bool {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_UNKNOWN, GetFileType};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return false;
        }
        GetFileType(h) != FILE_TYPE_UNKNOWN
    }
}

#[cfg(not(windows))]
fn stdout_usable() -> bool {
    true
}

/// 没有控制台时用弹窗报错，否则用户什么也看不到。
#[cfg(windows)]
fn report_error_no_console(msg: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let text: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let title: Vec<u16> = "mdv".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn report_error_no_console(msg: &str) {
    eprintln!("mdv: {msg}");
}

fn main() {
    attach_parent_console();
    if !stdout_usable() {
        QUIET.store(true, Ordering::Relaxed);
    }
    // 子命令：文件关联（双击 .md 直接渲染，无需常驻进程）
    let argv: Vec<String> = env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("--install") => {
            let no_default = argv.iter().any(|a| a == "--no-default");
            exit_with(install(no_default));
        }
        Some("--uninstall") => exit_with(uninstall()),
        Some("--status") => exit_with(status()),
        _ => {}
    }
    if let Err(e) = run() {
        if QUIET.load(Ordering::Relaxed) {
            report_error_no_console(&format!("无法渲染这个文件：\n{e}"));
        } else {
            say_err!("mdv: {e}");
        }
        std::process::exit(1);
    }
}

struct Args {
    file: PathBuf,
    print_only: bool,
    /// --shot <out.png>：渲染后用 headless Edge 截图（替代 tools/shot.mjs，不再需要 Node）
    shot: Option<PathBuf>,
    size: (u32, u32),
    /// --shot --dark：强制深色模式截图（视觉回归用）
    dark: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut file: Option<PathBuf> = None;
    let mut print_only = false;
    let mut shot: Option<PathBuf> = None;
    let mut size: (u32, u32) = (940, 1080);
    let mut dark = false;
    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--print" {
            print_only = true;
        } else if a == "--dark" {
            dark = true;
        } else if a == "--shot" {
            shot = Some(PathBuf::from(
                it.next()
                    .ok_or_else(|| "--shot 需要一个输出路径".to_string())?,
            ));
        } else if a == "--size" {
            let v = it.next().ok_or_else(|| "--size 需要 WxH".to_string())?;
            let (w, h) = v
                .split_once('x')
                .ok_or_else(|| format!("--size 格式应为 WxH，收到 {v}"))?;
            size = (
                w.parse().map_err(|_| format!("宽度非法: {w}"))?,
                h.parse().map_err(|_| format!("高度非法: {h}"))?,
            );
        } else if a == "-h" || a == "--help" {
            say!("usage: mdv <file.md> [--print] [--shot out.png] [--size 940x3000]");
            say!();
            say!("  --print        只打印生成的 HTML 路径，不开窗");
            say!("  --shot <png>   渲染后用 headless Edge 截图（视觉回归）");
            say!("  --size WxH     截图/窗口尺寸，默认 940x1080");
            say!("  --dark         配合 --shot：强制深色模式截图");
            say!();
            say!("  --install [--no-default]  注册 .md 文件关联（双击即渲染，无常驻进程）");
            say!("  --uninstall               移除文件关联");
            say!("  --status                  查看当前关联状态");
            say!();
            say!("env: MDV_CSS=<path>   开发时用外部样式覆盖内嵌的 mdv.css");
            std::process::exit(0);
        } else if a.starts_with("--") {
            return Err(format!("unknown option: {a}"));
        } else if file.is_none() {
            file = Some(PathBuf::from(a));
        } else {
            return Err(format!("unexpected argument: {a}"));
        }
    }
    if dark && shot.is_none() {
        return Err("--dark 只能配合 --shot 使用（它只影响截图）".to_string());
    }
    if size.0 == 0 || size.1 == 0 || size.0 > 10000 || size.1 > 10000 {
        return Err(format!(
            "--size 非法: {}x{}（宽高需在 1..=10000 之间）",
            size.0, size.1
        ));
    }
    let file = file.ok_or_else(|| "usage: mdv <file.md> [--print] [--shot out.png]".to_string())?;
    Ok(Args {
        file,
        print_only,
        shot,
        size,
        dark,
    })
}

fn run() -> Result<(), String> {
    let t_start = Instant::now();
    let args = parse_args()?;

    let source_path = fs::canonicalize(&args.file)
        .map_err(|e| format!("cannot open {}: {e}", args.file.display()))?;
    // canonicalize() yields a verbatim `\\?\C:\...` path on Windows; strip it so
    // the generated file:// URLs stay clean.
    let source_path = strip_verbatim(source_path);
    let source_dir = source_path
        .parent()
        .ok_or_else(|| "source file has no parent directory".to_string())?
        .to_path_buf();
    let raw = fs::read_to_string(&source_path)
        .map_err(|e| format!("cannot read {}: {e}", source_path.display()))?;
    // Normalise newlines so syntect line splitting and line counts agree.
    let source = raw.replace("\r\n", "\n").replace('\r', "\n");
    ensure_icon();
    let css = load_css();
    let katex = katex_assets();
    let file_name = source_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled.md".to_string());

    // ---- 0. 无缓存：每次打开都重新渲染，只写一个临时文件到 %TEMP%\mdv\
    let work = work_dir();
    prune_work(&work);
    let out_path = work.join(format!("view-{}.html", std::process::id()));

    // ---- 1. markdown -> html (raw HTML stays disabled: render.r#unsafe = false)
    let t_comrak = Instant::now();
    let options = comrak_options();
    let md_html = comrak::markdown_to_html(&source, &options);
    let comrak_ms = t_comrak.elapsed().as_secs_f64() * 1000.0;

    // ---- 2. fenced code blocks -> .code shell + syntect highlighting
    let t_hl = Instant::now();
    let body = process_code_blocks(&md_html);
    let highlight_ms = t_hl.elapsed().as_secs_f64() * 1000.0;

    // ---- 3. rewrite relative src/href to file:// URLs
    let body = rewrite_local_urls(&body, &source_dir);

    // ---- 4. assemble document (mdv.css inlined, no external assets)
    let kb = raw.len() as f64 / 1024.0;
    let render_ms = t_start.elapsed().as_secs_f64() * 1000.0;
    let pos_script = if env::var("MDV_RESTORE").map(|v| v == "0").unwrap_or(false) {
        None
    } else {
        Some(POS_SCRIPT.replace("__MDV_DOC__", &js_string(&source_path.to_string_lossy())))
    };
    let doc = build_document(
        &file_name,
        &css,
        PageParts {
            katex_base: katex.as_ref(),
            pos_script: pos_script.as_deref(),
            body: &body,
            kb,
            chars: source.chars().count(),
            render_ms,
        },
    );

    // ---- 5. 写临时文件（下次运行会清掉上一批）
    fs::create_dir_all(&work).map_err(|e| format!("mkdir {}: {e}", work.display()))?;
    let t_write = Instant::now();
    write_atomic(&out_path, doc.as_bytes())
        .map_err(|e| format!("write {}: {e}", out_path.display()))?;
    let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
    let total_ms = t_start.elapsed().as_secs_f64() * 1000.0;

    say!("{}", out_path.display());
    say!(
        "mdv: comrak {:.1} ms | highlight {:.1} ms | write {:.1} ms | total {:.1} ms",
        comrak_ms,
        highlight_ms,
        write_ms,
        total_ms
    );
    finish(&args, &out_path)
}

/// 渲染之后的收尾：截图 / 只打印 / 开窗。
fn finish(args: &Args, out_path: &Path) -> Result<(), String> {
    if let Some(out) = &args.shot {
        screenshot(out_path, out, args.size, args.dark)?;
        say!("shot: {}", out.display());
        return Ok(());
    }
    if args.print_only {
        return Ok(());
    }
    launch(out_path, args.size)
}

fn comrak_options() -> comrak::Options<'static> {
    let mut options = comrak::Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.tasklist = true;
    options.extension.autolink = true;
    options.extension.footnotes = true;
    // $...$ / $$...$$ -> <span data-math-style="inline|display">raw latex</span>,
    // which the page then hands to KaTeX. Keeps "&" out of markdown escaping.
    options.extension.math_dollars = true;
    // comrak 1.0.0-rc.1 renamed the old `header_ids: Option<String>` to
    // `header_id_prefix: Option<String>`; an empty prefix yields plain ids.
    options.extension.header_id_prefix = Some(String::new());
    // Security: raw HTML is never emitted.
    options.render.r#unsafe = false;
    options
}

/// 样式只有一份真相：编译期内嵌。MDV_CSS 仅用于开发时临时覆盖（改样式免重编译）。
fn load_css() -> String {
    if let Ok(p) = env::var("MDV_CSS") {
        match fs::read_to_string(&p) {
            Ok(s) => return s,
            Err(e) => say_err!("mdv: warning: MDV_CSS={p} 读取失败({e})，改用内嵌样式"),
        }
    }
    EMBEDDED_CSS.to_string()
}

// ---------------------------------------------------------------- code blocks

fn process_code_blocks(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len() + html.len() / 4);
    let mut pos = 0usize;
    let mut last = 0usize;

    while pos + 10 <= bytes.len() {
        let Some(rel) = find(&bytes[pos..], b"<pre><code") else {
            break;
        };
        let start = pos + rel;
        // NB: `<pre>` has its own '>', so start looking for the tag end after
        // the `<code` prefix (the `<pre ...><code ...>` pair).
        let after_code = start + b"<pre><code".len();
        let Some(open_rel) = find(&bytes[after_code..], b">") else {
            break;
        };
        let open_end = after_code + open_rel;
        let content_start = open_end + 1;
        let Some(close_rel) = find(&bytes[content_start..], b"</code></pre>") else {
            break;
        };
        let content_end = content_start + close_rel;

        let open_tag = &html[start..=open_end];
        let lang = extract_language(open_tag);
        let decoded = decode_entities(&html[content_start..content_end]);
        let code = decoded.strip_suffix('\n').unwrap_or(&decoded);
        let code = code.strip_suffix('\r').unwrap_or(code);

        out.push_str(&html[last..start]);
        out.push_str(&render_code_block(code, &lang));
        last = content_end + "</code></pre>".len();
        pos = last;
    }
    out.push_str(&html[last..]);
    out
}

fn extract_language(open_tag: &str) -> String {
    if let Some(i) = open_tag.find("class=\"language-") {
        let rest = &open_tag[i + "class=\"language-".len()..];
        if let Some(end) = rest.find('"') {
            return rest[..end].to_string();
        }
    }
    String::new()
}

/// 代码围栏里常见的写法 → 语法集认识的扩展名。
/// 不认识的（text / plaintext / console…）自然落到 None，按纯文本输出。
const LANG_ALIASES: &[(&str, &str)] = &[
    ("csharp", "cs"),
    ("c#", "cs"),
    ("kt", "kotlin"),
    ("kts", "kotlin"),
    ("docker", "dockerfile"),
    ("containerfile", "dockerfile"),
    ("shell", "sh"),
    ("zsh", "sh"),
    ("console", ""),
    ("output", ""),
    ("plaintext", ""),
    ("text", ""),
    ("ps", "powershell"),
    ("make", "makefile"),
    ("golang", "go"),
    ("proto3", "proto"),
    ("nl", ""),
];

fn highlight_language(lang: &str) -> Option<&'static syntect::parsing::SyntaxReference> {
    if lang.is_empty() {
        return None;
    }
    let lower = lang.to_ascii_lowercase();
    let direct = SYNTAXES
        .find_syntax_by_extension(&lower)
        .or_else(|| SYNTAXES.find_syntax_by_token(&lower))
        .or_else(|| SYNTAXES.find_syntax_by_token(lang))
        .or_else(|| SYNTAXES.find_syntax_by_name(lang));
    if direct.is_some() {
        return direct;
    }
    // 别名二级查找
    let alias = LANG_ALIASES
        .iter()
        .find(|(k, _)| *k == lower)
        .map(|(_, v)| *v)?;
    if alias.is_empty() {
        return None;
    }
    SYNTAXES
        .find_syntax_by_extension(alias)
        .or_else(|| SYNTAXES.find_syntax_by_token(alias))
}

/// 自带主题：与 mdv.css 的代码卡片配色一致（GitHub dark 风格）。
/// syntect 内置的 base16-* 主题在这块深色背景上对比度过低，看起来"像没有高亮"。
const MDV_THEME: &str = include_str!("../../assets/mdv-dark.tmTheme");
/// MDV_GRAY=1：给 <html> 一个初始的 mode-gray（页面上还有三个按钮可以随时切换）。
/// 注意：代码 token 的灰度配色由 mdv.css 里的 .mode-gray 定义，
/// 这里只决定"打开时默认是哪个模式"。
fn gray_mode() -> bool {
    env::var("MDV_GRAY")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false)
}

static THEME: Lazy<Theme> = Lazy::new(|| {
    ThemeSet::load_from_reader(&mut Cursor::new(MDV_THEME.as_bytes()))
        .expect("assets/mdv-dark.tmTheme 解析失败（编译期资产，坏了就是构建问题）")
});

fn theme() -> &'static Theme {
    &THEME
}

/// 把语法高亮的颜色映射成 CSS 类名，页面上再按模式给这些类上色。
/// 这样"正常/灰度/暗色"三种模式切换时不需要重新渲染，代码块也跟着变。
/// 颜色取自 assets/mdv-dark.tmTheme（我们自己写的主题，色值是固定的）。
fn token_class(r: u8, g: u8, b: u8) -> Option<&'static str> {
    Some(match (r, g, b) {
        (0xff, 0x7b, 0x72) => "kw",      // 关键字 / built_in
        (0xa5, 0xd6, 0xff) => "str",     // 字符串 / 属性
        (0x79, 0xc0, 0xff) => "num",     // 数字 / 常量
        (0xd2, 0xa8, 0xff) => "fn",      // 函数名
        (0x7e, 0xe7, 0x87) => "type",    // 类型 / 类
        (0xff, 0xa6, 0x57) => "meta",    // 预处理 / 注解
        (0x8b, 0x94, 0x9e) => "comment", // 注释
        (0xe6, 0xed, 0xf3) => "var",     // 变量 / 普通标识符
        _ => return None,
    })
}

fn render_code_block(code: &str, lang: &str) -> String {
    let display_lang = if lang.is_empty() { "text" } else { lang };
    let line_count = code.split('\n').count();
    let mut gutter = String::with_capacity(line_count * 4);
    for n in 1..=line_count {
        if n > 1 {
            gutter.push('\n');
        }
        gutter.push_str(&n.to_string());
    }

    let inner = match highlight_language(lang) {
        Some(syntax) => {
            let theme = theme();
            let mut hl = HighlightLines::new(syntax, theme);
            let mut out = String::with_capacity(code.len() * 2);
            for line in split_lines_with_endings(code) {
                match hl.highlight_line(line, &SYNTAXES) {
                    Ok(ranges) => {
                        for (style, text) in ranges {
                            let fg = style.foreground;
                            match token_class(fg.r, fg.g, fg.b) {
                                // 已知 token：只给类名，颜色交给 CSS（可随模式切换）
                                Some(kind) => {
                                    out.push_str("<span class=\"tk tk-");
                                    out.push_str(kind);
                                    out.push_str("\">");
                                }
                                // 主题之外的颜色（理论上不该有）保底用内联色
                                None => {
                                    out.push_str("<span style=\"color:#");
                                    push_hex(&mut out, fg.r);
                                    push_hex(&mut out, fg.g);
                                    push_hex(&mut out, fg.b);
                                    out.push_str("\">");
                                }
                            }
                            escape_html_into(text, &mut out);
                            out.push_str("</span>");
                        }
                    }
                    Err(_) => escape_html_into(line, &mut out),
                }
            }
            out
        }
        // Unknown language: plain escaped text, no hljs spans (as specified).
        None => {
            let mut out = String::with_capacity(code.len());
            escape_html_into(code, &mut out);
            out
        }
    };

    let lang_html = escape_html(display_lang);
    format!(
        "<div class=\"code\"><div class=\"bar\"><span class=\"lang\">{lang_html}</span>\
<button class=\"copy\" type=\"button\">复制</button></div><div class=\"body\"><div class=\"gutter\">{gutter}</div>\
<pre><code class=\"hljs\">{inner}</code></pre></div></div>"
    )
}

fn push_hex(out: &mut String, b: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0xf) as usize] as char);
}

/// Split like `LinesWithEndings`, but never lose a final line without a newline.
fn split_lines_with_endings(s: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            v.push(&s[start..=i]);
            start = i + 1;
        }
    }
    if start < s.len() {
        v.push(&s[start..]);
    }
    if v.is_empty() {
        v.push("");
    }
    v
}

/// 1842 -> "1,842"
fn group_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    escape_html_into(s, &mut out);
    out
}

fn escape_html_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let entity = [
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&#39;", '\''),
            ("&#x27;", '\''),
            ("&amp;", '&'),
            ("&#x2F;", '/'),
        ]
        .iter()
        .find(|(pat, _)| tail.starts_with(pat));
        match entity {
            Some((pat, ch)) => {
                out.push(*ch);
                rest = &tail[pat.len()..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ------------------------------------------------------------- URL rewriting

const URL_TAGS: [&[u8]; 5] = [b"img", b"a", b"source", b"video", b"audio"];

fn rewrite_local_urls(html: &str, base: &Path) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len() + 64);
    let mut i = 0usize;
    let mut last = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && let Some(after_name) = url_tag_name(bytes, i + 1)
            && let Some(close) = find_tag_end(bytes, after_name)
        {
            let tag = &html[i..=close];
            let new_tag = rewrite_tag_attrs(tag, base);
            out.push_str(&html[last..i]);
            out.push_str(&new_tag);
            last = close + 1;
            i = close + 1;
            continue;
        }
        i += 1;
    }
    out.push_str(&html[last..]);
    out
}

/// Returns the index just past a rewritten tag name, if `i` starts a matching tag.
fn url_tag_name(bytes: &[u8], i: usize) -> Option<usize> {
    for name in URL_TAGS {
        let end = i + name.len();
        if end <= bytes.len() && bytes[i..end].eq_ignore_ascii_case(name) {
            let boundary = bytes.get(end).copied();
            if matches!(
                boundary,
                Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'>') | Some(b'/')
            ) {
                return Some(end);
            }
        }
    }
    None
}

fn find_tag_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some(i),
                _ => {}
            },
        }
        i += 1;
    }
    None
}

fn rewrite_tag_attrs(tag: &str, base: &Path) -> String {
    let b = tag.as_bytes();
    let mut out = String::with_capacity(tag.len() + 64);
    let mut i = 0usize;
    let mut last = 0usize;
    while i < b.len() {
        let boundary = i == 0 || b[i - 1].is_ascii_whitespace();
        if boundary {
            let rest = &b[i..];
            let name_len = if rest.len() >= 3 && rest[..3].eq_ignore_ascii_case(b"src") {
                3
            } else if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(b"href") {
                4
            } else {
                0
            };
            if name_len > 0 {
                let mut j = i + name_len;
                while j < b.len() && b[j] == b' ' {
                    j += 1;
                }
                if j < b.len() && b[j] == b'=' {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                        let quote = b[j];
                        let start = j + 1;
                        if let Some(len) = b[start..].iter().position(|&c| c == quote) {
                            let end = start + len;
                            let value = &tag[start..end];
                            if let Some(new_url) = maybe_local_file_url(value, base) {
                                out.push_str(&tag[last..start]);
                                out.push_str(&new_url);
                                last = end;
                            }
                            i = end;
                            continue;
                        }
                    }
                }
            }
        }
        i += 1;
    }
    out.push_str(&tag[last..]);
    out
}

fn maybe_local_file_url(value: &str, base: &Path) -> Option<String> {
    let lower = value.trim_start().to_ascii_lowercase();
    if lower.is_empty()
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("data:")
        || lower.starts_with("file:")
        || lower.starts_with("//")
        || lower.starts_with('#')
    {
        return None;
    }
    let decoded = percent_decode(value);
    let joined = if Path::new(&decoded).is_absolute() {
        PathBuf::from(&decoded)
    } else {
        base.join(&decoded)
    };
    let normalized = normalize_path(&joined);
    Some(file_url(&normalized))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            if rest.len() >= 2 && rest.as_bytes()[1] == b':' {
                return PathBuf::from(rest);
            }
            if let Some(unc) = rest.strip_prefix(r"UNC\") {
                return PathBuf::from(format!(r"\\{unc}"));
            }
        }
    }
    p
}

fn normalize_path(p: &Path) -> String {
    let mut prefix = String::new();
    let mut parts: Vec<String> = Vec::new();
    for c in p.components() {
        match c {
            Component::Prefix(pr) => prefix = pr.as_os_str().to_string_lossy().replace('\\', "/"),
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
        }
    }
    if prefix.is_empty() {
        parts.join("/")
    } else {
        format!("{}/{}", prefix, parts.join("/"))
    }
}

fn file_url(normalized_path: &str) -> String {
    let mut s = String::from("file:///");
    for b in normalized_path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                s.push(b as char)
            }
            _ => {
                s.push('%');
                push_hex_upper(&mut s, b);
            }
        }
    }
    s
}

fn push_hex_upper(out: &mut String, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0xf) as usize] as char);
}

// ------------------------------------------------------- file association (HKCU)

fn exit_with(r: Result<(), String>) -> ! {
    match r {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            if QUIET.load(Ordering::Relaxed) {
                report_error_no_console(&e);
            } else {
                say_err!("mdv: {e}");
            }
            std::process::exit(1);
        }
    }
}

fn reg(args: &[&str]) -> Result<String, String> {
    let out = Command::new("reg")
        .args(args)
        .output()
        .map_err(|e| format!("调用 reg.exe 失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "reg {} 失败: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 读注册表某个键的默认值（HKCU 下），不存在返回 None。
/// 必须走 Win32 API：reg.exe 把 stdout 按 OEM 代码页（中文机器是 GBK）输出，
/// 用 UTF-8 解码会把中文变成 U+FFFD，再拿去 --uninstall 写回就把原值写坏了。
#[cfg(windows)]
fn reg_read_default(key: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    let sub = key.strip_prefix("HKCU\\").unwrap_or(key);
    let sub_w: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
    const MAX_CHARS: usize = 4096;
    let mut buf = vec![0u16; MAX_CHARS];
    let mut len_bytes = (MAX_CHARS * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub_w.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            &mut len_bytes,
        )
    };
    if rc != 0 {
        return None;
    }
    let chars = (len_bytes as usize / 2).saturating_sub(1).min(MAX_CHARS);
    Some(String::from_utf16_lossy(&buf[..chars]))
}

#[cfg(not(windows))]
fn reg_read_default(_key: &str) -> Option<String> {
    None
}

/// 读 HKLM 下某个子键的默认值（用于 App Paths 定位浏览器真实安装位置）。
#[cfg(windows)]
fn reg_read_hklm_default(subkey: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let sub_w: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 2048];
    let mut len_bytes = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub_w.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            &mut len_bytes,
        )
    };
    if rc != 0 {
        return None;
    }
    let chars = (len_bytes as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..chars]))
}

#[cfg(not(windows))]
fn reg_read_hklm_default(_subkey: &str) -> Option<String> {
    None
}

/// 找一个支持 --app 的 Chromium 浏览器：
/// 1) 注册表 App Paths（Edge/Chrome 装到 D 盘也能找到）
/// 2) 常见硬编码路径兜底
fn find_browser() -> Option<PathBuf> {
    for exe in BROWSER_EXES {
        let sub = format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe}");
        if let Some(path) = reg_read_hklm_default(&sub) {
            let p = PathBuf::from(path.trim().trim_matches('"'));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    BROWSER_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// 图标自愈：注册表的 DefaultIcon 指向 <LOCALAPPDATA>\mdv\mdv.ico，
/// 那个目录可能被用户/清理工具删掉，这里保证它存在（7.9 KB，缺了才写）。
fn ensure_icon() {
    let ico = work_dir().join("mdv.ico");
    let ok = fs::metadata(&ico)
        .map(|m| m.len() as usize == EMBEDDED_ICON.len())
        .unwrap_or(false);
    if ok {
        return;
    }
    if let Some(dir) = ico.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Err(e) = write_atomic(&ico, EMBEDDED_ICON) {
        say_err!("mdv: warning: 写图标失败: {e}");
    }
}

/// 安装文件关联：双击 .md -> 直接渲染并开窗，平时零进程（无托盘、无开机项）。
fn install(no_default: bool) -> Result<(), String> {
    let exe = strip_verbatim(env::current_exe().map_err(|e| format!("拿不到自身路径: {e}"))?);
    let exe_s = exe.display().to_string();

    // 图标：解到 %LOCALAPPDATA%mdvmdv.ico
    let ico = work_dir().join("mdv.ico");
    if let Some(d) = ico.parent() {
        fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
    }
    write_atomic(&ico, EMBEDDED_ICON).map_err(|e| format!("写图标失败: {e}"))?;
    let ico_s = ico.display().to_string();

    let cmd = format!("\"{exe_s}\" \"%1\"");
    reg(&[
        "add",
        KEY_PROGID,
        "/ve",
        "/t",
        "REG_SZ",
        "/d",
        "Markdown 文档 (mdv)",
        "/f",
    ])?;
    reg(&[
        "add",
        &format!("{KEY_PROGID}\\shell\\open\\command"),
        "/ve",
        "/t",
        "REG_SZ",
        "/d",
        &cmd,
        "/f",
    ])?;
    reg(&[
        "add",
        &format!("{KEY_PROGID}\\DefaultIcon"),
        "/ve",
        "/t",
        "REG_SZ",
        "/d",
        &format!("\"{ico_s}\",0"),
        "/f",
    ])?;

    // "打开方式"列表里也能看到 mdv
    reg(&["add", KEY_APP, "/ve", "/t", "REG_SZ", "/d", "mdv", "/f"])?;
    reg(&[
        "add",
        KEY_APP,
        "/v",
        "FriendlyAppName",
        "/t",
        "REG_SZ",
        "/d",
        "mdv",
        "/f",
    ])?;
    reg(&[
        "add",
        &format!("{KEY_APP}\\shell\\open\\command"),
        "/ve",
        "/t",
        "REG_SZ",
        "/d",
        &cmd,
        "/f",
    ])?;
    reg(&[
        "add",
        &format!("{KEY_APP}\\DefaultIcon"),
        "/ve",
        "/t",
        "REG_SZ",
        "/d",
        &format!("\"{ico_s}\",0"),
        "/f",
    ])?;

    // .md 的候选手势 + 记下原来的默认值（卸载时还原）
    reg(&[
        "add",
        &format!("{KEY_MD}\\OpenWithProgids"),
        "/v",
        PROGID,
        "/t",
        "REG_SZ",
        "/d",
        "",
        "/f",
    ])?;
    if let Some(prev) = reg_read_default(KEY_MD) {
        // 防御：旧版本曾用 UTF-8 解 reg.exe 的 GBK 输出，把 "(默认)" 之类写成 U+FFFD。
        // 这种值绝不能存下来，否则 --uninstall 会把 .md 的默认值写成一堆问号。
        let corrupted = prev.contains('\u{FFFD}') || prev.contains("???");
        if prev != PROGID && !prev.trim().is_empty() && !corrupted {
            reg(&[
                "add",
                KEY_STATE,
                "/v",
                "PrevMdDefault",
                "/t",
                "REG_SZ",
                "/d",
                &prev,
                "/f",
            ])?;
        } else if corrupted {
            let _ = reg(&["delete", KEY_STATE, "/v", "PrevMdDefault", "/f"]);
        }
    }

    if no_default {
        say!("mdv: 已注册（未改默认程序）。右键 .md -> 打开方式 -> 选择 mdv 即可。");
    } else {
        reg(&["add", KEY_MD, "/ve", "/t", "REG_SZ", "/d", PROGID, "/f"])?;
        say!("mdv: 已安装并设为 .md 默认程序。");
    }
    say!("     程序: {exe_s}");
    say!("     图标: {ico_s}");
    say!("     现在双击任意 .md 文件即可直接渲染（不驻留任何进程）。");
    say!("     如果双击仍然打开别的程序：设置 -> 应用 -> 默认应用 -> 按文件类型 -> .md 选 mdv。");
    say!("     撤销：mdv --uninstall");
    // 让资源管理器刷新图标/关联（失败无所谓）
    let _ = Command::new("ie4uinit.exe").arg("-show").status();
    Ok(())
}

fn uninstall() -> Result<(), String> {
    let _ = reg(&["delete", KEY_PROGID, "/f"]);
    let _ = reg(&["delete", KEY_APP, "/f"]);
    let _ = reg(&[
        "delete",
        &format!("{KEY_MD}\\OpenWithProgids"),
        "/v",
        PROGID,
        "/f",
    ]);
    if let Some(prev) = reg_read_default(KEY_STATE) {
        if prev.is_empty() {
            let _ = reg(&["delete", KEY_MD, "/ve", "/f"]);
        } else {
            let _ = reg(&["add", KEY_MD, "/ve", "/t", "REG_SZ", "/d", &prev, "/f"]);
        }
        let _ = reg(&["delete", KEY_STATE, "/v", "PrevMdDefault", "/f"]);
    } else {
        let _ = reg(&["delete", KEY_MD, "/ve", "/f"]);
    }
    let _ = Command::new("ie4uinit.exe").arg("-show").status();
    say!("mdv: 已移除文件关联（缓存目录保留，可随时手动删）。");
    Ok(())
}

fn status() -> Result<(), String> {
    let exe = strip_verbatim(env::current_exe().map_err(|e| e.to_string())?);
    say!("程序    : {}", exe.display());
    say!(
        "注册 ProgId : {}",
        match reg_read_default(KEY_PROGID) {
            Some(v) => format!("{PROGID} = {v}"),
            None => "未注册（跑 mdv --install）".to_string(),
        }
    );
    say!(
        ".md 默认 : {}",
        reg_read_default(KEY_MD).unwrap_or_else(|| "(未设置)".to_string())
    );
    if let Some(c) = reg_read_default(&format!("{KEY_PROGID}\\shell\\open\\command")) {
        say!("打开命令 : {c}");
    }
    say!("工作目录 : {}（临时文件，无缓存）", work_dir().display());
    match find_browser() {
        Some(b) => say!("浏览器   : {}（--app 模式）", b.display()),
        None => say!("浏览器   : 未找到 Chromium 系浏览器，将退回系统默认浏览器（无 app 模式）"),
    }
    say!(
        "默认配色 : {}（页面上右上角可切换 正常/灰度/暗色）",
        if gray_mode() {
            "灰度（MDV_GRAY=1）"
        } else {
            "正常"
        }
    );
    say!(
        "位置记忆 : {}",
        if env::var("MDV_RESTORE").map(|v| v == "0").unwrap_or(false) {
            "已关闭（MDV_RESTORE=0）"
        } else {
            "开启（状态存在浏览器 localStorage，不在 mdv 目录）"
        }
    );
    Ok(())
}

// ------------------------------------------------------------ browser-side math
//
// 交付物纯净策略：KaTeX 的 JS/CSS/字体全部在编译期进二进制（build.rs 生成字体表），
// 运行时不再读 node_modules 或任何项目文件；字体第一次运行解到 %LOCALAPPDATA%\mdv\assets，
// 之后直接命中。所以 mdv.exe 拷到任何机器上都能独立工作。

include!(concat!(env!("OUT_DIR"), "/fonts_table.rs"));

const KATEX_CSS: &str = include_str!("../../assets/katex/katex.min.css");
const KATEX_JS: &str = include_str!("../../assets/katex/katex.min.js");
const KATEX_VERSION: &str = "0.16.47";

/// 原子写：先写临时文件再 rename，避免断电/被杀后留下半截文件被当成有效缓存。
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// 把内嵌字体解到缓存目录（缺一个就补一个，长度不符视为损坏），返回 fonts 目录的 file:// URL 前缀。
fn ensure_katex_fonts() -> Result<String, String> {
    let dir = work_dir()
        .join("assets")
        .join(format!("katex-{KATEX_VERSION}"))
        .join("fonts");
    // 完整性用"长度是否等于内嵌字节数"判定：半截文件会被发现并重写
    let damaged = |target: &Path, len: usize| {
        fs::metadata(target)
            .map(|m| m.len() as usize != len)
            .unwrap_or(true)
    };
    if KATEX_FONTS
        .iter()
        .any(|(name, bytes)| damaged(&dir.join(name), bytes.len()))
    {
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        for (name, bytes) in KATEX_FONTS {
            let target = dir.join(name);
            if damaged(&target, bytes.len()) {
                write_atomic(&target, bytes)
                    .map_err(|e| format!("write {}: {e}", target.display()))?;
            }
        }
    }
    Ok(file_url(&normalize_path(&dir)))
}

/// KaTeX 资产（编译期内嵌）解到共享目录，返回该目录的 file:// URL。
/// 页面用 <link>/<script> 引用它，缓存 HTML 因此只有几 KB，而不是每篇都塞 300 KB。
/// 目录结构：katex-0.16.47/{katex.min.css, katex.min.js, fonts/*.woff2}
/// CSS 里的 url(fonts/…) 是相对路径，正好指向同目录的 fonts/，不需要改写。
fn katex_assets() -> Option<String> {
    let dir = work_dir()
        .join("assets")
        .join(format!("katex-{KATEX_VERSION}"));
    let _ = ensure_katex_fonts();
    let files: [(&str, &str); 2] = [("katex.min.css", KATEX_CSS), ("katex.min.js", KATEX_JS)];
    let damaged = |p: &Path, len: usize| {
        fs::metadata(p)
            .map(|m| m.len() as usize != len)
            .unwrap_or(true)
    };
    if files.iter().any(|(n, c)| damaged(&dir.join(n), c.len())) {
        fs::create_dir_all(&dir).ok()?;
        for (name, content) in files {
            let target = dir.join(name);
            if damaged(&target, content.len())
                && let Err(e) = write_atomic(&target, content.as_bytes())
            {
                say_err!(
                    "mdv: warning: 写 {} 失败（公式可能无法渲染）: {e}",
                    target.display()
                );
                return None;
            }
        }
    }
    Some(file_url(&normalize_path(&dir)))
}

/// 滚动位置记忆：完全在页面侧完成（localStorage，origin=file://）。
/// - mdv.exe 不写任何文件、不起任何进程；
/// - 状态存在浏览器自己的 profile 里（几 KB），可被"清除浏览数据"清掉；
/// - MDV_RESTORE=0 时整段不注入，恢复零状态。
const POS_SCRIPT: &str = r#"<script>
(function () {
  var KEY = "mdv.pos", DOC = __MDV_DOC__;
  var ls;
  try { ls = window.localStorage; } catch (e) { return; }
  if (!ls) return;
  function load() {
    try { return JSON.parse(ls.getItem(KEY) || "") || { v: 1, docs: {} }; }
    catch (e) { return { v: 1, docs: {} }; }
  }
  var db = load();
  if (!db.docs) db.docs = {};
  var rec = db.docs[DOC] || null;

  window.__mdvApplyPos = function () {
    if (!rec) return;
    var h = document.documentElement.scrollHeight || 1;
    var y = rec.y || 0;
    // 文档高度变化超过 25% 时按比例换算，避免改过文档后跳到奇怪位置
    if (rec.h > 0 && Math.abs(h - rec.h) / rec.h > 0.25) y = Math.round(rec.y * h / rec.h);
    window.scrollTo(0, Math.max(0, Math.min(y, h - window.innerHeight)));
  };
  window.__mdvApplyPos();

  var timer = null;
  function save() {
    var h = document.documentElement.scrollHeight;
    var y = window.scrollY || window.pageYOffset || 0;
    if (y < 40) { delete db.docs[DOC]; }
    else { db.docs[DOC] = { y: Math.round(y), h: h, ts: Date.now() }; }
    var ks = Object.keys(db.docs);
    if (ks.length > 200) {
      ks.sort(function (a, b) { return (db.docs[a].ts || 0) - (db.docs[b].ts || 0); });
      for (var i = 0; i < ks.length - 200; i++) delete db.docs[ks[i]];
    }
    try { ls.setItem(KEY, JSON.stringify(db)); } catch (e) {}
  }
  window.addEventListener("scroll", function () {
    if (timer) clearTimeout(timer);
    timer = setTimeout(save, 400);
  }, { passive: true });
  window.addEventListener("beforeunload", save);
})();
</script>
"#;

/// 把字符串变成安全的 JS 字面量（路径里可能有反斜杠、引号、</script>）。
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 三档配色切换：正常（彩色，跟随系统）/ 灰度 / 暗色。
/// 选择存在 localStorage（和滚动位置一样，不产生任何 mdv 侧文件）。
const MODE_HEAD_SCRIPT: &str = r#"<script>
(function () {
  var m = null;
  try { m = localStorage.getItem("mdv.mode"); } catch (e) {}
  // 只有用户明确选过才覆盖；否则保留服务端给的初始 class
  // （MDV_GRAY=1 时是 mode-gray），这样"环境变量设了灰度"不会被脚本清掉
  if (m === "gray") document.documentElement.className = "mode-gray";
  else if (m === "dark") document.documentElement.className = "mode-dark";
  else if (m === "normal") document.documentElement.className = "";
})();
</script>
"#;

const MODE_BUTTONS: &str = r#"<div class="modes" role="group" aria-label="配色模式">
<button type="button" data-mode="normal">正常</button>
<button type="button" data-mode="gray">灰度</button>
<button type="button" data-mode="dark">暗色</button>
</div>
<script>
(function () {
  var buttons = document.querySelectorAll(".modes button");
  function current() {
    var c = document.documentElement.className;
    return c.indexOf("mode-gray") >= 0 ? "gray" : (c.indexOf("mode-dark") >= 0 ? "dark" : "normal");
  }
  function paint() {
    var cur = current();
    buttons.forEach(function (b) {
      b.setAttribute("aria-pressed", String(b.getAttribute("data-mode") === cur));
    });
  }
  buttons.forEach(function (b) {
    b.addEventListener("click", function () {
      var m = b.getAttribute("data-mode");
      document.documentElement.className = m === "gray" ? "mode-gray" : (m === "dark" ? "mode-dark" : "");
      try { localStorage.setItem("mdv.mode", m); } catch (e) {}
      paint();
    });
  });
  paint();
})();
</script>
"#;

const KATEX_INIT: &str = r#"document.querySelectorAll("[data-math-style]").forEach((el) => {
  const display = el.getAttribute("data-math-style") === "display";
  const tex = el.textContent;                     // textContent 自动还原 &amp; 等实体
  if (display) el.style.display = "block";
  try { katex.render(tex.trim(), el, { displayMode: display, throwOnError: false, output: "html" }); }
  catch (e) { console.error("katex:", e); }
});"#;

// ------------------------------------------------------------------- document

/// 组装页面时需要的零件（参数太多，收成一个结构体）
struct PageParts<'a> {
    katex_base: Option<&'a String>,
    pos_script: Option<&'a str>,
    body: &'a str,
    kb: f64,
    chars: usize,
    render_ms: f64,
}

fn build_document(title: &str, css: &str, p: PageParts<'_>) -> String {
    let mut out = String::with_capacity(css.len() + p.body.len() + 2048);
    out.push_str("<!doctype html>\n<html lang=\"zh-CN\"");
    if gray_mode() {
        out.push_str(" class=\"mode-gray\"");
    }
    out.push_str("><head><meta charset=\"utf-8\">");
    out.push_str(MODE_HEAD_SCRIPT);
    out.push_str("<title>");
    escape_html_into(title, &mut out);
    out.push_str("</title>\n<meta name=\"color-scheme\" content=\"light dark\">\n");
    out.push_str("<link rel=\"icon\" href=\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='7' fill='%232f6f4f'/%3E%3Ctext x='16' y='23' font-size='17' font-family='Consolas' fill='%23fff' text-anchor='middle'%3EM%3C/text%3E%3C/svg%3E\">\n");
    // KaTeX 的样式/脚本走外部共享文件（每篇缓存 HTML 因此只有几 KB）
    if let Some(base) = p.katex_base {
        out.push_str("<link rel=\"stylesheet\" href=\"");
        out.push_str(base);
        out.push_str("/katex.min.css\">\n");
    }
    out.push_str("<style>");
    out.push_str(css);
    out.push_str("</style>\n</head><body>\n");
    out.push_str(MODE_BUTTONS);
    out.push_str("<main>\n");
    out.push_str(p.body);
    // 页脚做成信息条：左边文件名，右边元信息
    out.push_str("\n<p class=\"docfoot\"><span class=\"doc\">");
    escape_html_into(title, &mut out);
    out.push_str("</span><span class=\"meta\">");
    out.push_str(&format!(
        "{:.1} KB · {} 字 · 只读 · 渲染 {:.0} ms",
        p.kb,
        group_thousands(p.chars),
        p.render_ms
    ));
    out.push_str("</span></p>\n</main>\n");
    if let Some(base) = p.katex_base {
        out.push_str("<script src=\"");
        out.push_str(base);
        out.push_str("/katex.min.js\"></script>\n<script>\n");
        out.push_str(KATEX_INIT);
        out.push_str("\n</script>\n");
    }
    out.push_str("<script>\n");
    out.push_str(COPY_SCRIPT);
    out.push_str("</script>\n");
    // 滚动位置恢复放在最后：此时 KaTeX 已渲染完，文档高度是最终高度
    if let Some(js) = p.pos_script {
        out.push_str(js);
        out.push('\n');
    }
    out.push_str("</p.body></html>\n");
    out
}

const COPY_SCRIPT: &str = r#"document.querySelectorAll(".copy").forEach((b) => b.addEventListener("click", async () => {
  const code = b.closest(".code").querySelector("code").innerText;
  try { await navigator.clipboard.writeText(code); b.textContent = "已复制"; }
  catch { b.textContent = "复制失败"; }
  setTimeout(() => (b.textContent = "复制"), 1200);
}));
"#;

// ------------------------------------------------------------------- work dir

/// 工作目录：%TEMP%\mdv。**不是缓存** —— 每次打开都重渲染，
/// 里面只有本次的临时 HTML 和一次性解压的资产（资产靠长度自愈，删了会重写）。
fn work_dir() -> PathBuf {
    env::temp_dir().join("mdv")
}

/// 清掉上一次运行留下的临时 HTML。留 5 秒余量：Edge 从冷启动到读完页面最多 ~650 ms，
/// 5 秒足够安全，同时又不会让临时文件堆着。
fn prune_work(work: &Path) {
    let Ok(read) = fs::read_dir(work) else {
        return;
    };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(5);
    for e in read.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("view-") && name.ends_with(".html")) {
            continue;
        }
        if let Ok(meta) = e.metadata()
            && meta.modified().map(|t| t < cutoff).unwrap_or(false)
        {
            let _ = fs::remove_file(e.path());
        }
    }
}

// -------------------------------------------------------------------- launch

/// headless Edge 截图：替代 tools/shot.mjs，让交付物不需要 Node。
fn screenshot(html: &Path, out: &Path, (w, h): (u32, u32), dark: bool) -> Result<(), String> {
    let edge = find_browser().ok_or_else(|| {
        "找不到 Chromium 系浏览器（--shot 需要 Edge/Chrome 的 headless 模式）".to_string()
    })?;
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    // Edge 的 --screenshot 相对路径按它自己的 cwd 解析，必须给绝对路径
    let base = match out.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(d) => d.to_path_buf(),
        None => env::current_dir().map_err(|e| format!("cwd 不可用: {e}"))?,
    };
    let out_abs = fs::canonicalize(&base)
        .map(strip_verbatim)
        .unwrap_or(base)
        .join(out.file_name().unwrap_or_default());
    // 每次用独立 profile：并发调用不会互相抢锁，用完删掉（不再往 %TEMP% 堆 70 MB）
    let profile = env::temp_dir().join(format!("mdv-shot-{}", std::process::id()));
    let mut cmd = Command::new(edge);
    cmd.arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--hide-scrollbars")
        .arg("--no-first-run")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--screenshot={}", out_abs.display()))
        .arg(format!("--window-size={w},{h}"))
        .arg("--virtual-time-budget=5000");
    if dark {
        cmd.arg("--force-dark-mode")
            .arg("--blink-settings=preferredColorScheme=0");
    }
    let mut child = cmd
        .arg(file_url(&normalize_path(html)))
        .spawn()
        .map_err(|e| format!("启动 Edge 截图失败: {e}"))?;

    // 必须有超时：profile 被占用 / Edge 卡死时不能无限等
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        match child
            .try_wait()
            .map_err(|e| format!("等待 Edge 失败: {e}"))?
        {
            Some(st) => break Some(st),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(40)),
        }
    };
    let _ = fs::remove_dir_all(&profile);
    if !out_abs.is_file() {
        return Err(match status {
            Some(st) => format!(
                "截图没有生成（Edge 退出码 {}）: {}",
                st.code().unwrap_or(-1),
                out_abs.display()
            ),
            None => format!("截图超时（20 s），已终止 Edge: {}", out_abs.display()),
        });
    }
    Ok(())
}

fn launch(out_path: &Path, (w, h): (u32, u32)) -> Result<(), String> {
    let url = file_url(&normalize_path(out_path));
    if let Some(edge) = find_browser() {
        let mut cmd = Command::new(&edge);
        cmd.arg(format!("--app={url}"))
            .arg(format!("--window-size={w},{h}"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            cmd.creation_flags(DETACHED_PROCESS);
        }
        cmd.spawn()
            .map_err(|e| format!("failed to launch {}: {e}", edge.display()))?;
    } else {
        // 找不到任何 Chromium 浏览器：退回系统默认浏览器。
        // 代价是失去 --app（会有地址栏和标签页），但内容照样显示。
        say_err!(
            "mdv: 提示：没找到 Edge/Chrome 等 Chromium 浏览器，将用默认浏览器打开（无 app 模式）"
        );
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "start", "", &url]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            cmd.creation_flags(DETACHED_PROCESS);
        }
        cmd.spawn()
            .map_err(|e| format!("failed to launch browser via cmd: {e}"))?;
    }
    Ok(())
}

// -------------------------------------------------------------------- helpers

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
