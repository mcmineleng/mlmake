use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::sync::OnceLock;

use brush_core::builtins::{self, Command};
use brush_core::env::{EnvironmentLookup, EnvironmentScope};
use brush_core::openfiles::OpenFile;
use brush_core::variables::ShellValueLiteral;
use brush_core::{
    CreateOptions, ExecutionContext, ExecutionParameters, ExecutionResult,
    Shell as BrushShell, ShellFd,
};
use clap::Parser;
use serde::Deserialize;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_NAME: &str = env!("CARGO_PKG_NAME");

mod color {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const CYAN: &str = "\x1b[36m";
    pub const DIM: &str = "\x1b[2m";
}

fn default_true() -> bool { true }

// ============== max_depth 设置类型 ==============
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum MaxDepthSetting {
    Bool(bool),
    Num(usize),
}

impl Default for MaxDepthSetting {
    fn default() -> Self { MaxDepthSetting::Num(64) }
}

fn default_max_depth() -> MaxDepthSetting { MaxDepthSetting::Num(64) }

// ============== 配置结构 ==============
#[derive(Debug, Deserialize, Clone, Default)]
struct TaskSettings {
    #[serde(default)]
    actioninfo: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
struct GlobalSettings {
    #[serde(default = "default_true")]
    actioninfo: bool,
    #[serde(default = "default_max_depth")]
    max_depth: MaxDepthSetting,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            actioninfo: true,
            max_depth: MaxDepthSetting::Num(64),
        }
    }
}

#[derive(Debug, Deserialize)]
struct BuildFile {
    #[serde(rename = "task", default)]
    tasks: Vec<Task>,
    #[serde(default)]
    settings: GlobalSettings,
}

#[derive(Debug, Deserialize, Clone)]
struct Task {
    name: String,
    value: String,
    #[serde(default)]
    cmduse: bool,
    #[serde(default)]
    settings: TaskSettings,
    #[serde(rename = "step", default)]
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize, Clone)]
struct Step {
    name: String,
    #[serde(default)]
    require_success: bool,
    #[serde(rename = "action", default)]
    actions: Vec<Action>,
}

#[derive(Debug, Deserialize, Clone)]
struct Action {
    #[serde(default)]
    #[allow(dead_code)]
    require_success: bool,
    run: String,
}

// ============== 日志 ==============
fn log_line(show: bool, level: usize, label: &str, name: &str, status: String) {
    if !show {
        return;
    }
    let indent = "    ".repeat(level);
    let c = match label {
        "TASK" => color::CYAN,
        "STEP" => color::BLUE,
        _ => color::RESET,
    };
    // raw mode 下 \n 只下移不回列 0，因此：
    //   - 行首用 \r\x1b[K：回到列 0 并清掉当前行（吃掉 PTY 输出可能残留的半行）
    //   - 行尾用 \r\n：下移一行并回到列 0
    // 不要在前面再塞 \r\n，否则每条日志前会多出一个空行。
    let mut out = std::io::stdout().lock();
    let _ = write!(
        out,
        "\r\x1b[K{indent}{BOLD}{c}{label}{RESET} : {name} {s}\r\n",
        indent = indent,
        BOLD = color::BOLD,
        c = c,
        label = label,
        RESET = color::RESET,
        name = name,
        s = status,
    );
    let _ = out.flush();
}

fn s_start() -> String { format!("{}[开始]{}", color::YELLOW, color::RESET) }
fn s_ok() -> String { format!("{}[成功]{}", color::GREEN, color::RESET) }
fn s_fail(c: i32) -> String { format!("{}[失败({})]{}", color::RED, c, color::RESET) }
fn s_interrupt() -> String { format!("{}[中断(Ctrl+C)]{}", color::RED, color::RESET) }

// 统一的错误输出（raw mode 下需要用 \r\n）
fn err_line(msg: String) {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\r\x1b[K{msg}\r\n");
    let _ = out.flush();
}

// ============== 终端 raw mode guard（保留 ISIG） ==============
/// RAII guard：进入时把本地终端设为 raw mode 但保留 ISIG（Ctrl+C 产生 SIGINT），
/// drop 时恢复。即使 task 执行期间 panic，终端也会被恢复。
struct RawModeGuard {
    original: Option<libc::termios>,
}

impl RawModeGuard {
    fn enter() -> Self {
        if !std::io::stdin().is_terminal() {
            return Self { original: None };
        }

        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Self { original: None };
            }

            let mut raw = original;
            // cfmakeraw：关闭 ICANON/ECHO/IEXTEN 等，但会同时关掉 ISIG
            libc::cfmakeraw(&mut raw);
            // 手动把 ISIG 加回来 —— Ctrl+C 仍然产生 SIGINT
            raw.c_lflag |= libc::ISIG;

            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return Self { original: None };
            }

            Self { original: Some(original) }
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Some(ref orig) = self.original {
            unsafe {
                let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, orig);
            }
        }
    }
}

