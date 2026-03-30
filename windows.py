#!/usr/bin/env python3
"""
windows.py — 200阶魔方复原可视化控制台
标准库: tkinter, urllib, json, threading, time, os, sys, platform, subprocess, collections
无第三方依赖。
"""

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
from tkinter import Canvas, Frame, Label, Button, StringVar, Tk, PhotoImage
import tkinter as tk
import tkinter.font as tkfont

# ── 常量 ─────────────────────────────────────────────────────────────────────
API_BASE      = "http://127.0.0.1:62001"
POLL_HZ       = 60          # 状态轮询频率（次/秒）
POLL_INTERVAL = 1.0 / POLL_HZ
CPU_INTERVAL  = 2.0         # CPU 读取间隔（秒）

N             = 200         # 魔方阶数
FACES         = 6           # 面数
CELL_PX       = 2           # 每格像素（1 或 2）
FACE_PX       = N * CELL_PX # 每面像素宽/高 = 400

# 展开图布局（十字展开，单位：面）
#       U
#   L   F   R   B
#       D
LAYOUT = [
    (1, 0, "U"),  # 上
    (0, 1, "L"),  # 左
    (1, 1, "F"),  # 前
    (2, 1, "R"),  # 右
    (3, 1, "B"),  # 后
    (1, 2, "D"),  # 下
]
FACE_LABEL_ORDER = ["U", "R", "F", "D", "L", "B"]  # 与 data.bin 对齐

# 画布尺寸
CANVAS_W = 4 * FACE_PX + 10  # 展开图最宽 4 列
CANVAS_H = 3 * FACE_PX + 10  # 展开图最高 3 行

# 颜色映射（与 Rust init.rs 一致：0=U白 1=R红 2=F绿 3=D黄 4=L橙 5=B蓝）
COLOR_MAP = {
    0: "#F0F0F0",  # 白
    1: "#E53935",  # 红
    2: "#43A047",  # 绿
    3: "#FDD835",  # 黄
    4: "#FB8C00",  # 橙
    5: "#1E88E5",  # 蓝
}
UNKNOWN_COLOR = "#2a2a2a"

# ── 配色方案（深色工业风 UI）─────────────────────────────────────────────────
BG         = "#0e0e12"
BG2        = "#16161e"
BG3        = "#1e1e2a"
ACCENT     = "#00e5ff"
ACCENT2    = "#ff6b35"
TEXT_PRI   = "#e8e8f0"
TEXT_SEC   = "#6b6b80"
TEXT_DIM   = "#3a3a4a"
GREEN_OK   = "#00e676"
RED_WARN   = "#ff3d00"
BORDER     = "#2a2a3a"

# ── HTTP 工具 ─────────────────────────────────────────────────────────────────

def http_get(path: str, timeout: float = 0.4) -> dict | None:
    """GET 请求，失败静默返回 None。"""
    try:
        req = urllib.request.Request(
            f"{API_BASE}{path}",
            headers={"Accept": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read().decode())
    except Exception:
        return None


def http_post(path: str, body: str, timeout: float = 0.5) -> dict | None:
    """POST 请求，失败静默返回 None。"""
    try:
        data = body.encode()
        req = urllib.request.Request(
            f"{API_BASE}{path}",
            data=data,
            method="POST",
            headers={"Content-Type": "text/plain", "Content-Length": str(len(data))},
        )
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read().decode())
    except Exception:
        return None


# ── CPU 监控（后台线程，跨平台）──────────────────────────────────────────────

