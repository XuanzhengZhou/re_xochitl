//! 语言列表。只做 UI + 持久化——我们只有英文 UI，切语言不会真的换界面。
//!
//! 名字照官方 `HandwritingLanguageModel.languageNames()` / `Settings.languageName()`
//! 的风格写（英文原文），目前收了官方最常见的 8 种。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lang {
    pub code: &'static str,
    pub name: &'static str,
}

pub const HANDWRITING: [Lang; 8] = [
    Lang { code: "en-US", name: "English - United States" },
    Lang { code: "en-GB", name: "English - United Kingdom" },
    Lang { code: "zh-CN", name: "Chinese - Simplified" },
    Lang { code: "zh-TW", name: "Chinese - Traditional" },
    Lang { code: "ja-JP", name: "Japanese" },
    Lang { code: "ko-KR", name: "Korean" },
    Lang { code: "fr-FR", name: "French - France" },
    Lang { code: "de-DE", name: "German - Germany" },
];

pub const KEYBOARDS: [Lang; 8] = [
    Lang { code: "en-US", name: "English" },
    Lang { code: "en-GB", name: "English (UK)" },
    Lang { code: "zh-CN", name: "Chinese" },
    Lang { code: "ja-JP", name: "Japanese" },
    Lang { code: "ko-KR", name: "Korean" },
    Lang { code: "fr-FR", name: "French" },
    Lang { code: "de-DE", name: "German" },
    Lang { code: "es-ES", name: "Spanish" },
];

/// 按 code 找下标；找不到回 0（官方默认英文）。
pub fn index_of(list: &[Lang], code: &str) -> usize {
    list.iter().position(|l| l.code == code).unwrap_or(0)
}

pub fn name_of(list: &[Lang], code: &str) -> &'static str {
    list[index_of(list, code)].name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_english_us() {
        assert_eq!(index_of(&HANDWRITING, "en-US"), 0);
        assert_eq!(index_of(&KEYBOARDS, "nope"), 0);
        assert_eq!(name_of(&HANDWRITING, "ja-JP"), "Japanese");
        assert_eq!(name_of(&KEYBOARDS, "zz-ZZ"), "English");
    }

    #[test]
    fn codes_are_unique() {
        for list in [&HANDWRITING[..], &KEYBOARDS[..]] {
            for (i, a) in list.iter().enumerate() {
                for b in &list[i + 1..] {
                    assert_ne!(a.code, b.code, "语言 code 重复：{}", a.code);
                }
            }
        }
    }
}
