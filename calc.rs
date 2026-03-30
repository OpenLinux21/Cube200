// calc.rs — 200阶魔方多线程求解器 + 原生 HTTP API
// 编译: rustc -C opt-level=2 --edition 2021 -o calc calc.rs
// 运行: ./calc          (读取当前目录 data.bin，监听 127.0.0.1:62001)
//
// 线程模型:
//   main          : 启动所有线程，阻塞等待
//   scheduler (1) : HTTP服务器 + 200Hz Tick，管理全局状态
//   explorer  (8) : 并行探索下一步最优走法
//   executor  (1) : 汇总探索结果，执行最优步，更新全局魔方
//
// 严格仅限 std，零第三方依赖。

#![allow(clippy::needless_range_loop)]

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex, RwLock, atomic::{AtomicBool, AtomicU64, Ordering}},
    sync::mpsc::{self, SyncSender, Receiver},
    thread,
    time::{Duration, Instant},
};

// ============================================================
// 常量
// ============================================================

const N: usize = 200;
const FACES: usize = 6;
const FACE_SIZE: usize = N * N;
const TOTAL: usize = FACES * FACE_SIZE;

const EXPLORER_COUNT: usize = 8;
const HTTP_ADDR: &str = "127.0.0.1:62001";

/// 调度 Tick 间隔：5ms → 200Hz
const TICK_INTERVAL: Duration = Duration::from_millis(5);

// 六面索引（URFDLB 标准）
const U_FACE: usize = 0;
const R_FACE: usize = 1;
const F_FACE: usize = 2;
const D_FACE: usize = 3;
const L_FACE: usize = 4;
const B_FACE: usize = 5;

// ============================================================
// 移动动作定义
// ============================================================

/// 一个原子移动：(轴, 层, 方向)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    pub axis: u8,      // 0=U/D轴, 1=R/L轴, 2=F/B轴
    pub layer: u16,    // 0..N
    pub cw: bool,      // true=顺时针
}

impl Move {
    #[inline]
    pub fn inverse(self) -> Self {
        Move { axis: self.axis, layer: self.layer, cw: !self.cw }
    }
}

// ============================================================
// 魔方核心数据结构与操作（内联高性能版）
// ============================================================

/// 魔方状态——堆分配的固定大小一维 u8 数组
/// 布局：data[face * N*N + row * N + col]，颜色 0-5
#[derive(Clone)]
pub struct CubeState {
    pub data: Box<[u8; TOTAL]>,
}

impl CubeState {
    pub fn new_zeroed() -> Self {
        CubeState { data: Box::new([0u8; TOTAL]) }
    }

