//! 把编辑页面的源码片段拼成单文件 HTML。
//!
//! 页面按 store / api / render / actions / boot 拆成多个文件方便维护，
//! 但对外交付的必须仍然是**单个纯 HTML**（编辑服务直接内嵌这一份，
//! 不引入任何构建期 JS 工具链）。所以在 build 阶段用纯 Rust 拼回来，
//! 产物写到 `OUT_DIR/editor.html`，由 `include_str!` 内嵌进二进制。

use std::fs;
use std::path::PathBuf;

/// 拼接顺序即最终文件顺序，新增片段时改这里。
const PARTS: &[&str] = &[
    "head.html",
    "core.js",
    "api.js",
    "render.js",
    "actions.js",
    "boot.js",
    "tail.html",
];

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("rust/assets/editor");
    let mut page = String::new();
    for part in PARTS {
        let path = root.join(part);
        println!("cargo:rerun-if-changed={}", path.display());
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("读取页面片段 {} 失败: {error}", path.display()));
        page.push_str(&text);
    }

    let out_dir = std::env::var("OUT_DIR").expect("缺少 OUT_DIR 环境变量");
    let target = PathBuf::from(out_dir).join("editor.html");
    fs::write(&target, page)
        .unwrap_or_else(|error| panic!("写入 {} 失败: {error}", target.display()));
    println!("cargo:rerun-if-changed=build.rs");
}
