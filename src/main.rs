// На Windows не показувати чорне вікно консолі разом із програмою
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{Color32, FontId, RichText, TextStyle};
use serialport::{available_ports, SerialPortInfo, SerialPortType};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

const DEFAULT_BOARDS: &[(&str, &str)] = &[
    ("Arduino Uno", "arduino:avr:uno"),
    ("Arduino Nano", "arduino:avr:nano:cpu=atmega328p"),
    ("Arduino Nano (старий завантажувач)", "arduino:avr:nano:cpu=atmega328old"),
    ("Arduino Mega 2560", "arduino:avr:mega:cpu=atmega2560"),
    ("Arduino Leonardo", "arduino:avr:leonardo"),
];

const CPP_KEYWORDS: &[&str] = &[
    "void", "int", "float", "double", "char", "byte", "boolean", "bool", "long", "short",
    "unsigned", "String", "uint8_t", "uint16_t", "uint32_t", "setup", "loop", "pinMode",
    "digitalWrite", "digitalRead", "analogWrite", "analogRead", "delay", "delayMicroseconds",
    "millis", "micros", "map", "constrain", "tone", "noTone", "pulseIn", "HIGH", "LOW",
    "INPUT", "OUTPUT", "INPUT_PULLUP", "LED_BUILTIN", "if", "else", "for", "while", "do",
    "switch", "case", "break", "continue", "return", "true", "false", "const", "static",
    "Serial", "begin", "print", "println", "available", "read", "write",
];

const DEFAULT_SKETCH: &str = r#"void setup() {
  pinMode(LED_BUILTIN, OUTPUT);  // вбудований світлодіод - це вихід
  Serial.begin(9600);
}

void loop() {
  digitalWrite(LED_BUILTIN, HIGH);  // увімкнути світлодіод
  Serial.println("LED ON");
  delay(1000);                      // чекати 1 секунду
  digitalWrite(LED_BUILTIN, LOW);   // вимкнути світлодіод
  Serial.println("LED OFF");
  delay(1000);
}
"#;

// ---------- Палітра (спокійні відтінки синього) ----------
const BG: Color32 = Color32::from_rgb(13, 20, 38);
const PANEL: Color32 = Color32::from_rgb(18, 27, 50);
const CARD: Color32 = Color32::from_rgb(26, 38, 68);
const EDITOR_BG: Color32 = Color32::from_rgb(10, 16, 32);
const HOVER: Color32 = Color32::from_rgb(40, 58, 104);
const SECONDARY: Color32 = Color32::from_rgb(44, 62, 108);
const PRIMARY: Color32 = Color32::from_rgb(52, 104, 200);
const ACCENT: Color32 = Color32::from_rgb(72, 124, 220);
const ACCENT_LIGHT: Color32 = Color32::from_rgb(147, 190, 250);
const LINE: Color32 = Color32::from_rgb(52, 76, 130);
const RED: Color32 = Color32::from_rgb(225, 100, 110);

// ---------- Кольори підсвічування коду ----------
const COLOR_COMMENT: Color32 = Color32::from_rgb(105, 150, 125);
const COLOR_STRING: Color32 = Color32::from_rgb(240, 180, 140);
const COLOR_CHAR: Color32 = Color32::from_rgb(240, 190, 140);
const COLOR_NUMBER: Color32 = Color32::from_rgb(130, 200, 240);
const COLOR_KEYWORD: Color32 = Color32::from_rgb(150, 160, 255);
const COLOR_OPERATOR: Color32 = Color32::from_rgb(150, 165, 200);
const COLOR_TEXT: Color32 = Color32::from_rgb(225, 232, 245);

#[derive(PartialEq, Clone, Copy)]
enum BottomTab {
    Console,
    Monitor,
    Plotter,
}

enum Msg {
    Log(String),
    Done,
    IndexLoaded(Result<IndexData, String>),
}

// ---------- Менеджер бібліотек: каталог читається локально, тому пошук миттєвий ----------

const OK_GREEN: Color32 = Color32::from_rgb(110, 200, 140);

#[derive(Clone)]
struct LibInfo {
    name: String,
    version: String,
    author: String,
    sentence: String,
    name_lower: String,
    search_key: String,
}

struct IndexData {
    libs: Vec<LibInfo>,
    age_days: Option<u64>,
}

#[derive(Clone)]
struct InstalledLib {
    name: String,
    version: String,
}

#[derive(PartialEq, Clone)]
enum IndexState {
    Loading,
    Ready,
    Error(String),
}

#[derive(PartialEq, Clone, Copy)]
enum LibTab {
    Catalog,
    Installed,
}

enum LibAction {
    Install,
    Upgrade,
    Uninstall,
}

#[derive(serde::Deserialize)]
struct RawLib {
    name: String,
    version: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    sentence: String,
    #[serde(default)]
    category: String,
}

#[derive(serde::Deserialize)]
struct RawIndex {
    libraries: Vec<RawLib>,
}

/// Папка даних arduino-cli (там лежить library_index.json)
fn arduino_data_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        dirs::data_local_dir()
            .map(|d| d.join("Arduino15"))
            .unwrap_or_else(|| PathBuf::from("Arduino15"))
    } else if cfg!(target_os = "macos") {
        dirs::home_dir()
            .map(|h| h.join("Library/Arduino15"))
            .unwrap_or_else(|| PathBuf::from("Arduino15"))
    } else {
        dirs::home_dir()
            .map(|h| h.join(".arduino15"))
            .unwrap_or_else(|| PathBuf::from(".arduino15"))
    }
}

/// "1.2.10" -> [1, 2, 10] для порівняння версій
fn version_key(v: &str) -> Vec<u64> {
    let mut parts: Vec<u64> = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u64>().unwrap_or(0))
        .collect();
    while parts.last() == Some(&0) {
        parts.pop();
    }
    parts
}

fn truncate_text(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", t)
    }
}

/// Читає каталог бібліотек (залишає тільки найновішу версію кожної)
fn load_library_index(path: &Path) -> Result<IndexData, String> {
    if !path.exists() {
        return Err("missing".to_string());
    }
    let data = std::fs::read(path).map_err(|e| format!("Не вдалося прочитати каталог: {}", e))?;
    let raw: RawIndex =
        serde_json::from_slice(&data).map_err(|e| format!("Помилка читання каталогу: {}", e))?;

    let mut best: HashMap<String, RawLib> = HashMap::new();
    for lib in raw.libraries {
        let key = lib.name.to_lowercase();
        let newer = match best.get(&key) {
            Some(old) => version_key(&lib.version) > version_key(&old.version),
            None => true,
        };
        if newer {
            best.insert(key, lib);
        }
    }

    let mut libs: Vec<LibInfo> = best
        .into_values()
        .map(|l| {
            let name_lower = l.name.to_lowercase();
            let search_key = format!(
                "{} {} {} {}",
                name_lower,
                l.author.to_lowercase(),
                l.sentence.to_lowercase(),
                l.category.to_lowercase()
            );
            let author = l.author.split('<').next().unwrap_or("").trim().to_string();
            LibInfo {
                name: l.name,
                version: l.version,
                author,
                sentence: l.sentence,
                name_lower,
                search_key,
            }
        })
        .collect();
    libs.sort_by(|a, b| a.name_lower.cmp(&b.name_lower));

    let age_days = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs() / 86400);

    Ok(IndexData { libs, age_days })
}