    /// 从原始字节切片加载（长度必须为 TOTAL）
    pub fn from_bytes(bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), TOTAL, "data.bin 大小不符：期望 {} 字节", TOTAL);
        let mut s = Self::new_zeroed();
        s.data.copy_from_slice(bytes);
        s
    }

    #[inline(always)]
    fn idx(f: usize, r: usize, c: usize) -> usize {
        f * FACE_SIZE + r * N + c
    }

    #[inline(always)]
    pub fn get(&self, f: usize, r: usize, c: usize) -> u8 {
        self.data[Self::idx(f, r, c)]
    }

    #[inline(always)]
    pub fn set(&mut self, f: usize, r: usize, c: usize, v: u8) {
        self.data[Self::idx(f, r, c)] = v;
    }

    // ----------------------------------------------------------
    // 面片旋转（原地）
    // ----------------------------------------------------------

    /// 顺时针旋转面 f（转置 + 行翻转）
    fn rotate_face_cw(&mut self, f: usize) {
        let base = f * FACE_SIZE;
        let s = &mut self.data[base..base + FACE_SIZE];
        for r in 0..N {
            for c in (r + 1)..N {
                s.swap(r * N + c, c * N + r);
            }
        }
        for r in 0..N {
            s[r * N..r * N + N].reverse();
        }
    }

    /// 逆时针旋转面 f（行翻转 + 转置）
    fn rotate_face_ccw(&mut self, f: usize) {
        let base = f * FACE_SIZE;
        let s = &mut self.data[base..base + FACE_SIZE];
        for r in 0..N {
            s[r * N..r * N + N].reverse();
        }
        for r in 0..N {
            for c in (r + 1)..N {
                s.swap(r * N + c, c * N + r);
            }
        }
    }

    // ----------------------------------------------------------
    // 棱带交换——三轴实现
    // ----------------------------------------------------------

    /// U/D 轴水平层（行方向）移动
    /// layer=0 → U面；layer=N-1 → D面
    /// 顺时针（从U往下看）: F → R → B(镜像) → L → F
    #[inline]
    pub fn move_u_axis(&mut self, layer: usize, cw: bool) {
        let mut tmp = [0u8; N];
        for c in 0..N { tmp[c] = self.get(F_FACE, layer, c); }
        if cw {
            for c in 0..N { let v = self.get(L_FACE, layer, c);           self.set(F_FACE, layer, c, v); }
            for c in 0..N { let v = self.get(B_FACE, N-1-layer, N-1-c);   self.set(L_FACE, layer, c, v); }
            for c in 0..N { let v = self.get(R_FACE, layer, N-1-c);       self.set(B_FACE, N-1-layer, c, v); }
            for c in 0..N { self.set(R_FACE, layer, c, tmp[c]); }
        } else {
            for c in 0..N { let v = self.get(R_FACE, layer, c);           self.set(F_FACE, layer, c, v); }
            for c in 0..N { let v = self.get(B_FACE, N-1-layer, N-1-c);   self.set(R_FACE, layer, c, v); }
            for c in 0..N { let v = self.get(L_FACE, layer, N-1-c);       self.set(B_FACE, N-1-layer, c, v); }
            for c in 0..N { self.set(L_FACE, layer, c, tmp[c]); }
        }
        if layer == 0 {
            if cw { self.rotate_face_cw(U_FACE); } else { self.rotate_face_ccw(U_FACE); }
        } else if layer == N - 1 {
            if cw { self.rotate_face_ccw(D_FACE); } else { self.rotate_face_cw(D_FACE); }
        }
    }

    /// R/L 轴竖直列移动
    /// layer=0 → R面最外列；layer=N-1 → L面最外列
    /// 顺时针（从R往左看）: U → F → D → B(翻转) → U
    #[inline]
    pub fn move_r_axis(&mut self, layer: usize, cw: bool) {
        let col_f = N - 1 - layer;
        let col_b = layer;
        let mut tmp = [0u8; N];
        for r in 0..N { tmp[r] = self.get(U_FACE, r, col_f); }
        if cw {
            for r in 0..N { let v = self.get(F_FACE, r, col_f);     self.set(U_FACE, r, col_f, v); }
            for r in 0..N { let v = self.get(D_FACE, r, col_f);     self.set(F_FACE, r, col_f, v); }
            for r in 0..N { let v = self.get(B_FACE, N-1-r, col_b); self.set(D_FACE, r, col_f, v); }
            for r in 0..N { self.set(B_FACE, N-1-r, col_b, tmp[r]); }
        } else {
            for r in 0..N { let v = self.get(B_FACE, N-1-r, col_b); self.set(U_FACE, r, col_f, v); }
            for r in 0..N { let v = self.get(D_FACE, N-1-r, col_f); self.set(B_FACE, r, col_b, v); }
            for r in 0..N { let v = self.get(F_FACE, r, col_f);     self.set(D_FACE, r, col_f, v); }
            for r in 0..N { self.set(F_FACE, r, col_f, tmp[r]); }
        }
        if layer == 0 {
            if cw { self.rotate_face_cw(R_FACE); } else { self.rotate_face_ccw(R_FACE); }
        } else if layer == N - 1 {
            if cw { self.rotate_face_ccw(L_FACE); } else { self.rotate_face_cw(L_FACE); }
        }
    }

    /// F/B 轴深度切片移动
    /// layer=0 → F面；layer=N-1 → B面
    /// 顺时针（从F往后看）: U底行 → R左列 → D顶行(翻转) → L右列(翻转) → U
    #[inline]
    pub fn move_f_axis(&mut self, layer: usize, cw: bool) {
        let row_u = N - 1 - layer;
        let row_d = layer;
        let col_r = layer;
        let col_l = N - 1 - layer;
        let mut tmp = [0u8; N];
        for c in 0..N { tmp[c] = self.get(U_FACE, row_u, c); }
        if cw {
            for c in 0..N { let v = self.get(L_FACE, N-1-c, col_l); self.set(U_FACE, row_u, c, v); }
            for r in 0..N { let v = self.get(D_FACE, row_d, N-1-r); self.set(L_FACE, r, col_l, v); }
            for c in 0..N { let v = self.get(R_FACE, c, col_r);     self.set(D_FACE, row_d, c, v); }
            for r in 0..N { self.set(R_FACE, r, col_r, tmp[r]); }
        } else {
            for c in 0..N { let v = self.get(R_FACE, c, col_r);     self.set(U_FACE, row_u, c, v); }
            for r in 0..N { let v = self.get(D_FACE, row_d, N-1-r); self.set(R_FACE, r, col_r, v); }
            for c in 0..N { let v = self.get(L_FACE, c, col_l);     self.set(D_FACE, row_d, c, v); }
            for r in 0..N { self.set(L_FACE, r, col_l, tmp[N-1-r]); }
        }
        if layer == 0 {
            if cw { self.rotate_face_cw(F_FACE); } else { self.rotate_face_ccw(F_FACE); }
        } else if layer == N - 1 {
            if cw { self.rotate_face_ccw(B_FACE); } else { self.rotate_face_cw(B_FACE); }
        }
    }

    /// 统一入口：执行一个 Move
    #[inline]
    pub fn apply(&mut self, mv: Move) {
        match mv.axis {
            0 => self.move_u_axis(mv.layer as usize, mv.cw),
            1 => self.move_r_axis(mv.layer as usize, mv.cw),
            2 => self.move_f_axis(mv.layer as usize, mv.cw),
            _ => unreachable!(),
        }
    }

    /// 执行一组 Move
    #[inline]
    pub fn apply_seq(&mut self, moves: &[Move]) {
        for &mv in moves { self.apply(mv); }
    }

    // ----------------------------------------------------------
    // 启发式评估函数（降阶法代价估算）
    // ----------------------------------------------------------

    /// 统计"已归位"格子数：颜色与所属面编号一致的格子
    pub fn solved_count(&self) -> u32 {
        let mut count = 0u32;
        for f in 0..FACES {
            let expected = f as u8;
            let base = f * FACE_SIZE;
            for i in 0..FACE_SIZE {
                if self.data[base + i] == expected {
                    count += 1;
                }
            }
        }
        count
    }

    /// 每面中心块（内部 (N-2)×(N-2) 区域）已归位数
    /// 降阶法第一阶段目标：先将六面中心全部归位
    pub fn center_solved_count(&self) -> u32 {
        let mut count = 0u32;
        for f in 0..FACES {
            let expected = f as u8;
            for r in 1..N-1 {
                for c in 1..N-1 {
                    if self.get(f, r, c) == expected { count += 1; }
                }
            }
        }
        count
    }

    /// 棱块（边缘但非角块）已归位数
    /// 降阶法第二阶段目标
    pub fn edge_solved_count(&self) -> u32 {
        let mut count = 0u32;
        // 检查每条棱线（每面四条边，各 N-2 个非角格子）
        for f in 0..FACES {
            let expected = f as u8;
            // 顶行
            for c in 1..N-1 { if self.get(f, 0, c) == expected { count += 1; } }
            // 底行
            for c in 1..N-1 { if self.get(f, N-1, c) == expected { count += 1; } }
            // 左列
            for r in 1..N-1 { if self.get(f, r, 0) == expected { count += 1; } }
            // 右列
            for r in 1..N-1 { if self.get(f, r, N-1) == expected { count += 1; } }
        }
        count
    }

    /// 综合启发代价（越小越好；已解决的格子越多，代价越低）
    pub fn heuristic_cost(&self) -> i64 {
        let total = TOTAL as i64;
        let solved = self.solved_count() as i64;
        // 归一化为"未归位格子数"
        total - solved
    }
}

