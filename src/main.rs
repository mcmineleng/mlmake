use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Sender};
use std::thread;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Deserialize;

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

#[derive(Debug, Deserialize)]
struct BuildFile {
    #[serde(rename = "task", default)]
    tasks: Vec<Task>,
}
#[derive(Debug, Deserialize, Clone)]
struct Task {
    name: String,
    value: String,
    #[serde(default)]
    cmduse: bool,
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
    run: Option<String>,
    builtin: Option<Vec<String>>,
}

fn log_line(level: usize, label: &str, name: &str, status: String) {
    print!("\n");
    let indent = "    ".repeat(level);
    let c = match label {
        "TASK" => color::CYAN,
        "STEP" => color::BLUE,
        _ => color::RESET,
    };
    println!(
        "{indent}{BOLD}{c}{label}{RESET} : {name} {s}",
        indent = indent,
        BOLD = color::BOLD,
        c = c,
        label = label,
        RESET = color::RESET,
        name = name,
        s = status,
    );
}
fn s_start() -> String { format!("{}[开始]{}", color::YELLOW, color::RESET) }
fn s_ok() -> String { format!("{}[成功]{}", color::GREEN, color::RESET) }
fn s_fail(c: i32) -> String { format!("{}[失败({})]{}", color::RED, c, color::RESET) }

const SENTINEL_PREFIX: &str = "__MLMAKE_END_";
const SENTINEL_SUFFIX: &str = "__";
const HS_MARK: &str = "__MLMAKE_HS__";

// ============== 终端尺寸 ==============

#[cfg(unix)]
fn term_size() -> (u16, u16) {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
            return (ws.ws_row, ws.ws_col);
        }
    }
    (24, 80)
}

#[cfg(not(unix))]
fn term_size() -> (u16, u16) { (24, 80) }

// ============== 字节工具 ==============

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn partial_suffix_match_len(data: &[u8], prefix: &[u8]) -> usize {
    let max = data.len().min(prefix.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if data[data.len() - len..] == prefix[..len] {
            return len;
        }
    }
    0
}

// ============== 握手 ==============