/// Встановлені бібліотеки: ім'я і версія з library.properties
fn read_installed_libs(dir: &Path) -> Vec<InstalledLib> {
    let mut res = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            let mut name = e.file_name().to_string_lossy().to_string();
            let mut version = String::new();
            if let Ok(text) = std::fs::read_to_string(path.join("library.properties")) {
                for line in text.lines() {
                    let line = line.trim_start_matches('\u{feff}');
                    if let Some(v) = line.strip_prefix("name=") {
                        name = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("version=") {
                        version = v.trim().to_string();
                    }
                }
            }
            res.push(InstalledLib { name, version });
        }
    }
    res.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    res
}

// ---------- Збереження налаштувань між запусками ----------

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Settings {
    #[serde(default)]
    arduino_cli_path: Option<String>,
    #[serde(default)]
    board_idx: Option<usize>,
    #[serde(default)]
    baud_rate: Option<u32>,
}

fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("arduino-ide-rust").join("settings.json"))
}

fn load_settings() -> Settings {
    settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default()
}

fn save_settings(cur: &(String, usize, u32)) {
    if let Some(p) = settings_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let st = Settings {
            arduino_cli_path: Some(cur.0.clone()),
            board_idx: Some(cur.1),
            baud_rate: Some(cur.2),
        };
        if let Ok(text) = serde_json::to_string_pretty(&st) {
            let _ = std::fs::write(p, text);
        }
    }
}

/// Шукає arduino-cli: спочатку в PATH, потім у типових місцях
fn detect_arduino_cli() -> String {
    let name = if cfg!(target_os = "windows") {
        "arduino-cli.exe"
    } else {
        "arduino-cli"
    };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if dir.join(name).is_file() {
                return "arduino-cli".to_string();
            }
        }
    }
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("/usr/bin/arduino-cli"),
        PathBuf::from("/usr/local/bin/arduino-cli"),
    ];
    if let Some(h) = dirs::home_dir() {
        candidates.push(h.join("go/bin/arduino-cli"));
        candidates.push(h.join(".local/bin/arduino-cli"));
        candidates.push(h.join("bin/arduino-cli"));
    }
    for c in candidates {
        if c.is_file() {
            return c.to_string_lossy().to_string();
        }
    }
    "arduino-cli".to_string()
}

struct ArduinoIde {
    sketch: String,
    arduino_cli_path: String,
    boards: Vec<(String, String)>,
    selected_board_idx: usize,
    ports: Vec<SerialPortInfo>,
    selected_port: Option<String>,
    logs: VecDeque<String>,
    libraries_dir: PathBuf,
    libraries: Vec<String>,
    lib_index: Vec<LibInfo>,
    lib_by_name: HashMap<String, usize>,
    lib_filtered: Vec<usize>,
    lib_query: String,
    lib_tab: LibTab,
    index_state: IndexState,
    index_age_days: Option<u64>,
    reload_index_after: bool,
    installed: Vec<InstalledLib>,
    show_lib_manager: bool,
    last_saved: (String, usize, u32),

    serial_port: Option<Box<dyn serialport::SerialPort>>,
    serial_buf: String,
    serial_lines: VecDeque<String>,
    serial_input: String,
    baud_rate: u32,
    plotter_data: VecDeque<f64>,

    tab: BottomTab,
    busy: Option<&'static str>,
    show_settings: bool,

    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

fn default_libraries_dir() -> PathBuf {
    // arduino-cli на Linux зберігає бібліотеки в ~/Arduino/libraries,
    // на Windows і macOS - у Документи/Arduino/libraries
    let dir = if cfg!(target_os = "linux") {
        dirs::home_dir().map(|h| h.join("Arduino/libraries"))
    } else {
        dirs::document_dir().map(|d| d.join("Arduino/libraries"))
    };
    dir.unwrap_or_else(|| PathBuf::from("./libraries"))
}

fn port_label(p: &SerialPortInfo) -> String {
    match &p.port_type {
        SerialPortType::UsbPort(info) => match &info.product {
            Some(prod) => format!("{} - {}", p.port_name, prod),
            None => format!("{} - USB", p.port_name),
        },
        _ => p.port_name.clone(),
    }
}

impl Default for ArduinoIde {
    fn default() -> Self {
        let (tx, rx) = channel();
        let mut this = Self {
            sketch: DEFAULT_SKETCH.to_string(),
            arduino_cli_path: "arduino-cli".to_string(),
            boards: DEFAULT_BOARDS
                .iter()
                .map(|(n, f)| (n.to_string(), f.to_string()))
                .collect(),
            selected_board_idx: 0,
            ports: Vec::new(),
            selected_port: None,
            logs: VecDeque::new(),
            libraries_dir: default_libraries_dir(),
            libraries: Vec::new(),
            lib_index: Vec::new(),
            lib_by_name: HashMap::new(),
            lib_filtered: Vec::new(),
            lib_query: String::new(),
            lib_tab: LibTab::Catalog,
            index_state: IndexState::Loading,
            index_age_days: None,
            reload_index_after: false,
            installed: Vec::new(),
            show_lib_manager: false,
            last_saved: (String::new(), 0, 9600),
            serial_port: None,
            serial_buf: String::new(),
            serial_lines: VecDeque::new(),
            serial_input: String::new(),
            baud_rate: 9600,
            plotter_data: VecDeque::with_capacity(200),
            tab: BottomTab::Console,
            busy: None,
            show_settings: false,
            tx,
            rx,
        };
        // Шлях до arduino-cli, плата і швидкість: спочатку автопошук, потім збережені значення
        this.arduino_cli_path = detect_arduino_cli();
        let st = load_settings();
        if let Some(p) = st.arduino_cli_path {
            if !p.trim().is_empty() {
                this.arduino_cli_path = p;
            }
        }
        if let Some(i) = st.board_idx {
            if i < this.boards.len() {
                this.selected_board_idx = i;
            }
        }
        if let Some(b) = st.baud_rate {
            this.baud_rate = b;
        }
        this.last_saved = (
            this.arduino_cli_path.clone(),
            this.selected_board_idx,
            this.baud_rate,
        );
        this.log("Arduino IDE для дітей запущено");
        this.refresh_ports();
        this.refresh_libraries();
        this.start_index_load();
        this
    }
}

impl ArduinoIde {
    fn log(&mut self, msg: impl ToString) {
        self.logs.push_back(msg.to_string());
        while self.logs.len() > 1000 {
            self.logs.pop_front();
        }
    }

