#!/usr/bin/env python3
"""
windows.py — 200阶魔方复原可视化控制台
===========================================
协议:
  GET  /health          — 连接检查
  GET  /cube            — 首次 & 按需：完整 Base64 魔方数据（~330KB JSON）
  GET  /status          — 5Hz 轮询：精简进度 JSON
  POST /control         — start | pause | shutdown

渲染:
  tkinter.PhotoImage 像素直写，6 个面十字展开图
  窗口可任意拖拽调整大小，画布与面板比例自适应

线程:
  主线程(tk) — UI 渲染 + after() 驱动的状态应用
  StatusPoller — 5Hz GET /status
  CubePoller   — 按需 GET /cube（首次 + 每隔 N 秒）
  CpuMonitor   — 跨平台读取 calc 进程 CPU%
"""

import base64
import json
import os
import platform
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from collections import deque
from tkinter import (
    Canvas, Frame, Label, Button, StringVar,
    Tk, PhotoImage, PanedWindow, scrolledtext
)
import tkinter as tk

# ── API ───────────────────────────────────────────────────────
API_BASE      = "http://127.0.0.1:62001"
STATUS_HZ     = 5          # /status 轮询频率
CUBE_INTERVAL = 2.0        # /cube 完整刷新间隔（秒）

# ── 魔方参数 ──────────────────────────────────────────────────
N     = 200
FACES = 6
TOTAL = FACES * N * N      # 240_000

# 展开图：十字布局  (grid_col, grid_row, face_index, label)
#         U(0)
#   L(4)  F(2)  R(1)  B(5)
#         D(3)
LAYOUT = [
    (1, 0, 0, "U"),
    (0, 1, 4, "L"),
    (1, 1, 2, "F"),
    (2, 1, 1, "R"),
    (3, 1, 5, "B"),
    (1, 2, 3, "D"),
]
# 颜色表（face_index → hex），与 Rust init.rs 一致
# 0=U白 1=R红 2=F绿 3=D黄 4=L橙 5=B蓝
FACE_COLORS = [
    "#F0F0F0",  # 0 U 白
    "#E53935",  # 1 R 红
    "#43A047",  # 2 F 绿
    "#FDD835",  # 3 D 黄
    "#FB8C00",  # 4 L 橙
    "#1E88E5",  # 5 B 蓝
]
UNKNOWN_COLOR = "#1a1a2e"

# ── 配色 ──────────────────────────────────────────────────────
BG       = "#0d0d14"
BG2      = "#13131f"
BG3      = "#1a1a2e"
ACCENT   = "#00e5ff"
ACCENT2  = "#ff6b35"
GREEN    = "#00e676"
RED      = "#ff3d00"
YELLOW   = "#ffd600"
TEXT1    = "#e8e8f4"
TEXT2    = "#6b6b88"
BORDER   = "#252538"

# ══════════════════════════════════════════════════════════════
# HTTP 工具
# ══════════════════════════════════════════════════════════════

def _request(method: str, path: str, body: str = "", timeout: float = 1.5):
    try:
        data = body.encode() if body else None
        req = urllib.request.Request(
            f"{API_BASE}{path}", data=data, method=method,
            headers={"Content-Type": "text/plain"} if data else {},
        )
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode())
    except Exception:
        return None

def http_get(path, timeout=1.5):  return _request("GET",  path, timeout=timeout)
def http_post(path, body=""):     return _request("POST", path, body=body, timeout=1.5)

# ══════════════════════════════════════════════════════════════
# CPU 监控（后台线程，跨平台无 psutil）
# ══════════════════════════════════════════════════════════════

