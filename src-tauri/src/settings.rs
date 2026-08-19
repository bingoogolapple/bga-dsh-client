//! 用户设置：服务拉起方式（npx / 全局 dsh / 指定目录 pnpm / 内置 Node.js）与持久化。

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::i18n::{tr, Locale};

/// 四种拉起方式。
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMethod {
    /// 默认：`npx --yes @deepseek-ai/dsh web`
    #[default]
    Npx,
    /// 已全局安装：`dsh web`
    Dsh,
    /// 指定目录：`pnpm dsh web`
    Pnpm,
    /// 内置 Node.js：使用应用自带的 Node 与 dsh（离线可用，仅内置版打包有运行时）
    Builtin,
}

impl LaunchMethod {
    /// 把前端传来的字符串解析为枚举；未知值返回 None。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "npx" => Some(Self::Npx),
            "dsh" => Some(Self::Dsh),
            "pnpm" => Some(Self::Pnpm),
            "builtin" => Some(Self::Builtin),
            _ => None,
        }
    }

    /// 用于展示给用户的命令描述。
    pub fn display(self, dir: &str, locale: Locale) -> String {
        match self {
            Self::Npx => "npx --yes @deepseek-ai/dsh web".into(),
            Self::Dsh => "dsh web".into(),
            Self::Pnpm => {
                let dir = if dir.trim().is_empty() { "." } else { dir };
                tr(locale, "mth.pnpm", &[dir])
            }
            Self::Builtin => tr(locale, "mth.builtin", &[]),
        }
    }
}

/// 持久化到配置文件（JSON）的设置。
/// 字段名即 JSON 键，均按功能命名（尚未正式发布，不做旧键名兼容）。
#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct Settings {
    /// 拉起方式：npx / 全局 dsh / 指定目录 pnpm。
    pub launch_method: LaunchMethod,
    /// 拉起命令的工作目录（方式三：进入指定项目目录后执行 pnpm dsh web）。
    #[serde(default)]
    pub launch_dir: String,
    /// 点击托盘「退出应用」时是否停止本应用启动的服务（默认关闭：退出应用不停止服务）。
    #[serde(default)]
    pub stop_service_on_quit: bool,
}

impl Settings {
    /// 从前端参数构造并校验。
    pub fn from_parts(
        launch_method: &str,
        launch_dir: &str,
        stop_service_on_quit: bool,
        locale: Locale,
    ) -> Result<Self, String> {
        let m = LaunchMethod::parse(launch_method)
            .ok_or_else(|| tr(locale, "set.unknown_method", &[launch_method]))?;
        let dir = launch_dir.trim().to_string();
        if m == LaunchMethod::Pnpm && dir.is_empty() {
            return Err(tr(locale, "set.dir_required", &[]));
        }
        Ok(Self {
            launch_method: m,
            launch_dir: dir,
            stop_service_on_quit,
        })
    }

    /// 从文件加载；文件不存在或损坏时回退到默认值。
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 保存到文件（保证父目录存在）。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, json).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内置 Node.js 方式可解析为 Builtin；未知值返回 None。
    #[test]
    fn parses_builtin() {
        assert_eq!(LaunchMethod::parse("builtin"), Some(LaunchMethod::Builtin));
        assert_eq!(LaunchMethod::parse("npx"), Some(LaunchMethod::Npx));
        assert_eq!(LaunchMethod::parse("docker"), None);
        assert!(LaunchMethod::Builtin
            .display("", Locale::Zh)
            .contains("内置 Node.js"));
        assert!(LaunchMethod::Builtin
            .display("", Locale::En)
            .contains("Built-in Node.js"));
    }

    /// 新语义键名读取 + 缺省字段回退默认值。
    #[test]
    fn loads_semantic_keys() {
        let raw = r#"{"launch_method":"npx","launch_dir":"","stop_service_on_quit":true}"#;
        let s: Settings = serde_json::from_str(raw).unwrap();
        assert_eq!(s.launch_method, LaunchMethod::Npx);
        assert!(s.stop_service_on_quit);

        // 缺省 stop_service_on_quit 时（比如手工写的精简配置）默认关闭。
        let s: Settings = serde_json::from_str(r#"{"launch_method":"dsh"}"#).unwrap();
        assert_eq!(s.launch_method, LaunchMethod::Dsh);
        assert!(!s.stop_service_on_quit);
    }

    /// 保存时序列化出的键名是可读的语义名。
    #[test]
    fn serializes_semantic_keys() {
        let s = Settings {
            launch_method: LaunchMethod::Npx,
            launch_dir: String::new(),
            stop_service_on_quit: false,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"launch_method\""));
        assert!(json.contains("\"launch_dir\""));
        assert!(json.contains("\"stop_service_on_quit\""));
        assert!(!json.contains("\"method\""));
    }
}