    fn refresh_ports(&mut self) {
        match available_ports() {
            Ok(mut ports) => {
                // USB-порти (справжні Arduino) показуємо першими
                ports.sort_by_key(|p| !matches!(p.port_type, SerialPortType::UsbPort(_)));
                self.ports = ports;
                let still_there = self
                    .selected_port
                    .as_ref()
                    .map(|n| self.ports.iter().any(|p| &p.port_name == n))
                    .unwrap_or(false);
                if !still_there {
                    self.selected_port = self.ports.first().map(|p| p.port_name.clone());
                }
                let n = self.ports.len();
                self.log(format!("Знайдено портів: {}", n));
            }
            Err(e) => self.log(format!("❌ Помилка сканування портів: {}", e)),
        }
    }

    fn refresh_libraries(&mut self) {
        self.libraries.clear();
        if let Ok(entries) = std::fs::read_dir(&self.libraries_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    if let Ok(name) = entry.file_name().into_string() {
                        self.libraries.push(name);
                    }
                }
            }
        }
        self.libraries.sort();
        self.installed = read_installed_libs(&self.libraries_dir);
    }

    /// arduino-cli вимагає, щоб папка скетчу називалася так само, як і .ino файл
    fn save_sketch(&mut self) -> Option<String> {
        let dir = std::env::temp_dir().join("arduino_ide_rust").join("sketch");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.log(format!("❌ Не вдалося створити папку скетчу: {}", e));
            return None;
        }
        let file = dir.join("sketch.ino");
        if let Err(e) = std::fs::write(&file, &self.sketch) {
            self.log(format!("❌ Не вдалося зберегти скетч: {}", e));
            return None;
        }
        Some(dir.to_string_lossy().to_string())
    }

    /// Запускає одну або кілька команд arduino-cli в окремому потоці,
    /// щоб вікно не зависало
    fn spawn_cli(&mut self, ctx: &egui::Context, label: &'static str, cmds: Vec<Vec<String>>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(label);
        let tx = self.tx.clone();
        let cli = self.arduino_cli_path.clone();
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            for args in cmds {
                match Command::new(&cli).args(&args).output() {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                        for line in stdout.lines().chain(stderr.lines()) {
                            if !line.trim().is_empty() {
                                let _ = tx.send(Msg::Log(line.to_string()));
                            }
                        }
                        if out.status.success() {
                            let _ = tx.send(Msg::Log("✅ Готово".to_string()));
                        } else {
                            let _ = tx.send(Msg::Log("❌ Завершено з помилкою".to_string()));
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Log(format!(
                            "❌ Не вдалося запустити arduino-cli: {}",
                            e
                        )));
                        let _ = tx.send(Msg::Log(
                            "Відкрийте ⚙️ Налаштування і перевірте шлях до arduino-cli".to_string(),
                        ));
                        break;
                    }
                }
            }
            let _ = tx.send(Msg::Done);
            ctx.request_repaint();
        });
    }

    fn compile(&mut self, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        let Some(dir) = self.save_sketch() else {
            return;
        };
        let (name, fqbn) = self.boards[self.selected_board_idx].clone();
        self.tab = BottomTab::Console;
        self.log(format!("Компіляція для {} ...", name));
        self.spawn_cli(
            ctx,
            "Компіляція...",
            vec![vec!["compile".into(), "--fqbn".into(), fqbn, dir]],
        );
    }

    fn upload(&mut self, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        let Some(port) = self.selected_port.clone() else {
            self.log("❌ Не вибрано порт (зліва). Підключіть Arduino і натисніть 🔄");
            self.tab = BottomTab::Console;
            return;
        };
        let Some(dir) = self.save_sketch() else {
            return;
        };
        // Порт має бути вільним на час завантаження
        if self.serial_port.is_some() {
            self.disconnect_serial();
        }
        let (name, fqbn) = self.boards[self.selected_board_idx].clone();
        self.tab = BottomTab::Console;
        self.log(format!("Завантаження на {} ({}) ...", port, name));
        self.spawn_cli(
            ctx,
            "Завантаження...",
            vec![vec![
                "compile".into(),
                "--upload".into(),
                "-p".into(),
                port,
                "--fqbn".into(),
                fqbn,
                dir,
            ]],
        );
    }

    /// Читає каталог бібліотек у фоні (швидко, без інтернету)
    fn start_index_load(&mut self) {
        self.index_state = IndexState::Loading;
        let tx = self.tx.clone();
        let path = arduino_data_dir().join("library_index.json");
        std::thread::spawn(move || {
            let _ = tx.send(Msg::IndexLoaded(load_library_index(&path)));
        });
    }

    /// Миттєвий пошук по локальному каталогу
    fn apply_filter(&mut self) {
        let q = self.lib_query.trim().to_lowercase();
        let terms: Vec<&str> = q.split_whitespace().collect();
        if terms.is_empty() {
            self.lib_filtered = (0..self.lib_index.len()).collect();
            return;
        }
        let mut hits: Vec<(bool, usize)> = Vec::new();
        for (i, l) in self.lib_index.iter().enumerate() {
            if terms.iter().all(|t| l.search_key.contains(*t)) {
                // спочатку ті, у яких збіг у назві
                hits.push((!l.name_lower.contains(terms[0]), i));
            }
        }
        hits.sort();
        self.lib_filtered = hits.into_iter().map(|(_, i)| i).collect();
    }

    /// Завантажує свіжий каталог з інтернету (arduino-cli lib update-index)
    fn update_index(&mut self, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        self.tab = BottomTab::Console;
        self.log("Оновлення каталогу бібліотек (потрібен інтернет)...");
        self.reload_index_after = true;
        self.spawn_cli(
            ctx,
            "Оновлення каталогу...",
            vec![vec!["lib".into(), "update-index".into()]],
        );
    }

    fn install_library(&mut self, ctx: &egui::Context, name: &str, upgrade: bool) {
        if self.busy.is_some() {
            return;
        }
        self.tab = BottomTab::Console;
        let (verb, label): (&str, &'static str) = if upgrade {
            ("upgrade", "Оновлення бібліотеки...")
        } else {
            ("install", "Встановлення бібліотеки...")
        };
        self.log(format!(
            "{} бібліотеки {} ...",
            if upgrade { "Оновлення" } else { "Встановлення" },
            name
        ));
        self.spawn_cli(
            ctx,
            label,
            vec![vec!["lib".into(), verb.into(), name.to_string()]],
        );
    }

    fn uninstall_library_cli(&mut self, ctx: &egui::Context, name: &str) {
        if self.busy.is_some() {
            return;
        }
        self.tab = BottomTab::Console;
        self.log(format!("Видалення бібліотеки {} ...", name));
        self.spawn_cli(
            ctx,
            "Видалення бібліотеки...",
            vec![vec!["lib".into(), "uninstall".into(), name.to_string()]],
        );
    }

    fn remove_library(&mut self, name: &str) {
        let path = self.libraries_dir.join(name);
        if path.exists() {
            match std::fs::remove_dir_all(&path) {
                Ok(_) => self.log(format!("🗑️ Бібліотеку {} видалено", name)),
                Err(e) => self.log(format!("❌ Не вдалося видалити {}: {}", name, e)),
            }
            self.refresh_libraries();
        }
    }

    fn open_library_folder(&mut self) {
        let _ = std::fs::create_dir_all(&self.libraries_dir);
        let opener = if cfg!(target_os = "windows") {
            "explorer"
        } else if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = Command::new(opener).arg(&self.libraries_dir).spawn();
    }

    fn connect_serial(&mut self) {
        let Some(port_name) = self.selected_port.clone() else {
            self.log("❌ Оберіть порт зліва!");
            return;
        };
        if port_name.trim().is_empty() {
            self.log("❌ Не вибрано порт: ім'я порту порожнє.");
            return;
        }
        match serialport::new(&port_name, self.baud_rate)
            .timeout(Duration::from_millis(10))
            .open()
        {
            Ok(p) => {
                self.serial_port = Some(p);
                self.serial_buf.clear();
                self.log(format!("🔌 Підключено до {} @ {}", port_name, self.baud_rate));
                self.tab = BottomTab::Monitor;
            }
            Err(e) => self.log(format!("❌ Не вдалося підключитися: {}", e)),
        }
    }

    fn disconnect_serial(&mut self) {
        self.serial_port = None;
        self.log("🔌 Відключено");
    }

    fn push_serial_line(&mut self, line: String) {
        let first = line
            .split(|c: char| c == ',' || c.is_whitespace())
            .next()
            .unwrap_or("");
        if let Ok(val) = first.parse::<f64>() {
            self.plotter_data.push_back(val);
            while self.plotter_data.len() > 200 {
                self.plotter_data.pop_front();
            }
        }
        self.serial_lines.push_back(line);
        while self.serial_lines.len() > 2000 {
            self.serial_lines.pop_front();
        }
    }

    fn read_serial(&mut self) {
        let mut error: Option<String> = None;
        let mut received = String::new();

        if let Some(port) = self.serial_port.as_mut() {
            let mut buf = [0u8; 1024];
            for _ in 0..8 {
                match port.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        received.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if n < buf.len() {
                            break;
                        }
                    }
                    Err(ref e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        break
                    }
                    Err(e) => {
                        error = Some(e.to_string());
                        break;
                    }
                }
            }
        }

        if !received.is_empty() {
            self.serial_buf.push_str(&received);
            while let Some(pos) = self.serial_buf.find('\n') {
                let line: String = self.serial_buf.drain(..=pos).collect();
                let line = line.trim().to_string();
                if !line.is_empty() {
                    self.push_serial_line(line);
                }
            }
            if self.serial_buf.len() > 10_000 {
                self.serial_buf.clear();
            }
        }

        if let Some(e) = error {
            self.log(format!("❌ Помилка порту: {}", e));
            self.disconnect_serial();
        }
    }

    fn write_serial(&mut self, text: &str) {
        let mut error = None;
        if let Some(port) = self.serial_port.as_mut() {
            if let Err(e) = port.write_all(text.as_bytes()) {
                error = Some(e.to_string());
            }
        }
        if let Some(e) = error {
            self.log(format!("❌ Помилка надсилання: {}", e));
            self.disconnect_serial();
        } else {
            self.push_serial_line(format!("➡ {}", text.trim_end()));
        }
    }
}

