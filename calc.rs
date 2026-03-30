// calc.rs — 200阶魔方多线程求解器 + 原生 HTTP API
// 编译: rustc -C opt-level=2 --edition 2021 -o calc calc.rs
// 运行: ./calc  (读取当前目录 data.bin，监听 127.0.0.1:62001)
//
// API 端点:
//   GET  /health   — 健康检查（始终返回 "OK"）
//   GET  /status   — 5次/秒：精简进度 JSON（≈200字节）
//   GET  /cube     — 按需：完整魔方数据 Base64 JSON（≈330KB）
//   POST /control  — 指令：start | pause | shutdown
//
// 协议约定（与 windows.py 对齐）:
//   Python 端首次连接时请求 GET /cube 获取完整初始状态并渲染；
//   之后以 5Hz 轮询 GET /status 更新进度指标；
//   每次 Tick 后 /cube 同步更新，Python 可随时拉取最新完整状态。
//
// 线程模型:
//   scheduler (1) : 非阻塞 HTTP + 200ms Tick（5Hz）
//   broadcaster(1): 快照扇出到探索器
//   explorer  (8) : 并行探索最优步
//   executor  (1) : 汇总结果，修改全局状态
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
const TOTAL: usize = FACES * FACE_SIZE; // 240_000

const EXPLORER_COUNT: usize = 8;
const HTTP_ADDR: &str = "127.0.0.1:62001";

/// 5Hz Tick（每 200ms 刷新一次状态快照和 cube 快照）
const TICK_INTERVAL: Duration = Duration::from_millis(200);

const U_FACE: usize = 0;
const R_FACE: usize = 1;
const F_FACE: usize = 2;
const D_FACE: usize = 3;
const L_FACE: usize = 4;
const B_FACE: usize = 5;

// ============================================================
// 手写 Base64 编码（RFC 4648，无换行）
// 标准库无内建实现，约 50 行，覆盖全部边界情况。
// ============================================================

const B64_TABLE: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let len = data.len();
    let mut out = Vec::with_capacity((len + 2) / 3 * 4);
    let mut i = 0;
    while i + 2 < len {
        let (b0, b1, b2) = (data[i] as usize, data[i+1] as usize, data[i+2] as usize);
        out.push(B64_TABLE[b0 >> 2]);
        out.push(B64_TABLE[((b0 & 3) << 4) | (b1 >> 4)]);
        out.push(B64_TABLE[((b1 & 0xf) << 2) | (b2 >> 6)]);
        out.push(B64_TABLE[b2 & 0x3f]);
        i += 3;
    }
    if i < len {
        let b0 = data[i] as usize;
        out.push(B64_TABLE[b0 >> 2]);
        if i + 1 < len {
            let b1 = data[i+1] as usize;
            out.push(B64_TABLE[((b0 & 3) << 4) | (b1 >> 4)]);
            out.push(B64_TABLE[(b1 & 0xf) << 2]);
        } else {
            out.push(B64_TABLE[(b0 & 3) << 4]);
            out.push(b'=');
        }
        out.push(b'=');
    }
    // SAFETY: B64_TABLE 仅含 ASCII
    unsafe { String::from_utf8_unchecked(out) }
}

// ============================================================
// Move
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move { pub axis: u8, pub layer: u16, pub cw: bool }

impl Move {
    #[inline] pub fn inverse(self) -> Self { Move { cw: !self.cw, ..self } }
}

// ============================================================
// CubeState
// ============================================================

#[derive(Clone)]
pub struct CubeState { pub data: Box<[u8; TOTAL]> }