fn drain_until_handshake(r: &mut dyn Read) {
    let mut buf = [0u8; 1024];
    let mut acc: Vec<u8> = Vec::new();
    let hs = HS_MARK.as_bytes();
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                acc.extend_from_slice(&buf[..n]);
                if find_subslice(&acc, hs).is_some() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

// ============== 常驻 PTY shell ==============

struct PersistentShell {
    child: Box<dyn portable_pty::Child>,
    master: Box<dyn Write + Send>,
    code_rx: mpsc::Receiver<i32>,
}

impl PersistentShell {
    fn spawn(_task_name: &str, cwd: Option<&str>) -> std::io::Result<Self> {
        let pty = native_pty_system();

        // ★ 关键：PTY 宽度必须等于用户终端宽度，否则 cargo 输出 soft-wrap，
        //    \r 只会回到最后一个物理行的行首，看起来像每帧新起一行。
        let (rows, cols) = term_size();

        let pair = pty
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;

        let mut cmd = CommandBuilder::new("sh");
        let cwd_path = match cwd {
            Some(d) => std::path::PathBuf::from(d),
            None => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        };
        cmd.cwd(&cwd_path);
        cmd.env("TERM", "xterm-256color");
        cmd.env("PS1", "");
        cmd.env("PS2", "");
        cmd.env("PROMPT_COMMAND", "");

        let child = pair.slave.spawn_command(cmd)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;
        drop(pair.slave);

        let mut master = pair.master.take_writer().unwrap();
        let mut r0 = pair.master.try_clone_reader().unwrap();

        // 关回显；开输出处理；\n -> \r\n；不动 \r（保证进度条能回行首）
        master.write_all(b"stty -echo opost onlcr -ocrnl\n")?;
        // HS 标记在命令文本里拆开：回显是 echo __MLMA"KE_HS"__，输出是 __MLMAKE_HS__
        master.write_all(b"echo __MLMA\"KE_HS\"__\n")?;
        master.flush()?;
        drain_until_handshake(&mut r0);
        drop(r0);

        let reader = pair.master.try_clone_reader().unwrap();
        let (code_tx, code_rx) = mpsc::channel();
        spawn_pty_reader(reader, code_tx);

        Ok(Self { child, master, code_rx })
    }

    fn run_script(&mut self, script: &str) -> std::io::Result<i32> {
        // 哨兵同样拆开：回显是 echo __MLMA"KE_END_$?"__，输出是 __MLMAKE_END_<code>__
        let wrapped = format!("{script}\necho __MLMA\"KE_END_$?\"__\n");
        self.master.write_all(wrapped.as_bytes())?;
        self.master.flush()?;
        Ok(self.code_rx.recv().unwrap_or(-1))
    }

    fn set_args(&mut self, args: &[String]) -> std::io::Result<()> {
        let mut l = String::from("set --");
        for a in args {
            l.push(' ');
            l.push_str(&shell_quote(a));
        }
        l.push('\n');
        self.master.write_all(l.as_bytes())?;
        self.master.flush()?;
        Ok(())
    }
}

impl Drop for PersistentShell {
    fn drop(&mut self) {
        let _ = self.master.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ============== PTY reader：纯字节透传 ==============

fn spawn_pty_reader<R: Read + Send + 'static>(mut r: R, code_tx: Sender<i32>) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut pending: Vec<u8> = Vec::new();
        let prefix = SENTINEL_PREFIX.as_bytes();
        let suffix = SENTINEL_SUFFIX.as_bytes();

        let stdout = std::io::stdout();

        macro_rules! emit {
            ($bytes:expr) => {{
                let b: &[u8] = $bytes;
                if !b.is_empty() {
                    let mut lock = stdout.lock();
                    let _ = lock.write_all(b);
                    let _ = lock.flush();
                }
            }};
        }

        loop {
            let n = match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            pending.extend_from_slice(&buf[..n]);

            'scan: loop {
                match find_subslice(&pending, prefix) {
                    Some(p) => {
                        let after = p + prefix.len();
                        match find_subslice(&pending[after..], suffix) {
                            Some(rel) => {
                                emit!(&pending[..p]);

                                let code_end = after + rel;
                                let code = std::str::from_utf8(&pending[after..code_end])
                                    .unwrap_or("")
                                    .trim()
                                    .to_string();
                                if let Ok(c) = code.parse::<i32>() {
                                    let _ = code_tx.send(c);
                                }

                                let mut next = code_end + suffix.len();
                                if next < pending.len() && pending[next] == b'\n' {
                                    next += 1;
                                }
                                pending.drain(..next);
                                continue 'scan;
                            }
                            None => {
                                emit!(&pending[..p]);
                                pending.drain(..p);
                                break 'scan;
                            }
                        }
                    }
                    None => {
                        let keep = partial_suffix_match_len(&pending, prefix);
                        let emit_len = pending.len() - keep;
                        if emit_len > 0 {
                            emit!(&pending[..emit_len]);
                            pending.drain(..emit_len);
                        }
                        break 'scan;
                    }
                }
            }
        }

        if !pending.is_empty() {
            emit!(&pending);
        }
        let _ = stdout.lock().flush();
    });
}

