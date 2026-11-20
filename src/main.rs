use eframe::egui;
use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{Color32, FontId, RichText};
use serialport::{available_ports, SerialPortInfo, SerialPortType};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
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
  pinMode(LED_BUILTIN, OUTPUT);
  Serial.begin(9600);
}

void loop() {
  digitalWrite(LED_BUILTIN, HIGH);
  Serial.println("СВІТЛОДІОД УВІМКНЕНО");
  delay(1000);
  digitalWrite(LED_BUILTIN, LOW);
  Serial.println("СВІТЛОДІОД ВИМКНЕНО");
  delay(1000);
}
"#;

const COLOR_COMMENT: Color32 = Color32::from_rgb(100, 180, 100);
const COLOR_STRING: Color32 = Color32::from_rgb(255, 120, 120);
const COLOR_CHAR: Color32 = Color32::from_rgb(255, 180, 120);
const COLOR_NUMBER: Color32 = Color32::from_rgb(120, 160, 255);
const COLOR_KEYWORD: Color32 = Color32::from_rgb(210, 120, 255);
const COLOR_OPERATOR: Color32 = Color32::from_rgb(180, 180, 210);
const COLOR_TEXT: Color32 = Color32::from_rgb(230, 230, 230);

#[derive(PartialEq, Clone, Copy)]
enum BottomTab {
    Console,
    Monitor,
    Plotter,
}

enum Msg {
    Log(String),
    Done,
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
    new_lib_name: String,

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
    // arduino-cli на Linux хранит библиотеки в ~/Arduino/libraries,
    // на Windows и macOS - в Документы/Arduino/libraries
    let dir = if cfg!(target_os = "linux") {
        dirs::home_dir().map(|h| h.join("Arduino/libraries"))
    } else {
        dirs::document_dir().map(|d| d.join("Arduino/libraries"))
    };
    dir.unwrap_or_else(|| PathBuf::from("./libraries"))
}

fn arduino_cli_config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| {
        dir.join("arduino-ide-rust").join("arduino-cli-path.txt")
    })
}

fn load_arduino_cli_path() -> String {
    arduino_cli_config_file()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| "arduino-cli".to_string())
}

fn save_arduino_cli_path(path: &str) -> bool {
    let Some(file) = arduino_cli_config_file() else {
        return false;
    };

    if let Some(parent) = file.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }

    std::fs::write(file, path.trim()).is_ok()
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
            arduino_cli_path: load_arduino_cli_path(),
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
            new_lib_name: String::new(),
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
        this.log("Arduino IDE для дітей запущено");
        this.refresh_ports();
        this.refresh_libraries();
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
                // USB-порты (настоящие Arduino) показываем первыми
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
    }

    /// arduino-cli требует, чтобы папка скетча называлась так же, как .ino файл
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

    /// Запускает одну или несколько команд arduino-cli в отдельном потоке,
    /// чтобы окно не зависало
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
                            "Відкрийте ⚙️ Налаштування та перевірте шлях до arduino-cli".to_string(),
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
            self.log("❌ Не вибрано порт (ліворуч). Під’єднайте Arduino та натисніть 🔄");
            self.tab = BottomTab::Console;
            return;
        };
        let Some(dir) = self.save_sketch() else {
            return;
        };
        // Порт должен быть свободен на время загрузки
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

    fn install_library(&mut self, ctx: &egui::Context) {
        let name = self.new_lib_name.trim().to_string();
        if name.is_empty() || self.busy.is_some() {
            return;
        }
        self.tab = BottomTab::Console;
        self.log(format!("Встановлення бібліотеки {} ...", name));
        self.new_lib_name.clear();
        self.spawn_cli(
            ctx,
            "Встановлення бібліотеки...",
            vec![vec!["lib".into(), "install".into(), name]],
        );
    }

    fn remove_library(&mut self, name: &str) {
        let path = self.libraries_dir.join(name);
        if path.exists() {
            match std::fs::remove_dir_all(&path) {
                Ok(_) => self.log(format!("🗑️ {} видалена", name)),
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
            self.log("❌ Виберіть порт ліворуч!");
            return;
        };
        if port_name.trim().is_empty() {
            self.log("❌ Не вибрано порт: назва порту порожня.");
            return;
        }
        match serialport::new(&port_name, self.baud_rate)
            .timeout(Duration::from_millis(10))
            .open()
        {
            Ok(p) => {
                self.serial_port = Some(p);
                self.serial_buf.clear();
                self.log(format!("🔌 Під’єднано до {} @ {}", port_name, self.baud_rate));
                self.tab = BottomTab::Monitor;
            }
            Err(e) => self.log(format!("❌ Не вдалося під’єднатися: {}", e)),
        }
    }

    fn disconnect_serial(&mut self) {
        self.serial_port = None;
        self.log("🔌 Від’єднано");
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

fn push(job: &mut LayoutJob, s: &str, color: Color32) {
    job.append(
        s,
        0.0,
        TextFormat {
            font_id: FontId::monospace(14.0),
            color,
            ..Default::default()
        },
    );
}

/// Подсветка синтаксиса C++ прямо во время редактирования
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

        // // комментарий
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            let start = i;
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            push(&mut job, &slice(start, i), COLOR_COMMENT);
            continue;
        }
        // /* комментарий */
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
        // "строка" и 'символ'
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
        // операторы и всё остальное
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
                    text = text.color(Color32::from_rgb(100, 220, 100));
                }
                ui.label(text);
            }
        });
}