impl CubeState {
    pub fn new_zeroed() -> Self { CubeState { data: Box::new([0u8; TOTAL]) } }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), TOTAL);
        let mut s = Self::new_zeroed();
        s.data.copy_from_slice(bytes);
        s
    }

    #[inline(always)] fn idx(f: usize, r: usize, c: usize) -> usize { f*FACE_SIZE + r*N + c }
    #[inline(always)] pub fn get(&self, f: usize, r: usize, c: usize) -> u8 { self.data[Self::idx(f,r,c)] }
    #[inline(always)] pub fn set(&mut self, f: usize, r: usize, c: usize, v: u8) { self.data[Self::idx(f,r,c)] = v; }

    fn rotate_face_cw(&mut self, f: usize) {
        let b = f * FACE_SIZE;
        let s = &mut self.data[b..b+FACE_SIZE];
        for r in 0..N { for c in (r+1)..N { s.swap(r*N+c, c*N+r); } }
        for r in 0..N { s[r*N..r*N+N].reverse(); }
    }
    fn rotate_face_ccw(&mut self, f: usize) {
        let b = f * FACE_SIZE;
        let s = &mut self.data[b..b+FACE_SIZE];
        for r in 0..N { s[r*N..r*N+N].reverse(); }
        for r in 0..N { for c in (r+1)..N { s.swap(r*N+c, c*N+r); } }
    }

    #[inline]
    pub fn move_u_axis(&mut self, layer: usize, cw: bool) {
        let mut tmp = [0u8; N];
        for c in 0..N { tmp[c] = self.get(F_FACE, layer, c); }
        if cw {
            for c in 0..N { let v=self.get(L_FACE,layer,c);         self.set(F_FACE,layer,c,v); }
            for c in 0..N { let v=self.get(B_FACE,N-1-layer,N-1-c); self.set(L_FACE,layer,c,v); }
            for c in 0..N { let v=self.get(R_FACE,layer,N-1-c);     self.set(B_FACE,N-1-layer,c,v); }
            for c in 0..N { self.set(R_FACE,layer,c,tmp[c]); }
        } else {
            for c in 0..N { let v=self.get(R_FACE,layer,c);         self.set(F_FACE,layer,c,v); }
            for c in 0..N { let v=self.get(B_FACE,N-1-layer,N-1-c); self.set(R_FACE,layer,c,v); }
            for c in 0..N { let v=self.get(L_FACE,layer,N-1-c);     self.set(B_FACE,N-1-layer,c,v); }
            for c in 0..N { self.set(L_FACE,layer,c,tmp[c]); }
        }
        if layer==0 { if cw {self.rotate_face_cw(U_FACE);} else {self.rotate_face_ccw(U_FACE);} }
        else if layer==N-1 { if cw {self.rotate_face_ccw(D_FACE);} else {self.rotate_face_cw(D_FACE);} }
    }

    #[inline]
    pub fn move_r_axis(&mut self, layer: usize, cw: bool) {
        let (cf, cb) = (N-1-layer, layer);
        let mut tmp = [0u8; N];
        for r in 0..N { tmp[r] = self.get(U_FACE,r,cf); }
        if cw {
            for r in 0..N { let v=self.get(F_FACE,r,cf);     self.set(U_FACE,r,cf,v); }
            for r in 0..N { let v=self.get(D_FACE,r,cf);     self.set(F_FACE,r,cf,v); }
            for r in 0..N { let v=self.get(B_FACE,N-1-r,cb); self.set(D_FACE,r,cf,v); }
            for r in 0..N { self.set(B_FACE,N-1-r,cb,tmp[r]); }
        } else {
            for r in 0..N { let v=self.get(B_FACE,N-1-r,cb); self.set(U_FACE,r,cf,v); }
            for r in 0..N { let v=self.get(D_FACE,N-1-r,cf); self.set(B_FACE,r,cb,v); }
            for r in 0..N { let v=self.get(F_FACE,r,cf);     self.set(D_FACE,r,cf,v); }
            for r in 0..N { self.set(F_FACE,r,cf,tmp[r]); }
        }
        if layer==0 { if cw {self.rotate_face_cw(R_FACE);} else {self.rotate_face_ccw(R_FACE);} }
        else if layer==N-1 { if cw {self.rotate_face_ccw(L_FACE);} else {self.rotate_face_cw(L_FACE);} }
    }

    #[inline]
    pub fn move_f_axis(&mut self, layer: usize, cw: bool) {
        let (ru, rd, cr, cl) = (N-1-layer, layer, layer, N-1-layer);
        let mut tmp = [0u8; N];
        for c in 0..N { tmp[c] = self.get(U_FACE,ru,c); }
        if cw {
            for c in 0..N { let v=self.get(L_FACE,N-1-c,cl); self.set(U_FACE,ru,c,v); }
            for r in 0..N { let v=self.get(D_FACE,rd,N-1-r); self.set(L_FACE,r,cl,v); }
            for c in 0..N { let v=self.get(R_FACE,c,cr);     self.set(D_FACE,rd,c,v); }
            for r in 0..N { self.set(R_FACE,r,cr,tmp[r]); }
        } else {
            for c in 0..N { let v=self.get(R_FACE,c,cr);     self.set(U_FACE,ru,c,v); }
            for r in 0..N { let v=self.get(D_FACE,rd,N-1-r); self.set(R_FACE,r,cr,v); }
            for c in 0..N { let v=self.get(L_FACE,c,cl);     self.set(D_FACE,rd,c,v); }
            for r in 0..N { self.set(L_FACE,r,cl,tmp[N-1-r]); }
        }
        if layer==0 { if cw {self.rotate_face_cw(F_FACE);} else {self.rotate_face_ccw(F_FACE);} }
        else if layer==N-1 { if cw {self.rotate_face_ccw(B_FACE);} else {self.rotate_face_cw(B_FACE);} }
    }

    #[inline] pub fn apply(&mut self, mv: Move) {
        match mv.axis {
            0 => self.move_u_axis(mv.layer as usize, mv.cw),
            1 => self.move_r_axis(mv.layer as usize, mv.cw),
            2 => self.move_f_axis(mv.layer as usize, mv.cw),
            _ => unreachable!(),
        }
    }
    #[inline] pub fn apply_seq(&mut self, moves: &[Move]) { for &m in moves { self.apply(m); } }

    pub fn solved_count(&self) -> u32 {
        let mut n = 0u32;
        for f in 0..FACES { let e=f as u8; let b=f*FACE_SIZE;
            for i in 0..FACE_SIZE { if self.data[b+i]==e { n+=1; } } }
        n
    }
    pub fn center_solved_count(&self) -> u32 {
        let mut n = 0u32;
        for f in 0..FACES { let e=f as u8;
            for r in 1..N-1 { for c in 1..N-1 { if self.get(f,r,c)==e { n+=1; } } } }
        n
    }
    pub fn edge_solved_count(&self) -> u32 {
        let mut n = 0u32;
        for f in 0..FACES { let e=f as u8;
            for c in 1..N-1 { if self.get(f,0,c)==e { n+=1; } if self.get(f,N-1,c)==e { n+=1; } }
            for r in 1..N-1 { if self.get(f,r,0)==e { n+=1; } if self.get(f,r,N-1)==e { n+=1; } }
        }
        n
    }
    pub fn heuristic_cost(&self) -> i64 { TOTAL as i64 - self.solved_count() as i64 }

    /// 序列化为 Base64（供 /cube 端点，≈320KB 字符串）
    pub fn to_base64(&self) -> String { base64_encode(&*self.data) }
}