// ============================================================
// 降阶法求解骨架
// ============================================================

/// 求解阶段
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolvePhase {
    /// 阶段1：归位六面中心块（大魔方最耗时的阶段）
    CenterReduction,
    /// 阶段2：配对并归位棱块
    EdgePairing,
    /// 阶段3：当中心和棱均就位后，按3阶算法求解
    ThreeByThreeReduction,
    /// 已完成
    Solved,
}

/// 判断当前处于哪个求解阶段
pub fn detect_phase(state: &CubeState) -> SolvePhase {
    let total_centers = FACES * (N - 2) * (N - 2);
    let total_edges = FACES * 4 * (N - 2);

    let centers = state.center_solved_count() as usize;
    let edges = state.edge_solved_count() as usize;
    let solved = state.solved_count() as usize;

    if solved == TOTAL {
        SolvePhase::Solved
    } else if centers < total_centers {
        SolvePhase::CenterReduction
    } else if edges < total_edges {
        SolvePhase::EdgePairing
    } else {
        SolvePhase::ThreeByThreeReduction
    }
}

// ---- 阶段1：中心块归位辅助 ----

/// 生成"将面 `target_face` 第 r 行的中心条带移动到正确位置"的候选动作序列。
/// 实际大魔方求解器会有数十种具体手法；此处提供核心框架，
/// 探索线程会从这些候选中选代价最低的。
pub fn center_candidate_moves(
    _state: &CubeState,
    target_face: usize,
    layer: usize,
) -> Vec<Vec<Move>> {
    // 对目标面，尝试用同轴的层移动把该层的颜色推入正确面
    // 这里生成四个基本候选：顺/逆时针，两种轴
    let axis_for_face = [0u8, 1, 2, 0, 1, 2]; // 每个面对应的主轴
    let ax = axis_for_face[target_face];
    vec![
        vec![Move { axis: ax, layer: layer as u16, cw: true }],
        vec![Move { axis: ax, layer: layer as u16, cw: false }],
        // 双层复合动作（先移入再调整）
        vec![
            Move { axis: ax, layer: layer as u16, cw: true },
            Move { axis: (ax + 1) % 3, layer: (N / 2) as u16, cw: true },
            Move { axis: ax, layer: layer as u16, cw: false },
        ],
        vec![
            Move { axis: ax, layer: layer as u16, cw: false },
            Move { axis: (ax + 2) % 3, layer: (N / 2) as u16, cw: false },
            Move { axis: ax, layer: layer as u16, cw: true },
        ],
    ]
}