// ---------- Дизайн ----------

/// Яскрава тема: заокруглені кути, великі шрифти, кольорові акценти
fn setup_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = EDITOR_BG;
    v.faint_bg_color = CARD;
    v.window_rounding = 14.0.into();
    v.menu_rounding = 10.0.into();
    v.selection.bg_fill = LINE;
    v.selection.stroke = egui::Stroke::new(1.0_f32, Color32::WHITE);

    let r = egui::Rounding::same(10.0);
    v.widgets.noninteractive.rounding = r;
    v.widgets.inactive.rounding = r;
    v.widgets.hovered.rounding = r;
    v.widgets.active.rounding = r;
    v.widgets.open.rounding = r;

    v.widgets.inactive.bg_fill = CARD;
    v.widgets.inactive.weak_bg_fill = CARD;
    v.widgets.hovered.bg_fill = HOVER;
    v.widgets.hovered.weak_bg_fill = HOVER;
    v.widgets.active.bg_fill = LINE;
    v.widgets.active.weak_bg_fill = LINE;
    ctx.set_visuals(v);

    ctx.style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(22.0));
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(16.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(16.0));
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(15.0));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(13.0));
    });
}

/// Велика кольорова кнопка
fn big_button(
    ui: &mut egui::Ui,
    enabled: bool,
    text: &str,
    color: Color32,
    min_size: egui::Vec2,
) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).size(17.0).strong().color(Color32::WHITE))
            .fill(color)
            .rounding(12.0)
            .min_size(min_size),
    )
}

/// Кольорова картка з заголовком
fn card(ui: &mut egui::Ui, title: &str, color: Color32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(CARD)
        .rounding(14.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(18.0).strong().color(color));
            ui.add_space(4.0);
            add(ui);
        });
}

/// Ледь помітний візерунок із крапок на фоні редактора
fn draw_dots(ui: &egui::Ui) {
    let rect = ui.max_rect().expand(10.0);
    let painter = ui.painter().with_clip_rect(rect);
    let color = Color32::from_rgb(28, 42, 76);
    let step = 28.0;
    let mut y = rect.top() + step / 2.0;
    while y < rect.bottom() {
        let mut x = rect.left() + step / 2.0;
        while x < rect.right() {
            painter.circle_filled(egui::pos2(x, y), 1.2, color);
            x += step;
        }
        y += step;
    }
}

fn push(job: &mut LayoutJob, s: &str, color: Color32) {
    job.append(
        s,
        0.0,
        TextFormat {
            font_id: FontId::monospace(15.0),
            color,
            ..Default::default()
        },
    );
}