fn draw_serial_plotter(ui: &mut egui::Ui, data: &VecDeque<f64>) {
    if data.is_empty() {
        ui.label("📊 На графіку поки що немає даних");
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
    painter.rect_filled(rect, 6.0, Color32::from_rgb(25, 25, 30));
    painter.rect_stroke(rect, 6.0, egui::Stroke::new(1.5_f32, Color32::from_rgb(70, 70, 80)));

    let n = data.len();
    if n > 1 {
        let mut points = Vec::with_capacity(n);
        for (idx, val) in data.iter().enumerate() {
            let x = rect.left() + rect.width() * idx as f32 / (n - 1) as f32;
            let t = ((*val - y_min) / (y_max - y_min)).clamp(0.0, 1.0) as f32;
            let y = rect.bottom() - rect.height() * t;
            points.push(egui::pos2(x, y));
        }
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(2.5_f32, Color32::from_rgb(100, 220, 255)),
        ));
    }

    let gray = Color32::from_rgb(150, 150, 160);
    painter.text(
        rect.left_top() + egui::vec2(6.0, 4.0),
        egui::Align2::LEFT_TOP,
        format!("{:.1}", max_val),
        FontId::monospace(11.0),
        gray,
    );
    painter.text(
        rect.left_bottom() + egui::vec2(6.0, -4.0),
        egui::Align2::LEFT_BOTTOM,
        format!("{:.1}", min_val),
        FontId::monospace(11.0),
        gray,
    );
    if let Some(last) = data.back() {
        painter.text(
            rect.right_top() + egui::vec2(-8.0, 6.0),
            egui::Align2::RIGHT_TOP,
            format!("{:.1}", last),
            FontId::monospace(13.0),
            Color32::WHITE,
        );
    }
}