// ============== Engine ==============
struct Engine {
    tasks: HashMap<String, Task>,
    global_vars: HashMap<String, String>,
    global_actioninfo: bool,
    task_depth: usize,
    max_depth: Option<usize>,
}

impl Engine {
    fn new(
        tasks: Vec<Task>,
        global_actioninfo: bool,
        max_depth: MaxDepthSetting,
    ) -> Result<Self, String> {
        let max_depth = match max_depth {
            MaxDepthSetting::Num(n) => Some(n),
            MaxDepthSetting::Bool(false) => None,
            MaxDepthSetting::Bool(true) => {
                return Err(
                    "settings.max_depth 为布尔值时只能为 false（表示不限制递归）".to_string(),
                );
            }
        };

        let mut map = HashMap::new();
        for t in tasks {
            let key = t.value.clone();
            if map.contains_key(&key) {
                return Err(format!("task value 重复: {}", key));
            }
            map.insert(key, t);
        }
        Ok(Self {
            tasks: map,
            global_vars: HashMap::new(),
            global_actioninfo,
            task_depth: 0,
            max_depth,
        })
    }
}

// ============== 全局 Engine ==============
static ENGINE: OnceLock<Arc<tokio::sync::Mutex<Engine>>> = OnceLock::new();

fn get_engine() -> Arc<tokio::sync::Mutex<Engine>> {
    ENGINE.get().expect("Engine 未初始化").clone()
}

// ============== 内建命令：task ==============
#[derive(Parser, Debug)]
#[command(name = "task", about = "调用一个 task")]
struct TaskCommand {
    #[arg(required = true)]
    target: String,
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

impl Command for TaskCommand {
    type Error = brush_core::Error;

    async fn execute(
        &self,
        _context: ExecutionContext<'_>,
    ) -> Result<ExecutionResult, Self::Error> {
        let engine = get_engine();
        match execute_task(engine, &self.target, &self.args).await {
            Ok(()) => Ok(ExecutionResult::success()),
            Err(code) => Ok(ExecutionResult::new(code as u8)),
        }
    }
}

// ============== 内建命令：setallvar ==============
#[derive(Parser, Debug)]
#[command(name = "setallvar", about = "将变量设置到全局变量池（唯一写入入口）")]
struct SetallvarCommand {
    #[arg(required = true)]
    name: String,
    #[arg(required = true)]
    value: String,
}

impl Command for SetallvarCommand {
    type Error = brush_core::Error;

    async fn execute(
        &self,
        _context: ExecutionContext<'_>,
    ) -> Result<ExecutionResult, Self::Error> {
        let engine = get_engine();
        let mut e = engine.lock().await;
        e.global_vars.insert(self.name.clone(), self.value.clone());
        Ok(ExecutionResult::success())
    }
}

// ============== 内建命令：getallvar ==============
#[derive(Parser, Debug)]
#[command(name = "getallvar", about = "从全局变量池读取变量并设为当前 shell 变量（唯一读取入口）")]
struct GetallvarCommand {
    #[arg(required = true)]
    name: String,
}

impl Command for GetallvarCommand {
    type Error = brush_core::Error;

    async fn execute(
        &self,
        context: ExecutionContext<'_>,
    ) -> Result<ExecutionResult, Self::Error> {
        let engine = get_engine();
        let e = engine.lock().await;
        if let Some(val) = e.global_vars.get(&self.name) {
            context
                .shell
                .env
                .update_or_add(
                    self.name.as_str(),
                    ShellValueLiteral::Scalar(val.clone()),
                    |_| Ok(()),
                    EnvironmentLookup::Anywhere,
                    EnvironmentScope::Global,
                )
                .map_err(brush_core::Error::from)?;
            Ok(ExecutionResult::success())
        } else {
            err_line(format!(
                "{}[错误] 全局变量池中不存在: {}{}",
                color::RED, self.name, color::RESET
            ));
            Ok(ExecutionResult::new(1))
        }
    }
}

// ============== 内建命令：listallvar ==============
#[derive(Parser, Debug)]
#[command(name = "listallvar", about = "列出全局变量池中的所有变量")]
struct ListallvarCommand {
    #[arg(short = 'n')]
    names_only: bool,
    #[arg(short = 'v')]
    values: bool,
}

impl Command for ListallvarCommand {
    type Error = brush_core::Error;