class CpuMonitor(threading.Thread):
    """
    读取 `calc` / `calc.exe` 进程的 CPU 占用率。
    Linux : 解析 /proc/<pid>/stat 两次采样差分
    Windows: 调用 wmic process 命令行（非阻塞子进程）
    macOS  : 调用 ps 命令
    结果存入 self.cpu_pct（float，-1 表示未知）。
    """
    def __init__(self):
        super().__init__(daemon=True, name="cpu-monitor")
        self.cpu_pct: float = -1.0
        self._lock = threading.Lock()
        self._stop = threading.Event()

    def stop(self):
        self._stop.set()

    @property
    def pct(self) -> float:
        with self._lock:
            return self.cpu_pct

    # ---- Linux ----
    def _find_pid_linux(self, name: str) -> int | None:
        try:
            for pid in os.listdir("/proc"):
                if not pid.isdigit():
                    continue
                try:
                    cmdline = open(f"/proc/{pid}/cmdline", "rb").read()
                    if name.encode() in cmdline:
                        return int(pid)
                except OSError:
                    pass
        except Exception:
            pass
        return None

    def _read_cpu_linux(self, pid: int) -> float:
        """两次采样 /proc/<pid>/stat，计算 CPU%。"""
        def read_stat(p):
            try:
                parts = open(f"/proc/{p}/stat").read().split()
                utime, stime = int(parts[13]), int(parts[14])
                total = int(open("/proc/stat").readline().split()[1:8]
                             |  __builtins__.__import__  # 占位，下面重写
                             and 0)  # 不用这行
            except Exception:
                return None, None
            return utime + stime, None

        # 正确实现：手动解析两次
        def sample():
            try:
                parts = open(f"/proc/{pid}/stat").read().split()
                proc_ticks = int(parts[13]) + int(parts[14])
                total_ticks = sum(int(x) for x in
                                  open("/proc/stat").readline().split()[1:])
                return proc_ticks, total_ticks
            except Exception:
                return None, None

        p1, t1 = sample()
        if p1 is None:
            return -1.0
        time.sleep(CPU_INTERVAL)
        p2, t2 = sample()
        if p2 is None:
            return -1.0
        dp = p2 - p1
        dt = t2 - t1
        if dt <= 0:
            return 0.0
        return (dp / dt) * 100.0

    # ---- Windows ----
    def _read_cpu_windows(self, name: str) -> float:
        try:
            cmd = (
                f'wmic process where "name=\'{name}\'" '
                f'get PercentProcessorTime /value'
            )
            out = subprocess.check_output(
                cmd, shell=True, timeout=3,
                stderr=subprocess.DEVNULL
            ).decode(errors="ignore")
            match = re.search(r"PercentProcessorTime=(\d+)", out)
            if match:
                return float(match.group(1))
        except Exception:
            pass
        return -1.0

    # ---- macOS ----
    def _read_cpu_macos(self, name: str) -> float:
        try:
            out = subprocess.check_output(
                ["ps", "-eo", "pcpu,comm"],
                timeout=3, stderr=subprocess.DEVNULL
            ).decode(errors="ignore")
            total = 0.0
            for line in out.splitlines():
                if name in line:
                    try:
                        total += float(line.strip().split()[0])
                    except ValueError:
                        pass
            return total if total > 0 else -1.0
        except Exception:
            return -1.0

    def run(self):
        system = platform.system()
        proc_name = "calc.exe" if system == "Windows" else "calc"

        while not self._stop.is_set():
            pct = -1.0
            try:
                if system == "Linux":
                    pid = self._find_pid_linux(proc_name)
                    if pid:
                        pct = self._read_cpu_linux(pid)
                    else:
                        time.sleep(CPU_INTERVAL)
                elif system == "Windows":
                    pct = self._read_cpu_windows(proc_name)
                    time.sleep(CPU_INTERVAL)
                elif system == "Darwin":
                    pct = self._read_cpu_macos(proc_name)
                    time.sleep(CPU_INTERVAL)
                else:
                    time.sleep(CPU_INTERVAL)
            except Exception:
                time.sleep(CPU_INTERVAL)

            with self._lock:
                self.cpu_pct = pct

            # Linux 已在采样函数内 sleep，其他平台已 sleep
            # 防止 Linux pid 未找到时空转
            if system == "Linux" and pct == -1.0:
                time.sleep(CPU_INTERVAL)


# ── 状态轮询线程 ──────────────────────────────────────────────────────────────

class StatusPoller(threading.Thread):
    """
    以 ~60Hz 轮询 /status，将结果推入共享 deque。
    """
    def __init__(self, queue: deque, stop_event: threading.Event):
        super().__init__(daemon=True, name="status-poller")
        self.queue = queue
        self.stop_event = stop_event
        self.last_ok: float = 0.0
        self.error_count: int = 0

    def run(self):
        while not self.stop_event.is_set():
            t0 = time.perf_counter()
            data = http_get("/status")
            if data is not None:
                self.last_ok = time.perf_counter()
                self.error_count = 0
                # 队列保持最新一条
                self.queue.clear()
                self.queue.append(data)
            else:
                self.error_count += 1

            elapsed = time.perf_counter() - t0
            sleep = max(0.0, POLL_INTERVAL - elapsed)
            time.sleep(sleep)


# ── 像素画布 ──────────────────────────────────────────────────────────────────

