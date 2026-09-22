//! 编辑页面的静态自检。
//!
//! 页面是单文件 HTML、脚本内联，`cargo test` 没法执行它。像「调用了已经被删掉的本地函数」
//! 这种断链只能靠人工点浏览器发现，所以这里加一层**窄范围**检查：把脚本里形如
//! `name(` 的标识符和已声明的名字做差集。它已经拦下过一次真实回归（URL 输入里残留
//! 调用被删掉的 `updatePasswordState`）。
//!
//! 明确的边界，不要把它当成完整覆盖：
//!
//! - 只认 `name(` 形式的直接调用；`list.map(removedFn)`、`setTimeout(removedFn)` 这类
//!   把函数当值传递的用法抓不到；
//! - 不做作用域、字符串、模板文本、正则字面量的真正解析，靠正则前缀排除来减少误报，
//!   因此仍可能出现误报/漏报；
//! - 白名单只放关键字和 JS/浏览器内置，遇到误报应当修源码写法或改前缀规则，
//!   不要继续往白名单里塞项目自己的名字，否则检查会退化成空跑。
//!
//! 想要真正覆盖页面行为，需要真实 JS 引擎或浏览器 smoke 测试，那是另一件事。

use regex::Regex;
use std::collections::HashSet;

/// 关键字和宿主/内置对象：这些出现在 `(` 前面不代表调用了本地函数。
const KNOWN_CALLEES: &[&str] = &[
    // 关键字
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "return",
    "typeof",
    "delete",
    "in",
    "of",
    "instanceof",
    "void",
    "yield",
    "case",
    "else",
    "do",
    "await",
    "new",
    "function",
    // JS 内置
    "Array",
    "Boolean",
    "Date",
    "Error",
    "JSON",
    "Map",
    "Math",
    "Number",
    "Object",
    "Promise",
    "RegExp",
    "Set",
    "String",
    "Symbol",
    "Uint8Array",
    "Uint16Array",
    "Uint32Array",
    "WeakMap",
    "BigInt",
    "decodeURIComponent",
    "encodeURIComponent",
    "isFinite",
    "isNaN",
    "parseFloat",
    "parseInt",
    "structuredClone",
    "queueMicrotask",
    "Intl",
    // 浏览器宿主
    "Blob",
    "CustomEvent",
    "Event",
    "FileReader",
    "File",
    "FormData",
    "MutationObserver",
    "ResizeObserver",
    "TextDecoder",
    "TextEncoder",
    "URL",
    "URLSearchParams",
    "clearInterval",
    "clearTimeout",
    "console",
    "document",
    "fetch",
    "history",
    "location",
    "matchMedia",
    "navigator",
    "requestAnimationFrame",
    "cancelAnimationFrame",
    "screen",
    "setInterval",
    "setTimeout",
    "window",
    "alert",
    "confirm",
    "prompt",
    "getComputedStyle",
    // 形如 `async (x) => {}` 和 CSS 里的 var(--x)，都不是 JS 函数调用
    "async",
    "var",
];

/// 抽出页面里唯一的 `<script>` 内容。
pub fn extract_script(html: &str) -> Option<String> {
    let start = html.find("<script>")? + "<script>".len();
    let end = html[start..].find("</script>")? + start;
    Some(html[start..end].to_string())
}

/// 收集 `function foo` / `const foo =` / 解构声明等所有局部名字。
fn declared_names(script: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let function_re = Regex::new(r"function\s+([A-Za-z_$][\w$]*)").expect("函数声明正则必须合法");
    for capture in function_re.captures_iter(script) {
        names.insert(capture[1].to_string());
    }
    let binding_re =
        Regex::new(r"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)").expect("变量声明正则必须合法");
    for capture in binding_re.captures_iter(script) {
        names.insert(capture[1].to_string());
    }
    let destructure_re =
        Regex::new(r"\b(?:const|let|var)\s*[\[{]([^\]}]*)[\]}]").expect("解构正则必须合法");
    let identifier_re = Regex::new(r"^[A-Za-z_$][\w$]*$").expect("标识符正则必须合法");
    for capture in destructure_re.captures_iter(script) {
        for part in capture[1].split(',') {
            let name = part.split(':').next().unwrap_or("").trim();
            if identifier_re.is_match(name) {
                names.insert(name.to_string());
            }
        }
    }
    names
}

/// 返回「被调用但没有声明、也不在已知集合里」的名字，按字母序去重。
pub fn undefined_calls(script: &str) -> Vec<String> {
    // 先把函数声明改成普通标识符，避免声明本身被当成调用
    let declaration_re =
        Regex::new(r"function\s+([A-Za-z_$][\w$]*)").expect("函数声明正则必须合法");
    let body = declaration_re.replace_all(script, "function ").to_string();

    let declared = declared_names(script);
    let known: HashSet<&str> = KNOWN_CALLEES.iter().copied().collect();
    // 前缀排除 `.`（方法调用）、`\`（正则字面量里的 \b( ）和单词字符
    let call_re = Regex::new(r"(?:^|[^.\w$\\])([A-Za-z_$][\w$]*)\s*\(").expect("调用正则必须合法");
    let mut missing = HashSet::new();
    for capture in call_re.captures_iter(&body) {
        let name = capture[1].to_string();
        if declared.contains(&name) || known.contains(name.as_str()) {
            continue;
        }
        missing.insert(name);
    }
    let mut result: Vec<String> = missing.into_iter().collect();
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_call_to_removed_function() {
        let script = "function a() { b(); } function b() { return 1; }";
        assert!(undefined_calls(script).is_empty());
        let broken = "function a() { removedHelper(); }";
        assert_eq!(undefined_calls(broken), vec!["removedHelper".to_string()]);
    }

    #[test]
    fn ignores_methods_builtins_and_keywords() {
        let script = r#"
            function run() {
              if (value) { document.title = String(Number(value)); }
              items.forEach((item) => item.render());
              window.setTimeout(run, 10);
            }
            const value = 1;
            const items = [];
        "#;
        assert!(
            undefined_calls(script).is_empty(),
            "{:?}",
            undefined_calls(script)
        );
    }
}
