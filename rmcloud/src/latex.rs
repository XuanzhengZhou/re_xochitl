//! 回答 → LaTeX → PDF。S1（`docs/AI-REFS.md` §4）。
//!
//! 设备上不可能跑 LaTeX，所以**回答的纸面形态**由服务器决定：模型给的是
//! 「中文正文 + `$...$` 公式」，这里把文字段转义、公式原样留下，套一份固定
//! 导言区，直接 `xelatex`。
//!
//! 为什么不走 markdown/pandoc：用户明确要求直接 LaTeX；而且 pandoc 那条路
//! 多一层（md → tex）就多一层出错的地方，我们只需要"段落 + 公式 + 粗体"。
//!
//! # 三条硬规矩
//!
//! 1. **文字段必须转义**。模型很爱在正文里写 `100%`、`x_1`、`a&b`、`#3`，
//!    不转义就是编译错误（`%` 还会把后半行注释掉）。
//! 2. **公式段一个字节都不动**。`$...$` 里出现 `_`、`^`、`&`（对齐环境）都是
//!    合法的 LaTeX，转义反而坏事。
//! 3. **编译失败要降级**：把 `$...$` 也当普通文字（公式不解释）再编一次。
//!    再失败就返回 `Err`，绝不写半份 PDF —— 设备那边拿不到 PDF 就只留一行提示。
//!
//! # 页尺寸
//!
//! rM2 屏 1404×1872 px @226 dpi ≈ 157.8×210.4 mm（3:4）。照这个开纸，
//! pdfium 在设备上 `contain` 进来正好铺满，不留黑边。

use std::path::Path;
use std::time::Duration;

/// `xelatex` 单次编译上限。正常一页 1~3 s；字体缓存冷启动最坏 ~20 s。
const COMPILE_TIMEOUT: Duration = Duration::from_secs(120);

/// 正文里要转义的字符 → LaTeX。
///
/// 键是**文字段**里的字符；`$` 也在这里（未配对的 `$` 落到文字段时要变成 `\$`）。
pub fn escape_text(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '\\' => o.push_str(r"\textbackslash{}"),
            '{' => o.push_str(r"\{"),
            '}' => o.push_str(r"\}"),
            '$' => o.push_str(r"\$"),
            '&' => o.push_str(r"\&"),
            '%' => o.push_str(r"\%"),
            '#' => o.push_str(r"\#"),
            '_' => o.push_str(r"\_"),
            '~' => o.push_str(r"\textasciitilde{}"),
            '^' => o.push_str(r"\textasciicircum{}"),
            _ => o.push(c),
        }
    }
    o
}

/// 一行里的片段：文字 / 行内公式 / 行间公式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Text(String),
    Math(String),
    Display(String),
}

/// 把一行切成「文字 / 公式」。`$...$` 行内，`$$...$$` 行间；未配对的 `$`
/// 留在文字段里（模型流式中途会写出半个公式，这里只是兜底）。
pub fn split_line(s: &str) -> Vec<Seg> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut rest = s;
    loop {
        let Some(i) = rest.find('$') else {
            buf.push_str(rest);
            break;
        };
        buf.push_str(&rest[..i]);
        let after = &rest[i..];
        // `$$…$$` 先试行间
        if let Some(body) = after.strip_prefix("$$") {
            if let Some(j) = body.find("$$") {
                if !buf.is_empty() {
                    out.push(Seg::Text(std::mem::take(&mut buf)));
                }
                out.push(Seg::Display(body[..j].to_string()));
                rest = &body[j + 2..];
                continue;
            }
        }
        if let Some(body) = after.strip_prefix('$') {
            if let Some(j) = body.find('$') {
                if !buf.is_empty() {
                    out.push(Seg::Text(std::mem::take(&mut buf)));
                }
                out.push(Seg::Math(body[..j].to_string()));
                rest = &body[j + 1..];
                continue;
            }
        }
        // 没配上：这个 `$` 当普通字符
        buf.push('$');
        rest = &after[1..];
    }
    if !buf.is_empty() {
        out.push(Seg::Text(buf));
    }
    out
}

/// 一行 → LaTeX（文字转义、公式原样）。
pub fn render_line(line: &str, keep_math: bool) -> String {
    let mut o = String::new();
    for seg in split_line(line) {
        match seg {
            Seg::Text(t) => o.push_str(&escape_text(&t)),
            Seg::Math(m) if keep_math => {
                o.push('$');
                o.push_str(&m);
                o.push('$');
            }
            Seg::Display(m) if keep_math => {
                o.push_str("\\[");
                o.push_str(&m);
                o.push_str("\\]");
            }
            // 降级档：公式当普通文字
            Seg::Math(m) => o.push_str(&escape_text(&format!("${m}$"))),
            Seg::Display(m) => o.push_str(&escape_text(&format!("$${m}$$"))),
        }
    }
    o
}

/// 回答正文 → LaTeX 主体（不含导言区）。
///
/// 一行一个段落：模型的分行本身就是结构（`1.` / `- ` / 独立公式），合并成
/// 一段反而看不清。段间用空行，不写 `\\`（行尾 `\\` 在段末会报错）。
pub fn body(answer: &str, keep_math: bool) -> String {
    let mut o = String::new();
    for line in strip_document(answer).lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let r = render_line(line, keep_math);
        if r.trim().is_empty() {
            continue;
        }
        o.push_str(&r);
        o.push_str("\n\n");
    }
    o
}

/// 模型如果直接吐了一份完整文档，只要 `document` 环境里的东西。
pub fn strip_document(s: &str) -> String {
    let Some(i) = s.find("\\begin{document}") else {
        return s.to_string();
    };
    let after = &s[i + "\\begin{document}".len()..];
    match after.find("\\end{document}") {
        Some(j) => after[..j].to_string(),
        None => after.to_string(),
    }
}