class CpuMonitor(threading.Thread):
    def __init__(self):
        super().__init__(daemon=True, name="cpu-mon")
        self._pct  = -1.0
        self._lock = threading.Lock()
        self._stop = threading.Event()

    def stop(self): self._stop.set()

    @property
    def pct(self):
        with self._lock: return self._pct

    def _set(self, v):
        with self._lock: self._pct = v

    # Linux: /proc/<pid>/stat 双采样
    @staticmethod
    def _find_pid_linux(name):
        try:
            for pid in os.listdir("/proc"):
                if not pid.isdigit(): continue
                try:
                    if name.encode() in open(f"/proc/{pid}/cmdline", "rb").read():
                        return int(pid)
                except OSError: pass
        except Exception: pass
        return None

    @staticmethod
    def _sample_linux(pid):
        try:
            parts = open(f"/proc/{pid}/stat").read().split()
            proc  = int(parts[13]) + int(parts[14])
            total = sum(int(x) for x in open("/proc/stat").readline().split()[1:])
            return proc, total
        except Exception: return None, None

    def _cpu_linux(self, pid, interval=2.0):
        p1, t1 = self._sample_linux(pid)
        if p1 is None: return -1.0
        time.sleep(interval)
        p2, t2 = self._sample_linux(pid)
        if p2 is None: return -1.0
        dt = t2 - t1
        return (p2 - p1) / dt * 100.0 if dt > 0 else 0.0

    @staticmethod
    def _cpu_windows(name):
        try:
            out = subprocess.check_output(
                f'wmic process where "name=\'{name}\'" get PercentProcessorTime /value',
                shell=True, timeout=3, stderr=subprocess.DEVNULL
            ).decode(errors="ignore")
            m = re.search(r"PercentProcessorTime=(\d+)", out)
            return float(m.group(1)) if m else -1.0
        except Exception: return -1.0

    @staticmethod
    def _cpu_macos(name):
        try:
            out = subprocess.check_output(
                ["ps", "-eo", "pcpu,comm"], timeout=3, stderr=subprocess.DEVNULL
            ).decode(errors="ignore")
            total = sum(float(l.split()[0]) for l in out.splitlines()
                        if name in l and l.strip())
            return total if total > 0 else -1.0
        except Exception: return -1.0

    def run(self):
        sys_ = platform.system()
        name = "calc.exe" if sys_ == "Windows" else "calc"
        while not self._stop.is_set():
            try:
                if sys_ == "Linux":
                    pid = self._find_pid_linux(name)
                    if pid:
                        self._set(self._cpu_linux(pid, interval=2.0))
                    else:
                        self._set(-1.0)
                        time.sleep(2.0)
                elif sys_ == "Windows":
                    self._set(self._cpu_windows(name))
                    time.sleep(2.0)
                elif sys_ == "Darwin":
                    self._set(self._cpu_macos(name))
                    time.sleep(2.0)
                else:
                    time.sleep(2.0)
            except Exception:
                time.sleep(2.0)

# ══════════════════════════════════════════════════════════════
# 状态轮询（5Hz GET /status）
# ══════════════════════════════════════════════════════════════

class StatusPoller(threading.Thread):
    def __init__(self, queue: deque, stop: threading.Event):
        super().__init__(daemon=True, name="status-poller")
        self.queue  = queue
        self.stop   = stop
        self.errors = 0

    def run(self):
        interval = 1.0 / STATUS_HZ
        while not self.stop.is_set():
            t0 = time.perf_counter()
            data = http_get("/status", timeout=0.5)
            if data is not None:
                self.errors = 0
                self.queue.clear()
                self.queue.append(data)
            else:
                self.errors += 1
            elapsed = time.perf_counter() - t0
            time.sleep(max(0.0, interval - elapsed))

# ══════════════════════════════════════════════════════════════
# 魔方数据拉取（按需 GET /cube）
# ══════════════════════════════════════════════════════════════

class CubePoller(threading.Thread):
    """
    首次连接时立即拉取完整魔方数据；
    之后每隔 CUBE_INTERVAL 秒拉取一次（同步最新状态到渲染器）。
    """
    def __init__(self, on_data, stop: threading.Event):
        super().__init__(daemon=True, name="cube-poller")
        self.on_data = on_data   # callback(face_arrays: list[list[int]])
        self.stop    = stop
        self.last_ok = 0.0
        self.ready   = threading.Event()  # 首次拉取完成信号

    def _fetch_and_deliver(self):
        data = http_get("/cube", timeout=3.0)
        if data is None:
            return False
        raw_b64 = data.get("data", "")
        if not raw_b64:
            return False
        raw = base64.b64decode(raw_b64)
        if len(raw) != TOTAL:
            return False
        # 分割为 6 个面的列表
        faces = []
        for f in range(FACES):
            start = f * N * N
            faces.append(list(raw[start:start + N * N]))
        self.on_data(faces)
        self.last_ok = time.perf_counter()
        return True

    def run(self):
        # 首次：持续重试直到成功
        while not self.stop.is_set():
            if self._fetch_and_deliver():
                self.ready.set()
                break
            time.sleep(0.5)

        # 后续：每 CUBE_INTERVAL 秒刷新一次
        while not self.stop.is_set():
            time.sleep(0.2)
            if time.perf_counter() - self.last_ok >= CUBE_INTERVAL:
                self._fetch_and_deliver()