class CubeCanvas:
    """
    在 tk.Canvas 上渲染 200 阶魔方展开图。
    每格 CELL_PX×CELL_PX 像素。
    使用 PhotoImage 像素直写（比 create_rectangle × 240000 快数十倍）。
    """
    def __init__(self, parent):
        self.img = PhotoImage(width=CANVAS_W, height=CANVAS_H)
        self.canvas = Canvas(
            parent,
            width=CANVAS_W, height=CANVAS_H,
            bg=BG, highlightthickness=0, bd=0,
        )
        self.canvas.pack(padx=8, pady=8)
        self._img_id = self.canvas.create_image(0, 0, anchor="nw", image=self.img)

        # 预计算每个面的像素偏移
        self._face_offsets: dict[str, tuple[int, int]] = {}
        for col, row, label in LAYOUT:
            self._face_offsets[label] = (
                col * FACE_PX + 5,
                row * FACE_PX + 5,
            )

        self._draw_blank()

    def _draw_blank(self):
        """初始绘制深色背景 + 占位格。"""
        # 用 put 批量填色（每行一条 put 调用，远快于逐像素）
        row_data = [UNKNOWN_COLOR] * CANVAS_W
        row_str = "{" + " ".join(row_data) + "}"
        for y in range(CANVAS_H):
            self.img.put(row_str, to=(0, y, CANVAS_W, y + 1))

        # 绘制面轮廓（仅一次）
        for _, (ox, oy) in self._face_offsets.items():
            self.canvas.create_rectangle(
                ox - 1, oy - 1,
                ox + FACE_PX, oy + FACE_PX,
                outline=BORDER, width=1,
            )

    def update_face(self, face_idx: int, face_data: list[int]):
        """
        更新单个面的像素。
        face_data: N*N 个颜色值（0-5）的列表，行优先。
        """
        label = FACE_LABEL_ORDER[face_idx]
        ox, oy = self._face_offsets[label]

        # 批量构建每行的颜色字符串
        for r in range(N):
            row_start = r * N
            if CELL_PX == 1:
                # 每行 N 像素
                colors = [COLOR_MAP.get(face_data[row_start + c], UNKNOWN_COLOR)
                          for c in range(N)]
                row_str = "{" + " ".join(colors) + "}"
                self.img.put(row_str, to=(ox, oy + r, ox + N, oy + r + 1))
            else:
                # 每格 2px → 先构建 2N 宽的行
                colors = []
                for c in range(N):
                    col = COLOR_MAP.get(face_data[row_start + c], UNKNOWN_COLOR)
                    colors.append(col)
                    colors.append(col)
                row_str = "{" + " ".join(colors) + "}"
                self.img.put(row_str, to=(ox, oy + r * 2,     ox + FACE_PX, oy + r * 2 + 1))
                self.img.put(row_str, to=(ox, oy + r * 2 + 1, ox + FACE_PX, oy + r * 2 + 2))

    def update_solved_overlay(self, solved_pct: float):
        """在展开图左上角绘制完成度扫描线（视觉效果）。"""
        # 已完成比例 → 覆盖扫描线颜色（绿色渐进）
        # 仅更新 U 面作为"指示器"——保持简洁
        pass  # 实际颜色已由 update_face 承载，此处留钩子


# ── 主窗口 ────────────────────────────────────────────────────────────────────

