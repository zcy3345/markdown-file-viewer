//! 把 KaTeX 的数学字体编译进二进制：生成 OUT_DIR/fonts_table.rs。
//! 这样交付物只是一个 exe，运行时不需要 node_modules 或任何外部字体文件。
//!
//! 中文字体不内嵌：正文走系统字体栈（微软雅黑 / system-ui）。
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let katex_dir = Path::new(&manifest).join("../assets/katex/fonts");
    println!("cargo:rerun-if-changed=../assets/katex");

    let mut names: Vec<String> = fs::read_dir(&katex_dir)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {e}", katex_dir.display()))
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".woff2"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "assets/katex/fonts 里没有 woff2");

    let mut out = String::new();
    writeln!(out, "pub static KATEX_FONTS: &[(&str, &[u8])] = &[").unwrap();
    for n in &names {
        writeln!(
            out,
            "    ({n:?}, include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../assets/katex/fonts/{n}\"))),"
        )
        .unwrap();
    }
    writeln!(out, "];").unwrap();

    let dst = Path::new(&env::var("OUT_DIR").unwrap()).join("fonts_table.rs");
    fs::write(&dst, out).unwrap();
}
