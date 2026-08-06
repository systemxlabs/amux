use std::fs;
use std::path::PathBuf;
use tauri::Manager;

/// GUI 配置路径：`<AMUX_DATA_DIR | ~/.amux>/gui/config.json`（docs/DESIGN.md §5）。
fn gui_config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let home = app.path().home_dir().map_err(|e| e.to_string())?;
    let base = std::env::var("AMUX_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".amux"));
    Ok(base.join("gui").join("config.json"))
}

/// 读取 GUI 配置；文件不存在返回 null（由前端决定首次初始化/迁移）。
#[tauri::command]
fn load_gui_config(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = gui_config_path(&app)?;
    if !path.exists() {
        return Ok(None);
    }
    fs::read_to_string(&path).map(Some).map_err(|e| e.to_string())
}

/// 原子写 GUI 配置（tmp + rename）；含 token 等敏感项，Unix 权限 0600。
#[tauri::command]
fn save_gui_config(app: tauri::AppHandle, json: String) -> Result<(), String> {
    let path = gui_config_path(&app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_file_name("config.json.tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        f.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&tmp, &json).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![load_gui_config, save_gui_config])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
