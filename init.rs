// init.rs — 200阶魔方初始化与打乱程序
// 编译: rustc -O2 -o init init.rs && ./init
// 严格仅使用 std，零第三方依赖。

use std::io::Write;
use std::time::SystemTime;

// ============================================================
// 常量定义
// ============================================================

/// 魔方阶数
const N: usize = 200;

/// 面数
const FACES: usize = 6;

/// 每面格子数
const FACE_SIZE: usize = N * N;

/// 总格子数
const TOTAL: usize = FACES * FACE_SIZE;

/// 标准打乱基准步数（240000）
const BASE_MOVES: u64 = 240_000;

// 六个面的编号索引（助记：URFDLB 国际惯例）
const U: usize = 0; // 上 (Up)
const R: usize = 1; // 右 (Right)
const F: usize = 2; // 前 (Front)
const D: usize = 3; // 下 (Down)
const L: usize = 4; // 左 (Left)
const B: usize = 5; // 后 (Back)

// ============================================================
// Xorshift64 伪随机数发生器
// ============================================================

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    /// 以给定种子构造，保证 state 非零（零状态 xorshift 会退化为全零）
    fn new(seed: u64) -> Self {
        let state = if seed == 0 { 0xDEAD_BEEF_CAFE_1337 } else { seed };
        Self { state }
    }

    /// 生成下一个 u64 伪随机数（George Marsaglia 的三元组参数）
    #[inline(always)]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// 生成 [0, n) 范围内的均匀随机数（轻量截断法，对大 n 偏差极小）
    #[inline(always)]
    fn next_bounded(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

// ============================================================
// 魔方状态
// ============================================================

/// 魔方状态：6 面，每面 N×N，展平为一维 u8 数组。
/// 颜色编码：0=U(白) 1=R(红) 2=F(绿) 3=D(黄) 4=L(橙) 5=B(蓝)
struct Cube {
    /// Box 堆分配，避免 240000 字节栈溢出（此处 240000 B ≈ 234 KB）
    data: Box<[u8; TOTAL]>,
}

impl Cube {
    /// 构造完美复原状态：每面的所有格子填入该面的颜色编号
    fn new_solved() -> Self {
        let mut data = Box::new([0u8; TOTAL]);
        for face in 0..FACES {
            let color = face as u8;
            let start = face * FACE_SIZE;
            data[start..start + FACE_SIZE].fill(color);
        }
        Self { data }
    }

    /// 获取面 f、行 r、列 c 处格子的可变引用
    /// 布局：data[f * N*N + r * N + c]
    #[inline(always)]
    fn cell_mut(&mut self, f: usize, r: usize, c: usize) -> &mut u8 {
        &mut self.data[f * FACE_SIZE + r * N + c]
    }

    #[inline(always)]
    fn cell(&self, f: usize, r: usize, c: usize) -> u8 {
        self.data[f * FACE_SIZE + r * N + c]
    }

    // ----------------------------------------------------------
    // 核心操作：旋转某一面的面片（仅当该层为表面层时才需要）
    // ----------------------------------------------------------

    /// 顺时针旋转面 f 的 N×N 面片（原地转置+行翻转）
    fn rotate_face_cw(&mut self, f: usize) {
        let base = f * FACE_SIZE;
        let s = &mut self.data[base..base + FACE_SIZE];
        // 先转置
        for r in 0..N {
            for c in (r + 1)..N {
                s.swap(r * N + c, c * N + r);
            }
        }
        // 再每行翻转
        for r in 0..N {
            s[r * N..r * N + N].reverse();
        }
    }

    /// 逆时针旋转面 f 的 N×N 面片（转置+列翻转 == 行翻转+转置）
    fn rotate_face_ccw(&mut self, f: usize) {
        let base = f * FACE_SIZE;
        let s = &mut self.data[base..base + FACE_SIZE];
        // 先每行翻转
        for r in 0..N {
            s[r * N..r * N + N].reverse();
        }
        // 再转置
        for r in 0..N {
            for c in (r + 1)..N {
                s.swap(r * N + c, c * N + r);
            }
        }
    }

    // ----------------------------------------------------------
    // 核心操作：移动某一层的四条棱带
    // ----------------------------------------------------------

    /// 执行一次移动动作。
    ///
    /// # 参数
    /// - `axis`      : 0=U/D轴(水平层), 1=R/L轴(竖直列), 2=F/B轴(深度层)
    /// - `layer`     : 0..N，0 为正面/顶面/右面层
    /// - `clockwise` : true=顺时针, false=逆时针（从正方向俯视）
    fn do_move(&mut self, axis: u8, layer: usize, clockwise: bool) {
        match axis {
            0 => self.move_u_axis(layer, clockwise),
            1 => self.move_r_axis(layer, clockwise),
            2 => self.move_f_axis(layer, clockwise),
            _ => unreachable!(),
        }
    }

    // ------ U/D 轴：水平层（行方向） ------
    // layer=0 对应 U 面；layer=N-1 对应 D 面
    // 顺时针（从 U 往下看）：F行 → R行 → B行(翻转) → L行(翻转) → F行
    // 实际赤道层（内部层）只移动棱带，不旋转面
    fn move_u_axis(&mut self, layer: usize, clockwise: bool) {
        // 涉及的四条棱带行在各面中的行索引：
        // U 面看向下时，F/R/B/L 各面第 layer 行
        // B 面的行是"镜像"的：B 面第 layer 行对应视觉上的翻转
        //
        // 四面顺序（顺时针从上往下看）：F -> L -> B -> R -> F
        // 注意：B 面的列方向与 F 相反，故 B 的 layer 行需整体翻转方向
        //
        // 暂存 F 的 layer 行
        let mut tmp = [0u8; N];
        for c in 0..N {
            tmp[c] = self.cell(F, layer, c);
        }

        if clockwise {
            // F[layer] ← L[layer]
            // L[layer] ← B[N-1-layer] (B行逆序)
            // B[N-1-layer] ← R[layer] (存入时逆序)
            // R[layer] ← tmp (原 F)
            for c in 0..N {
                let f_val = self.cell(L, layer, c);
                *self.cell_mut(F, layer, c) = f_val;
            }
            for c in 0..N {
                let l_val = self.cell(B, N - 1 - layer, N - 1 - c);
                *self.cell_mut(L, layer, c) = l_val;
            }
            for c in 0..N {
                let b_val = self.cell(R, layer, N - 1 - c);
                *self.cell_mut(B, N - 1 - layer, c) = b_val;
            }
            for c in 0..N {
                *self.cell_mut(R, layer, c) = tmp[c];
            }
        } else {
            // 逆时针：F[layer] ← R[layer], R[layer] ← B[N-1-layer](逆), ...
            for c in 0..N {
                let f_val = self.cell(R, layer, c);
                *self.cell_mut(F, layer, c) = f_val;
            }
            for c in 0..N {
                let r_val = self.cell(B, N - 1 - layer, N - 1 - c);
                *self.cell_mut(R, layer, c) = r_val;
            }
            for c in 0..N {
                let b_val = self.cell(L, layer, N - 1 - c);
                *self.cell_mut(B, N - 1 - layer, c) = b_val;
            }
            for c in 0..N {
                *self.cell_mut(L, layer, c) = tmp[c];
            }
        }

        // 若为表面层则同时旋转面片
        if layer == 0 {
            if clockwise { self.rotate_face_cw(U); } else { self.rotate_face_ccw(U); }
        } else if layer == N - 1 {
            // D 面：从下往上看，顺时针对应 U 轴逆时针
            if clockwise { self.rotate_face_ccw(D); } else { self.rotate_face_cw(D); }
        }
    }

    // ------ R/L 轴：竖直列 ------
    // layer=0 对应 R 面所在列（各面第 N-1 列）；layer=N-1 对应 L 面（各面第 0 列）
    // 顺时针（从 R 面往左看）：U列 → F列 → D列 → B列(翻转) → U列
    fn move_r_axis(&mut self, layer: usize, clockwise: bool) {
        // 在各面中，R轴第 layer 层对应的列索引
        let col_f = N - 1 - layer; // F/U/D 面的列
        let col_b = layer;         // B 面列（镜像）

        let mut tmp = [0u8; N];
        for r in 0..N {
            tmp[r] = self.cell(U, r, col_f);
        }

        if clockwise {
            // U ← F, F ← D, D ← B(翻转), B ← U(翻转)
            for r in 0..N {
                let v = self.cell(F, r, col_f);
                *self.cell_mut(U, r, col_f) = v;
            }
            for r in 0..N {
                let v = self.cell(D, r, col_f);
                *self.cell_mut(F, r, col_f) = v;
            }
            for r in 0..N {
                let v = self.cell(B, N - 1 - r, col_b);
                *self.cell_mut(D, r, col_f) = v;
            }
            for r in 0..N {
                *self.cell_mut(B, N - 1 - r, col_b) = tmp[r];
            }
        } else {
            // U ← B(翻转), B ← D(翻转), D ← F, F ← U
            for r in 0..N {
                let v = self.cell(B, N - 1 - r, col_b);
                *self.cell_mut(U, r, col_f) = v;
            }
            for r in 0..N {
                let v = self.cell(D, N - 1 - r, col_f);
                *self.cell_mut(B, r, col_b) = v;
            }
            for r in 0..N {
                let v = self.cell(F, r, col_f);
                *self.cell_mut(D, r, col_f) = v;
            }
            for r in 0..N {
                *self.cell_mut(F, r, col_f) = tmp[r];
            }
        }

        if layer == 0 {
            if clockwise { self.rotate_face_cw(R); } else { self.rotate_face_ccw(R); }
        } else if layer == N - 1 {
            if clockwise { self.rotate_face_ccw(L); } else { self.rotate_face_cw(L); }
        }
    }

    // ------ F/B 轴：深度层（切片） ------
    // layer=0 对应 F 面；layer=N-1 对应 B 面
    // 顺时针（从 F 面往后看）：U底行 → R左列 → D顶行(翻转) → L右列(翻转) → U底行
    fn move_f_axis(&mut self, layer: usize, clockwise: bool) {
        // U 面的行索引：layer=0 对应 U 面最后一行(N-1)
        let row_u = N - 1 - layer;
        // D 面的行索引：layer=0 对应 D 面第 0 行
        let row_d = layer;
        // R 面的列索引：layer=0 对应 R 面第 0 列
        let col_r = layer;
        // L 面的列索引：layer=0 对应 L 面最后一列(N-1)
        let col_l = N - 1 - layer;

        let mut tmp = [0u8; N];
        for c in 0..N {
            tmp[c] = self.cell(U, row_u, c);
        }

        if clockwise {
            // U行 ← L列(逆序), L列 ← D行(逆序), D行 ← R列, R列 ← U行
            for c in 0..N {
                let v = self.cell(L, N - 1 - c, col_l);
                *self.cell_mut(U, row_u, c) = v;
            }
            for r in 0..N {
                let v = self.cell(D, row_d, N - 1 - r);
                *self.cell_mut(L, r, col_l) = v;
            }
            for c in 0..N {
                let v = self.cell(R, c, col_r);
                *self.cell_mut(D, row_d, c) = v;
            }
            for r in 0..N {
                *self.cell_mut(R, r, col_r) = tmp[r];
            }
        } else {
            // U行 ← R列, R列 ← D行(逆序), D行 ← L列, L列 ← U行(逆序)
            for c in 0..N {
                let v = self.cell(R, c, col_r);
                *self.cell_mut(U, row_u, c) = v;
            }
            for r in 0..N {
                let v = self.cell(D, row_d, N - 1 - r);
                *self.cell_mut(R, r, col_r) = v;
            }
            for c in 0..N {
                let v = self.cell(L, c, col_l);
                *self.cell_mut(D, row_d, c) = v;
            }
            for r in 0..N {
                *self.cell_mut(L, r, col_l) = tmp[N - 1 - r];
            }
        }

        if layer == 0 {
            if clockwise { self.rotate_face_cw(F); } else { self.rotate_face_ccw(F); }
        } else if layer == N - 1 {
            if clockwise { self.rotate_face_ccw(B); } else { self.rotate_face_cw(B); }
        }
    }
}

// ============================================================
// 动作去重辅助结构
// ============================================================

/// 编码一个动作为 (axis, layer, clockwise)
#[derive(Clone, Copy, PartialEq, Eq)]
struct Move {
    axis: u8,
    layer: u16,  // 支持最高 65535 阶
    clockwise: bool,
}

/// 判断两个动作是否"抵消"（同轴同层，方向相反）
#[inline(always)]
fn is_inverse(a: Move, b: Move) -> bool {
    a.axis == b.axis && a.layer == b.layer && a.clockwise != b.clockwise
}

/// 判断 prev2、prev1、candidate 是否构成"同轴同层连续三次"（等价于反方向一次，无效）
#[inline(always)]
fn is_triple(prev2: Option<Move>, prev1: Option<Move>, candidate: Move) -> bool {
    if let (Some(p2), Some(p1)) = (prev2, prev1) {
        p2.axis == candidate.axis
            && p2.layer == candidate.layer
            && p1.axis == candidate.axis
            && p1.layer == candidate.layer
    } else {
        false
    }
}

// ============================================================
// 主函数
// ============================================================

fn main() {
    // --- 1. 生成随机种子（纳秒时间戳截断为 u64）---
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("系统时间早于 UNIX EPOCH")
        .as_nanos() as u64;

    let mut rng = Xorshift64::new(seed);

    // --- 2. 确定打乱步数：BASE_MOVES * [1.05, 1.25) ---
    // 使用整数运算：步数 = BASE_MOVES * (105 + rand%21) / 100
    let factor = 105 + rng.next_bounded(21); // [105, 125]
    let total_moves = BASE_MOVES * factor / 100;

    // --- 3. 初始化魔方 ---
    let mut cube = Cube::new_solved();

    // --- 4. 打乱循环 ---
    let mut prev1: Option<Move> = None;
    let mut prev2: Option<Move> = None;

    // 进度打印频率（每 1000 步打印一次，减少 I/O 开销）
    const PRINT_INTERVAL: u64 = 1000;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for step in 0..total_moves {
        // 实时进度输出
        if step % PRINT_INTERVAL == 0 {
            let _ = write!(
                out,
                "\r[{:>9} / {}] Scrambling...",
                step, total_moves
            );
            let _ = out.flush();
        }

        // 生成一个不被去重规则排除的随机动作
        let mv = loop {
            let axis      = rng.next_bounded(3) as u8;   // 0,1,2
            let layer     = rng.next_bounded(N as u64) as u16;
            let clockwise = rng.next_bounded(2) == 0;

            let candidate = Move { axis, layer, clockwise };

            // 规则1：不能与上一步完全互逆（顺+逆或逆+顺抵消）
            if let Some(p1) = prev1 {
                if is_inverse(p1, candidate) {
                    continue;
                }
            }

            // 规则2：不能与前两步构成同轴同层连续三次（等价于反转）
            if is_triple(prev2, prev1, candidate) {
                continue;
            }

            break candidate;
        };

        // 执行动作
        cube.do_move(mv.axis, mv.layer as usize, mv.clockwise);

        // 更新历史
        prev2 = prev1;
        prev1 = Some(mv);
    }

    // 完成提示（覆盖进度行）
    let _ = writeln!(
        out,
        "\r[{:>9} / {}] Scrambling... Done!    ",
        total_moves, total_moves
    );

    // --- 5. 持久化：将 data 原始字节写入 data.bin ---
    let path = "data.bin";
    let bytes: &[u8] = &*cube.data;
    std::fs::write(path, bytes).expect("写入 data.bin 失败");

    let _ = writeln!(
        out,
        "已将魔方状态（{} 字节）写入 {}",
        TOTAL, path
    );
}