    async fn execute(
        &self,
        _context: ExecutionContext<'_>,
    ) -> Result<ExecutionResult, Self::Error> {
        let engine = get_engine();
        let e = engine.lock().await;

        if e.global_vars.is_empty() {
            return Ok(ExecutionResult::success());
        }

        if self.names_only && !self.values {
            for k in e.global_vars.keys() {
                err_line(k.clone());
            }
        } else if !self.names_only && self.values {
            let parts: Vec<String> = e
                .global_vars
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect();
            err_line(parts.join(" "));
        } else if self.names_only && self.values {
            for (k, v) in &e.global_vars {
                err_line(format!("{}={}", k, v));
            }
        } else {
            let keys: Vec<&str> = e.global_vars.keys().map(|s| s.as_str()).collect();
            err_line(keys.join(" "));
        }

        Ok(ExecutionResult::success())
    }
}

// ============== 创建绑定到 PTY 的 shell ==============
async fn create_shell_with_pty(
    args: &[String],
    pts: Option<Arc<pty_process::blocking::Pts>>,
    arg0: &str,
) -> Result<BrushShell, String> {
    let mut builtins_map =
        brush_builtins::default_builtins(brush_builtins::BuiltinSet::BashMode);

    builtins_map.insert("task".into(), builtins::builtin::<TaskCommand>());
    builtins_map.insert("setallvar".into(), builtins::builtin::<SetallvarCommand>());
    builtins_map.insert("getallvar".into(), builtins::builtin::<GetallvarCommand>());
    builtins_map.insert("listallvar".into(), builtins::builtin::<ListallvarCommand>());

    let fds = if let Some(pts) = pts {
        let pts_fd = pts.as_raw_fd();
        let mut map = HashMap::new();
        for target in 0..3 {
            let d = unsafe { libc::dup(pts_fd) };
            if d < 0 {
                return Err("dup pty slave 失败".to_string());
            }
            let file = unsafe { std::fs::File::from_raw_fd(d) };
            map.insert(target as ShellFd, OpenFile::from(file));
        }
        Some(map)
    } else {
        None
    };

    let options = CreateOptions {
        no_editing: true,
        no_profile: true,
        no_rc: true,
        interactive: true,
        login: false,
        builtins: builtins_map,
        fds,
        ..Default::default()
    };

    let mut shell = BrushShell::new(options)
        .await
        .map_err(|e| format!("创建 shell 失败: {}", e))?;

    // $0 设为任务名（或其它调用者指定的名称）
    shell.shell_name = Some(arg0.to_string());
    // 位置参数：$1, $2, ...
    shell.positional_parameters = args.to_vec();
    Ok(shell)
}

// ============== 执行 task ==============
async fn execute_task(
    engine: Arc<tokio::sync::Mutex<Engine>>,
    value: &str,
    args: &[String],
) -> Result<(), i32> {
    let (task, actioninfo) = {
        let mut e = engine.lock().await;
        if let Some(max) = e.max_depth {
            if e.task_depth >= max {
                err_line(format!(
                    "{}[错误] task 递归深度超限 ({}): {}{}",
                    color::RED, max, value, color::RESET
                ));
                return Err(1);
            }
        }
        let t = match e.tasks.get(value) {
            Some(t) => t.clone(),
            None => {
                err_line(format!(
                    "{}[错误] 找不到 task: {}{}",
                    color::RED, value, color::RESET
                ));
                return Err(127);
            }
        };
        e.task_depth += 1;
        let ai = t.settings.actioninfo.unwrap_or(e.global_actioninfo);
        (t, ai)
    };

    log_line(actioninfo, 0, "TASK", &task.name, s_start());

    // ── 打开 PTY ──
    let (pty, pts) = match pty_process::blocking::open() {
        Ok(p) => p,
        Err(e) => {
            err_line(format!(
                "{}[错误] 打开 pty 失败: {}{}",
                color::RED, e, color::RESET
            ));
            let mut e = engine.lock().await;
            e.task_depth -= 1;
            log_line(actioninfo, 0, "TASK", &task.name, s_fail(1));
            return Err(1);
        }
    };

    // ── 同步 PTY 尺寸到当前终端 ──
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        let _ = pty.resize(pty_process::Size::new(rows, cols));
    }