// ============================================================
// 降阶法求解骨架
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolvePhase { CenterReduction, EdgePairing, ThreeByThreeReduction, Solved }

pub fn detect_phase(s: &CubeState) -> SolvePhase {
    if s.solved_count() as usize == TOTAL { return SolvePhase::Solved; }
    if (s.center_solved_count() as usize) < FACES*(N-2)*(N-2) { return SolvePhase::CenterReduction; }
    if (s.edge_solved_count() as usize)   < FACES*4*(N-2)     { return SolvePhase::EdgePairing; }
    SolvePhase::ThreeByThreeReduction
}

pub fn center_candidate_moves(_: &CubeState, face: usize, layer: usize) -> Vec<Vec<Move>> {
    let ax = [0u8,1,2,0,1,2][face];
    vec![
        vec![Move{axis:ax,layer:layer as u16,cw:true}],
        vec![Move{axis:ax,layer:layer as u16,cw:false}],
        vec![Move{axis:ax,layer:layer as u16,cw:true},  Move{axis:(ax+1)%3,layer:(N/2)as u16,cw:true},  Move{axis:ax,layer:layer as u16,cw:false}],
        vec![Move{axis:ax,layer:layer as u16,cw:false}, Move{axis:(ax+2)%3,layer:(N/2)as u16,cw:false}, Move{axis:ax,layer:layer as u16,cw:true}],
    ]
}

