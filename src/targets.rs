use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use idevice::core_device::ProcessToken;
use percent_encoding::percent_decode_str;

pub const PROCESS_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Default)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: String,
    executable: Option<String>,
    bundle_path: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    executable_path: Option<String>,
}

pub fn debuggable_apps(apps: HashMap<String, plist::Value>) -> Vec<AppInfo> {
    let mut apps: Vec<_> = apps
        .into_iter()
        .filter_map(|(bundle_id, value)| {
            let app = value.as_dictionary()?;
            if app
                .get("Entitlements")?
                .as_dictionary()?
                .get("get-task-allow")?
                .as_boolean()
                != Some(true)
            {
                return None;
            }
            let name = app
                .get("CFBundleDisplayName")
                .or_else(|| app.get("CFBundleName"))
                .and_then(|name| name.as_string())
                .unwrap_or(&bundle_id)
                .to_owned();
            let executable = app
                .get("CFBundleExecutable")
                .and_then(plist::Value::as_string)
                .map(str::to_owned);
            let bundle_path = app
                .get("Path")
                .and_then(plist::Value::as_string)
                .map(str::to_owned);
            Some(AppInfo {
                name,
                bundle_id,
                executable,
                bundle_path,
            })
        })
        .collect();
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.bundle_id.cmp(&b.bundle_id))
    });
    apps
}

pub fn running_processes(processes: impl IntoIterator<Item = ProcessToken>) -> Vec<ProcessInfo> {
    processes
        .into_iter()
        .map(|process| {
            let executable_path = process.executable_url.map(|url| {
                percent_decode_str(&url.relative)
                    .decode_utf8()
                    .map(|path| path.into_owned())
                    .unwrap_or(url.relative)
            });
            let name = executable_path
                .as_deref()
                .and_then(|path| path.rsplit('/').next())
                .filter(|name| !name.is_empty())
                .unwrap_or("Unknown Process")
                .to_owned();
            ProcessInfo {
                pid: process.pid,
                name,
                executable_path,
            }
        })
        .collect()
}

pub fn filter_debuggable_processes(processes: &mut Vec<ProcessInfo>, apps: &[AppInfo]) {
    let names: HashSet<_> = apps
        .iter()
        .flat_map(|app| {
            [
                Some(app.name.as_str()),
                Some(app.bundle_id.as_str()),
                app.executable.as_deref(),
            ]
        })
        .flatten()
        .filter(|name| !name.is_empty())
        .map(str::to_lowercase)
        .collect();
    let paths: HashSet<_> = apps
        .iter()
        .filter_map(|app| app.bundle_path.as_deref())
        .map(device_path)
        .filter(|path| !path.is_empty())
        .collect();

    processes.retain(|process| {
        process.executable_path.as_deref().is_some_and(|path| {
            let path = device_path(path);
            let mut ancestor = path;
            while let Some((parent, _)) = ancestor.rsplit_once('/') {
                if paths.contains(parent) {
                    return true;
                }
                ancestor = parent;
            }
            path.split('/').any(|component| {
                let component = component.to_lowercase();
                let name = component
                    .strip_suffix(".app")
                    .or_else(|| component.strip_suffix(".appex"))
                    .unwrap_or(&component);
                names.contains(name)
            })
        })
    });
    processes.sort_by_cached_key(|process| (process.name.to_lowercase(), process.pid));
}

fn device_path(path: &str) -> &str {
    let path = path.strip_prefix("file://").unwrap_or(path);
    let path = path.strip_prefix("/private/").unwrap_or(path);
    path.trim_matches('/')
}