// ---- 阶段2：棱块配对辅助 ----

/// 棱块配对的候选移动：将散落在各处的同色棱块两两配对。
/// 核心手法：U层隔离 + 切片插入 + U层恢复
pub fn edge_pairing_candidates(layer: usize) -> Vec<Vec<Move>> {
    // 标准"翻转插入"手法的4种变体
    let l = layer as u16;
    vec![
        vec![
            Move { axis: 0, layer: 0,    cw: true  },
            Move { axis: 1, layer: l,    cw: true  },
            Move { axis: 0, layer: 0,    cw: false },
            Move { axis: 1, layer: l,    cw: false },
        ],
        vec![
            Move { axis: 0, layer: 0,    cw: false },
            Move { axis: 1, layer: l,    cw: false },
            Move { axis: 0, layer: 0,    cw: true  },
            Move { axis: 1, layer: l,    cw: true  },
        ],
        vec![
            Move { axis: 2, layer: 0,    cw: true  },
            Move { axis: 0, layer: l,    cw: true  },
            Move { axis: 2, layer: 0,    cw: false },
            Move { axis: 0, layer: l,    cw: false },
        ],
        vec![
            Move { axis: 2, layer: 0,    cw: false },
            Move { axis: 0, layer: l,    cw: false },
            Move { axis: 2, layer: 0,    cw: true  },
            Move { axis: 0, layer: l,    cw: true  },
        ],
    ]
}

// ---- 阶段3：3阶求解占位 ----

/// 将大魔方降阶为等效3阶后，利用 CFOP/Kociemba 算法框架求解。
/// 此处仅搭建骨架：提取3阶等效状态，并返回基本面旋转候选。
/// 完整实现需要预计算转换表（约 ~50KB），可在初始化时建立。
pub fn three_stage_candidates() -> Vec<Vec<Move>> {
    // 六面各一次旋转，共12个基本动作
    let layers = [0u16, (N - 1) as u16];
    let mut candidates = Vec::with_capacity(12);
    for &layer in &layers {
        for axis in 0u8..3 {
            candidates.push(vec![Move { axis, layer, cw: true }]);
            candidates.push(vec![Move { axis, layer, cw: false }]);
        }
    }
    candidates
}

// ============================================================
// 共享全局状态
// ============================================================

/// 求解器运行状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Paused,
    Running,
}

/// 探索线程向执行线程提交的候选结果
pub struct ExplorerResult {
    /// 哪个探索器产生（0-7）
    pub explorer_id: usize,
    /// 建议执行的动作序列
    pub moves: Vec<Move>,
    /// 执行后的预测代价（越小越好）
    pub predicted_cost: i64,
    /// 探索耗时
    pub elapsed_us: u64,
}

/// 全局共享状态（调度线程与执行线程共用）
pub struct SharedState {
    /// 当前魔方状态（执行线程独写，其他只读快照）
    pub cube: Mutex<CubeState>,
    /// 运行/暂停
    pub run_state: Mutex<RunState>,
    /// 总已执行步数
    pub total_moves: AtomicU64,
    /// 总耗时（微秒）
    pub total_elapsed_us: AtomicU64,
    /// 当前求解阶段（供 HTTP /status 展示）
    pub phase: Mutex<SolvePhase>,
    /// HTTP /status 快照缓冲（调度线程定期刷新，HTTP 线程只读）
    pub status_snapshot: RwLock<String>,
    /// 是否停止所有线程
    pub shutdown: AtomicBool,
}

