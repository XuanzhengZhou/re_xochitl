//! 屏幕锁：密码的哈希存储 + 连错 5 次锁 30 秒。
//!
//! **恢复路径（重要）**：密码只存在 `~/.config/rmdev/passcode`。
//! 忘记密码时，`rm ~/.config/rmdev/passcode`（或删掉整个 `~/.config/rmdev`）
//! 再启动就是无锁状态。这是刻意的设计——没有这条后路，设备就变砖了。
//!
//! 哈希只是「别把 4 位数字明文写在盘上」这个级别的防护：
//! FNV-1a 64 + 随机盐，没有引第三方 crate（任务要求）。
//! 它**不是**密码学 KDF，挡不住拿到文件后暴力枚举（4 位数字本来就只有 1 万种）。
//! 4 位数字的屏锁本来也挡不住物理接触，够用就行。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::config_dir;

pub const PASSCODE_LEN: usize = 4;
pub const MAX_TRIES: u32 = 5;
pub const LOCK_SECS: u64 = 30;

const MAGIC: &str = "rmdev-passcode-v1";
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// passcode 文件路径。
pub fn path() -> PathBuf {
    config_dir().join("passcode")
}

fn fnv1a(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// 盐 + 4 位码 → 64 位哈希（迭代两轮，纯 std）。
pub fn hash_code(salt: u64, code: &str) -> u64 {
    let mut h = FNV_OFFSET ^ salt;
    for _ in 0..2 {
        h = fnv1a(h, code.as_bytes());
        h = fnv1a(h, &salt.to_le_bytes());
    }
    h
}

/// 文件内容：`rmdev-passcode-v1 <salt hex> <hash hex>`。
pub fn encode(salt: u64, hash: u64) -> String {
    format!("{MAGIC} {salt:016x} {hash:016x}\n")
}

pub fn decode(text: &str) -> Option<(u64, u64)> {
    let mut it = text.split_whitespace();
    if it.next()? != MAGIC {
        return None;
    }
    let salt = u64::from_str_radix(it.next()?, 16).ok()?;
    let hash = u64::from_str_radix(it.next()?, 16).ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((salt, hash))
}

fn fresh_salt() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ ((std::process::id() as u64) << 32) ^ 0x9e37_79b9_7f4a_7c15
}

/// 这个目录下有没有密码（路径可注入，方便测试）。
pub fn enabled_at(dir: &Path) -> bool {
    decode(&std::fs::read_to_string(dir.join("passcode")).unwrap_or_default()).is_some()
}

/// 设备上有没有设密码。
pub fn is_enabled() -> bool {
    enabled_at(&config_dir())
}

/// 校验密码。文件不存在/损坏都返回 false。
pub fn verify_at(dir: &Path, code: &str) -> bool {
    let text = match std::fs::read_to_string(dir.join("passcode")) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let Some((salt, hash)) = decode(&text) else {
        return false;
    };
    hash_code(salt, code) == hash
}

pub fn verify(code: &str) -> bool {
    verify_at(&config_dir(), code)
}

