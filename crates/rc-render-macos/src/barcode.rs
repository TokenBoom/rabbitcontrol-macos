//! NV12 Y 平面光学视觉条码编解码（仅 `bench-metrics` 启用，零 `unsafe`）。
//!
//! 专用于在不依赖外设高速相机的条件下，打通「源探针窗真实上屏 → SCK 采集 →
//! VT 编解码 → 控制端 Metal 真上屏」全链路闭环：
//! - 只使用 NV12 的亮度 Y 平面，每比特占 `8×8` 像素块（中心 `4×4` 均值判决），
//!   四周留 `4px` 黑边 Quiet Zone，对 H.264 4:2:0 有损压缩及 2x→1x 下采样完全鲁棒；
//! - 带固定魔数 `0xA5` 与 CRC-8（多项式 `0x07`）校验，任何半帧撕裂或噪声误码直接拒收。

/// 条码每行比特格子数（80 bit 分两行，每行 40 格）。
pub const BARCODE_COLS: usize = 40;
/// 条码行数。
pub const BARCODE_ROWS: usize = 2;
/// 单个比特格子的边长（像素）。
pub const BARCODE_CELL_PX: usize = 8;
/// 条码外围黑边保护区宽度（像素）。
pub const BARCODE_QUIET_PX: usize = 4;
/// 条码整体像素宽度（含左右 Quiet Zone）。
pub const BARCODE_WIDTH: usize = BARCODE_COLS * BARCODE_CELL_PX + BARCODE_QUIET_PX * 2;
/// 条码整体像素高度（含上下 Quiet Zone）。
pub const BARCODE_HEIGHT: usize = BARCODE_ROWS * BARCODE_CELL_PX + BARCODE_QUIET_PX * 2;
/// 条码在探针屏幕上的安全水平偏移（避开圆角与边缘裁剪）。
pub const BARCODE_OFFSET_X: usize = 48;
/// 条码在探针屏幕上的安全垂直偏移（避开 macOS 顶部菜单栏/刘海区域）。
pub const BARCODE_OFFSET_Y: usize = 48;
/// 固定头部魔数。
pub const BARCODE_MAGIC: u8 = 0xA5;

/// 条码承载的帧与输入状态载荷（8 字节有效数据 + 1 字节魔数 + 1 字节 CRC-8）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BarcodePayload {
    /// 源探针窗单调递增帧号（低 16 位环形映射）。
    pub frame_id: u16,
    /// 探针窗最近收到的输入事件序号。
    pub event_seq: u16,
    /// 探针窗最近收到的光标 X（逻辑点，向上取整夹紧到 `u16`）。
    pub cursor_x: u16,
    /// 探针窗最近收到的光标 Y（逻辑点，向上取整夹紧到 `u16`）。
    pub cursor_y: u16,
}