impl SharedState {
    pub fn new(cube: CubeState) -> Arc<Self> {
        Arc::new(SharedState {
            cube: Mutex::new(cube),
            run_state: Mutex::new(RunState::Paused),
            total_moves: AtomicU64::new(0),
            total_elapsed_us: AtomicU64::new(0),
            phase: Mutex::new(SolvePhase::CenterReduction),
            status_snapshot: RwLock::new(String::from("{\"status\":\"initializing\"}")),
            shutdown: AtomicBool::new(false),
        })
    }
}

// ============================================================
// 极简 HTTP 服务器（非阻塞 TcpListener）
// ============================================================

/// 解析 HTTP 请求的第一行，返回 (method, path)
fn parse_request_line(buf: &[u8]) -> Option<(&str, &str)> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut iter = text.lines();
    let first = iter.next()?;
    let mut parts = first.splitn(3, ' ');
    let method = parts.next()?;
    let path = parts.next()?;
    Some((method, path))
}

/// 读取 HTTP 请求体（Content-Length 字节）
fn read_body(stream: &mut TcpStream, header_end: usize, buf: &[u8]) -> String {
    // 从已缓冲数据中找 Content-Length
    let header_str = std::str::from_utf8(&buf[..header_end]).unwrap_or("");
    let content_length: usize = header_str
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.splitn(2, ':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    let already = buf.len().saturating_sub(header_end + 4); // 4 = \r\n\r\n
    let mut body_bytes = buf[header_end + 4..].to_vec();
    let remaining = content_length.saturating_sub(already);
    if remaining > 0 {
        let mut extra = vec![0u8; remaining];
        let _ = stream.read(&mut extra);
        body_bytes.extend_from_slice(&extra);
    }
    String::from_utf8_lossy(&body_bytes).into_owned()
}

/// 构建 HTTP 响应
fn http_response(status: u16, content_type: &str, body: &str) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _   => "Internal Server Error",
    };
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{}",
        status, status_text, content_type, body.len(), body
    ).into_bytes()
}

/// 处理单个 HTTP 连接
fn handle_connection(mut stream: TcpStream, shared: &Arc<SharedState>) {
    // 读取请求（最多 8KB，足够 HTTP header）
    let mut buf = vec![0u8; 8192];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    buf.truncate(n);

    // 找到 header 结束位置
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(n.saturating_sub(4));

    let response = match parse_request_line(&buf[..header_end.min(n)]) {
        None => http_response(400, "text/plain", "Bad Request"),

        Some(("GET", "/status")) => {
            let snap = shared.status_snapshot.read().unwrap();
            http_response(200, "application/json", &snap)
        }

        Some(("GET", "/health")) => {
            http_response(200, "text/plain", "OK")
        }

        Some(("POST", "/control")) => {
            let body = read_body(&mut stream, header_end, &buf);
            let body = body.trim().to_ascii_lowercase();

            let reply = if body.contains("start") || body.contains("resume") {
                *shared.run_state.lock().unwrap() = RunState::Running;
                "{\"ok\":true,\"state\":\"running\"}"
            } else if body.contains("pause") || body.contains("stop") {
                *shared.run_state.lock().unwrap() = RunState::Paused;
                "{\"ok\":true,\"state\":\"paused\"}"
            } else if body.contains("shutdown") {
                shared.shutdown.store(true, Ordering::SeqCst);
                "{\"ok\":true,\"state\":\"shutdown\"}"
            } else {
                "{\"ok\":false,\"error\":\"unknown command\"}"
            };
            http_response(200, "application/json", reply)
        }

        Some((_, "/status")) | Some((_, "/control")) => {
            http_response(405, "text/plain", "Method Not Allowed")
        }

        Some(_) => http_response(404, "text/plain", "Not Found"),
    };

    let _ = stream.write_all(&response);
}

// ============================================================
// 调度线程（Scheduler）
// ============================================================
//
// 职责：
//   1. 运行 HTTP 服务器（非阻塞 accept）
//   2. 每 5ms（200Hz）Tick：刷新 status_snapshot，向探索线程广播快照
//
// 使用非阻塞 TcpListener，避免 accept() 阻塞 Tick 计时器。
// ============================================================

