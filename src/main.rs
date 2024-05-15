#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    fs::create_dir_all,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    Icon, TrayIconBuilder, TrayIconEvent,
};
use windows_capture::capture::GraphicsCaptureApiHandler;
use winsafe::{prelude::*, GetLastError, HMONITOR, HPROCESSLIST, HWND};

#[derive(Deserialize, Clone)]
struct Config {
    target_folder: PathBuf,
    screenshot_delay: u64,
    #[serde(default)]
    rules: Vec<RuleEntry>,
}

#[derive(Deserialize, Clone)]
#[serde(default)]
struct RuleEntry {
    name: String,
    ignore: bool,
    needs_fullscreen: bool,
    use_window_name: bool,
    override_name: Option<String>,
}

impl Default for RuleEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            ignore: false,
            needs_fullscreen: true,
            use_window_name: false,
            override_name: None,
        }
    }
}

fn normalize_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' | ' ' => c,
            _ => '_',
        })
        .collect()
}

fn get_process_name_from_pid(pid: u32) -> Result<String> {
    use winsafe::co::TH32CS;
    let mut process_list = HPROCESSLIST::CreateToolhelp32Snapshot(TH32CS::SNAPPROCESS, None)?;
    let process = process_list
        .iter_processes()
        .filter_map(|p| p.ok())
        .find(|p| p.th32ProcessID == pid)
        .context("PID not found")?;
    let process_name = Path::new(&process.szExeFile())
        .file_stem()
        .context("Getting file stem")?
        .to_str()
        .context("File to String")?
        .to_owned();
    Ok(process_name)
}

fn get_name(window: &HWND) -> Result<String> {
    let (_, pid) = window.GetWindowThreadProcessId();
    if pid == 0 {
        Ok(normalize_name(&window.GetWindowText()?))
    } else {
        get_process_name_from_pid(pid)
    }
}

fn get_valid_window(config: &Config) -> Result<(u32, String)> {
    let window = HWND::GetForegroundWindow().context("Failed to get foreground window")?;
    let name = get_name(&window)?;

    let associated_config = config
        .rules
        .iter()
        .find(|e| e.name.to_lowercase() == name.to_lowercase())
        .map(Clone::clone)
        .unwrap_or_default();
    if associated_config.ignore {
        bail!("Executable is ignored")
    }

    if associated_config.needs_fullscreen {
        let rect = window.GetWindowRect()?;
        let monitor = HMONITOR::MonitorFromRect(rect, winsafe::co::MONITOR::DEFAULTTOPRIMARY);
        let mut monitor_info = winsafe::MONITORINFOEX::default();
        monitor.GetMonitorInfo(&mut monitor_info)?;

        if !(rect.left <= monitor_info.rcMonitor.left
            && rect.right >= monitor_info.rcMonitor.right
            && rect.top <= monitor_info.rcMonitor.top
            && rect.bottom >= monitor_info.rcMonitor.bottom)
        {
            bail!("Window is not fullscreen");
        }
    }

    let name = if let Some(n) = associated_config.override_name {
        n
    } else if associated_config.use_window_name {
        normalize_name(&window.GetWindowText()?)
    } else {
        name
    };

    Ok((window.ptr() as u32, name))
}

struct Screenshot {
    target: String,
}

impl GraphicsCaptureApiHandler for Screenshot {
    type Flags = String;
    type Error = anyhow::Error;

    fn new(flags: Self::Flags) -> Result<Self, Self::Error> {
        Ok(Self { target: flags })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut windows_capture::frame::Frame,
        capture_control: windows_capture::graphics_capture_api::InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        frame.save_as_image(&self.target, windows_capture::frame::ImageFormat::Jpeg)?;
        capture_control.stop();
        Ok(())
    }
}

fn save_screenshot(target_path: &Path, id: u32, name: &str) -> Result<()> {
    let window = windows_capture::window::Window::from_raw_hwnd(id as _);
    let monitor = window.monitor().context("No monitor for window")?;

    let gamedir = target_path.join(name);
    create_dir_all(&gamedir)?;

    let filename_str = chrono::Local::now()
        .format("%Y-%m-%d_%H-%M-%S.jpg")
        .to_string();
    let filename = Path::new(&filename_str);
    let filename = gamedir.join(filename);
    let filename = filename.to_str().context("path to string")?;

    Screenshot::start(windows_capture::settings::Settings::new(
        monitor,
        windows_capture::settings::CursorCaptureSettings::Default,
        windows_capture::settings::DrawBorderSettings::WithoutBorder,
        windows_capture::settings::ColorFormat::Bgra8,
        filename.to_string(),
    ))?;
    Ok(())
}

fn get_last_input_time() -> Result<u32> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { !GetLastInputInfo(&mut info as *mut LASTINPUTINFO).as_bool() } {
        bail!(GetLastError())
    }
    Ok(info.dwTime)
}

fn screenshot_thread(config: Config) -> ! {
    let mut last_input = 0;

    loop {
        std::thread::sleep(Duration::from_secs(config.screenshot_delay));
        let (id, name) = match get_valid_window(&config) {
            Err(e) => {
                println!("No valid window: {e:?}");
                continue;
            }
            Ok(o) => o,
        };

        match get_last_input_time() {
            Ok(time) => {
                if time <= last_input {
                    println!("No input since last screenshot");
                    continue;
                }
                last_input = time;
            }
            Err(e) => {
                println!("Failed to get last input: {e:?}");
            }
        }

        if let Err(e) = save_screenshot(&config.target_folder, id, &name) {
            println!("Could not save screenshot: {e:?}");
            continue;
        }
        println!("Saved screenshot for {name}");
    }
}

fn main() {
    let config: Config = toml::from_str(&std::fs::read_to_string("config.toml").unwrap()).unwrap();
    let target_path = config
        .target_folder
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let _thread = std::thread::spawn(|| screenshot_thread(config));
    let mut _tray_icon = None;

    let quit_menu_item = MenuItem::new("Quit", true, None);
    let open_menu_item = MenuItem::new("Open", true, None);

    let event_loop = EventLoopBuilder::new().build();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(50));

        if let Event::NewEvents(StartCause::Init) = event {
            let image = image::open("Icon.png").unwrap().into_rgba8();
            let (w, h) = image.dimensions();

            let menu = Menu::new();
            menu.append(&quit_menu_item).unwrap();
            menu.append(&open_menu_item).unwrap();

            _tray_icon = Some(
                TrayIconBuilder::new()
                    .with_menu(Box::new(menu))
                    .with_icon(Icon::from_rgba(image.into_raw(), w, h).unwrap())
                    .with_tooltip("autogamejournal")
                    .build()
                    .unwrap(),
            );
        }

        let _ = TrayIconEvent::receiver().try_recv();
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == quit_menu_item.id() {
                *control_flow = ControlFlow::Exit;
            }
            if event.id == open_menu_item.id() {
                use winsafe::co::SW;
                if let Err(e) =
                    HWND::NULL.ShellExecute("explore", &target_path, None, None, SW::SHOWNORMAL)
                {
                    println!("Error opening folder {target_path:?} {e:?}");
                }
            }
        }
    });
}
