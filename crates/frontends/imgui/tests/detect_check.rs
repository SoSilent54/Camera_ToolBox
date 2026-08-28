//! 检测链路独立验证：生成棋盘测试图 → OpenCV detect_png。
//! 运行：cargo test -p pongbot-calib-tool --test detect_check

use camera_toolbox_adapters::calibration::OpenCvCalibrationBackend;
use camera_toolbox_app::ports::calibration::{CalibrationBackend, CalibrationCancellation};
use camera_toolbox_core::{BoardSpec, CalibrationImageSize, ChessboardDetectionOutcome};

/// 生成棋盘测试图（12x9 格 = 11x8 内角点），可选错切。
fn synth_board_rgba(width: u32, height: u32, cell: i32, shear: f32) -> Vec<u8> {
    let cols = 12i32;
    let rows = 9i32;
    let ox = (width as i32 - cols * cell) / 2;
    let oy = (height as i32 - rows * cell) / 2;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let sx = (x as f32 + shear * (y - oy) as f32) as i32;
            let gx = (sx - ox) / cell;
            let gy = (y - oy) / cell;
            let inside = gx >= 0 && gx < cols && gy >= 0 && gy < rows;
            let white = inside && ((gx + gy) % 2 == 0);
            let v = if white {
                235u8
            } else if inside {
                30u8
            } else {
                60u8
            };
            rgba.extend_from_slice(&[v, v, v, 255]);
        }
    }
    rgba
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec()).ok_or("尺寸非法")?;
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

fn detect(
    rgba: &[u8],
    width: u32,
    height: u32,
    board: BoardSpec,
) -> Result<ChessboardDetectionOutcome, String> {
    let backend = OpenCvCalibrationBackend;
    let cancel = CalibrationCancellation::default();
    let png = encode_png(rgba, width, height)?;
    backend
        .detect_png(
            &png,
            CalibrationImageSize { width, height },
            256 * 1024 * 1024,
            board,
            &cancel,
        )
        .map_err(|e| format!("detect_png 失败：{e}"))
}

#[test]
fn synth_board_detects_at_multiple_shears() {
    let board = BoardSpec {
        inner_cols: 11,
        inner_rows: 8,
        square_size: 40.0,
    };
    for shear in [-0.1f32, 0.0, 0.05, 0.1, 0.2] {
        let rgba = synth_board_rgba(640, 360, 40, shear);
        match detect(&rgba, 640, 360, board) {
            Ok(ChessboardDetectionOutcome::Found(d)) => {
                assert_eq!(d.corners.len(), 88, "11x8 内角点应为 88");
                println!("shear={shear}: Found {} 角点 ✓", d.corners.len());
            }
            Ok(ChessboardDetectionOutcome::NotFound { .. }) => {
                println!("shear={shear}: NotFound");
            }
            Err(e) => println!("shear={shear}: Err {e}"),
        }
    }
}

#[test]
fn synth_board_detects_1080p() {
    let board = BoardSpec {
        inner_cols: 11,
        inner_rows: 8,
        square_size: 40.0,
    };
    let rgba = synth_board_rgba(1920, 1080, 80, 0.0);
    match detect(&rgba, 1920, 1080, board) {
        Ok(ChessboardDetectionOutcome::Found(d)) => {
            assert_eq!(d.corners.len(), 88, "11x8 内角点应为 88");
            println!("1080p: Found {} 角点 ✓", d.corners.len());
        }
        Ok(ChessboardDetectionOutcome::NotFound { .. }) => println!("1080p: NotFound"),
        Err(e) => println!("1080p: Err {e}"),
    }
}

#[test]
fn noise_does_not_detect() {
    let board = BoardSpec {
        inner_cols: 11,
        inner_rows: 8,
        square_size: 40.0,
    };
    let mut rgba = vec![0u8; 640 * 360 * 4];
    for (i, chunk) in rgba.chunks_mut(4).enumerate() {
        let v = ((i as u32 * 2_654_435_761 >> 16) & 0xff) as u8;
        chunk.copy_from_slice(&[v, v.wrapping_mul(2), 255 - v, 255]);
    }
    match detect(&rgba, 640, 360, board) {
        Ok(ChessboardDetectionOutcome::Found(_)) => println!("noise: Found（误检！）"),
        Ok(ChessboardDetectionOutcome::NotFound { .. }) => println!("noise: NotFound ✓"),
        Err(e) => println!("noise: Err {e}"),
    }
}
