//! 笔迹捕获与笔宽模型——`out/modules/M1-pen-input.md` 的 Rust 落地。

pub mod line;
pub mod pencil;
pub mod stroke;
pub mod width;

pub use line::{Color, InputPoint, Line, Point, RectF, Tool};
pub use stroke::StrokeBuilder;
pub use width::Width;