impl eframe::App for ArduinoIde {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Сообщения из фонового потока arduino-cli
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Log(s) => self.log(s),
                Msg::Done => {
                    self.busy = None;
                    self.refresh_libraries();
                }
            }
        }

        if self.serial_port.is_some() {
            self.read_serial();
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if self.busy.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let bg_color = Color32::from_rgb(26, 26, 30);
        let panel_color = Color32::from_rgb(35, 35, 40);
        let is_busy = self.busy.is_some();

        // ---------- Верхняя строка ----------
        egui::TopBottomPanel::top("status_bar")
            .frame(
                egui::Frame::none()
                    .fill(panel_color)
                    .inner_margin(egui::Margin::symmetric(10.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Arduino IDE для дітей").strong().color(Color32::WHITE));
                    ui.separator();
                    ui.label(format!("Плата: {}", self.boards[self.selected_board_idx].0));
                    ui.separator();
                    ui.label(format!(
                        "Порт: {}",
                        self.selected_port.clone().unwrap_or_else(|| "не вибрано".to_string())
                    ));
                    if let Some(label) = self.busy {
                        ui.separator();
                        ui.spinner();
                        ui.label(label);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("⚙️").on_hover_text("Налаштування").clicked() {
                            self.show_settings = true;
                        }
                    });
                });
            });

        // ---------- Нижняя панель (консоль / монитор / график) ----------
        let max_bottom = (ctx.screen_rect().height() * 0.6).max(150.0);
        egui::TopBottomPanel::bottom("bottom_panel")
            .resizable(true)
            .default_height(240.0)
            .min_height(120.0)
            .max_height(max_bottom)
            .frame(egui::Frame::none().fill(panel_color).inner_margin(8.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, BottomTab::Console, "🖥 Консоль");
                    ui.selectable_value(&mut self.tab, BottomTab::Monitor, "📡 Монітор порту");
                    ui.selectable_value(&mut self.tab, BottomTab::Plotter, "📈 Графік");
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
                        if ui.button("❌ Від’єднати").clicked() {
                            self.disconnect_serial();
                        }
                    } else if ui.button("🔌 Під’єднати").clicked() {
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

                // Строка отправки в Arduino
                if self.serial_port.is_some() && self.tab == BottomTab::Monitor {
                    let mut send = false;
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.serial_input)
                                .desired_width((ui.available_width() - 100.0).max(100.0))
                                .hint_text("Надіслати до Arduino..."),
                        );
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            send = true;
                            resp.request_focus();
                        }
                        if ui.button("Надіслати").clicked() {
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
                            ui.label("🔌 Виберіть порт ліворуч і натисніть «Під’єднати»");
                        } else {
                            draw_lines(ui, &self.serial_lines);
                        }
                    }
                    BottomTab::Plotter => {
                        if self.serial_port.is_none() && self.plotter_data.is_empty() {
                            ui.label("🔌 Під’єднайтеся до Arduino, щоб бачити графік");
                        } else {
                            draw_serial_plotter(ui, &self.plotter_data);
                        }
                    }
                }
            });

        // ---------- Боковая панель: плата, порт, библиотеки ----------
        egui::SidePanel::left("side_panel")
            .resizable(true)
            .default_width(250.0)
            .min_width(200.0)
            .frame(egui::Frame::none().fill(panel_color).inner_margin(10.0))
            .show(ctx, |ui| {
                ui.heading("🔧 Плата");
                let mut idx = self.selected_board_idx;
                egui::ComboBox::from_id_source("board_combo")
                    .width((ui.available_width() - 8.0).max(100.0))
                    .selected_text(self.boards[idx].0.clone())
                    .show_ui(ui, |ui| {
                        for (i, (name, _)) in self.boards.iter().enumerate() {
                            ui.selectable_value(&mut idx, i, name.as_str());
                        }
                    });
                self.selected_board_idx = idx;

                ui.add_space(10.0);
                ui.heading("🔌 Порт");
                let mut sel = self.selected_port.clone();
                let mut refresh = false;
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_source("port_combo")
                        .width((ui.available_width() - 40.0).max(80.0))
                        .selected_text(sel.clone().unwrap_or_else(|| "не вибрано".to_string()))
                        .show_ui(ui, |ui| {
                            for p in &self.ports {
                                ui.selectable_value(&mut sel, Some(p.port_name.clone()), port_label(p));
                            }
                        });
                    if ui.button("🔄").on_hover_text("Оновити список портів").clicked() {
                        refresh = true;
                    }
                });
                self.selected_port = sel;
                if refresh {
                    self.refresh_ports();
                }
                if self.ports.is_empty() {
                    ui.label(
                        RichText::new("Під’єднайте Arduino USB-кабелем і натисніть 🔄")
                            .small()
                            .weak(),
                    );
                }

                ui.add_space(10.0);
                ui.separator();
                ui.heading("📚 Бібліотеки");

                let mut install = false;
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_lib_name)
                            .hint_text("Наприклад: Servo")
                            .desired_width((ui.available_width() - 40.0).max(80.0)),
                    );
                    if ui
                        .add_enabled(!is_busy, egui::Button::new("➕"))
                        .on_hover_text("Встановити бібліотеку")
                        .clicked()
                    {
                        install = true;
                    }
                });
                if install {
                    self.install_library(ctx);
                }

                let mut to_remove: Option<String> = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .max_height((ui.available_height() - 40.0).max(50.0))
                    .show(ui, |ui| {
                        if self.libraries.is_empty() {
                            ui.label(RichText::new("Поки що немає бібліотек").weak());
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
                if let Some(name) = to_remove {
                    self.remove_library(&name);
                }
                if ui.button("📁 Відкрити теку").clicked() {
                    self.open_library_folder();
                }
            });

        // ---------- Центр: редактор кода ----------
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg_color).inner_margin(10.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !is_busy,
                            egui::Button::new("✅ Перевірити").min_size(egui::vec2(120.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.compile(ctx);
                    }
                    if ui
                        .add_enabled(
                            !is_busy,
                            egui::Button::new("⬆️ Завантажити на Arduino")
                                .min_size(egui::vec2(180.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.upload(ctx);
                    }
                    if ui
                        .add(egui::Button::new("↺ Скинути код").min_size(egui::vec2(120.0, 34.0)))
                        .clicked()
                    {
                        self.sketch = DEFAULT_SKETCH.to_string();
                    }
                });
                ui.add_space(8.0);

                egui::Frame::none()
                    .fill(Color32::from_rgb(20, 20, 25))
                    .rounding(6.0)
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        let mut layouter = |ui: &egui::Ui, text: &str, wrap_width: f32| {
                            let mut job = highlight_job(text);
                            job.wrap.max_width = wrap_width;
                            ui.fonts(|f| f.layout_job(job))
                        };

                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let rows = ((ui.available_height() / 18.0) as usize).max(10);
                                ui.add(
                                    egui::TextEdit::multiline(&mut self.sketch)
                                        .code_editor()
                                        .desired_rows(rows)
                                        .desired_width(f32::INFINITY)
                                        .lock_focus(true)
                                        .layouter(&mut layouter),
                                );
                            });
                    });
            });

        // ---------- Окно настроек ----------
        if self.show_settings {
            let mut open = true;
            let mut close = false;
            egui::Window::new("⚙️ Налаштування arduino-cli")
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("Шлях до arduino-cli:");
                    ui.text_edit_singleline(&mut self.arduino_cli_path);
                    if ui.button("💾 Зберегти шлях").clicked() {
                        if save_arduino_cli_path(&self.arduino_cli_path) {
                            self.log("✅ Шлях до Arduino CLI збережено");
                        } else {
                            self.log("❌ Не вдалося зберегти шлях");
                        }
                    }
                    ui.add_space(8.0);

                    if ui
                        .add_enabled(!is_busy, egui::Button::new("Перевірити arduino-cli"))
                        .clicked()
                    {
                        self.tab = BottomTab::Console;
                        self.spawn_cli(ctx, "Перевірка...", vec![vec!["version".into()]]);
                    }
                    ui.add_space(4.0);
                    if ui
                        .add_enabled(
                            !is_busy,
                            egui::Button::new("Встановити підтримку Arduino Uno/Nano/Mega"),
                        )
                        .on_hover_text("Потрібен інтернет. Виконує core update-index і core install arduino:avr")
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
    // Нужна графическая сессия: Wayland (WAYLAND_DISPLAY) или X11 (DISPLAY)
    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        eprintln!("[arduino-ide-rust] ❌ Не знайдено графічну сесію (WAYLAND_DISPLAY / DISPLAY).");
        eprintln!("[arduino-ide-rust] Запустіть програму з термінала в графічному середовищі.");
        std::process::exit(2);
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 800.0])
            .with_title("Arduino IDE для дітей")
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Arduino IDE для дітей",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(ArduinoIde::default()))
        }),
    )
}