// ============== shell 引用 / 变量展开 ==============

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn expand_vars(s: &str, args: &[String]) -> String {
    let ch: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < ch.len() {
        if ch[i] == '$' && i + 1 < ch.len() {
            match ch[i + 1] {
                '@' => { out.push_str(&args.join(" ")); i += 2; continue; }
                '$' => { out.push('$'); i += 2; continue; }
                d if d.is_ascii_digit() => {
                    let n = d.to_digit(10).unwrap() as usize;
                    if n >= 1 && n <= args.len() {
                        out.push_str(&args[n - 1]);
                    }
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(ch[i]);
        i += 1;
    }
    out
}

fn split_quoted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut sq = false;
    let mut dq = false;
    let mut has = false;
    for c in s.chars() {
        match c {
            '\'' if !dq => { sq = !sq; has = true; }
            '"' if !sq => { dq = !dq; has = true; }
            ' ' | '\t' | '\n' if !sq && !dq => {
                if has { out.push(std::mem::take(&mut cur)); has = false; }
            }
            _ => { cur.push(c); has = true; }
        }
    }
    if has { out.push(cur); }
    out
}

// ============== 内建 ==============

type BuiltinFn = fn(&mut Engine, &[String]) -> Result<(), i32>;
fn builtin_table() -> HashMap<&'static str, BuiltinFn> {
    let mut m = HashMap::new();
    m.insert("task", builtin_task as BuiltinFn);
    m
}
fn builtin_task(e: &mut Engine, args: &[String]) -> Result<(), i32> {
    if args.is_empty() {
        eprintln!("{}[builtin task] 缺参数{}", color::RED, color::RESET);
        return Err(1);
    }
    e.execute_task(&args[0], &args[1..])
}

// ============== 引擎 ==============

struct Engine {
    tasks: HashMap<String, Task>,
    current_args: Vec<String>,
}

impl Engine {
    fn new(tasks: Vec<Task>) -> Result<Self, String> {
        let mut map = HashMap::new();
        for t in tasks {
            let key = t.value.clone();
            if map.contains_key(&key) {
                return Err(format!("task value 重复: {}", key));
            }
            map.insert(key, t);
        }
        Ok(Self { tasks: map, current_args: Vec::new() })
    }

    fn execute_task(&mut self, value: &str, args: &[String]) -> Result<(), i32> {
        let (name, steps) = match self.tasks.get(value) {
            Some(t) => (t.name.clone(), t.steps.clone()),
            None => {
                eprintln!("{}[错误] 找不到 task: {}{}", color::RED, value, color::RESET);
                return Err(127);
            }
        };
        log_line(0, "TASK", &name, s_start());

        let mut shell = match PersistentShell::spawn(&name, None) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}[错误] pty 启动失败: {}{}", color::RED, e, color::RESET);
                log_line(0, "TASK", &name, s_fail(127));
                return Err(127);
            }
        };
        let _ = shell.set_args(args);
        let saved = std::mem::replace(&mut self.current_args, args.to_vec());
        let r = self.run_steps(&steps, &mut shell);
        self.current_args = saved;

        match &r {
            Ok(()) => log_line(0, "TASK", &name, s_ok()),
            Err(c) => log_line(0, "TASK", &name, s_fail(*c)),
        }
        r
    }

    fn run_steps(&mut self, steps: &[Step], shell: &mut PersistentShell) -> Result<(), i32> {
        let mut last: Option<Result<(), i32>> = None;
        for st in steps {
            if st.require_success {
                if let Some(Err(c)) = last {
                    log_line(1, "STEP", &st.name, s_fail(c));
                    last = Some(Err(c));
                    continue;
                }
            }
            log_line(1, "STEP", &st.name, s_start());
            let r = self.run_actions(&st.actions, shell);
            match &r {
                Ok(()) => log_line(1, "STEP", &st.name, s_ok()),
                Err(c) => log_line(1, "STEP", &st.name, s_fail(*c)),
            }
            last = Some(r);
        }
        match last {
            Some(Err(c)) => Err(c),
            _ => Ok(()),
        }
    }

    fn run_actions(&mut self, acts: &[Action], shell: &mut PersistentShell) -> Result<(), i32> {
        for a in acts {
            self.run_one(a, shell)?;
        }
        Ok(())
    }

    fn run_one(&mut self, a: &Action, shell: &mut PersistentShell) -> Result<(), i32> {
        match (&a.run, &a.builtin) {
            (Some(script), None) => match shell.run_script(script) {
                Ok(0) => Ok(()),
                Ok(c) => Err(c),
                Err(e) => {
                    eprintln!("{}[错误] 写 pty 失败: {}{}", color::RED, e, color::RESET);
                    Err(1)
                }
            },
            (None, Some(bs)) => {
                let table = builtin_table();
                for line in bs {
                    let exp = expand_vars(line, &self.current_args);
                    let parts = split_quoted(&exp);
                    if parts.is_empty() {
                        continue;
                    }
                    match table.get(parts[0].as_str()) {
                        Some(f) => f(self, &parts[1..])?,
                        None => {
                            eprintln!("{}[错误] 未知内建: {}{}", color::RED, parts[0], color::RESET);
                            return Err(127);
                        }
                    }
                }
                Ok(())
            }
            _ => {
                eprintln!("{}[错误] action 只能有 run/builtin 之一{}", color::RED, color::RESET);
                Err(1)
            }
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
    for t in tasks {
        for s in &t.steps {
            for a in &s.actions {
                if let Some(bs) = &a.builtin {
                    for b in bs {
                        let p = split_quoted(b);
                        if !p.is_empty() && p[0] == "task" && p.len() >= 2 && !vals.contains(&p[1]) {
                            return Err(format!("task `{}` 引用不存在的 task: {}", t.value, p[1]));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ============== CLI ==============

fn print_help() {
    println!(
        "{B}mlmake{R} - 常驻 PTY 任务编排器\n\n\
         用法:\n  mlmake [--cfg <file.toml>] <task> [args...]\n  mlmake [--cfg <file.toml>] list\n  mlmake [--cfg <file.toml>] list all step\n  mlmake help\n\n\
         选项:\n  --cfg <file.toml>  指定本次使用的构建文件 (默认: build.toml)\n",
        B = color::BOLD,
        R = color::RESET
    );
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
            println!("{}TASK{} : {} ({})", color::CYAN, color::RESET, t.name, t.value);
            for s in &t.steps {
                println!("    {}STEP{} : {}", color::BLUE, color::RESET, s.name);
            }
        }
        return;
    }
    if args.get(1).map(|s| s.as_str()) == Some("step") {
        match b.tasks.iter().find(|t| t.value == args[0]) {
            Some(t) => {
                println!("{}TASK{} : {} ({})", color::CYAN, color::RESET, t.name, t.value);
                for s in &t.steps {
                    println!("    {}STEP{} : {}", color::BLUE, color::RESET, s.name);
                }
            }
            None => {
                eprintln!("{}[错误] 找不到 task: {}{}", color::RED, args[0], color::RESET);
                std::process::exit(1);
            }
        }
        return;
    }
    eprintln!("{}[错误] 无法识别的 list 参数{}", color::RED, color::RESET);
    std::process::exit(1);
}

fn main() {
    let mut args: Vec<String> = std::env::args().collect();

    // ★ 新增：解析前置选项 --cfg <file.toml>
    let mut cfg_path = String::from("build.toml");
    if args.len() >= 2 && args[1] == "--cfg" {
        if args.len() < 3 {
            eprintln!("{}[错误] --cfg 缺少文件路径{}", color::RED, color::RESET);
            std::process::exit(1);
        }
        cfg_path = args[2].clone();
        args.drain(1..3); // 去掉 "--cfg" 与其路径，之后逻辑完全复用
    }

    let content = match std::fs::read_to_string(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}[错误] 读 {}: {}{}", color::RED, cfg_path, e, color::RESET);
            std::process::exit(1);
        }
    };
    let build: BuildFile = match toml::from_str(&content) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}[错误] 解析 toml: {}{}", color::RED, e, color::RESET);
            std::process::exit(1);
        }
    };
    if let Err(e) = validate(&build.tasks) {
        eprintln!("{}[错误] {}{}", color::RED, e, color::RESET);
        std::process::exit(1);
    }
    if args.len() < 2 {
        print_help();
        return;
    }
    match args[1].as_str() {
        "help" | "-h" | "--help" => { print_help(); return; }
        "list" => { cmd_list(&build, &args[2..]); return; }
        _ => {}
    }
    let target = args[1].clone();
    let task_args = args[2..].to_vec();
    let task = match build.tasks.iter().find(|t| t.value == target) {
        Some(t) => t,
        None => {
            eprintln!("{}[错误] 找不到 task: {}{}", color::RED, target, color::RESET);
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
    let mut engine = Engine::new(build.tasks).unwrap();
    std::process::exit(match engine.execute_task(&target, &task_args) {
        Ok(()) => 0,
        Err(c) => c.max(1),
    });
}
