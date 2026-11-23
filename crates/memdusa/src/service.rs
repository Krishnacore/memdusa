//! `memdusa sync service`: install/uninstall a launchd agent (macOS) or a
//! systemd user unit (Linux) that runs `memdusa sync --watch`. Windows prints
//! Task Scheduler instructions instead.
//!
//! Tests point `MEMDUSA_SERVICE_DIR` at a temp directory, so no real service
//! directory is ever touched by the test suite.

// The launchd/systemd renderers are only called on macOS/Linux (and in tests); Windows builds print
// instructions instead, so they would be dead code there.
#![cfg_attr(windows, allow(dead_code))]

use std::path::PathBuf;

#[cfg(not(windows))]
use anyhow::Context;
use clap::Subcommand;

use crate::config;

#[derive(Debug, Clone, Subcommand)]
pub enum ServiceCmd {
    /// Write the service file and print how to start it.
    Install,
    /// Remove the service file and print how to stop it.
    Uninstall,
}

pub const LAUNCHD_LABEL: &str = "com.memdusa.sync";
pub const SYSTEMD_UNIT: &str = "memdusa-sync.service";

fn service_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MEMDUSA_SERVICE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = config::home_dir()?;
    #[cfg(target_os = "macos")]
    {
        Ok(home.join("Library").join("LaunchAgents"))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(home.join(".config").join("systemd").join("user"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        Ok(home)
    }
}

pub fn service_file() -> anyhow::Result<PathBuf> {
    let dir = service_dir()?;
    #[cfg(target_os = "macos")]
    {
        Ok(dir.join(format!("{LAUNCHD_LABEL}.plist")))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(dir.join(SYSTEMD_UNIT))
    }
}

/// Executable to put in the service file. Overridable so rendered files can be
/// asserted in tests without depending on the test binary's path.
fn bin() -> anyhow::Result<String> {
    if let Some(bin) = std::env::var("MEMDUSA_SERVICE_BIN").ok().filter(|b| !b.trim().is_empty()) {
        return Ok(bin);
    }
    crate::config::stable_exe()
}

pub fn render_launchd(bin: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
           <key>Label</key>\n\
           <string>{LAUNCHD_LABEL}</string>\n\
           <key>ProgramArguments</key>\n\
           <array>\n\
             <string>{bin}</string>\n\
             <string>sync</string>\n\
             <string>--watch</string>\n\
           </array>\n\
           <key>RunAtLoad</key>\n\
           <true/>\n\
           <key>KeepAlive</key>\n\
           <true/>\n\
         </dict>\n\
         </plist>\n"
    )
}

pub fn render_systemd(bin: &str) -> String {
    format!(
        "[Unit]\n\
         Description=memdusa local/hosted sync watcher\n\
         After=network-online.target\n\n\
         [Service]\n\
         ExecStart={bin} sync --watch\n\
         Restart=always\n\
         RestartSec=5\n\n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

#[cfg(target_os = "windows")]
pub fn run(cmd: ServiceCmd) -> anyhow::Result<()> {
    let action = match cmd {
        ServiceCmd::Install => "create",
        ServiceCmd::Uninstall => "delete",
    };
    println!(
        "Windows: use Task Scheduler to {action} a task that runs `\"{}\" sync --watch`.",
        bin().unwrap_or_else(|_| "memdusa".into())
    );
    println!("  - Trigger: At log on (and repeat every 15 minutes as a safety net).");
    println!("  - Action: Start a program, Program/script = the memdusa executable, Arguments = sync --watch");
    println!(
        "  - Tick 'Run only when the user is logged on'. Multiple tasks are prevented by the per-MEMDUSA_HOME lock."
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn run(cmd: ServiceCmd) -> anyhow::Result<()> {
    let path = service_file()?;
    match cmd {
        ServiceCmd::Install => {
            let b = bin()?;
            let text = if cfg!(target_os = "macos") { render_launchd(&b) } else { render_systemd(&b) };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&path, &text)?;
            println!("wrote {}", path.display());
            println!("{}", text.trim_end());
            println!();
            if cfg!(target_os = "macos") {
                println!("start: launchctl load -w {}", path.display());
                println!("stop:  launchctl unload -w {}", path.display());
            } else {
                println!("start: systemctl --user daemon-reload && systemctl --user enable --now {SYSTEMD_UNIT}");
                println!("stop:  systemctl --user disable --now {SYSTEMD_UNIT}");
            }
        }
        ServiceCmd::Uninstall => {
            match std::fs::remove_file(&path) {
                Ok(()) => println!("removed {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("no service file at {}", path.display()),
                Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
            }
            if cfg!(target_os = "macos") {
                println!("stop: launchctl unload -w {}", path.display());
            } else {
                println!("stop: systemctl --user disable --now {SYSTEMD_UNIT} && systemctl --user daemon-reload");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_dir(f: impl FnOnce(PathBuf)) {
        let _env = config::TEST_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("memdusa-service-{}", memdusa_core::model::new_id()));
        let old_dir = std::env::var_os("MEMDUSA_SERVICE_DIR");
        unsafe {
            std::env::set_var("MEMDUSA_SERVICE_DIR", &dir);
            std::env::set_var("MEMDUSA_SERVICE_BIN", "/usr/local/bin/memdusa");
        }
        f(dir.clone());
        unsafe {
            match old_dir {
                Some(v) => std::env::set_var("MEMDUSA_SERVICE_DIR", v),
                None => std::env::remove_var("MEMDUSA_SERVICE_DIR"),
            }
            std::env::remove_var("MEMDUSA_SERVICE_BIN");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn renders_both_service_formats() {
        let launchd = render_launchd("/usr/local/bin/memdusa");
        assert!(launchd.contains(LAUNCHD_LABEL));
        assert!(launchd.contains("<string>/usr/local/bin/memdusa</string>"));
        assert!(launchd.contains("<string>--watch</string>"));
        assert!(launchd.contains("<key>KeepAlive</key>"));
        let systemd = render_systemd("/usr/local/bin/memdusa");
        assert!(systemd.contains("ExecStart=/usr/local/bin/memdusa sync --watch"));
        assert!(systemd.contains("Restart=always"));
        assert!(systemd.contains("WantedBy=default.target"));
    }

    #[test]
    fn install_and_uninstall_only_touch_the_override_dir() {
        if cfg!(target_os = "windows") {
            return; // Windows prints instructions; no file is written.
        }
        with_dir(|dir| {
            run(ServiceCmd::Install).unwrap();
            let file = service_file().unwrap();
            assert!(file.starts_with(&dir), "{:?} escaped {:?}", file, dir);
            let text = std::fs::read_to_string(&file).unwrap();
            assert!(text.contains("--watch"));
            run(ServiceCmd::Uninstall).unwrap();
            assert!(!file.exists());
        });
    }
}