fn write_secure(p: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 设/改密码（覆盖旧文件）。`code` 必须是 4 位数字。
pub fn set_at(dir: &Path, code: &str) -> std::io::Result<()> {
    let salt = fresh_salt();
    write_secure(&dir.join("passcode"), &encode(salt, hash_code(salt, code)))
}

pub fn set(code: &str) -> std::io::Result<()> {
    set_at(&config_dir(), code)
}

/// 关掉密码：删文件。删掉即解锁。
pub fn clear_at(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(dir.join("passcode")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn clear() -> std::io::Result<()> {
    clear_at(&config_dir())
}

/// 4 位数字？
pub fn valid_code(code: &str) -> bool {
    code.len() == PASSCODE_LEN && code.chars().all(|c| c.is_ascii_digit())
}

/// 连错计数。5 次 → 锁 30 秒，计时结束后次数清零。
#[derive(Clone, Debug, Default)]
pub struct Lockout {
    fails: u32,
    until: Option<Instant>,
}

impl Lockout {
    pub fn remaining_secs(&self, now: Instant) -> u64 {
        match self.until {
            Some(t) => {
                let d = t.saturating_duration_since(now);
                if d.is_zero() {
                    0
                } else {
                    d.as_secs() + u64::from(d.subsec_nanos() > 0)
                }
            }
            None => 0,
        }
    }

    pub fn can_try(&self, now: Instant) -> bool {
        self.remaining_secs(now) == 0
    }

    pub fn fails(&self) -> u32 {
        self.fails
    }

    /// 记一次失败；返回刚刚触发锁定没有。
    pub fn record_failure(&mut self, now: Instant) -> bool {
        if !self.can_try(now) {
            return false;
        }
        // 上一轮锁定已经过期：计数从 0 开始。
        if self.until.is_some() {
            self.until = None;
            self.fails = 0;
        }
        self.fails += 1;
        if self.fails >= MAX_TRIES {
            self.fails = 0;
            self.until = Some(now + Duration::from_secs(LOCK_SECS));
            true
        } else {
            false
        }
    }

    pub fn reset(&mut self) {
        self.fails = 0;
        self.until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rmdev-pass-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn hash_is_deterministic_and_salted() {
        assert_eq!(hash_code(7, "1234"), hash_code(7, "1234"));
        assert_ne!(hash_code(7, "1234"), hash_code(8, "1234"));
        assert_ne!(hash_code(7, "1234"), hash_code(7, "1235"));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let s = encode(0xdead_beef, 0x1234_5678_9abc_def0);
        assert_eq!(decode(&s), Some((0xdead_beef, 0x1234_5678_9abc_def0)));
        assert_eq!(decode("garbage"), None);
        assert_eq!(decode("rmdev-passcode-v1 zz 11"), None);
    }

    #[test]
    fn set_verify_clear() {
        let d = tmpdir("rw");
        assert!(!enabled_at(&d));
        set_at(&d, "0420").unwrap();
        assert!(enabled_at(&d));
        assert!(verify_at(&d, "0420"));
        assert!(!verify_at(&d, "0421"));
        // 明文绝不能落盘
        let raw = std::fs::read_to_string(d.join("passcode")).unwrap();
        assert!(!raw.contains("0420"), "文件里出现了明文密码：{raw}");
        clear_at(&d).unwrap();
        assert!(!enabled_at(&d));
        assert!(!verify_at(&d, "0420"));
        // 重复删不报错
        clear_at(&d).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_file_reads_as_disabled() {
        let d = tmpdir("corrupt");
        std::fs::write(d.join("passcode"), "not a passcode").unwrap();
        assert!(!enabled_at(&d));
        assert!(!verify_at(&d, "0000"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn validates_four_digits() {
        assert!(valid_code("0000"));
        assert!(!valid_code("000"));
        assert!(!valid_code("00000"));
        assert!(!valid_code("12a4"));
    }

    #[test]
    fn five_failures_lock_for_thirty_seconds() {
        let t0 = Instant::now();
        let mut l = Lockout::default();
        for i in 0..4 {
            assert!(!l.record_failure(t0), "第 {} 次不该锁", i + 1);
            assert!(l.can_try(t0));
        }
        assert!(l.record_failure(t0), "第 5 次应触发锁定");
        assert!(!l.can_try(t0));
        assert_eq!(l.remaining_secs(t0), LOCK_SECS);
        assert!(!l.can_try(t0 + Duration::from_secs(29)));
        assert!(l.can_try(t0 + Duration::from_secs(31)));
        // 锁定期间再点不算失败
        assert!(!l.record_failure(t0));
        // 锁完从头计数
        l.record_failure(t0 + Duration::from_secs(31));
        assert_eq!(l.fails(), 1);
        l.reset();
        assert_eq!(l.fails(), 0);
        assert!(l.can_try(t0));
    }
}
