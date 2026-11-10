//! rmdev —— reMarkable 2 官方笔记软件的干净重写。
//!
//! 规格来源：home-server `~/文档/rev-Xochitl/out/modules/M1..M7-*.md`（7 份逆向规格）。
//! 分模块优先级见 `REWRITE-SPEC.md` §5。

pub mod airefs;
pub mod aijobs;
pub mod canvas;
pub mod cpu;
pub mod doc;
pub mod chrome;
pub mod cloud;
pub mod fb;
pub mod input;
pub mod pen;
pub mod net;
pub mod notify;
pub mod ocr;
pub mod panel;
pub mod power;
pub mod share;
pub mod sync;
pub mod ui;
pub mod webui;

// 并行开发的新模块（骨架先占位，实现由子模块填）
pub mod panels;
pub mod select;
pub mod settings;
pub mod viewport;