pub fn edge_pairing_candidates(layer: usize) -> Vec<Vec<Move>> {
    let l = layer as u16;
    vec![
        vec![Move{axis:0,layer:0,cw:true}, Move{axis:1,layer:l,cw:true},  Move{axis:0,layer:0,cw:false},Move{axis:1,layer:l,cw:false}],
        vec![Move{axis:0,layer:0,cw:false},Move{axis:1,layer:l,cw:false}, Move{axis:0,layer:0,cw:true}, Move{axis:1,layer:l,cw:true}],
        vec![Move{axis:2,layer:0,cw:true}, Move{axis:0,layer:l,cw:true},  Move{axis:2,layer:0,cw:false},Move{axis:0,layer:l,cw:false}],
        vec![Move{axis:2,layer:0,cw:false},Move{axis:0,layer:l,cw:false}, Move{axis:2,layer:0,cw:true}, Move{axis:0,layer:l,cw:true}],
    ]
}

pub fn three_stage_candidates() -> Vec<Vec<Move>> {
    let mut v = Vec::with_capacity(12);
    for &layer in &[0u16,(N-1)as u16] {
        for axis in 0u8..3 {
            v.push(vec![Move{axis,layer,cw:true}]);
            v.push(vec![Move{axis,layer,cw:false}]);
        }
    }
    v
}

// ============================================================
// SharedState
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState { Paused, Running }

pub struct ExplorerResult { pub explorer_id: usize, pub moves: Vec<Move>, pub predicted_cost: i64, pub elapsed_us: u64 }

pub struct SharedState {
    pub cube: Mutex<CubeState>,
    pub run_state: Mutex<RunState>,
    pub total_moves: AtomicU64,
    pub total_elapsed_us: AtomicU64,
    pub phase: Mutex<SolvePhase>,
    /// 精简进度快照，5Hz 刷新，供 GET /status
    pub status_snapshot: RwLock<String>,
    /// 完整魔方 Base64 快照，5Hz 刷新，供 GET /cube
    pub cube_snapshot: RwLock<String>,
    pub shutdown: AtomicBool,
}

impl SharedState {
    pub fn new(cube: CubeState) -> Arc<Self> {
        let b64 = cube.to_base64();
        let cube_json = format!(
            "{{\"n\":{N},\"faces\":{FACES},\"state\":\"paused\",\"data\":\"{b64}\"}}"
        );
        Arc::new(Self {
            cube: Mutex::new(cube),
            run_state: Mutex::new(RunState::Paused),
            total_moves: AtomicU64::new(0),
            total_elapsed_us: AtomicU64::new(0),
            phase: Mutex::new(SolvePhase::CenterReduction),
            status_snapshot: RwLock::new("{\"status\":\"initializing\"}".into()),
            cube_snapshot: RwLock::new(cube_json),
            shutdown: AtomicBool::new(false),
        })
    }
}

// ============================================================
// HTTP 工具
// ============================================================

fn parse_request_line(buf: &[u8]) -> Option<(&str, &str)> {
    let text = std::str::from_utf8(buf).ok()?;
    let first = text.lines().next()?;
    let mut it = first.splitn(3, ' ');
    Some((it.next()?, it.next()?))
}

fn read_body(stream: &mut TcpStream, hdr_end: usize, buf: &[u8]) -> String {
    let hdr = std::str::from_utf8(&buf[..hdr_end]).unwrap_or("");
    let cl: usize = hdr.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.splitn(2,':').nth(1))
        .and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let body_start = (hdr_end + 4).min(buf.len());
    let mut body = buf[body_start..].to_vec();
    let rem = cl.saturating_sub(body.len());
    if rem > 0 { let mut ex = vec![0u8; rem]; let _ = stream.read(&mut ex); body.extend(ex); }
    String::from_utf8_lossy(&body).into_owned()
}