# ══════════════════════════════════════════════════════════════
# 魔方展开图渲染（PhotoImage 像素直写）
# ══════════════════════════════════════════════════════════════

class CubeRenderer:
    """
    使用 PhotoImage.put(row_str, to=...) 批量写像素。
    支持任意缩放：cell_px 由外部传入（1~4），变化时重建图像。
    """
    def __init__(self, canvas: Canvas):
        self.canvas   = canvas
        self.cell_px  = 1
        self.img      = None
        self._img_id  = None
        self._face_offsets: dict[int, tuple[int, int]] = {}  # face_idx → (ox, oy)
        self._pending_faces: list | None = None  # 待渲染的完整数据
        self._lock = threading.Lock()

    def set_pending(self, faces: list):
        """CubePoller 调用：存储待渲染数据（由 tk 主线程实际渲染）"""
        with self._lock:
            self._pending_faces = faces

    def get_pending(self):
        with self._lock:
            f = self._pending_faces
            self._pending_faces = None
            return f

    def rebuild(self, canvas_w: int, canvas_h: int):
        """窗口尺寸变化时重建图像尺寸和 face 偏移表"""
        # 十字展开图：4列 × 3行 → 每格面积 = min(W/4, H/3)
        cell = min(canvas_w // 4, canvas_h // 3)
        self.cell_px = max(1, cell // N)  # 每格像素数（至少1）
        face_px = self.cell_px * N        # 每面像素宽/高

        img_w = 4 * face_px
        img_h = 3 * face_px

        if self.img is None or self.img.width() != img_w or self.img.height() != img_h:
            self.img = PhotoImage(width=img_w, height=img_h)
            if self._img_id is not None:
                self.canvas.itemconfigure(self._img_id, image=self.img)
            else:
                # 居中显示
                self._img_id = self.canvas.create_image(
                    canvas_w // 2, canvas_h // 2, anchor="center", image=self.img
                )
            # 填充背景
            row_str = "{" + " ".join([UNKNOWN_COLOR] * img_w) + "}"
            for y in range(img_h):
                self.img.put(row_str, to=(0, y, img_w, y + 1))

        # 重算 face 偏移（相对于图像左上角）
        self._face_offsets.clear()
        for grid_c, grid_r, fi, _ in LAYOUT:
            self._face_offsets[fi] = (grid_c * face_px, grid_r * face_px)

        # 更新图像在 canvas 上的位置（始终居中）
        if self._img_id is not None:
            self.canvas.coords(self._img_id, canvas_w // 2, canvas_h // 2)

        return face_px

    def render_faces(self, faces: list):
        """
        faces: list of 6 sublists, each N*N int (color 0-5), 行优先。
        调用前须先 rebuild() 至少一次。
        """
        if self.img is None:
            return
        cpx = self.cell_px
        for fi, face_data in enumerate(faces):
            if fi not in self._face_offsets:
                continue
            ox, oy = self._face_offsets[fi]
            for r in range(N):
                row_start = r * N
                # 构建该行的颜色字符串（每格 cpx 像素宽）
                colors = []
                for c in range(N):
                    col = FACE_COLORS[face_data[row_start + c]]
                    for _ in range(cpx):
                        colors.append(col)
                row_str = "{" + " ".join(colors) + "}"
                # 每格纵向重复 cpx 次
                for dy in range(cpx):
                    y = oy + r * cpx + dy
                    self.canvas.after_idle(
                        lambda rs=row_str, x0=ox, y0=y, x1=ox + cpx * N, y1=y + 1:
                            self.img.put(rs, to=(x0, y0, x1, y1))
                    )

    def render_faces_sync(self, faces: list):
        """同步版（在 tk 主线程调用时使用）"""
        if self.img is None:
            return
        cpx = self.cell_px
        for fi, face_data in enumerate(faces):
            if fi not in self._face_offsets:
                continue
            ox, oy = self._face_offsets[fi]
            for r in range(N):
                row_start = r * N
                colors = []
                for c in range(N):
                    col = FACE_COLORS[face_data[row_start + c]]
                    for _ in range(cpx):
                        colors.append(col)
                row_str = "{" + " ".join(colors) + "}"
                for dy in range(cpx):
                    y = oy + r * cpx + dy
                    self.img.put(row_str, to=(ox, y, ox + cpx * N, y + 1))

# ══════════════════════════════════════════════════════════════
# 主 App
# ══════════════════════════════════════════════════════════════

class App:
    # ── 初始化 ────────────────────────────────────────────────

    def __init__(self, root: Tk):
        self.root = root
        self.root.title("Cube·200 Solver Monitor")
        self.root.configure(bg=BG)
        self.root.minsize(640, 400)

        # 内部状态
        self._status_queue: deque = deque(maxlen=1)
        self._stop         = threading.Event()
        self._running      = False
        self._start_ns: int | None = None
        self._last_status: dict = {}
        self._canvas_w     = 0
        self._canvas_h     = 0
        self._needs_rebuild= True   # 尺寸变化标志

        # 线程
        self._status_poller = StatusPoller(self._status_queue, self._stop)
        self._cube_poller   = CubePoller(self._on_cube_data, self._stop)
        self._cpu_mon       = CpuMonitor()

        # 构建 UI
        self._build_ui()
        self._renderer = CubeRenderer(self._canvas)

        # 绑定窗口大小变化
        self.root.bind("<Configure>", self._on_resize)

        # 启动后台线程
        self._status_poller.start()
        self._cube_poller.start()
        self._cpu_mon.start()

        # 启动渲染循环
        self.root.after(50, self._tick)
        self.root.protocol("WM_DELETE_WINDOW", self._on_close)

    # ── UI 构建 ───────────────────────────────────────────────

    def _build_ui(self):
        # 顶部标题栏
        hdr = Frame(self.root, bg=BG, height=36)
        hdr.pack(fill="x", side="top")
        hdr.pack_propagate(False)

        Label(hdr, text=" ⬡  CUBE·200  SOLVER", bg=BG, fg=ACCENT,
              font=("Courier New", 13, "bold")).pack(side="left", padx=10)

        self._conn_var = StringVar(value="● OFFLINE")
        Label(hdr, textvariable=self._conn_var, bg=BG, fg=RED,
              font=("Courier New", 9)).pack(side="right", padx=12)

        Frame(self.root, bg=BORDER, height=1).pack(fill="x", side="top")

        # 主体：PanedWindow（左=画布，右=控制面板），允许拖拽分隔线
        paned = PanedWindow(self.root, orient="horizontal",
                            bg=BG, sashwidth=4, sashrelief="flat",
                            handlesize=0)
        paned.pack(fill="both", expand=True)

        # 左：魔方画布（自适应缩放）
        left = Frame(paned, bg=BG)
        paned.add(left, stretch="always", minsize=320)

        self._canvas = Canvas(left, bg=BG3, highlightthickness=0, bd=0)
        self._canvas.pack(fill="both", expand=True, padx=6, pady=6)

        # 右：控制面板（固定宽度 240，可手动拖宽）
        right = Frame(paned, bg=BG2, width=240)
        paned.add(right, stretch="never", minsize=200)
        right.pack_propagate(False)

        self._build_panel(right)

    def _build_panel(self, parent):
        pad = {"padx": 10}

        # ── 按钮 ──────────────────────────────────────────────
        btn_row = Frame(parent, bg=BG2)
        btn_row.pack(fill="x", pady=(10, 4), **pad)

        bstyle = dict(font=("Courier New", 9, "bold"), relief="flat",
                      cursor="hand2", padx=10, pady=6)
        self._btn_start = Button(btn_row, text="▶ START", bg=GREEN, fg=BG,
            activebackground="#00c853", activeforeground=BG,
            command=self._cmd_start, **bstyle)
        self._btn_start.pack(side="left", fill="x", expand=True, padx=(0, 3))

        self._btn_pause = Button(btn_row, text="⏸ PAUSE", bg=ACCENT2, fg=BG,
            activebackground="#e64a19", activeforeground=BG,
            command=self._cmd_pause, **bstyle)
        self._btn_pause.pack(side="left", fill="x", expand=True)

        btn_row2 = Frame(parent, bg=BG2)
        btn_row2.pack(fill="x", pady=(0, 8), **pad)
        Button(btn_row2, text="⟳ REFRESH CUBE", bg=BG3, fg=ACCENT,
               activebackground=BG3, activeforeground=ACCENT,
               font=("Courier New", 9), relief="flat", cursor="hand2",
               padx=10, pady=5,
               command=self._cmd_refresh).pack(fill="x")

        Frame(parent, bg=BORDER, height=1).pack(fill="x", **pad)

        # ── 指标 ──────────────────────────────────────────────
        self._metrics: dict[str, StringVar] = {}
        rows = [
            ("phase",    "PHASE",     "—"),
            ("pct",      "COMPLETED", "0.000000 %"),
            ("cells",    "CELLS",     f"0 / {TOTAL:,}"),
            ("moves",    "MOVES",     "0"),
            ("avg_us",   "AVG/MOVE",  "0 µs"),
            ("cpu",      "CALC CPU",  "—"),
            ("fps",      "UI FPS",    "—"),
        ]
        for key, label, default in rows:
            sv = StringVar(value=default)
            self._metrics[key] = sv
            row = Frame(parent, bg=BG2)
            row.pack(fill="x", pady=1, **pad)
            Label(row, text=label, bg=BG2, fg=TEXT2,
                  font=("Courier New", 8), width=11, anchor="w").pack(side="left")
            Label(row, textvariable=sv, bg=BG2, fg=TEXT1,
                  font=("Courier New", 9, "bold"), anchor="e").pack(side="right")
            Frame(parent, bg=BG3, height=1).pack(fill="x", **pad)

        Frame(parent, bg=BORDER, height=1).pack(fill="x", **pad, pady=(4,0))

        # ── 纳秒精确计时器 ────────────────────────────────────
        Label(parent, text="ELAPSED (ns precision)", bg=BG2, fg=TEXT2,
              font=("Courier New", 7)).pack(anchor="w", **pad, pady=(8, 0))
        self._timer_var = StringVar(value="00:00:00.000 000 000")
        Label(parent, textvariable=self._timer_var, bg=BG2, fg=ACCENT,
              font=("Courier New", 11, "bold"), justify="center").pack(**pad, pady=(2, 6))

        Frame(parent, bg=BORDER, height=1).pack(fill="x", **pad)

        # ── 进度条 ────────────────────────────────────────────
        Label(parent, text="SOLVE PROGRESS", bg=BG2, fg=TEXT2,
              font=("Courier New", 7)).pack(anchor="w", **pad, pady=(8, 0))
        pb_wrap = Frame(parent, bg=BG3, height=12)
        pb_wrap.pack(fill="x", **pad, pady=(2, 8))
        pb_wrap.pack_propagate(False)
        self._pb_fill = Frame(pb_wrap, bg=GREEN, height=12)
        self._pb_fill.place(x=0, y=0, height=12, relwidth=0.0)
        self._pb_wrap = pb_wrap

        Frame(parent, bg=BORDER, height=1).pack(fill="x", **pad)

        # ── 错误提示 ──────────────────────────────────────────
        self._err_var = StringVar(value="")
        Label(parent, textvariable=self._err_var, bg=BG2, fg=RED,
              font=("Courier New", 8), wraplength=220, justify="left",
              anchor="w").pack(fill="x", **pad, pady=(6, 4))

        # ── 连接说明 ──────────────────────────────────────────
        Label(parent, text=f"API  {API_BASE}", bg=BG2, fg=TEXT2,
              font=("Courier New", 7)).pack(anchor="w", **pad, pady=(4, 0))

    # ── 回调：来自 CubePoller（后台线程）─────────────────────

    def _on_cube_data(self, faces: list):
        """由 CubePoller 线程调用，通过 renderer 存储待渲染数据"""
        self._renderer.set_pending(faces)

    # ── 回调：窗口尺寸变化 ────────────────────────────────────

    def _on_resize(self, event):
        # 只响应画布本身的尺寸变化
        if event.widget is self._canvas:
            w, h = event.width, event.height
            if w != self._canvas_w or h != self._canvas_h:
                self._canvas_w  = w
                self._canvas_h  = h
                self._needs_rebuild = True

    # ── 控制按钮 ──────────────────────────────────────────────

    def _cmd_start(self):
        def _go():
            r = http_post("/control", "start")
            if r and r.get("ok"):
                if not self._running:
                    self._running  = True
                    self._start_ns = time.perf_counter_ns()
            else:
                self._err_var.set("⚠ 连接失败，请确认 calc 已启动")
        threading.Thread(target=_go, daemon=True).start()

    def _cmd_pause(self):
        threading.Thread(target=lambda: http_post("/control", "pause"),
                         daemon=True).start()

    def _cmd_refresh(self):
        """强制立即拉取一次完整魔方数据"""
        def _go():
            data = http_get("/cube", timeout=3.0)
            if not data: return
            raw = base64.b64decode(data.get("data", ""))
            if len(raw) != TOTAL: return
            faces = [list(raw[f*N*N:(f+1)*N*N]) for f in range(FACES)]
            self._renderer.set_pending(faces)
        threading.Thread(target=_go, daemon=True).start()

    # ── 主 Tick（~30fps UI 更新）─────────────────────────────

    _frame_times: deque = deque(maxlen=60)

    def _tick(self):
        now_ns = time.perf_counter_ns()
        self._frame_times.append(now_ns)

        # 1. 若画布尺寸变化，重建渲染器
        if self._needs_rebuild and self._canvas_w > 10 and self._canvas_h > 10:
            self._needs_rebuild = False
            self._renderer.rebuild(self._canvas_w, self._canvas_h)

        # 2. 应用待渲染的魔方数据（CubePoller 放入）
        pending = self._renderer.get_pending()
        if pending is not None and self._canvas_w > 10:
            if self._renderer.img is None:
                self._renderer.rebuild(self._canvas_w, self._canvas_h)
            self._renderer.render_faces_sync(pending)

        # 3. 拉取最新 /status 数据
        if self._status_queue:
            s = self._status_queue[-1]
            self._last_status = s
            self._apply_status(s)

        # 4. 连接状态指示
        errs = self._status_poller.errors
        if errs > 3:
            self._conn_var.set(f"● OFFLINE (×{errs})")
            # 使用 tk 的 configure 而非在 Label 上直接调用
            self._err_var.set("⚠ 无法连接 calc 服务")
        else:
            self._conn_var.set("● ONLINE")
            if not self._err_var.get().startswith("⚠ 连接"):
                self._err_var.set("")

        # 5. 纳秒计时器
        if self._running and self._start_ns is not None:
            elapsed = time.perf_counter_ns() - self._start_ns
            self._timer_var.set(self._fmt_ns(elapsed))

        # 6. CPU 占比
        cpu = self._cpu_mon.pct
        self._metrics["cpu"].set(f"{cpu:.1f} %" if cpu >= 0 else "N/A")

        # 7. FPS
        if len(self._frame_times) >= 2:
            span = (self._frame_times[-1] - self._frame_times[0]) / 1e9
            fps  = (len(self._frame_times) - 1) / span if span > 0 else 0
            self._metrics["fps"].set(f"{fps:.0f}")

        # 调度下一帧（~30fps）
        self.root.after(33, self._tick)

    # ── 状态应用 ──────────────────────────────────────────────

    def _apply_status(self, s: dict):
        phase_map = {
            "center_reduction":  "CENTER  ▸",
            "edge_pairing":      "EDGES   ▸",
            "3x3_reduction":     "3×3     ▸",
            "solved":            "SOLVED  ✓",
        }
        self._metrics["phase"].set(phase_map.get(s.get("phase",""), s.get("phase","—")))

        pct = s.get("solved_pct", 0.0)
        self._metrics["pct"].set(f"{pct:.6f} %")

        solved = s.get("solved_cells", 0)
        self._metrics["cells"].set(f"{solved:,} / {TOTAL:,}")
        self._metrics["moves"].set(f"{s.get('total_moves', 0):,}")
        self._metrics["avg_us"].set(f"{s.get('avg_move_us', 0):,} µs")

        # 进度条
        try:
            self._pb_fill.place_configure(relwidth=min(1.0, pct / 100.0))
        except Exception:
            pass

        # 自动开始计时
        if s.get("state") == "running" and not self._running:
            self._running  = True
            self._start_ns = time.perf_counter_ns()

    # ── 纳秒格式化 ────────────────────────────────────────────

    @staticmethod
    def _fmt_ns(ns: int) -> str:
        h,  rem = divmod(ns, 3_600_000_000_000)
        m,  rem = divmod(rem, 60_000_000_000)
        s,  rem = divmod(rem, 1_000_000_000)
        ms, rem = divmod(rem, 1_000_000)
        us, ns_ = divmod(rem, 1_000)
        return f"{int(h):02d}:{int(m):02d}:{int(s):02d}.{int(ms):03d} {int(us):03d} {int(ns_):03d}"

    # ── 关闭 ──────────────────────────────────────────────────

    def _on_close(self):
        self._stop.set()
        self._cpu_mon.stop()
        self.root.destroy()

# ══════════════════════════════════════════════════════════════
# 入口
# ══════════════════════════════════════════════════════════════

def main():
    root = Tk()
    root.geometry("1100x660")
    root.configure(bg=BG)

    try:
        root.tk.call("tk", "scaling", 1.0)
    except Exception:
        pass

    app = App(root)

    # 初始强制触发一次尺寸更新（确保首帧正确渲染）
    root.update_idletasks()
    root.event_generate("<Configure>")

    root.mainloop()


if __name__ == "__main__":
    main()