    let pts = Arc::new(pts);

    // ── 进入 raw mode（保留 ISIG） ──
    let raw_guard = RawModeGuard::enter();

    // ── 监听 SIGINT（Ctrl+C）──
    // 收到后：恢复终端 → 打印中断日志 → 直接退出进程（130 = 128+SIGINT）
    let ctrl_c_task_name = task.name.clone();
    let ctrl_c_actioninfo = actioninfo;
    let ctrl_c_handle = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            // 恢复终端（不要用 RAII，直接就地恢复，避免退出时还处于 raw mode）
            unsafe {
                let mut term: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(libc::STDIN_FILENO, &mut term) == 0 {
                    libc::cfmakeraw(&mut term);
                    // 加回最常用的几个 flag，恢复"正常"终端
                    term.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
                    term.c_oflag |= libc::OPOST;
                    libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term);
                }
            }
            log_line(
                ctrl_c_actioninfo,
                0,
                "TASK",
                &ctrl_c_task_name,
                s_interrupt(),
            );
            // 强制退出整个进程
            std::process::exit(130);
        }
    });

    // ── reader 线程：PTY master → stdout ──
    let raw_fd = pty.as_raw_fd();
    let dup_fd = unsafe { libc::dup(raw_fd) };
    if dup_fd < 0 {
        err_line(format!("{}[错误] dup pty fd 失败{}", color::RED, color::RESET));
        drop(raw_guard);
        ctrl_c_handle.abort();
        let mut e = engine.lock().await;
        e.task_depth -= 1;
        log_line(actioninfo, 0, "TASK", &task.name, s_fail(1));
        return Err(1);
    }
    let mut reader = unsafe { std::fs::File::from_raw_fd(dup_fd) };

    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let stdout = std::io::stdout();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut lock = stdout.lock();
                    let _ = lock.write_all(&buf[..n]);
                    let _ = lock.flush();
                }
                Err(_) => break,
            }
        }
    });

    // ── writer 线程：mlmake 的 stdin → PTY master ──
    //    Ctrl-] (0x1d) 作为脱离信号，避免交互式程序无法退出。
    let raw_fd_w = pty.as_raw_fd();
    let dup_fd_w = unsafe { libc::dup(raw_fd_w) };
    let _writer_thread = if dup_fd_w >= 0 {
        let mut writer = unsafe { std::fs::File::from_raw_fd(dup_fd_w) };
        Some(std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // Ctrl+] 脱离
                        if buf[..n].contains(&0x1d) {
                            break;
                        }
                        // 保险：ISIG 已开启时 0x03 通常到不了这里；
                        // 若某些终端不产生 SIGINT，则自己中断。
                        if buf[..n].contains(&0x03) {
                            std::process::exit(130);
                        }
                        if writer.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                    Err(_) => break,
                }
            }
        }))
    } else {
        None
    };

    // ── 创建绑定到 PTY 的 shell（$0 = task.value） ──
    let mut shell = match create_shell_with_pty(args, Some(pts), &task.value).await {
        Ok(s) => s,
        Err(err) => {
            err_line(format!("{}[错误] {} {}", color::RED, err, color::RESET));
            drop(pty);
            let _ = reader_thread.join();
            ctrl_c_handle.abort();
            drop(raw_guard);
            let mut e = engine.lock().await;
            e.task_depth -= 1;
            log_line(actioninfo, 0, "TASK", &task.name, s_fail(127));
            return Err(127);
        }
    };

    // ── 跑所有 step ──
    let result = run_steps(&task.steps, &mut shell, actioninfo).await;

    // ── 清理 ──
    drop(shell);
    drop(pty);
    let _ = reader_thread.join();

    // 取消 SIGINT 监听（如果没触发，则正常取消）
    ctrl_c_handle.abort();

    // writer 线程可能仍阻塞在 stdin.read() 上，让它在进程结束时自动消失。
    if let Some(t) = _writer_thread {
        let _ = t;
    }

    // 恢复终端
    drop(raw_guard);

    {
        let mut e = engine.lock().await;
        e.task_depth -= 1;
    }

    match &result {
        Ok(()) => log_line(actioninfo, 0, "TASK", &task.name, s_ok()),
        Err(c) => log_line(actioninfo, 0, "TASK", &task.name, s_fail(*c)),
    }
    result
}

