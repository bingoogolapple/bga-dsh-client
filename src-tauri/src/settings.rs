//! 用户设置：仅保留「退出应用时是否停止服务」。
//!
//! 说明：早期版本提供「服务拉起方式」（npx / 全局 dsh / 指定目录 pnpm / 内置 Node.js）
//! 的用户配置。现已改为**完全自动**（普通版自动用 npx，内置版自动用内置 Node.js，
//! 无需用户选择），因此拉去方式相关字段（launch_method / launch_dir）与 LaunchMethod
//! 枚举已一并移除，只保留退出行为这一项用户可配置项。

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 持久化到配置文件（JSON）的设置。
#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct Settings {
    /// 点击托盘「退出应用」时是否停止本应用启动的服务（默认关闭：退出应用不停止服务）。
    #[serde(default)]
    pub stop_service_on_quit: bool,
    /// 用户选定的 DSH 版本：Some("0.1.0-rc.8") 表示用户指定用该版本；
    /// None 表示使用默认逻辑（内置版用 bundled 运行时，普通版用 npx latest）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsh_version: Option<String>,
    /// npm 下载源：None → "https://registry.npmjs.org"（默认官方源）。
    /// 用户可在设置页切换为淘宝镜像源 "https://registry.npmmirror.com"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm_registry: Option<String>,
}

impl Settings {
    /// 保存到文件（保证父目录存在）。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, json).map_err(|e| e.to_string())
    }

    /// 从文件加载；文件不存在或损坏时回退到默认值。
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 兼容旧配置文件：旧键名（launch_method / launch_dir）自动忽略，仅保留退出行为。
    #[test]
    fn loads_and_ignores_old_launch_keys() {
        // 旧版全字段配置（含已废弃的 launch_method / launch_dir）
        let raw = r#"{"launch_method":"npx","launch_dir":"","stop_service_on_quit":true}"#;
        let s: Settings = serde_json::from_str(raw).unwrap();
        assert!(s.stop_service_on_quit);

        // 缺省 stop_service_on_quit 时（如精简配置）默认关闭。
        let s: Settings = serde_json::from_str(r#"{"stop_service_on_quit":false}"#).unwrap();
        assert!(!s.stop_service_on_quit);
    }

    /// 保存时只序列化退出行为与版本选择，不再出现已废弃的拉起方式字段。
    #[test]
    fn serializes_only_quit_behavior() {
        let s = Settings {
            stop_service_on_quit: false,
            dsh_version: None,
            npm_registry: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"stop_service_on_quit\""));
        assert!(!json.contains("launch_method"));
        assert!(!json.contains("launch_dir"));
        // dsh_version=None 时不应出现
        assert!(!json.contains("dsh_version"));
        // npm_registry=None 时不应出现
        assert!(!json.contains("npm_registry"));
    }

    /// dsh_version 字段序列化与反序列化。
    #[test]
    fn dsh_version_roundtrip() {
        let s = Settings {
            stop_service_on_quit: true,
            dsh_version: Some("0.1.0-rc.8".into()),
            npm_registry: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"dsh_version\":\"0.1.0-rc.8\""));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dsh_version.as_deref(), Some("0.1.0-rc.8"));
    }

    /// npm_registry 字段序列化与反序列化。
    #[test]
    fn npm_registry_roundtrip() {
        let s = Settings {
            stop_service_on_quit: false,
            dsh_version: None,
            npm_registry: Some("https://registry.npmmirror.com".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("npmmirror"));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.npm_registry.as_deref(),
            Some("https://registry.npmmirror.com")
        );
    }
}