class App:
    def __init__(self, root: Tk):
        self.root = root
        self.root.title("200-Order Cube Solver Monitor")
        self.root.configure(bg=BG)
        self.root.resizable(False, False)

        # ── 状态变量 ──────────────────────────────────────────────────────────
        self._queue: deque = deque(maxlen=1)
        self._stop = threading.Event()
        self._poller = StatusPoller(self._queue, self._stop)
        self._cpu_mon = CpuMonitor()

        # 计时
        self._start_ns: int | None = None   # 开始复原的纳秒时间戳
        self._running: bool = False
        self._last_status: dict = {}

        # FPS 统计
        self._frame_times: deque = deque(maxlen=120)

        # ── 构建 UI ───────────────────────────────────────────────────────────
        self._build_ui()

        # ── 启动后台线程 ──────────────────────────────────────────────────────
        self._poller.start()
        self._cpu_mon.start()

        # ── 启动渲染循环 ──────────────────────────────────────────────────────
        self.root.after(16, self._render_loop)  # ~60fps
        self.root.protocol("WM_DELETE_WINDOW", self._on_close)

    # ── UI 构建 ───────────────────────────────────────────────────────────────

    def _build_ui(self):
        # 标题栏
        title_frame = Frame(self.root, bg=BG, pady=6)
        title_frame.pack(fill="x")

        Label(
            title_frame,
            text="CUBE·200  SOLVER",
            bg=BG, fg=ACCENT,
            font=("Courier New", 15, "bold"),
        ).pack(side="left", padx=12)

        self._conn_dot = Label(title_frame, text="●", bg=BG, fg=TEXT_DIM, font=("Courier", 10))
        self._conn_dot.pack(side="right", padx=12)
        self._conn_label = Label(title_frame, text="OFFLINE", bg=BG, fg=TEXT_DIM,
                                  font=("Courier New", 9))
        self._conn_label.pack(side="right")

        # 分隔线
        Frame(self.root, bg=BORDER, height=1).pack(fill="x")

        # 主体 = 画布 | 控制面板
        body = Frame(self.root, bg=BG)
        body.pack(fill="both", expand=True)

        # 左：魔方展开图
        left = Frame(body, bg=BG)
        left.pack(side="left", fill="both")
        self._cube_canvas = CubeCanvas(left)

        # 右：控制 + 状态面板
        right = Frame(body, bg=BG2, width=260, padx=12, pady=12)
        right.pack(side="right", fill="y")
        right.pack_propagate(False)

        self._build_control_panel(right)

    def _build_control_panel(self, parent):
        # 按钮行
        btn_frame = Frame(parent, bg=BG2)
        btn_frame.pack(fill="x", pady=(0, 12))

        btn_style = dict(
            font=("Courier New", 10, "bold"),
            relief="flat", cursor="hand2",
            padx=14, pady=7, bd=0,
        )
        self._btn_start = Button(
            btn_frame, text="▶  START",
            bg=GREEN_OK, fg=BG,
            activebackground="#00c853", activeforeground=BG,
            command=self._on_start, **btn_style,
        )
        self._btn_start.pack(side="left", fill="x", expand=True, padx=(0, 4))

        self._btn_pause = Button(
            btn_frame, text="⏸  PAUSE",
            bg=ACCENT2, fg=BG,
            activebackground="#e64a19", activeforeground=BG,
            command=self._on_pause, **btn_style,
        )
        self._btn_pause.pack(side="left", fill="x", expand=True)

        Frame(parent, bg=BORDER, height=1).pack(fill="x", pady=6)

        # 状态指标格
        self._metrics: dict[str, StringVar] = {}
        metrics_defs = [
            ("phase",      "PHASE",      "—"),
            ("solved_pct", "COMPLETED",  "0.0000 %"),
            ("cells",      "CELLS",      "0 / 240000"),
            ("moves",      "MOVES",      "0"),
            ("avg_move",   "AVG/MOVE",   "0 µs"),
            ("elapsed",    "ELAPSED",    "0 ns"),
            ("fps",        "POLL FPS",   "—"),
            ("cpu",        "CALC CPU",   "—"),
        ]

        for key, label, default in metrics_defs:
            sv = StringVar(value=default)
            self._metrics[key] = sv
            self._build_metric_row(parent, label, sv)
            Frame(parent, bg=TEXT_DIM, height=1).pack(fill="x", pady=1)

        Frame(parent, bg=BORDER, height=1).pack(fill="x", pady=8)

        # 纳秒级精确计时器（大字体高亮）
        Label(parent, text="PRECISION TIMER", bg=BG2, fg=TEXT_SEC,
              font=("Courier New", 8)).pack(anchor="w")
        self._timer_var = StringVar(value="00:00:00.000000000")
        Label(
            parent,
            textvariable=self._timer_var,
            bg=BG2, fg=ACCENT,
            font=("Courier New", 12, "bold"),
            justify="center",
        ).pack(fill="x", pady=(2, 8))

        Frame(parent, bg=BORDER, height=1).pack(fill="x", pady=4)

        # 进度条
        Label(parent, text="SOLVE PROGRESS", bg=BG2, fg=TEXT_SEC,
              font=("Courier New", 8)).pack(anchor="w")
        pb_bg = Frame(parent, bg=TEXT_DIM, height=10)
        pb_bg.pack(fill="x", pady=(2, 8))
        pb_bg.pack_propagate(False)
        self._progress_bar = Frame(pb_bg, bg=GREEN_OK, height=10, width=0)
        self._progress_bar.place(x=0, y=0, height=10, width=0)
        self._pb_container = pb_bg  # 记录容器宽度用

        # 错误信息行
        self._err_var = StringVar(value="")
        Label(
            parent, textvariable=self._err_var,
            bg=BG2, fg=RED_WARN,
            font=("Courier New", 8), wraplength=230,
            justify="left",
        ).pack(anchor="w", pady=(4, 0))

    def _build_metric_row(self, parent, label: str, sv: StringVar):
        row = Frame(parent, bg=BG2)
        row.pack(fill="x", pady=1)
        Label(row, text=label, bg=BG2, fg=TEXT_SEC,
              font=("Courier New", 8), width=11, anchor="w").pack(side="left")
        Label(row, textvariable=sv, bg=BG2, fg=TEXT_PRI,
              font=("Courier New", 9, "bold"), anchor="e").pack(side="right")

    # ── 控制按钮回调 ──────────────────────────────────────────────────────────

    def _on_start(self):
        def _send():
            result = http_post("/control", "start")
            if result and result.get("ok"):
                if not self._running:
                    self._start_ns = time.perf_counter_ns()
                    self._running = True
            else:
                self._err_var.set("⚠ 无法连接服务器")
        threading.Thread(target=_send, daemon=True).start()

    def _on_pause(self):
        def _send():
            http_post("/control", "pause")
            # 不重置计时器，保留已用时间
        threading.Thread(target=_send, daemon=True).start()

    # ── 主渲染循环（~60fps，运行于 tkinter 主线程）────────────────────────────

    def _render_loop(self):
        t_frame_start = time.perf_counter_ns()

        # 1. 拉取最新状态
        status = None
        if self._queue:
            status = self._queue[-1]

        # 2. 更新连接指示器
        if self._poller.error_count > 5:
            self._conn_dot.config(fg=RED_WARN)
            self._conn_label.config(text="OFFLINE", fg=RED_WARN)
            self._err_var.set(f"⚠ 连接失败 × {self._poller.error_count}")
        else:
            self._conn_dot.config(fg=GREEN_OK)
            self._conn_label.config(text="ONLINE ", fg=GREEN_OK)
            self._err_var.set("")

        # 3. 解析状态 → 更新指标
        if status:
            self._last_status = status
            self._update_metrics(status)

        # 4. 更新精确计时器
        self._update_timer()

        # 5. 更新 CPU 监控
        cpu_pct = self._cpu_mon.pct
        if cpu_pct < 0:
            self._metrics["cpu"].set("N/A")
        else:
            self._metrics["cpu"].set(f"{cpu_pct:.1f} %")

        # 6. 更新 FPS
        self._frame_times.append(t_frame_start)
        if len(self._frame_times) >= 2:
            span = (self._frame_times[-1] - self._frame_times[0]) / 1e9
            fps = (len(self._frame_times) - 1) / span if span > 0 else 0
            self._metrics["fps"].set(f"{fps:.1f}")

        # 7. 调度下一帧（目标 60fps）
        frame_ns = time.perf_counter_ns() - t_frame_start
        delay_ms = max(1, 16 - frame_ns // 1_000_000)
        self.root.after(int(delay_ms), self._render_loop)

    def _update_metrics(self, s: dict):
        phase_map = {
            "center_reduction":  "CENTER ▸",
            "edge_pairing":      "EDGES  ▸",
            "3x3_reduction":     "3×3    ▸",
            "solved":            "SOLVED ✓",
        }
        phase = phase_map.get(s.get("phase", ""), s.get("phase", "—"))
        self._metrics["phase"].set(phase)

        pct = s.get("solved_pct", 0.0)
        self._metrics["solved_pct"].set(f"{pct:.4f} %")

        solved = s.get("solved_cells", 0)
        total  = s.get("total_cells", 240000)
        self._metrics["cells"].set(f"{solved:,} / {total:,}")

        moves = s.get("total_moves", 0)
        self._metrics["moves"].set(f"{moves:,}")

        avg_us = s.get("avg_move_us", 0)
        self._metrics["avg_move"].set(f"{avg_us} µs")

        # 进度条
        self.root.update_idletasks()
        try:
            pb_w = self._pb_container.winfo_width()
            fill_w = int(pb_w * pct / 100.0)
            self._progress_bar.place_configure(width=fill_w)
        except Exception:
            pass

        # 如果阶段变为 running 且本地还未记录开始时间
        if s.get("state") == "running" and not self._running:
            self._running = True
            if self._start_ns is None:
                self._start_ns = time.perf_counter_ns()

        # 将服务端总步数对应时长写入指标
        total_us = moves * avg_us if avg_us > 0 else 0
        self._metrics["elapsed"].set(f"{total_us:,} µs")

    def _update_timer(self):
        """本地纳秒级精确计时，独立于网络延迟。"""
        if self._start_ns is None or not self._running:
            return
        now_ns = time.perf_counter_ns()
        elapsed_ns = now_ns - self._start_ns

        ns_total = elapsed_ns
        hours, rem = divmod(ns_total, 3_600_000_000_000)
        minutes, rem = divmod(rem, 60_000_000_000)
        seconds, rem = divmod(rem, 1_000_000_000)
        ms, ns_part = divmod(rem, 1_000_000)

        self._timer_var.set(
            f"{int(hours):02d}:{int(minutes):02d}:{int(seconds):02d}"
            f".{int(ms):03d}{int(ns_part // 1000):03d}{int(ns_part % 1000):03d}"
        )

    # ── 面数据渲染（独立于状态轮询，防止阻塞 UI）─────────────────────────────
    # /status 不直接返回完整 240000 字节的颜色数组（太大），
    # 这里展示基于统计信息的"色温"映射：
    # 每面颜色用 solved_pct 插值为从"混乱"到"完成"的渐变色块。
    # 若未来 /status 返回完整数据，可改为逐像素渲染。

    def render_heatmap(self, solved_pct: float):
        """
        用热图近似表示各面状态。
        solved_pct ∈ [0, 100]
        """
        ratio = solved_pct / 100.0
        for face_idx, label in enumerate(FACE_LABEL_ORDER):
            correct_color = face_idx  # 该面的目标颜色
            # 按比例混合正确色和随机色
            row_data = []
            for r in range(N):
                for c in range(N):
                    # 伪随机偏移——使用固定哈希以避免闪烁
                    h = (face_idx * 40003 + r * 201 + c * 7) % 1000
                    is_correct = (h / 1000.0) < ratio
                    row_data.append(correct_color if is_correct else (h % 6))
            self._cube_canvas.update_face(face_idx, row_data)

    # ── 关闭 ──────────────────────────────────────────────────────────────────

    def _on_close(self):
        self._stop.set()
        self._cpu_mon.stop()
        self.root.destroy()


# ── Linux /proc 解析修正（替换 run 方法中的内联错误）─────────────────────────
# 重写 CpuMonitor._read_cpu_linux，不使用意外引入的语法错误

def _fixed_read_cpu_linux(self, pid: int) -> float:
    """采样两次 /proc/<pid>/stat 与 /proc/stat，计算 CPU 占用率。"""
    def _sample():
        try:
            proc_parts = open(f"/proc/{pid}/stat").read().split()
            proc_ticks = int(proc_parts[13]) + int(proc_parts[14])
            cpu_parts  = open("/proc/stat").readline().split()
            total_ticks = sum(int(x) for x in cpu_parts[1:])  # cpu user nice ...
            return proc_ticks, total_ticks
        except Exception:
            return None, None

    p1, t1 = _sample()
    if p1 is None:
        return -1.0
    time.sleep(CPU_INTERVAL)
    p2, t2 = _sample()
    if p2 is None:
        return -1.0
    dp, dt = p2 - p1, t2 - t1
    if dt <= 0:
        return 0.0
    return (dp / dt) * 100.0

# 动态替换方法（修复原始类中的 inline 错误）
CpuMonitor._read_cpu_linux = _fixed_read_cpu_linux  # type: ignore


# ── 定期触发热图渲染（降低绑定到 render_loop 的耦合）────────────────────────

class HeatmapUpdater:
    """每 500ms 更新一次展开图热图（避免每帧重绘 240000 格导致卡顿）。"""
    def __init__(self, app: App):
        self.app = app
        self._last_pct: float = -1.0

    def schedule(self):
        self.app.root.after(500, self._update)

    def _update(self):
        status = self.app._last_status
        pct = status.get("solved_pct", 0.0)
        # 仅在数值变化时重绘
        if abs(pct - self._last_pct) > 0.001:
            self._last_pct = pct
            try:
                self.app.render_heatmap(pct)
            except Exception:
                pass
        self.app.root.after(500, self._update)


# ── 入口 ──────────────────────────────────────────────────────────────────────

def main():
    root = Tk()
    root.configure(bg=BG)

    # 尝试设置窗口图标（可选，失败不影响运行）
    try:
        root.iconbitmap(default="")
    except Exception:
        pass

    app = App(root)

    # 启动热图更新器
    hm = HeatmapUpdater(app)
    hm.schedule()

    # 初始渲染一次空白热图
    root.after(100, lambda: app.render_heatmap(0.0))

    root.mainloop()


if __name__ == "__main__":
    main()