// ============== 跑步骤 ==============
async fn run_steps(
    steps: &[Step],
    shell: &mut BrushShell,
    actioninfo: bool,
) -> Result<(), i32> {
    let mut last: Option<Result<(), i32>> = None;

    for st in steps {
        if st.require_success {
            if let Some(Err(c)) = last {
                log_line(actioninfo, 1, "STEP", &st.name, s_fail(c));
                last = Some(Err(c));
                continue;
            }
        }
        log_line(actioninfo, 1, "STEP", &st.name, s_start());

        let r = run_actions(&st.actions, shell).await;
        match &r {
            Ok(()) => log_line(actioninfo, 1, "STEP", &st.name, s_ok()),
            Err(c) => log_line(actioninfo, 1, "STEP", &st.name, s_fail(*c)),
        }
        last = Some(r);
    }

    match last {
        Some(Err(c)) => Err(c),
        _ => Ok(()),
    }
}

async fn run_actions(acts: &[Action], shell: &mut BrushShell) -> Result<(), i32> {
    for a in acts {
        run_one(a, shell).await?;
    }
    Ok(())
}

// ============== run_one：所有命令都通过 PTY 上的 shell 执行 ==============
async fn run_one(a: &Action, shell: &mut BrushShell) -> Result<(), i32> {
    let script = a.run.trim();

    let params = ExecutionParameters::default();
    let result = shell.run_string(script.to_string(), &params).await;

    match result {
        Ok(exec_result) => {
            let code = u8::from(exec_result.exit_code) as i32;
            if code == 0 { Ok(()) } else { Err(code) }
        }
        Err(e) => {
            err_line(format!("{}[错误] 执行失败: {}{}", color::RED, e, color::RESET));
            Err(1)
        }
    }
}

// ============== 校验 ==============
fn validate(tasks: &[Task]) -> Result<(), String> {
    let mut vals = std::collections::HashSet::new();
    for t in tasks {
        let key = t.value.clone();
        if !vals.insert(key.clone()) {
            return Err(format!("task value 重复: {}", key));
        }
    }
    Ok(())
}

// ============== CLI ==============
fn print_help() {
    println!(
        concat!(
            "{B}{N}{R} v{V}\n\n",
            "用法:\n",
            "  {N} [--cfg <file.toml>] <task> [args...]\n",
            "  {N} [--cfg <file.toml>] list [all|task] [step]\n",
            "  {N} [--cfg <file.toml>] task <task> [args...]\n",
            "  {N} --help|-h\n",
            "  {N} --version|-V\n\n",
            "说明:\n",
            "  每个 task 拥有独立、干净的 shell 实例，cd/export 只影响当前 task。\n",
            "  同一个 task 的所有命令共享同一个 PTY 终端，stdin/stdout 双向转发，\n",
            "  支持 read -p、vim、top 等交互式程序。\n",
            "  交互中按 Ctrl-] 可脱离当前 PTY 会话。\n",
            "  交互中按 Ctrl+C 可中止当前 task（并退出 mlmake）。\n",
            "  参数传递：$0 = task.value，$1..$n = 命令行附加参数。\n",
            "  全局变量池没有任何自动同步：\n",
            "    写入请用 setallvar <name> <value>\n",
            "    读取请用 getallvar <name>（会设为当前 shell 变量）\n",
            "    查看请用 listallvar [-n|-v|-nv]\n\n",
            "配置 [settings] 项:\n",
            "  actioninfo = true|false   # 是否输出日志，默认 true\n",
            "  max_depth  = 64           # 最大递归层数，默认 64；\n",
            "                            # 设为 false 表示不限制（只允许 false，写 true 会报错）\n",
        ),
        B = color::BOLD,
        R = color::RESET,
        N = PKG_NAME,
        V = VERSION,
    );
}

