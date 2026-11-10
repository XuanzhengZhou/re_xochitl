//! AI 任务（Job status）：`~/.local/share/rmdev/ai-jobs.json`。
//!
//! 用户 2026-11-09：难题问出去之后可以**把卡片关掉**，任务继续在服务器上跑；
//! 跑完服务器自己发布 PDF，设备 sync 就把回答和引用都拿到手，回头再看。
//! 那么"关掉之后还剩哪些在跑、哪些失败了"必须有个地方能看到 —— 就是这个列表。
//!
//! 一行只需要两样东西：状态 + 来源文档（用户明确说的"只显示列表和来源文件"）。
//! 失败的可以 `Retry`，而且**沿用同一个 `ai` uuid**（回答文档 id 不换，
//! 引用索引里那条 `ai` 也就不会变成孤儿）。
//!
//! 这个文件是设备自己的状态，服务器不读它；坏了就整份丢掉（宁可丢一个列表，
//! 也不能让坏 JSON 挡住提问）。

use std::path::PathBuf;

/// 一条任务。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Job {
    /// 回答文档 uuid，同时也是任务 id（Retry 沿用）。
    pub ai: String,
    /// 源文档 uuid。
    pub doc: String,
    /// 源文档的可见名（列表里显示这个，不用再去读 metadata）。
    pub doc_name: String,
    /// 原问题；整页提问可能是空串。
    pub question: String,
    /// 提问所在页（1 基，和请求体一致）。
    pub page: u32,
    /// 框选区域（设备像素）；整页提问是 `None`。
    pub rect: Option<[f64; 4]>,
    /// `running` / `done` / `failed`。
    pub status: String,
    /// 开始时间（毫秒）。
    pub at: u64,
}

pub const RUNNING: &str = "running";
pub const DONE: &str = "done";
pub const FAILED: &str = "failed";

/// 任务表（新的在前）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Jobs {
    pub items: Vec<Job>,
}

fn path() -> PathBuf {
    if let Ok(p) = std::env::var("RMDEV_AI_JOBS") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/root".into());
    PathBuf::from(home).join(".local/share/rmdev/ai-jobs.json")
}

impl Jobs {
    /// 读盘。没有/坏了都返回空表。
    pub fn load() -> Jobs {
        match std::fs::read(path()) {
            Ok(b) => Jobs::parse(&b),
            Err(_) => Jobs::default(),
        }
    }

    pub fn parse(b: &[u8]) -> Jobs {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(b) else {
            return Jobs::default();
        };
        let mut items = Vec::new();
        for it in v.get("items").and_then(|x| x.as_array()).into_iter().flatten() {
            let ai = it.get("ai").and_then(|x| x.as_str()).unwrap_or("");
            if ai.is_empty() {
                continue;
            }
            let rect = it.get("rect").and_then(|x| x.as_array()).and_then(|a| {
                let g = |i: usize| a.get(i).and_then(|v| v.as_f64());
                Some([g(0)?, g(1)?, g(2)?, g(3)?])
            });
            items.push(Job {
                ai: ai.to_string(),
                doc: it.get("doc").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                doc_name: it
                    .get("doc_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                question: it
                    .get("question")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                page: it.get("page").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                rect,
                status: it
                    .get("status")
                    .and_then(|x| x.as_str())
                    .unwrap_or(RUNNING)
                    .to_string(),
                at: it.get("at").and_then(|x| x.as_u64()).unwrap_or(0),
            });
        }
        Jobs { items }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let items: Vec<serde_json::Value> = self
            .items
            .iter()
            .map(|j| {
                let mut v = serde_json::json!({
                    "ai": j.ai,
                    "doc": j.doc,
                    "doc_name": j.doc_name,
                    "question": j.question,
                    "page": j.page,
                    "status": j.status,
                    "at": j.at,
                });
                if let Some(r) = j.rect {
                    v["rect"] = serde_json::json!(r);
                }
                v
            })
            .collect();
        serde_json::to_vec_pretty(&serde_json::json!({ "version": 1, "items": items }))
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let p = path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, self.to_bytes());
    }

    /// 记一条"开始跑"。同一个 `ai` 已经在表里就覆盖（重试）。
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        ai: &str,
        doc: &str,
        doc_name: &str,
        question: &str,
        page: u32,
        rect: Option<[f64; 4]>,
        at: u64,
    ) {
        self.items.retain(|j| j.ai != ai);
        self.items.insert(
            0,
            Job {
                ai: ai.to_string(),
                doc: doc.to_string(),
                doc_name: doc_name.to_string(),
                question: question.to_string(),
                page,
                rect,
                status: RUNNING.to_string(),
                at,
            },
        );
        // 只留最近 40 条：这是个"最近在忙什么"的列表，不是档案
        self.items.truncate(40);
    }

    /// 收尾（`done` / `failed`）。
    pub fn finish(&mut self, ai: &str, status: &str) -> bool {
        if let Some(j) = self.items.iter_mut().find(|j| j.ai == ai) {
            if j.status != status {
                j.status = status.to_string();
                return true;
            }
        }
        false
    }

    pub fn get(&self, ai: &str) -> Option<&Job> {
        self.items.iter().find(|j| j.ai == ai)
    }

    /// 还在跑 / 失败的条数（侧栏和列表页头用不到就先留着）。
    pub fn running(&self) -> usize {
        self.items.iter().filter(|j| j.status == RUNNING).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_running_and_finishes() {
        let mut j = Jobs::default();
        j.start("ai-1", "doc-1", "Exercise 5.3", "这题怎么做", 3, Some([1.0, 2.0, 30.0, 40.0]), 7);
        assert_eq!(j.items.len(), 1);
        assert_eq!(j.items[0].status, RUNNING);
        assert_eq!(j.running(), 1);
        assert!(j.finish("ai-1", DONE));
        assert_eq!(j.get("ai-1").unwrap().status, DONE);
        assert_eq!(j.running(), 0);
        // 同一个状态再收一次：没有变化
        assert!(!j.finish("ai-1", DONE));
    }

    #[test]
    fn retry_replaces_the_same_job() {
        let mut j = Jobs::default();
        j.start("ai-1", "doc-1", "A", "q", 1, None, 1);
        j.finish("ai-1", FAILED);
        // Retry：同一个 ai uuid 重新开始，不该多出一行
        j.start("ai-1", "doc-1", "A", "q", 1, None, 2);
        assert_eq!(j.items.len(), 1);
        assert_eq!(j.items[0].status, RUNNING);
        assert_eq!(j.items[0].at, 2);
    }

    #[test]
    fn newest_first_and_capped() {
        let mut j = Jobs::default();
        for i in 0..45 {
            j.start(&format!("ai-{i}"), "d", "n", "q", 1, None, i as u64);
        }
        assert_eq!(j.items.len(), 40);
        assert_eq!(j.items[0].ai, "ai-44");
    }

    #[test]
    fn roundtrip_and_tolerates_garbage() {
        let mut j = Jobs::default();
        j.start("ai-1", "doc-1", "书", "问", 2, Some([5.0, 6.0, 7.0, 8.0]), 9);
        let b = j.to_bytes();
        assert_eq!(Jobs::parse(&b), j);
        // 坏 JSON / 缺 ai 的条目：整份丢或只丢那条，不 panic
        assert!(Jobs::parse(b"not json").items.is_empty());
        assert!(Jobs::parse(br#"{"items":[{"doc":"x"},{"ai":"ok"}]}"#).items.len() == 1);
    }
}