fn http_response(status: u16, ct: &str, body: &str) -> Vec<u8> {
    let st = match status { 200=>"OK",400=>"Bad Request",404=>"Not Found",405=>"Method Not Allowed",_=>"Error" };
    format!("HTTP/1.1 {status} {st}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}

fn handle_connection(mut stream: TcpStream, shared: &Arc<SharedState>) {
    let mut buf = vec![0u8; 8192];
    let n = match stream.read(&mut buf) { Ok(n) if n>0 => n, _ => return };
    buf.truncate(n);
    let hdr_end = buf.windows(4).position(|w| w==b"\r\n\r\n").unwrap_or(n.saturating_sub(4));

    let response = match parse_request_line(&buf[..hdr_end.min(n)]) {
        None => http_response(400, "text/plain", "Bad Request"),

        Some(("GET", "/status")) => {
            let s = shared.status_snapshot.read().unwrap();
            http_response(200, "application/json", &s)
        }

        // 完整魔方数据（Base64 编码），首次或按需拉取
        Some(("GET", "/cube")) => {
            let s = shared.cube_snapshot.read().unwrap();
            http_response(200, "application/json", &s)
        }

        Some(("GET", "/health")) => http_response(200, "text/plain", "OK"),

        Some(("POST", "/control")) => {
            let body = read_body(&mut stream, hdr_end, &buf).to_ascii_lowercase();
            let reply = if body.contains("start") || body.contains("resume") {
                *shared.run_state.lock().unwrap() = RunState::Running;
                "{\"ok\":true,\"state\":\"running\"}"
            } else if body.contains("pause") || body.contains("stop") {
                *shared.run_state.lock().unwrap() = RunState::Paused;
                "{\"ok\":true,\"state\":\"paused\"}"
            } else if body.contains("shutdown") {
                shared.shutdown.store(true, Ordering::SeqCst);
                "{\"ok\":true,\"state\":\"shutdown\"}"
            } else { "{\"ok\":false,\"error\":\"unknown command\"}" };
            http_response(200, "application/json", reply)
        }

        Some((_, "/status")) | Some((_, "/control")) | Some((_, "/cube")) =>
            http_response(405, "text/plain", "Method Not Allowed"),

        Some(_) => http_response(404, "text/plain", "Not Found"),
    };
    let _ = stream.write_all(&response);
}

// ============================================================
// 调度线程：非阻塞 HTTP + 5Hz Tick
// ============================================================

fn scheduler_thread(shared: Arc<SharedState>, snapshot_tx: SyncSender<CubeState>) {
    let listener = TcpListener::bind(HTTP_ADDR).expect("绑定端口失败");
    listener.set_nonblocking(true).expect("设置非阻塞失败");
    eprintln!("[scheduler] http://{} 已就绪（5Hz Tick）", HTTP_ADDR);

    let mut last_tick = Instant::now();

    loop {
        if shared.shutdown.load(Ordering::Relaxed) { break; }

        // 非阻塞 accept
        loop {
            match listener.accept() {
                Ok((s, _)) => {
                    let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                    let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
                    let sc = Arc::clone(&shared);
                    thread::spawn(move || handle_connection(s, &sc));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // 5Hz Tick
        if last_tick.elapsed() >= TICK_INTERVAL {
            last_tick = Instant::now();

            let (cube_snap, phase, run_st, mv, us) = {
                let cube = shared.cube.lock().unwrap();
                (cube.clone(),
                 *shared.phase.lock().unwrap(),
                 *shared.run_state.lock().unwrap(),
                 shared.total_moves.load(Ordering::Relaxed),
                 shared.total_elapsed_us.load(Ordering::Relaxed))
            };

            let solved = cube_snap.solved_count();
            let pct    = solved as f64 / TOTAL as f64 * 100.0;
            let avg_us = if mv > 0 { us / mv } else { 0 };

            let phase_str = match phase {
                SolvePhase::CenterReduction       => "center_reduction",
                SolvePhase::EdgePairing           => "edge_pairing",
                SolvePhase::ThreeByThreeReduction => "3x3_reduction",
                SolvePhase::Solved                => "solved",
            };
            let run_str = if run_st == RunState::Running { "running" } else { "paused" };

            // 精简 status JSON（≈200 字节）
            *shared.status_snapshot.write().unwrap() = format!(
                "{{\"state\":\"{run_str}\",\"phase\":\"{phase_str}\",\
                  \"solved_cells\":{solved},\"total_cells\":{TOTAL},\
                  \"solved_pct\":{pct:.6},\"total_moves\":{mv},\"avg_move_us\":{avg_us}}}"
            );

            // 完整 cube JSON（包含 Base64 数据，≈330KB）
            // Python 端每次需要重渲染展开图时 GET /cube
            *shared.cube_snapshot.write().unwrap() = format!(
                "{{\"n\":{N},\"faces\":{FACES},\"state\":\"{run_str}\",\"data\":\"{}\"}}",
                cube_snap.to_base64()
            );

            // 广播给探索器
            if run_st == RunState::Running {
                let _ = snapshot_tx.try_send(cube_snap);
            }

            eprintln!("[tick] {run_str} | {phase_str} | {solved}/{TOTAL} = {pct:.2}% | moves={mv}");
        }

        thread::sleep(Duration::from_millis(10));
    }

    eprintln!("[scheduler] 退出");
}

// ============================================================
// 探索线程（×8）
// ============================================================

fn explorer_thread(id: usize, shared: Arc<SharedState>, rx: Receiver<CubeState>, tx: SyncSender<ExplorerResult>) {
    eprintln!("[explorer-{id}] 启动");
    loop {
        if shared.shutdown.load(Ordering::Relaxed) { break; }
        let state = match rx.recv_timeout(Duration::from_millis(50)) { Ok(s)=>s, Err(_)=>continue };
        let t0 = Instant::now();
        let phase = detect_phase(&state);
        if phase == SolvePhase::Solved { eprintln!("[explorer-{id}] 已解决"); break; }

        let cands: Vec<Vec<Move>> = match (phase, id) {
            (SolvePhase::CenterReduction, 0|1) => {
                let r0=id*(N/4); (r0..(r0+N/4).min(N)).flat_map(|l| center_candidate_moves(&state,U_FACE,l)).collect()
            }
            (SolvePhase::CenterReduction, 2|3) => {
                let r0=(id-2)*(N/4)+N/2; (r0..(r0+N/4).min(N)).flat_map(|l| center_candidate_moves(&state,R_FACE,l)).collect()
            }
            (SolvePhase::CenterReduction, _) => {
                let face=[F_FACE,D_FACE,L_FACE,B_FACE][(id-4).min(3)];
                (id*10..(id*10+20).min(N)).flat_map(|l| center_candidate_moves(&state,face,l)).collect()
            }
            (SolvePhase::EdgePairing, _) => {
                let r0=id*(N/EXPLORER_COUNT);
                (r0..(r0+N/EXPLORER_COUNT).min(N)).flat_map(|l| edge_pairing_candidates(l)).collect()
            }
            _ => three_stage_candidates(),
        };

        let (mut best_cost, mut best_moves) = (i64::MAX, Vec::new());
        for moves in &cands {
            let mut sim = state.clone(); sim.apply_seq(moves);
            let cost = sim.heuristic_cost();
            if cost < best_cost { best_cost = cost; best_moves = moves.clone(); }
        }
        let _ = tx.try_send(ExplorerResult { explorer_id:id, moves:best_moves, predicted_cost:best_cost, elapsed_us:t0.elapsed().as_micros()as u64 });
    }
    eprintln!("[explorer-{id}] 退出");
}

// ============================================================
// 执行线程
// ============================================================

fn executor_thread(shared: Arc<SharedState>, rx: Receiver<ExplorerResult>) {
    eprintln!("[executor] 启动");
    loop {
        if shared.shutdown.load(Ordering::Relaxed) { break; }
        if *shared.run_state.lock().unwrap() == RunState::Paused { thread::sleep(Duration::from_millis(10)); continue; }

        let t_win = Instant::now();
        let mut results = Vec::with_capacity(EXPLORER_COUNT);
        loop {
            let rem = Duration::from_millis(20).saturating_sub(t_win.elapsed());
            if rem.is_zero() { break; }
            match rx.recv_timeout(rem.min(Duration::from_millis(5))) { Ok(r)=>results.push(r), Err(_)=>break }
        }
        if results.is_empty() { thread::sleep(Duration::from_millis(5)); continue; }

        let best = results.iter().min_by_key(|r| r.predicted_cost).unwrap();
        if best.moves.is_empty() { continue; }

        let t0 = Instant::now();
        {
            let mut cube = shared.cube.lock().unwrap();
            cube.apply_seq(&best.moves);
            let ph = detect_phase(&cube);
            *shared.phase.lock().unwrap() = ph;
            if ph == SolvePhase::Solved {
                eprintln!("[executor] 🎉 复原完成！");
                shared.shutdown.store(true, Ordering::SeqCst);
            }
        }
        shared.total_moves.fetch_add(best.moves.len() as u64, Ordering::Relaxed);
        shared.total_elapsed_us.fetch_add(t0.elapsed().as_micros() as u64 + best.elapsed_us, Ordering::Relaxed);
    }
    eprintln!("[executor] 退出");
}

// ============================================================
// main
// ============================================================

fn main() {
    let raw = std::fs::read("data.bin").unwrap_or_else(|e| {
        eprintln!("无法读取 data.bin: {e}"); std::process::exit(1);
    });
    let cube = CubeState::from_bytes(&raw);
    eprintln!("已加载 data.bin — 归位 {}/{TOTAL} ({:.2}%) 阶段: {:?}",
        cube.solved_count(), cube.solved_count() as f64/TOTAL as f64*100.0, detect_phase(&cube));

    let shared = SharedState::new(cube);

    // 探索器通道（×8，容量1）
    let mut snap_txs = Vec::with_capacity(EXPLORER_COUNT);
    let mut snap_rxs: Vec<_> = (0..EXPLORER_COUNT).map(|_| {
        let (tx,rx) = mpsc::sync_channel::<CubeState>(1); snap_txs.push(tx); Some(rx)
    }).collect();

    // 执行器通道
    let (res_tx, res_rx) = mpsc::sync_channel::<ExplorerResult>(EXPLORER_COUNT);

    // 启动探索线程
    for id in 0..EXPLORER_COUNT {
        let (sc, rx, tx) = (Arc::clone(&shared), snap_rxs[id].take().unwrap(), res_tx.clone());
        thread::Builder::new().name(format!("explorer-{id}")).stack_size(4*1024*1024)
            .spawn(move || explorer_thread(id, sc, rx, tx)).unwrap();
    }
    drop(res_tx);

    // 启动执行线程
    { let sc=Arc::clone(&shared); thread::Builder::new().name("executor".into()).stack_size(4*1024*1024)
        .spawn(move || executor_thread(sc, res_rx)).unwrap(); }

    // 广播代理
    let bcast_txs = Arc::new(Mutex::new(snap_txs));
    let (bcast_tx, bcast_rx) = mpsc::sync_channel::<CubeState>(1);
    { let txs=Arc::clone(&bcast_txs);
      thread::Builder::new().name("broadcaster".into()).spawn(move || {
          while let Ok(snap)=bcast_rx.recv() { let ts=txs.lock().unwrap(); for t in ts.iter() { let _=t.try_send(snap.clone()); } }
      }).unwrap(); }

    eprintln!("=== 就绪 === GET /health /status /cube | POST /control");
    scheduler_thread(shared, bcast_tx);
    eprintln!("退出。");
}