fn load_build(cfg_path: &str) -> BuildFile {
    let content = match std::fs::read_to_string(cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{}[错误] 读 {}: {}{}",
                color::RED, cfg_path, e, color::RESET
            );
            std::process::exit(1);
        }
    };
    match toml::from_str(&content) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}[错误] 解析 toml: {}{}", color::RED, e, color::RESET);
            std::process::exit(1);
        }
    }
}

fn cmd_list(b: &BuildFile, args: &[String]) {
    if args.is_empty() {
        println!("{}可用任务:{}", color::BOLD, color::RESET);
        for t in &b.tasks {
            let tag = if t.cmduse {
                format!("{}[cmduse]{}", color::GREEN, color::RESET)
            } else {
                format!("{}[内部]{}", color::DIM, color::RESET)
            };
            println!("    {:<12} {:<16} {}", t.value, t.name, tag);
        }
        return;
    }
    if args[0] == "all" && args.get(1).map(|s| s.as_str()) == Some("step") {
        for t in &b.tasks {
            println!(
                "{}TASK{} : {} ({})",
                color::CYAN, color::RESET, t.name, t.value
            );
            for s in &t.steps {
                println!("    {}STEP{} : {}", color::BLUE, color::RESET, s.name);
            }
        }
        return;
    }
    if args.get(1).map(|s| s.as_str()) == Some("step") {
        match b.tasks.iter().find(|t| t.value == args[0]) {
            Some(t) => {
                println!(
                    "{}TASK{} : {} ({})",
                    color::CYAN, color::RESET, t.name, t.value
                );
                for s in &t.steps {
                    println!("    {}STEP{} : {}", color::BLUE, color::RESET, s.name);
                }
            }
            None => {
                eprintln!(
                    "{}[错误] 找不到 task: {}{}",
                    color::RED, args[0], color::RESET
                );
                std::process::exit(1);
            }
        }
        return;
    }
    eprintln!("{}[错误] 无法识别的 list 参数{}", color::RED, color::RESET);
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().collect();

    let mut cfg_path = String::from("build.toml");
    if args.len() >= 2 && args[1] == "--cfg" {
        if args.len() < 3 {
            eprintln!("{}[错误] --cfg 缺少文件路径{}", color::RED, color::RESET);
            std::process::exit(1);
        }
        cfg_path = args[2].clone();
        args.drain(1..3);
    }

    if args
        .get(1)
        .map(|s| s == "--version" || s == "-V")
        .unwrap_or(false)
    {
        println!("{} v{}", PKG_NAME, VERSION);
        return;
    }
    if args
        .get(1)
        .map(|s| s == "--help" || s == "-h")
        .unwrap_or(false)
    {
        print_help();
        return;
    }
    if args.len() < 2 {
        print_help();
        return;
    }

    let build = load_build(&cfg_path);
    if let Err(e) = validate(&build.tasks) {
        eprintln!("{}[错误] {}{}", color::RED, e, color::RESET);
        std::process::exit(1);
    }

    let cmd = args[1].as_str();

    let (target, task_args) = if cmd == "task" {
        if args.len() < 3 {
            eprintln!("{}[错误] task 缺少任务名{}", color::RED, color::RESET);
            std::process::exit(1);
        }
        (args[2].clone(), args[3..].to_vec())
    } else {
        if cmd == "list" {
            cmd_list(&build, &args[2..]);
            return;
        }
        (args[1].clone(), args[2..].to_vec())
    };

    let task = match build.tasks.iter().find(|t| t.value == target) {
        Some(t) => t,
        None => {
            eprintln!(
                "{}[错误] 找不到 task: {}{}",
                color::RED, target, color::RESET
            );
            std::process::exit(1);
        }
    };
    if !task.cmduse {
        eprintln!(
            "{}[错误] task `{}` 不可命令行调用 (cmduse=false){}",
            color::RED, target, color::RESET
        );
        std::process::exit(1);
    }

    let engine = match Engine::new(
        build.tasks,
        build.settings.actioninfo,
        build.settings.max_depth,
    ) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("{}[错误] {}{}", color::RED, err, color::RESET);
            std::process::exit(1);
        }
    };
    let engine_arc = Arc::new(tokio::sync::Mutex::new(engine));
    let _ = ENGINE.set(engine_arc.clone());

    let code = match execute_task(engine_arc, &target, &task_args).await {
        Ok(()) => 0,
        Err(c) => c.max(1),
    };
    std::process::exit(code);
}