/// Підсвічування синтаксису C++ прямо під час редагування
fn highlight_job(text: &str) -> LayoutJob {
    let mut job = LayoutJob::default();
    if text.is_empty() {
        push(&mut job, "", COLOR_TEXT);
        return job;
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let slice = |a: usize, b: usize| -> String { chars[a..b].iter().collect() };
    let mut i = 0;

    while i < n {
        let c = chars[i];

        // // коментар
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            let start = i;
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            push(&mut job, &slice(start, i), COLOR_COMMENT);
            continue;
        }
        // /* коментар */
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            let start = i;
            i += 2;
            while i < n && !(chars[i] == '*' && i + 1 < n && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(n);
            push(&mut job, &slice(start, i), COLOR_COMMENT);
            continue;
        }
        // "рядок" і 'символ'
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            i += 1;
            while i < n && chars[i] != quote && chars[i] != '\n' {
                if chars[i] == '\\' && i + 1 < n && chars[i + 1] != '\n' {
                    i += 1;
                }
                i += 1;
            }
            if i < n && chars[i] == quote {
                i += 1;
            }
            let color = if quote == '"' { COLOR_STRING } else { COLOR_CHAR };
            push(&mut job, &slice(start, i), color);
            continue;
        }
        // #include, #define
        if c == '#' {
            let start = i;
            i += 1;
            while i < n && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            push(&mut job, &slice(start, i), COLOR_KEYWORD);
            continue;
        }
        // числа
        if c.is_ascii_digit() || (c == '.' && i + 1 < n && chars[i + 1].is_ascii_digit()) {
            let start = i;
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            push(&mut job, &slice(start, i), COLOR_NUMBER);
            continue;
        }
        // слова
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let ident = slice(start, i);
            let color = if CPP_KEYWORDS.contains(&ident.as_str()) {
                COLOR_KEYWORD
            } else {
                COLOR_TEXT
            };
            push(&mut job, &ident, color);
            continue;
        }
        // оператори і все інше
        let color = if "+-*/=<>!&|^%.,;:()[]{}".contains(c) {
            COLOR_OPERATOR
        } else {
            COLOR_TEXT
        };
        push(&mut job, &c.to_string(), color);
        i += 1;
    }
    job
}

fn draw_lines(ui: &mut egui::Ui, lines: &VecDeque<String>) {
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show_rows(ui, row_h, lines.len(), |ui, range| {
            for i in range {
                let line = &lines[i];
                let mut text = RichText::new(line.as_str()).monospace();
                if line.starts_with('❌') {
                    text = text.color(Color32::from_rgb(255, 120, 120));
                } else if line.starts_with('✅') {
                    text = text.color(Color32::from_rgb(100, 230, 120));
                } else if line.starts_with('➡') {
                    text = text.color(ACCENT_LIGHT);
                }
                ui.label(text);
            }
        });
}

fn draw_serial_plotter(ui: &mut egui::Ui, data: &VecDeque<f64>) {
    if data.is_empty() {
        ui.label("📊 На графіку поки немає даних");
        ui.label("Надсилайте числа зі скетчу: Serial.println(значення);");
        return;
    }

    let width = ui.available_width().max(250.0);
    let height = (ui.available_height() - 4.0).clamp(100.0, 400.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());

    let min_val = data.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_val = data.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = if (max_val - min_val).abs() < 0.001 {
        1.0
    } else {
        max_val - min_val
    };
    let padding = range * 0.1;
    let y_min = min_val - padding;
    let y_max = max_val + padding;

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 10.0, EDITOR_BG);
    painter.rect_stroke(rect, 10.0, egui::Stroke::new(1.5_f32, LINE));

    // Сітка
    for k in 1..4 {
        let y = rect.top() + rect.height() * k as f32 / 4.0;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(1.0_f32, Color32::from_rgb(32, 46, 82)),
        );
    }

    let n = data.len();
    if n > 1 {
        let mut points = Vec::with_capacity(n);
        for (idx, val) in data.iter().enumerate() {
            let x = rect.left() + rect.width() * idx as f32 / (n - 1) as f32;
            let t = ((*val - y_min) / (y_max - y_min)).clamp(0.0, 1.0) as f32;
            let y = rect.bottom() - rect.height() * t;
            points.push(egui::pos2(x, y));
        }
        painter.add(egui::Shape::line(points, egui::Stroke::new(3.0_f32, ACCENT_LIGHT)));
    }

    let gray = Color32::from_rgb(170, 170, 200);
    painter.text(
        rect.left_top() + egui::vec2(8.0, 6.0),
        egui::Align2::LEFT_TOP,
        format!("{:.1}", max_val),
        FontId::monospace(12.0),
        gray,
    );
    painter.text(
        rect.left_bottom() + egui::vec2(8.0, -6.0),
        egui::Align2::LEFT_BOTTOM,
        format!("{:.1}", min_val),
        FontId::monospace(12.0),
        gray,
    );
    if let Some(last) = data.back() {
        painter.text(
            rect.right_top() + egui::vec2(-10.0, 8.0),
            egui::Align2::RIGHT_TOP,
            format!("{:.1}", last),
            FontId::monospace(16.0),
            ACCENT_LIGHT,
        );
    }
}

impl eframe::App for ArduinoIde {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Автозбереження налаштувань, якщо щось змінилося
        let cur = (
            self.arduino_cli_path.clone(),
            self.selected_board_idx,
            self.baud_rate,
        );
        if cur != self.last_saved {
            save_settings(&cur);
            self.last_saved = cur;
        }