/// 计算 CRC-8（多项式 `x^8 + x^2 + x + 1`，即 `0x07`）。
fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in bytes {
        crc ^= b;
        for _ in 0..8 {
            if (crc & 0x80) != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn pack_payload(payload: BarcodePayload) -> [u8; 10] {
    let mut raw = [0u8; 10];
    raw[0] = BARCODE_MAGIC;
    raw[1..3].copy_from_slice(&payload.frame_id.to_be_bytes());
    raw[3..5].copy_from_slice(&payload.event_seq.to_be_bytes());
    raw[5..7].copy_from_slice(&payload.cursor_x.to_be_bytes());
    raw[7..9].copy_from_slice(&payload.cursor_y.to_be_bytes());
    raw[9] = crc8(&raw[..9]);
    raw
}

fn unpack_payload(raw: &[u8; 10]) -> Option<BarcodePayload> {
    if raw[0] != BARCODE_MAGIC || crc8(&raw[..9]) != raw[9] {
        return None;
    }
    Some(BarcodePayload {
        frame_id: u16::from_be_bytes([raw[1], raw[2]]),
        event_seq: u16::from_be_bytes([raw[3], raw[4]]),
        cursor_x: u16::from_be_bytes([raw[5], raw[6]]),
        cursor_y: u16::from_be_bytes([raw[7], raw[8]]),
    })
}

/// 将 [`BarcodePayload`] 编码为 `BARCODE_WIDTH × BARCODE_HEIGHT` 连续 Y 平面亮度缓冲区。
///
/// Quiet Zone 与 bit `0` 使用 video-range 黑 `16`，bit `1` 使用 video-range 白 `235`。
pub fn encode_barcode_y(payload: BarcodePayload) -> Vec<u8> {
    let raw = pack_payload(payload);
    let mut buf = vec![16u8; BARCODE_WIDTH * BARCODE_HEIGHT];
    for bit_idx in 0..80 {
        let byte = raw[bit_idx / 8];
        let bit = (byte >> (7 - (bit_idx % 8))) & 1;
        if bit == 0 {
            continue;
        }
        let row = bit_idx / BARCODE_COLS;
        let col = bit_idx % BARCODE_COLS;
        let x0 = BARCODE_QUIET_PX + col * BARCODE_CELL_PX;
        let y0 = BARCODE_QUIET_PX + row * BARCODE_CELL_PX;
        for dy in 0..BARCODE_CELL_PX {
            let offset = (y0 + dy) * BARCODE_WIDTH + x0;
            buf[offset..offset + BARCODE_CELL_PX].fill(235);
        }
    }
    buf
}

/// 从 Y 平面左上角缓冲解码 [`BarcodePayload`]；魔数或 CRC-8 校验失败或尺寸不足均返回 `None`。
pub fn decode_barcode_y(
    y: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Option<BarcodePayload> {
    if width < BARCODE_WIDTH || height < BARCODE_HEIGHT || stride < width {
        return None;
    }
    let required = (BARCODE_HEIGHT - 1) * stride + BARCODE_WIDTH;
    if y.len() < required {
        return None;
    }

    let mut raw = [0u8; 10];
    for bit_idx in 0..80 {
        let row = bit_idx / BARCODE_COLS;
        let col = bit_idx % BARCODE_COLS;
        // 取每个 8×8 像素格子的中心 4×4 采样求均值，规避边界压缩振铃。
        let x0 = BARCODE_QUIET_PX + col * BARCODE_CELL_PX + 2;
        let y0 = BARCODE_QUIET_PX + row * BARCODE_CELL_PX + 2;
        let mut sum: u32 = 0;
        for dy in 0..4 {
            let base = (y0 + dy) * stride + x0;
            for dx in 0..4 {
                sum += u32::from(y[base + dx]);
            }
        }
        if sum / 16 >= 126 {
            raw[bit_idx / 8] |= 1 << (7 - (bit_idx % 8));
        }
    }

    unpack_payload(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barcode_roundtrip_nominal_and_boundaries() {
        let cases = [
            BarcodePayload::default(),
            BarcodePayload {
                frame_id: 1,
                event_seq: 42,
                cursor_x: 756,
                cursor_y: 491,
            },
            BarcodePayload {
                frame_id: u16::MAX,
                event_seq: u16::MAX,
                cursor_x: u16::MAX,
                cursor_y: u16::MAX,
            },
        ];
        for payload in cases {
            let y = encode_barcode_y(payload);
            assert_eq!(y.len(), BARCODE_WIDTH * BARCODE_HEIGHT);
            let decoded = decode_barcode_y(&y, BARCODE_WIDTH, BARCODE_WIDTH, BARCODE_HEIGHT);
            assert_eq!(decoded, Some(payload));
        }
    }

    #[test]
    fn barcode_survives_blur_and_luma_noise() {
        let payload = BarcodePayload {
            frame_id: 1234,
            event_seq: 99,
            cursor_x: 1024,
            cursor_y: 640,
        };
        let src = encode_barcode_y(payload);
        let stride = BARCODE_WIDTH + 32;
        let mut noisy = vec![16u8; stride * (BARCODE_HEIGHT + 8)];
        for y in 0..BARCODE_HEIGHT {
            for x in 0..BARCODE_WIDTH {
                // 3×3 盒式模糊 + ±30 周期性量化噪声，模拟 H.264 NV12 压缩伪影。
                let mut acc = 0i32;
                let mut cnt = 0i32;
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let nx = (x as i32 + dx).clamp(0, BARCODE_WIDTH as i32 - 1) as usize;
                        let ny = (y as i32 + dy).clamp(0, BARCODE_HEIGHT as i32 - 1) as usize;
                        acc += i32::from(src[ny * BARCODE_WIDTH + nx]);
                        cnt += 1;
                    }
                }
                let jitter = if ((x + y) % 2) == 0 { 30 } else { -30 };
                let val = (acc / cnt + jitter).clamp(0, 255) as u8;
                noisy[y * stride + x] = val;
            }
        }
        let decoded = decode_barcode_y(&noisy, stride, BARCODE_WIDTH, BARCODE_HEIGHT);
        assert_eq!(decoded, Some(payload));
    }

    #[test]
    fn barcode_rejects_corrupted_bit_via_crc8() {
        let payload = BarcodePayload {
            frame_id: 77,
            event_seq: 13,
            cursor_x: 500,
            cursor_y: 300,
        };
        let mut y = encode_barcode_y(payload);
        // 翻转第 12 格（frame_id 区域）整格颜色以模拟局部比特错误。
        let col = 12usize;
        let x0 = BARCODE_QUIET_PX + col * BARCODE_CELL_PX;
        let y0 = BARCODE_QUIET_PX;
        let flipped = if y[y0 * BARCODE_WIDTH + x0 + 3] > 126 {
            16u8
        } else {
            235u8
        };
        for dy in 0..BARCODE_CELL_PX {
            let off = (y0 + dy) * BARCODE_WIDTH + x0;
            y[off..off + BARCODE_CELL_PX].fill(flipped);
        }
        assert_eq!(
            decode_barcode_y(&y, BARCODE_WIDTH, BARCODE_WIDTH, BARCODE_HEIGHT),
            None
        );
    }

    #[test]
    fn barcode_rejects_all_black_and_undersized_buffers() {
        let black = vec![16u8; BARCODE_WIDTH * BARCODE_HEIGHT];
        assert_eq!(
            decode_barcode_y(&black, BARCODE_WIDTH, BARCODE_WIDTH, BARCODE_HEIGHT),
            None
        );
        assert_eq!(
            decode_barcode_y(&black, BARCODE_WIDTH, BARCODE_WIDTH - 1, BARCODE_HEIGHT),
            None
        );
        assert_eq!(
            decode_barcode_y(&black[..100], BARCODE_WIDTH, BARCODE_WIDTH, BARCODE_HEIGHT),
            None
        );
    }
}
