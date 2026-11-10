//! 配置：`/etc/rmcloud.json`（`RMCLOUD_CONFIG` 可覆盖），缺字段走默认值。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// 监听地址。默认只绑 Tailscale 地址，别写 0.0.0.0。
    #[serde(default = "d_bind")]
    pub bind: String,
    /// 本机回环监听（`rmcloud-tool` 给 agent 用，走 127.0.0.1）。置空则只听 Tailscale。
    #[serde(default = "d_local")]
    pub local_bind: String,
    /// 状态目录：`blobs/`、`mirror/`、`cache/`、`rmcloud.sqlite`。
    #[serde(default = "d_state")]
    pub state: PathBuf,
    /// 设备 token；`rmcloud pair` 之后自动写回。空 = 还没配对。
    #[serde(default)]
    pub token: String,
    /// dsh 可执行文件（agent 网关用）。
    #[serde(default = "d_dsh")]
    pub dsh: String,
    /// dsh 的环境文件（里面有 DEEPSEEK_API_KEY）。
    #[serde(default = "d_env")]
    pub dsh_env: String,
    /// 发信账号。
    #[serde(default)]
    pub smtp: Option<Smtp>,
    /// 是否开启 IMAP 收信轮询。默认**关**：收信要常驻连外网、还要把授权码
    /// 交给 python 子进程，用不到就不开。要用在 json 里写 `"inbox": true`。
    #[serde(default)]
    pub inbox: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Smtp {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
}

fn d_bind() -> String {
    "100.64.0.1:8787".into()
}
fn d_local() -> String {
    "127.0.0.1:8787".into()
}
fn d_state() -> PathBuf {
    PathBuf::from("/srv/rm")
}
fn d_dsh() -> String {
    "/home/user/.nvm/versions/node/v24.14.1/bin/dsh".into()
}
fn d_env() -> String {
    "/home/user/.dsh/web.env".into()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: d_bind(),
            local_bind: d_local(),
            state: d_state(),
            token: String::new(),
            dsh: d_dsh(),
            dsh_env: d_env(),
            smtp: None,
            inbox: false,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        match std::env::var("RMCLOUD_CONFIG") {
            Ok(p) => PathBuf::from(p),
            Err(_) => PathBuf::from("/etc/rmcloud.json"),
        }
    }

    pub fn load() -> anyhow::Result<Config> {
        let p = Self::path();
        let mut cfg = match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s)?,
            Err(_) => Config::default(),
        };
        if let Ok(v) = std::env::var("RMCLOUD_BIND") {
            cfg.bind = v;
        }
        cfg.save()?;
        Ok(cfg)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = Self::path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&p, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