fn scheduler_thread(
    shared: Arc<SharedState>,
    snapshot_tx: SyncSender<CubeState>, // 向探索线程广播最新快照
) {
    let listener = TcpListener::bind(HTTP_ADDR).expect("绑定 HTTP 端口失败");
    listener.set_nonblocking(true).expect("设置非阻塞失败");
    eprintln!("[scheduler] HTTP 服务器监听 http://{}", HTTP_ADDR);

    let mut last_tick = Instant::now();

    loop {
        if shared.shutdown.load(Ordering::Relaxed) { break; }

        // ---- 非阻塞 accept：处理所有待接受的连接 ----
        loop {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    // 每个连接克隆 Arc，spawn 短生命周期线程处理
                    let shared_clone = Arc::clone(&shared);
                    // 设置读写超时防止连接挂起
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                    thread::spawn(move || handle_connection(stream, &shared_clone));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // ---- 200Hz Tick ----
        let now = Instant::now();
        if now.duration_since(last_tick) >= TICK_INTERVAL {
            last_tick = now;

            // 生成状态快照
            let (cube_snap, phase, run_st, total_mv, total_us) = {
                let cube = shared.cube.lock().unwrap();
                let phase = *shared.phase.lock().unwrap();
                let run_st = *shared.run_state.lock().unwrap();
                let total_mv = shared.total_moves.load(Ordering::Relaxed);
                let total_us = shared.total_elapsed_us.load(Ordering::Relaxed);
                (cube.clone(), phase, run_st, total_mv, total_us)
            };

            // 更新 HTTP /status 快照（JSON 格式）
            let solved = cube_snap.solved_count();
            let pct = solved as f64 / TOTAL as f64 * 100.0;
            let phase_str = match phase {
                SolvePhase::CenterReduction      => "center_reduction",
                SolvePhase::EdgePairing          => "edge_pairing",
                SolvePhase::ThreeByThreeReduction => "3x3_reduction",
                SolvePhase::Solved               => "solved",
            };
            let run_str = match run_st {
                RunState::Running => "running",
                RunState::Paused  => "paused",
            };
            let avg_us = if total_mv > 0 { total_us / total_mv } else { 0 };
            let snap_json = format!(
                concat!(
                    "{{",
                    "\"state\":\"{state}\",",
                    "\"phase\":\"{phase}\",",
                    "\"solved_cells\":{solved},",
                    "\"total_cells\":{total},",
                    "\"solved_pct\":{pct:.4},",
                    "\"total_moves\":{moves},",
                    "\"avg_move_us\":{avg_us}",
                    "}}"
                ),
                state  = run_str,
                phase  = phase_str,
                solved = solved,
                total  = TOTAL,
                pct    = pct,
                moves  = total_mv,
                avg_us = avg_us,
            );
            *shared.status_snapshot.write().unwrap() = snap_json;

            // 若正在运行，将快照推给探索线程（非阻塞 try_send，丢弃则跳过）
            if run_st == RunState::Running {
                let _ = snapshot_tx.try_send(cube_snap);
            }
        }

        // 短暂 sleep，避免空转烧 CPU（1ms 足够保证 200Hz 精度）
        thread::sleep(Duration::from_millis(1));
    }

    eprintln!("[scheduler] 退出");
}

// ============================================================
// 探索线程（Explorer × 8）
// ============================================================
//
// 每个探索线程：
//   1. 阻塞等待调度线程推来的最新快照
//   2. 根据当前阶段生成候选动作序列
//   3. 对每个候选在本地副本上快速模拟，计算启发代价
//   4. 将最优候选通过 result_tx 提交给执行线程
// ============================================================

fn explorer_thread(
    id: usize,
    shared: Arc<SharedState>,
    snapshot_rx: Receiver<CubeState>,
    result_tx: SyncSender<ExplorerResult>,
) {
    eprintln!("[explorer-{}] 启动", id);

    loop {
        if shared.shutdown.load(Ordering::Relaxed) { break; }

        // 阻塞等待新快照（超时 50ms 后重新检查 shutdown）
        let state = match snapshot_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let t0 = Instant::now();

        // 检测当前阶段
        let phase = detect_phase(&state);
        if phase == SolvePhase::Solved {
            eprintln!("[explorer-{}] 魔方已解决！", id);
            break;
        }

        // 根据阶段和探索器 ID 分配候选集
        // 8个探索器分工：
        //   0-1: 中心块候选（U/D 轴各层）
        //   2-3: 中心块候选（R/L 轴各层）
        //   4-5: 棱块配对候选
        //   6-7: 3阶或综合候选
        let candidates: Vec<Vec<Move>> = match (phase, id) {
            (SolvePhase::CenterReduction, 0 | 1) => {
                // U/D 轴：按探索器 ID 分配不同层范围
                let range_start = id * (N / 2 / 2);
                let range_end   = (range_start + N / 4).min(N);
                let mut cands = Vec::new();
                for layer in range_start..range_end {
                    cands.extend(center_candidate_moves(&state, U_FACE, layer));
                }
                cands
            }
            (SolvePhase::CenterReduction, 2 | 3) => {
                let offset = (id - 2) * (N / 4);
                let range_start = N / 2 + offset;
                let range_end   = (range_start + N / 4).min(N);
                let mut cands = Vec::new();
                for layer in range_start..range_end {
                    cands.extend(center_candidate_moves(&state, R_FACE, layer));
                }
                cands
            }
            (SolvePhase::CenterReduction, _) => {
                // 剩余探索器覆盖 F/B/L 面
                let face = [F_FACE, D_FACE, L_FACE, B_FACE][(id - 4).min(3)];
                let mut cands = Vec::new();
                for layer in (id * 10)..(id * 10 + 20).min(N) {
                    cands.extend(center_candidate_moves(&state, face, layer));
                }
                cands
            }
            (SolvePhase::EdgePairing, _) => {
                // 棱块：每个探索器负责一段层区间
                let range_start = id * (N / EXPLORER_COUNT);
                let range_end = (range_start + N / EXPLORER_COUNT).min(N);
                let mut cands = Vec::new();
                for layer in range_start..range_end {
                    cands.extend(edge_pairing_candidates(layer));
                }
                cands
            }
            (SolvePhase::ThreeByThreeReduction, _) | (SolvePhase::Solved, _) => {
                three_stage_candidates()
            }
        };

        // 评估所有候选，找代价最低的
        let mut best_cost = i64::MAX;
        let mut best_moves: Vec<Move> = Vec::new();

        for moves in &candidates {
            let mut sim = state.clone();
            sim.apply_seq(moves);
            let cost = sim.heuristic_cost();
            if cost < best_cost {
                best_cost = cost;
                best_moves = moves.clone();
            }
        }

        let elapsed_us = t0.elapsed().as_micros() as u64;

        // 提交结果（非阻塞，若通道满则本轮结果丢弃）
        let _ = result_tx.try_send(ExplorerResult {
            explorer_id: id,
            moves: best_moves,
            predicted_cost: best_cost,
            elapsed_us,
        });
    }

    eprintln!("[explorer-{}] 退出", id);
}

// ============================================================
// 执行线程（Executor）
// ============================================================
//
// 从 8 个探索线程收集结果，选择预测代价最低的，
// 实际修改全局魔方状态，更新统计数据。
// ============================================================

fn executor_thread(
    shared: Arc<SharedState>,
    result_rx: Receiver<ExplorerResult>,
) {
    eprintln!("[executor] 启动");

    // 每轮收集窗口：最多等 20ms，收集尽量多的探索结果后选最优
    const COLLECT_WINDOW: Duration = Duration::from_millis(20);

    loop {
        if shared.shutdown.load(Ordering::Relaxed) { break; }

        // 检查是否暂停
        {
            let rs = shared.run_state.lock().unwrap();
            if *rs == RunState::Paused {
                drop(rs);
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        }

        // 收集窗口内的所有探索结果
        let window_start = Instant::now();
        let mut results: Vec<ExplorerResult> = Vec::with_capacity(EXPLORER_COUNT);

        loop {
            let remaining = COLLECT_WINDOW.saturating_sub(window_start.elapsed());
            if remaining.is_zero() { break; }

            match result_rx.recv_timeout(remaining.min(Duration::from_millis(5))) {
                Ok(r) => { results.push(r); }
                Err(_) => break,
            }
        }

        if results.is_empty() {
            thread::sleep(Duration::from_millis(5));
            continue;
        }

        // 选预测代价最低的
        let best = results.iter().min_by_key(|r| r.predicted_cost).unwrap();

        if best.moves.is_empty() { continue; }

        let t0 = Instant::now();

        // 实际执行
        {
            let mut cube = shared.cube.lock().unwrap();
            cube.apply_seq(&best.moves);

            // 更新阶段
            let new_phase = detect_phase(&cube);
            *shared.phase.lock().unwrap() = new_phase;

            if new_phase == SolvePhase::Solved {
                eprintln!("[executor] 🎉 魔方已完全复原！");
                shared.shutdown.store(true, Ordering::SeqCst);
            }
        }

        let exec_us = t0.elapsed().as_micros() as u64;
        let move_count = best.moves.len() as u64;

        shared.total_moves.fetch_add(move_count, Ordering::Relaxed);
        shared.total_elapsed_us.fetch_add(
            exec_us + best.elapsed_us,
            Ordering::Relaxed,
        );

        // 每 1000 步打印进度
        let total = shared.total_moves.load(Ordering::Relaxed);
        if total % 1000 == 0 {
            let cube = shared.cube.lock().unwrap();
            let solved = cube.solved_count();
            let pct = solved as f64 / TOTAL as f64 * 100.0;
            eprintln!(
                "[executor] steps={} solved={}/{} ({:.2}%) best_from=explorer-{}",
                total, solved, TOTAL, pct, best.explorer_id
            );
        }
    }

    eprintln!("[executor] 退出");
}

// ============================================================
// 通道拓扑构建
// ============================================================
//
// 调度 ──(SyncSender<CubeState> × 8)──► 探索器[0..7]
//                                             │
//                              (SyncSender<ExplorerResult>)
//                                             ▼
//                                         执行线程
//
// SyncSender 有界通道（容量=1）保证低延迟：若消费者跟不上，
// 生产者 try_send 直接丢弃，不阻塞关键路径。
// ============================================================

// ============================================================
// main
// ============================================================

fn main() {
    // ---- 1. 加载 data.bin ----
    let bin_path = "data.bin";
    let raw = std::fs::read(bin_path).unwrap_or_else(|e| {
        eprintln!("无法读取 {}: {}（请先运行 init 生成 data.bin）", bin_path, e);
        std::process::exit(1);
    });
    let cube = CubeState::from_bytes(&raw);
    eprintln!(
        "已加载 {} — 当前已归位格子: {}/{} ({:.2}%)",
        bin_path,
        cube.solved_count(),
        TOTAL,
        cube.solved_count() as f64 / TOTAL as f64 * 100.0
    );
    eprintln!("初始求解阶段: {:?}", detect_phase(&cube));

    // ---- 2. 构建共享状态 ----
    let shared = SharedState::new(cube);

    // ---- 3. 构建通道 ----

    // 调度 → 探索器：每个探索器一个有界通道（容量=1，低延迟快照）
    let mut snapshot_txs: Vec<SyncSender<CubeState>> = Vec::with_capacity(EXPLORER_COUNT);
    let mut snapshot_rxs: Vec<Option<Receiver<CubeState>>> = (0..EXPLORER_COUNT).map(|_| {
        let (tx, rx) = mpsc::sync_channel::<CubeState>(1);
        snapshot_txs.push(tx);
        Some(rx)
    }).collect();

    // 探索器 → 执行线程：合并通道（容量=EXPLORER_COUNT）
    let (result_tx, result_rx) = mpsc::sync_channel::<ExplorerResult>(EXPLORER_COUNT);

    // ---- 4. 启动探索线程（8个）----
    for id in 0..EXPLORER_COUNT {
        let shared_c = Arc::clone(&shared);
        let snap_rx = snapshot_rxs[id].take().unwrap();
        let res_tx = result_tx.clone();
        thread::Builder::new()
            .name(format!("explorer-{}", id))
            .stack_size(4 * 1024 * 1024) // 4MB 栈（模拟用临时数组）
            .spawn(move || explorer_thread(id, shared_c, snap_rx, res_tx))
            .expect("启动探索线程失败");
    }
    drop(result_tx); // main 不再持有发送端

    // ---- 5. 启动执行线程（1个）----
    {
        let shared_c = Arc::clone(&shared);
        thread::Builder::new()
            .name("executor".into())
            .stack_size(4 * 1024 * 1024)
            .spawn(move || executor_thread(shared_c, result_rx))
            .expect("启动执行线程失败");
    }

    // ---- 6. 调度线程广播器（将单个 snapshot_tx 扇出给8个通道）----
    // 构建一个合并发送者：调度线程每 Tick 向所有探索器发送同一份快照
    // 此处用 Mutex<Vec<SyncSender>> 实现动态广播
    let broadcast_txs: Arc<Mutex<Vec<SyncSender<CubeState>>>> =
        Arc::new(Mutex::new(snapshot_txs));

    // 实际调度线程需要一个单一的 SyncSender；我们用一个"广播代理线程"桥接
    let (bcast_tx, bcast_rx) = mpsc::sync_channel::<CubeState>(1);
    {
        let txs = Arc::clone(&broadcast_txs);
        thread::Builder::new()
            .name("broadcaster".into())
            .spawn(move || {
                while let Ok(snap) = bcast_rx.recv() {
                    let txs = txs.lock().unwrap();
                    for tx in txs.iter() {
                        let _ = tx.try_send(snap.clone());
                    }
                }
            })
            .expect("启动广播线程失败");
    }

    // ---- 7. 主线程运行调度（HTTP + Tick）----
    eprintln!("所有线程已启动。");
    eprintln!("API: GET  http://{}/status", HTTP_ADDR);
    eprintln!("API: POST http://{}/control  body: start|pause|shutdown", HTTP_ADDR);
    eprintln!("发送 POST /control 并携带 \"start\" 开始求解。");

    scheduler_thread(shared, bcast_tx);

    eprintln!("主线程退出。");
}