        // Повідомлення з фонового потоку arduino-cli
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Log(s) => self.log(s),
                Msg::Done => {
                    self.busy = None;
                    self.refresh_libraries();
                    if self.reload_index_after {
                        self.reload_index_after = false;
                        self.start_index_load();
                    }
                }
                Msg::IndexLoaded(res) => match res {
                    Ok(data) => {
                        self.lib_by_name = data
                            .libs
                            .iter()
                            .enumerate()
                            .map(|(i, l)| (l.name_lower.clone(), i))
                            .collect();
                        self.lib_index = data.libs;
                        self.index_age_days = data.age_days;
                        self.index_state = IndexState::Ready;
                        self.apply_filter();
                        let n = self.lib_index.len();
                        self.log(format!("📚 Каталог бібліотек завантажено: {}", n));
                    }
                    Err(e) => {
                        if e != "missing" {
                            self.log(format!("❌ {}", e));
                        }
                        self.index_state = IndexState::Error(e);
                    }
                },
            }
        }

        if self.serial_port.is_some() {
            self.read_serial();
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if self.busy.is_some() || self.index_state == IndexState::Loading {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let is_busy = self.busy.is_some();

        // ---------- Верхній рядок ----------
        egui::TopBottomPanel::top("status_bar")
            .frame(
                egui::Frame::none()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(14.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Arduino Rust")
                            .size(22.0)
                            .strong()
                            .color(ACCENT_LIGHT),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(format!("Плата: {}", self.boards[self.selected_board_idx].0))
                            .color(ACCENT_LIGHT),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(format!(
                            "Порт: {}",
                            self.selected_port
                                .clone()
                                .unwrap_or_else(|| "не вибрано".to_string())
                        ))
                        .color(ACCENT_LIGHT),
                    );
                    if let Some(label) = self.busy {
                        ui.separator();
                        ui.spinner();
                        ui.label(RichText::new(label).color(ACCENT_LIGHT).strong());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("⚙️").size(18.0))
                                    .fill(CARD)
                                    .rounding(12.0),
                            )
                            .on_hover_text("Налаштування")
                            .clicked()
                        {
                            self.show_settings = true;
                        }
                    });
                });
            });

        // ---------- Нижня панель (консоль / монітор / графік) ----------
        let max_bottom = (ctx.screen_rect().height() * 0.6).max(150.0);
        egui::TopBottomPanel::bottom("bottom_panel")
            .resizable(true)
            .default_height(240.0)
            .min_height(120.0)
            .max_height(max_bottom)
            .frame(egui::Frame::none().fill(PANEL).inner_margin(10.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let tabs = [
                        (BottomTab::Console, "🖥 Консоль", ACCENT),
                        (BottomTab::Monitor, "📡 Монітор порту", PRIMARY),
                        (BottomTab::Plotter, "📈 Графік", ACCENT),
                    ];
                    for (t, label, color) in tabs {
                        let selected = self.tab == t;
                        let text = RichText::new(label)
                            .strong()
                            .color(Color32::WHITE);
                        let fill = if selected { color } else { CARD };
                        if ui
                            .add(egui::Button::new(text).fill(fill).rounding(10.0))
                            .clicked()
                        {
                            self.tab = t;
                        }
                    }
                    ui.separator();

                    ui.label("Швидкість:");
                    let connected = self.serial_port.is_some();
                    ui.add_enabled_ui(!connected, |ui| {
                        egui::ComboBox::from_id_source("baud_bottom")
                            .selected_text(self.baud_rate.to_string())
                            .show_ui(ui, |ui| {
                                for baud in [9600u32, 19200, 38400, 57600, 115200] {
                                    ui.selectable_value(&mut self.baud_rate, baud, baud.to_string());
                                }
                            });
                    });

                    if connected {
                        if big_button(ui, true, "❌ Відключити", RED, egui::vec2(0.0, 0.0)).clicked() {
                            self.disconnect_serial();
                        }
                    } else if big_button(ui, true, "🔌 Підключити", PRIMARY, egui::vec2(0.0, 0.0))
                        .clicked()
                    {
                        self.connect_serial();
                    }

                    if ui.button("🧹 Очистити").clicked() {
                        match self.tab {
                            BottomTab::Console => self.logs.clear(),
                            BottomTab::Monitor => self.serial_lines.clear(),
                            BottomTab::Plotter => self.plotter_data.clear(),
                        }
                    }
                });
                ui.separator();

                // Рядок надсилання в Arduino
                if self.serial_port.is_some() && self.tab == BottomTab::Monitor {
                    let mut send = false;
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.serial_input)
                                .desired_width((ui.available_width() - 160.0).max(100.0))
                                .hint_text("Надіслати в Arduino..."),
                        );
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            send = true;
                            resp.request_focus();
                        }
                        if big_button(ui, true, "Надіслати", ACCENT, egui::vec2(0.0, 0.0)).clicked() {
                            send = true;
                        }
                    });
                    if send && !self.serial_input.is_empty() {
                        let text = format!("{}\n", self.serial_input);
                        self.write_serial(&text);
                        self.serial_input.clear();
                    }
                }

                match self.tab {
                    BottomTab::Console => draw_lines(ui, &self.logs),
                    BottomTab::Monitor => {
                        if self.serial_port.is_none() && self.serial_lines.is_empty() {
                            ui.label("🔌 Оберіть порт зліва і натисніть «Підключити»");
                        } else {
                            draw_lines(ui, &self.serial_lines);
                        }
                    }
                    BottomTab::Plotter => {
                        if self.serial_port.is_none() && self.plotter_data.is_empty() {
                            ui.label("🔌 Підключіться до Arduino, щоб бачити графік");
                        } else {
                            draw_serial_plotter(ui, &self.plotter_data);
                        }
                    }
                }
            });

        // ---------- Бічна панель: плата, порт, бібліотеки ----------
        egui::SidePanel::left("side_panel")
            .resizable(true)
            .default_width(270.0)
            .min_width(220.0)
            .max_width(380.0)
            .frame(egui::Frame::none().fill(PANEL).inner_margin(12.0))
            .show(ctx, |ui| {
                // --- Плата ---
                let mut idx = self.selected_board_idx;
                card(ui, "🔧 Плата", ACCENT_LIGHT, |ui| {
                    egui::ComboBox::from_id_source("board_combo")
                        .width((ui.available_width() - 8.0).max(100.0))
                        .selected_text(self.boards[idx].0.clone())
                        .show_ui(ui, |ui| {
                            for (i, (name, _)) in self.boards.iter().enumerate() {
                                ui.selectable_value(&mut idx, i, name.as_str());
                            }
                        });
                });
                self.selected_board_idx = idx;

                ui.add_space(8.0);

                // --- Порт ---
                let mut sel = self.selected_port.clone();
                let mut refresh = false;
                card(ui, "🔌 Порт", ACCENT_LIGHT, |ui| {
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_source("port_combo")
                            .width((ui.available_width() - 80.0).max(80.0))
                            .selected_text(sel.clone().unwrap_or_else(|| "не вибрано".to_string()))
                            .show_ui(ui, |ui| {
                                for p in &self.ports {
                                    ui.selectable_value(
                                        &mut sel,
                                        Some(p.port_name.clone()),
                                        port_label(p),
                                    );
                                }
                            });
                        if ui.button("🔄").on_hover_text("Оновити список портів").clicked() {
                            refresh = true;
                        }
                    });
                    if self.ports.is_empty() {
                        ui.label(
                            RichText::new("Підключіть Arduino USB-кабелем і натисніть 🔄")
                                .small()
                                .weak(),
                        );
                    }
                });
                self.selected_port = sel;
                if refresh {
                    self.refresh_ports();
                }

                ui.add_space(8.0);

                // --- Бібліотеки ---
                let mut open_manager = false;
                let mut to_remove: Option<String> = None;
                let mut open_folder = false;
                card(ui, "📚 Бібліотеки", ACCENT_LIGHT, |ui| {
                    if big_button(
                        ui,
                        true,
                        "🔎 Менеджер бібліотек",
                        PRIMARY,
                        egui::vec2((ui.available_width() - 4.0).max(60.0), 34.0),
                    )
                    .clicked()
                    {
                        open_manager = true;
                    }
                    ui.add_space(4.0);

                    egui::ScrollArea::vertical()
                        .id_source("libs_scroll")
                        .auto_shrink([false, true])
                        .max_height(180.0)
                        .show(ui, |ui| {
                            if self.libraries.is_empty() {
                                ui.label(RichText::new("Поки немає бібліотек").weak());
                            }
                            for lib in &self.libraries {
                                ui.horizontal(|ui| {
                                    if ui.small_button("🗑").on_hover_text("Видалити").clicked() {
                                        to_remove = Some(lib.clone());
                                    }
                                    ui.label(lib.as_str());
                                });
                            }
                        });

                    if ui.button("📁 Відкрити папку").clicked() {
                        open_folder = true;
                    }
                });
                if open_manager {
                    self.show_lib_manager = true;
                }
                if let Some(name) = to_remove {
                    self.remove_library(&name);
                }
                if open_folder {
                    self.open_library_folder();
                }
            });

        // ---------- Центр: редактор коду ----------
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG).inner_margin(12.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if big_button(ui, !is_busy, "✅ Перевірити", PRIMARY, egui::vec2(130.0, 38.0))
                        .on_hover_text("Перевірити, чи правильний код")
                        .clicked()
                    {
                        self.compile(ctx);
                    }
                    if big_button(
                        ui,
                        !is_busy,
                        "⬆️ Завантажити на Arduino",
                        ACCENT,
                        egui::vec2(210.0, 38.0),
                    )
                    .on_hover_text("Відправити програму на плату")
                    .clicked()
                    {
                        self.upload(ctx);
                    }
                    if big_button(ui, true, "↺ Скинути код", SECONDARY, egui::vec2(130.0, 38.0))
                        .on_hover_text("Повернути приклад із мигаючим світлодіодом")
                        .clicked()
                    {
                        self.sketch = DEFAULT_SKETCH.to_string();
                    }
                });
                ui.add_space(8.0);

                egui::Frame::none()
                    .fill(EDITOR_BG)
                    .stroke(egui::Stroke::new(1.5_f32, LINE))
                    .rounding(14.0)
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        draw_dots(ui);
                        let mut layouter = |ui: &egui::Ui, text: &str, wrap_width: f32| {
                            let mut job = highlight_job(text);
                            job.wrap.max_width = wrap_width;
                            ui.fonts(|f| f.layout_job(job))
                        };

                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let rows = ((ui.available_height() / 20.0) as usize).max(10);
                                ui.add(
                                    egui::TextEdit::multiline(&mut self.sketch)
                                        .code_editor()
                                        .frame(false)
                                        .desired_rows(rows)
                                        .desired_width(f32::INFINITY)
                                        .lock_focus(true)
                                        .layouter(&mut layouter),
                                );
                            });
                    });
            });

        // ---------- Менеджер бібліотек ----------
        if self.show_lib_manager {
            let mut open = true;
            let mut query_changed = false;
            let mut do_refresh = false;
            let mut action: Option<(String, LibAction)> = None;
            let installed_map: HashMap<String, String> = self
                .installed
                .iter()
                .map(|l| (l.name.to_lowercase(), l.version.clone()))
                .collect();

            egui::Window::new("📚 Менеджер бібліотек")
                .open(&mut open)
                .default_size([700.0, 540.0])
                .resizable(true)
                .collapsible(false)
                .show(ctx, |ui| {
                    // --- пошук і оновлення каталогу ---
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.lib_query)
                                .hint_text("Пошук: Servo, LCD, DHT...")
                                .desired_width((ui.available_width() - 240.0).max(120.0)),
                        );
                        if resp.changed() {
                            query_changed = true;
                        }
                        if big_button(ui, !is_busy, "🔄 Оновити каталог", PRIMARY, egui::vec2(0.0, 0.0))
                            .on_hover_text("Завантажити свіжий список бібліотек з інтернету")
                            .clicked()
                        {
                            do_refresh = true;
                        }
                    });

                    // --- вкладки ---
                    ui.horizontal(|ui| {
                        let tabs = [
                            (LibTab::Catalog, "📦 Каталог"),
                            (LibTab::Installed, "✅ Встановлені"),
                        ];
                        for (t, label) in tabs {
                            let selected = self.lib_tab == t;
                            let b = egui::Button::new(
                                RichText::new(label).strong().color(Color32::WHITE),
                            )
                            .fill(if selected { ACCENT } else { CARD })
                            .rounding(10.0);
                            if ui.add(b).clicked() {
                                self.lib_tab = t;
                            }
                        }
                        if let Some(label) = self.busy {
                            ui.spinner();
                            ui.label(RichText::new(label).small().color(ACCENT_LIGHT));
                        }
                    });

                    let status = match &self.index_state {
                        IndexState::Loading => "⏳ Читаю каталог...".to_string(),
                        IndexState::Ready => format!(
                            "Бібліотек у каталозі: {} · оновлено: {}",
                            self.lib_index.len(),
                            match self.index_age_days {
                                Some(0) => "сьогодні".to_string(),
                                Some(d) => format!("{} дн. тому", d),
                                None => "—".to_string(),
                            }
                        ),
                        IndexState::Error(_) => String::new(),
                    };
                    if !status.is_empty() {
                        ui.label(RichText::new(status).small().color(ACCENT_LIGHT));
                    }
                    ui.add_space(4.0);

                    match self.lib_tab {
                        LibTab::Catalog => match &self.index_state {
                            IndexState::Error(e) if self.lib_index.is_empty() => {
                                if e.as_str() == "missing" {
                                    ui.label("Каталог ще не завантажено. Натисніть «Оновити каталог» (потрібен інтернет).");
                                } else {
                                    ui.label(e.as_str());
                                }
                            }
                            IndexState::Loading if self.lib_index.is_empty() => {
                                ui.spinner();
                            }
                            _ => {
                                if self.lib_filtered.is_empty() {
                                    ui.label("Нічого не знайдено");
                                } else {
                                    let row_h = 66.0;
                                    let total = self.lib_filtered.len();
                                    egui::ScrollArea::vertical()
                                        .id_source("catalog_scroll")
                                        .auto_shrink([false, false])
                                        .show_rows(ui, row_h, total, |ui, range| {
                                            for i in range {
                                                let lib = &self.lib_index[self.lib_filtered[i]];
                                                let installed_ver = installed_map.get(&lib.name_lower);
                                                egui::Frame::none()
                                                    .fill(CARD)
                                                    .rounding(10.0)
                                                    .inner_margin(8.0)
                                                    .show(ui, |ui| {
                                                        ui.set_width(ui.available_width());
                                                        ui.set_height(row_h - 16.0);
                                                        ui.horizontal(|ui| {
                                                            let left_w = (ui.available_width() - 190.0).max(100.0);
                                                            ui.allocate_ui(
                                                                egui::vec2(left_w, row_h - 16.0),
                                                                |ui| {
                                                                    ui.vertical(|ui| {
                                                                        ui.horizontal(|ui| {
                                                                            ui.label(
                                                                                RichText::new(&lib.name)
                                                                                    .strong()
                                                                                    .size(16.0)
                                                                                    .color(Color32::WHITE),
                                                                            );
                                                                            ui.label(
                                                                                RichText::new(format!("v{}", lib.version))
                                                                                    .small()
                                                                                    .color(ACCENT_LIGHT),
                                                                            );
                                                                            if let Some(v) = installed_ver {
                                                                                ui.label(
                                                                                    RichText::new(format!("✅ v{}", v))
                                                                                        .small()
                                                                                        .color(OK_GREEN),
                                                                                );
                                                                            }
                                                                        });
                                                                        let info = if lib.author.is_empty() {
                                                                            lib.sentence.clone()
                                                                        } else {
                                                                            format!("{} — {}", lib.author, lib.sentence)
                                                                        };
                                                                        ui.label(
                                                                            RichText::new(truncate_text(
                                                                                &info,
                                                                                (left_w / 7.0) as usize,
                                                                            ))
                                                                            .small()
                                                                            .weak(),
                                                                        );
                                                                    });
                                                                },
                                                            );
                                                            ui.with_layout(
                                                                egui::Layout::right_to_left(egui::Align::Center),
                                                                |ui| match installed_ver {
                                                                    Some(v)
                                                                        if version_key(v) < version_key(&lib.version) =>
                                                                    {
                                                                        if big_button(ui, !is_busy, "⬆ Оновити", ACCENT, egui::vec2(0.0, 34.0))
                                                                            .clicked()
                                                                        {
                                                                            action = Some((lib.name.clone(), LibAction::Upgrade));
                                                                        }
                                                                    }
                                                                    Some(_) => {
                                                                        ui.label(RichText::new("Встановлено").color(OK_GREEN));
                                                                    }
                                                                    None => {
                                                                        if big_button(ui, !is_busy, "⬇ Встановити", PRIMARY, egui::vec2(0.0, 34.0))
                                                                            .clicked()
                                                                        {
                                                                            action = Some((lib.name.clone(), LibAction::Install));
                                                                        }
                                                                    }
                                                                },
                                                            );
                                                        });
                                                    });
                                            }
                                        });
                                }
                            }
                        },
                        LibTab::Installed => {
                            if self.installed.is_empty() {
                                ui.label("Поки немає встановлених бібліотек");
                            }
                            egui::ScrollArea::vertical()
                                .id_source("installed_scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for lib in &self.installed {
                                        let newer = self
                                            .lib_by_name
                                            .get(&lib.name.to_lowercase())
                                            .map(|&i| &self.lib_index[i].version)
                                            .filter(|cv| version_key(cv) > version_key(&lib.version));
                                        egui::Frame::none()
                                            .fill(CARD)
                                            .rounding(10.0)
                                            .inner_margin(8.0)
                                            .show(ui, |ui| {
                                                ui.set_width(ui.available_width());
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        RichText::new(&lib.name)
                                                            .strong()
                                                            .size(16.0)
                                                            .color(Color32::WHITE),
                                                    );
                                                    if !lib.version.is_empty() {
                                                        ui.label(
                                                            RichText::new(format!("v{}", lib.version))
                                                                .small()
                                                                .color(ACCENT_LIGHT),
                                                        );
                                                    }
                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(egui::Align::Center),
                                                        |ui| {
                                                            if big_button(ui, !is_busy, "🗑 Видалити", RED, egui::vec2(0.0, 0.0))
                                                                .clicked()
                                                            {
                                                                action = Some((lib.name.clone(), LibAction::Uninstall));
                                                            }
                                                            if newer.is_some()
                                                                && big_button(ui, !is_busy, "⬆ Оновити", ACCENT, egui::vec2(0.0, 0.0))
                                                                    .clicked()
                                                            {
                                                                action = Some((lib.name.clone(), LibAction::Upgrade));
                                                            }
                                                        },
                                                    );
                                                });
                                            });
                                    }
                                });
                        }
                    }
                });

            self.show_lib_manager = open;
            if query_changed {
                self.apply_filter();
            }
            if do_refresh {
                self.update_index(ctx);
            }
            if let Some((name, act)) = action {
                match act {
                    LibAction::Install => self.install_library(ctx, &name, false),
                    LibAction::Upgrade => self.install_library(ctx, &name, true),
                    LibAction::Uninstall => self.uninstall_library_cli(ctx, &name),
                }
            }
        }

        // ---------- Вікно налаштувань ----------
        if self.show_settings {
            let mut open = true;
            let mut close = false;
            egui::Window::new("⚙️ Налаштування arduino-cli")
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("Шлях до arduino-cli:");
                    ui.text_edit_singleline(&mut self.arduino_cli_path);
                    if ui.button("🔍 Знайти автоматично").clicked() {
                        self.arduino_cli_path = detect_arduino_cli();
                    }
                    ui.add_space(8.0);

                    if big_button(ui, !is_busy, "Перевірити arduino-cli", ACCENT, egui::vec2(0.0, 0.0))
                        .clicked()
                    {
                        self.tab = BottomTab::Console;
                        self.spawn_cli(ctx, "Перевірка...", vec![vec!["version".into()]]);
                    }
                    ui.add_space(4.0);
                    if big_button(
                        ui,
                        !is_busy,
                        "Встановити підтримку Arduino Uno/Nano/Mega",
                        PRIMARY,
                        egui::vec2(0.0, 0.0),
                    )
                    .on_hover_text("Потрібен інтернет. Виконує core update-index та core install arduino:avr")
                    .clicked()
                    {
                        self.tab = BottomTab::Console;
                        self.log("Встановлення пакета arduino:avr (може тривати кілька хвилин)...");
                        self.spawn_cli(
                            ctx,
                            "Встановлення пакета плат...",
                            vec![
                                vec!["core".into(), "update-index".into()],
                                vec!["core".into(), "install".into(), "arduino:avr".into()],
                            ],
                        );
                    }
                    ui.add_space(8.0);
                    if ui.button("Закрити").clicked() {
                        close = true;
                    }
                });
            self.show_settings = open && !close;
        }
    }
}

fn main() -> eframe::Result {
    // Потрібна графічна сесія: Wayland (WAYLAND_DISPLAY) або X11 (DISPLAY)
    if cfg!(target_os = "linux")
        && std::env::var("WAYLAND_DISPLAY").is_err()
        && std::env::var("DISPLAY").is_err()
    {
        eprintln!("[arduino-ide-rust] ❌ Не знайдено графічну сесію (WAYLAND_DISPLAY / DISPLAY).");
        eprintln!("[arduino-ide-rust] Запустіть програму з термінала всередині графічної сесії.");
        std::process::exit(2);
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1150.0, 820.0])
            .with_title("Arduino IDE для дітей")
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Arduino IDE для дітей",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(ArduinoIde::default()))
        }),
    )
}