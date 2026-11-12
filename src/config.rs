//! 配置落在 %APPDATA%\FlowDock\config.json，并负责“开机自启”计划任务。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dock {
    None,
    Left,
    Right,
}

impl Default for Dock {
    fn default() -> Self {
        Dock::None
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub x: Option<i32>,
    pub y: Option<i32>,
    /// 卡片圆角，DIP
    pub radius: f32,
    /// 整体不透明度 80..=255
    pub opacity: u8,
    pub autostart: bool,
    pub dock: Dock,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            radius: 6.0,
            opacity: 245,
            autostart: false,
            dock: Dock::None,
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("FlowDock")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(Self::dir());
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), s);
        }
    }
}

const TASK_NAME: &str = "FlowDock";

/// 用计划任务实现“登录即启动 + 最高权限”，避免每次开机弹 UAC。
pub fn apply_autostart(enabled: bool) -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };
    if enabled {
        let exe_xml = exe.display().to_string().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>FlowDock 流量浮窗</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <RunLevel>HighestAvailable</RunLevel>
      <LogonType>InteractiveToken</LogonType>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
    </Exec>
  </Actions>
</Task>
"#,
            exe_xml
        );
        let path = std::env::temp_dir().join("flowdock_task.xml");
        let mut bytes = vec![0xFFu8, 0xFE];
        for u in xml.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        if std::fs::write(&path, bytes).is_err() {
            return false;
        }
        let out = std::process::Command::new("schtasks")
            .args(["/Create", "/TN", TASK_NAME, "/XML"])
            .arg(&path)
            .arg("/F")
            .output();
        matches!(out, Ok(o) if o.status.success())
    } else {
        let out = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .output();
        matches!(out, Ok(o) if o.status.success())
    }
}