/// 固定导言区。`ctexart` 自带中文支持（xeCJK + 断行），字号 11pt。
pub fn preamble() -> &'static str {
    r"\documentclass[11pt]{ctexart}
\usepackage[paperwidth=157.8mm,paperheight=210.4mm,margin=12mm,top=14mm,bottom=16mm]{geometry}
\usepackage{amsmath,amssymb,graphicx}
\usepackage{xcolor}
\pagestyle{plain}
\setlength{\parindent}{0pt}
\setlength{\parskip}{3pt}
\linespread{1.15}
\begin{document}
"
}

/// 问题只留一行，太长截断（避免一个超长问题把第一页塞满）。
pub fn question_line(q: &str) -> String {
    let q: String = q
        .trim()
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    let q = if q.chars().count() > 120 {
        let mut s: String = q.chars().take(120).collect();
        s.push('…');
        s
    } else {
        q
    };
    format!(
        "\\textbf{{问题：}}{}\n\n\\noindent\\rule{{\\linewidth}}{{0.4pt}}\n\n\\vspace{{2pt}}\n",
        escape_text(&q)
    )
}

/// 整份 `.tex`：问题 + 分隔线 + 回答。
pub fn document(question: &str, answer: &str) -> String {
    document_opt(question, answer, true)
}

/// `keep_math=false` 是编译失败后的降级档（公式不解释，原样显示）。
pub fn document_opt(question: &str, answer: &str, keep_math: bool) -> String {
    let mut s = String::from(preamble());
    if !question.trim().is_empty() {
        s.push_str(&question_line(question));
    }
    s.push_str(&body(answer, keep_math));
    s.push_str("\\end{document}\n");
    s
}

/// 在 `dir` 里编译 `tex`，产物必须是 `dir/a.pdf`。
///
/// 编译失败时调用方负责换降级档重试（[`document_opt`]）。
pub async fn compile(dir: &Path, tex: &str) -> Result<std::path::PathBuf, String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("建目录失败：{e}"))?;
    let src = dir.join("a.tex");
    let pdf = dir.join("a.pdf");
    let _ = tokio::fs::remove_file(&pdf).await;
    tokio::fs::write(&src, tex)
        .await
        .map_err(|e| format!("写 tex 失败：{e}"))?;

    let mut cmd = tokio::process::Command::new("xelatex");
    cmd.arg("-interaction=nonstopmode")
        .arg("-halt-on-error")
        .arg("-no-shell-escape")
        .arg("a.tex")
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(COMPILE_TIMEOUT, cmd.output()).await {
        Err(_) => return Err("xelatex 超时（120s）".into()),
        Ok(Err(e)) => return Err(format!("跑不了 xelatex：{e}")),
        Ok(Ok(o)) => o,
    };
    if !out.status.success() || !pdf.exists() {
        let log = String::from_utf8_lossy(&out.stdout);
        // 只要第一个 `! ` 开头的错误行，整份日志几十 KB。
        let first = log
            .lines()
            .find(|l| l.trim_start().starts_with('!'))
            .unwrap_or("")
            .trim()
            .to_string();
        return Err(format!(
            "xelatex 失败（{}）：{}",
            out.status.code().unwrap_or(-1),
            if first.is_empty() {
                "没有 PDF 产物".to_string()
            } else {
                first
            }
        ));
    }
    Ok(pdf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_text_specials() {
        assert_eq!(escape_text("100% & #3 a_b"), r"100\% \& \#3 a\_b");
        assert_eq!(escape_text("$5"), r"\$5");
        assert_eq!(escape_text("{x}"), r"\{x\}");
        assert_eq!(escape_text(r"a\b"), r"a\textbackslash{}b");
        assert_eq!(escape_text("^~"), r"\textasciicircum{}\textasciitilde{}");
        // 中文/标点原样
        assert_eq!(escape_text("中文，标点。"), "中文，标点。");
    }

    #[test]
    fn splits_inline_and_display_math() {
        assert_eq!(
            split_line("设 $x^2$ 为"),
            vec![
                Seg::Text("设 ".into()),
                Seg::Math("x^2".into()),
                Seg::Text(" 为".into())
            ]
        );
        assert_eq!(
            split_line("$$\\int_0^1$$"),
            vec![Seg::Display("\\int_0^1".into())]
        );
        // 未配对：当普通文字
        assert_eq!(
            split_line("价格 $5 元"),
            vec![Seg::Text("价格 $5 元".into())]
        );
    }

    #[test]
    fn renders_math_without_escaping() {
        assert_eq!(
            render_line("值 $\\frac{a_b}{2}$ 是", true),
            r"值 $\frac{a_b}{2}$ 是"
        );
        // 降级档：公式不解释
        assert_eq!(
            render_line("值 $x_1$ 是", false),
            r"值 \$x\_1\$ 是"
        );
    }

    #[test]
    fn body_keeps_line_structure() {
        let b = body("第一行\n\n第二行 & 三", true);
        assert_eq!(b, "第一行\n\n第二行 \\& 三\n\n");
    }

    #[test]
    fn strips_whole_document() {
        let s = "前言\n\\begin{document}\n正文 $x$\n\\end{document}\n后记";
        assert_eq!(strip_document(s).trim(), "正文 $x$");
    }

    #[test]
    fn question_is_escaped_and_cut() {
        let q = question_line("100% 对吗");
        assert!(q.contains(r"100\% 对吗"));
        let long = "字".repeat(200);
        let q = question_line(&long);
        assert!(q.contains('…'));
    }
}
