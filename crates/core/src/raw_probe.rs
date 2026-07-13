//! 无头 RAW 文件参数候选推断。
//!
//! 推断结果只用于辅助填写参数；无头字节流不能唯一确定分辨率、有效位深或端序。

use std::cmp::Reverse;

const CANDIDATE_LIMIT: usize = 8;
const MAX_DIMENSION: u32 = 32_768;
const EFFECTIVE_BIT_DEPTHS: [u8; 5] = [8, 10, 12, 14, 16];
const COMMON_RESOLUTIONS: [(u32, u32); 15] = [
    (320, 240),
    (640, 480),
    (800, 600),
    (1280, 720),
    (1280, 960),
    (1600, 1200),
    (1920, 1080),
    (1920, 1200),
    (2048, 1536),
    (2560, 1440),
    (2592, 1944),
    (2688, 1520),
    (3072, 2048),
    (3840, 2160),
    (4096, 2160),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawProbeInput {
    pub file_name: Option<String>,
    pub file_len: u64,
    pub samples: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RawContainer {
    UnpackedU16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RawEndian {
    Little,
    Big,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawProbeCandidate {
    pub width: u32,
    pub height: u32,
    pub effective_bit_depth: u8,
    pub container: RawContainer,
    pub endian: RawEndian,
    pub score: i32,
    pub out_of_range_samples: usize,
    pub total_samples: usize,
    pub reasons: Vec<String>,
}

impl RawProbeCandidate {
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        matches!(self.container, RawContainer::UnpackedU16)
            && matches!(self.endian, RawEndian::Little)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawProbeReport {
    pub file_len: u64,
    pub candidates: Vec<RawProbeCandidate>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DimensionCandidate {
    width: u32,
    height: u32,
    filename_hint: bool,
    common: bool,
    score: i32,
}

#[derive(Debug, Clone, Copy)]
struct SampleStats {
    max: u16,
    total: usize,
    zeroes: usize,
}

#[derive(Debug, Clone, Copy)]
struct ScoredCandidate {
    dimensions: DimensionCandidate,
    effective_bit_depth: u8,
    endian: RawEndian,
    score: i32,
    out_of_range_samples: usize,
    total_samples: usize,
    observed_max: u16,
}

#[must_use]
pub fn probe_raw_candidates(input: &RawProbeInput) -> RawProbeReport {
    let mut diagnostics = Vec::new();
    if input.file_len == 0 {
        diagnostics.push("RAW 文件为空".to_owned());
        return RawProbeReport {
            file_len: input.file_len,
            candidates: Vec::new(),
            diagnostics,
        };
    }
    if !input.file_len.is_multiple_of(2) {
        diagnostics.push("文件长度不是 uint16 字节宽度的整数倍".to_owned());
        return RawProbeReport {
            file_len: input.file_len,
            candidates: Vec::new(),
            diagnostics,
        };
    }

    let pixel_count = input.file_len / 2;
    if pixel_count > u64::from(MAX_DIMENSION) * u64::from(MAX_DIMENSION) {
        diagnostics.push(format!("文件超过候选推断支持的最大边长 {MAX_DIMENSION}"));
        return RawProbeReport {
            file_len: input.file_len,
            candidates: Vec::new(),
            diagnostics,
        };
    }
    let dimensions = dimension_candidates(input.file_name.as_deref(), pixel_count);
    if dimensions.is_empty() {
        diagnostics.push("未找到与文件长度匹配的合理分辨率候选".to_owned());
        return RawProbeReport {
            file_len: input.file_len,
            candidates: Vec::new(),
            diagnostics,
        };
    }

    let little_values = decode_samples(&input.samples, RawEndian::Little);
    let big_values = decode_samples(&input.samples, RawEndian::Big);
    let little_stats = sample_stats(&little_values);
    let big_stats = sample_stats(&big_values);

    if little_stats.total == 0 {
        diagnostics.push("文件样本不足，候选仅依据文件名和长度排序".to_owned());
    } else {
        diagnostics.push("参数为候选推断，必须人工确认后再打开".to_owned());
    }

    let scored = score_candidates(
        &dimensions,
        &little_values,
        little_stats,
        &big_values,
        big_stats,
    );
    let candidates = select_candidates(scored)
        .into_iter()
        .map(build_candidate)
        .collect();
    RawProbeReport {
        file_len: input.file_len,
        candidates,
        diagnostics,
    }
}
fn score_candidates(
    dimensions: &[DimensionCandidate],
    little_values: &[u16],
    little_stats: SampleStats,
    big_values: &[u16],
    big_stats: SampleStats,
) -> Vec<ScoredCandidate> {
    let mut scored = Vec::new();
    for &dimensions in dimensions {
        for (endian, values, stats) in [
            (RawEndian::Little, little_values, little_stats),
            (RawEndian::Big, big_values, big_stats),
        ] {
            for effective_bit_depth in EFFECTIVE_BIT_DEPTHS {
                let max_code = max_code(effective_bit_depth);
                let out_of_range_samples = values
                    .iter()
                    .filter(|&&value| u32::from(value) > max_code)
                    .count();
                let bit_score = bit_depth_score(stats, effective_bit_depth, out_of_range_samples);
                scored.push(ScoredCandidate {
                    dimensions,
                    effective_bit_depth,
                    endian,
                    score: dimensions.score + bit_score,
                    out_of_range_samples,
                    total_samples: stats.total,
                    observed_max: stats.max,
                });
            }
        }
    }
    scored
}

fn select_candidates(mut scored: Vec<ScoredCandidate>) -> Vec<ScoredCandidate> {
    scored.sort_by_key(|candidate| {
        (
            Reverse(candidate.score),
            candidate.out_of_range_samples,
            Reverse(candidate.dimensions.filename_hint),
            Reverse(candidate.dimensions.common),
            candidate.dimensions.width,
            candidate.dimensions.height,
            candidate.endian,
            candidate.effective_bit_depth,
        )
    });

    let mut selected = Vec::with_capacity(CANDIDATE_LIMIT);
    for candidate in scored
        .iter()
        .copied()
        .filter(|candidate| candidate.endian == RawEndian::Little)
    {
        if selected.iter().any(|selected: &ScoredCandidate| {
            selected.dimensions.width == candidate.dimensions.width
                && selected.dimensions.height == candidate.dimensions.height
        }) {
            continue;
        }
        selected.push(candidate);
        if selected.len() == CANDIDATE_LIMIT {
            return selected;
        }
    }
    for candidate in scored {
        if selected.len() == CANDIDATE_LIMIT {
            break;
        }
        if selected.iter().any(|selected| {
            selected.dimensions.width == candidate.dimensions.width
                && selected.dimensions.height == candidate.dimensions.height
                && selected.effective_bit_depth == candidate.effective_bit_depth
                && selected.endian == candidate.endian
        }) {
            continue;
        }
        selected.push(candidate);
    }
    selected
}

fn dimension_candidates(file_name: Option<&str>, pixel_count: u64) -> Vec<DimensionCandidate> {
    let filename_dimensions = file_name.map_or_else(Vec::new, parse_filename_dimensions);
    let mut dimensions = Vec::new();

    for (width, height) in filename_dimensions {
        if u64::from(width) * u64::from(height) == pixel_count {
            insert_dimension(
                &mut dimensions,
                width,
                height,
                true,
                is_common(width, height),
            );
        }
    }
    for (width, height) in COMMON_RESOLUTIONS {
        if u64::from(width) * u64::from(height) == pixel_count {
            insert_dimension(&mut dimensions, width, height, false, true);
        }
    }

    for width in 64..=MAX_DIMENSION {
        let width_u64 = u64::from(width);
        if !pixel_count.is_multiple_of(width_u64) {
            continue;
        }
        let height = pixel_count / width_u64;
        let Ok(height) = u32::try_from(height) else {
            continue;
        };
        if !(64..=MAX_DIMENSION).contains(&height)
            || width < height
            || width > height.saturating_mul(4)
        {
            continue;
        }
        insert_dimension(
            &mut dimensions,
            width,
            height,
            false,
            is_common(width, height),
        );
    }

    dimensions.sort_by_key(|candidate| {
        (
            Reverse(candidate.score),
            Reverse(candidate.filename_hint),
            Reverse(candidate.common),
            candidate.width,
            candidate.height,
        )
    });
    dimensions.truncate(CANDIDATE_LIMIT);
    dimensions
}

fn insert_dimension(
    dimensions: &mut Vec<DimensionCandidate>,
    width: u32,
    height: u32,
    filename_hint: bool,
    common: bool,
) {
    if let Some(existing) = dimensions
        .iter_mut()
        .find(|candidate| candidate.width == width && candidate.height == height)
    {
        existing.filename_hint |= filename_hint;
        existing.common |= common;
        existing.score = dimension_score(width, height, existing.filename_hint, existing.common);
        return;
    }
    dimensions.push(DimensionCandidate {
        width,
        height,
        filename_hint,
        common,
        score: dimension_score(width, height, filename_hint, common),
    });
}

fn dimension_score(width: u32, height: u32, filename_hint: bool, common: bool) -> i32 {
    let mut score = 0;
    if filename_hint {
        score += 160;
    }
    if common {
        score += 80;
    }
    if width.is_multiple_of(16) {
        score += 16;
    }
    if height.is_multiple_of(2) {
        score += 4;
    }
    let ratio = f64::from(width) / f64::from(height);
    let common_ratio_distance = [4.0 / 3.0, 16.0 / 9.0, 3.0 / 2.0]
        .into_iter()
        .map(|common_ratio| (ratio - common_ratio).abs())
        .fold(f64::INFINITY, f64::min);
    if common_ratio_distance < 0.01 {
        score += 20;
    } else if common_ratio_distance < 0.05 {
        score += 10;
    }
    score
}

fn bit_depth_score(stats: SampleStats, bit_depth: u8, out_of_range: usize) -> i32 {
    if stats.total == 0 {
        return 0;
    }
    if out_of_range != 0 {
        let ratio = out_of_range.saturating_mul(100) / stats.total;
        return -i32::try_from(ratio.min(100)).unwrap_or(100);
    }

    let observed_bits = u8::try_from((u16::BITS - stats.max.leading_zeros()).max(1))
        .expect("u16 bit width fits u8");
    let distance = bit_depth.saturating_sub(observed_bits);
    let mut score = 40 - i32::from(distance) * 2;
    if stats.zeroes == stats.total {
        score -= 10;
    }
    score
}

fn build_candidate(candidate: ScoredCandidate) -> RawProbeCandidate {
    let mut reasons = Vec::new();
    if candidate.dimensions.filename_hint {
        reasons.push("文件名包含匹配的分辨率".to_owned());
    }
    if candidate.dimensions.common {
        reasons.push("匹配常见分辨率".to_owned());
    }
    reasons.push("文件长度与紧密 uint16 布局一致".to_owned());
    if candidate.total_samples == 0 {
        reasons.push("没有足够像素样本判断有效位深".to_owned());
    } else if candidate.out_of_range_samples == 0 {
        reasons.push(format!(
            "样本最大值 {} 未超出 {}-bit 范围",
            candidate.observed_max, candidate.effective_bit_depth
        ));
    } else {
        reasons.push(format!(
            "{}/{} 个样本超出 {}-bit 范围",
            candidate.out_of_range_samples, candidate.total_samples, candidate.effective_bit_depth
        ));
    }
    if candidate.endian == RawEndian::Big {
        reasons.push("大端候选当前不能直接加载".to_owned());
    }

    RawProbeCandidate {
        width: candidate.dimensions.width,
        height: candidate.dimensions.height,
        effective_bit_depth: candidate.effective_bit_depth,
        container: RawContainer::UnpackedU16,
        endian: candidate.endian,
        score: candidate.score,
        out_of_range_samples: candidate.out_of_range_samples,
        total_samples: candidate.total_samples,
        reasons,
    }
}

fn decode_samples(samples: &[Vec<u8>], endian: RawEndian) -> Vec<u16> {
    let total_bytes = samples.iter().map(Vec::len).sum::<usize>();
    let mut values = Vec::with_capacity(total_bytes / 2);
    for sample in samples {
        values.extend(sample.chunks_exact(2).map(|chunk| match endian {
            RawEndian::Little => u16::from_le_bytes([chunk[0], chunk[1]]),
            RawEndian::Big => u16::from_be_bytes([chunk[0], chunk[1]]),
        }));
    }
    values
}

fn sample_stats(values: &[u16]) -> SampleStats {
    SampleStats {
        max: values.iter().copied().max().unwrap_or(0),
        total: values.len(),
        zeroes: values.iter().filter(|&&value| value == 0).count(),
    }
}

const fn max_code(bit_depth: u8) -> u32 {
    if bit_depth >= 16 {
        u16::MAX as u32
    } else {
        (1u32 << bit_depth) - 1
    }
}

fn is_common(width: u32, height: u32) -> bool {
    COMMON_RESOLUTIONS.contains(&(width, height))
}

fn parse_filename_dimensions(file_name: &str) -> Vec<(u32, u32)> {
    let bytes = file_name.as_bytes();
    let mut dimensions = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let (Some(width), next) = parse_u32(bytes, index) else {
            index += 1;
            continue;
        };
        if next >= bytes.len() || !matches!(bytes[next], b'x' | b'X' | b'_') {
            index = next;
            continue;
        }
        let (Some(height), end) = parse_u32(bytes, next + 1) else {
            index = next + 1;
            continue;
        };
        if width != 0 && height != 0 && !dimensions.contains(&(width, height)) {
            dimensions.push((width, height));
        }
        index = end;
    }
    dimensions
}

fn parse_u32(bytes: &[u8], start: usize) -> (Option<u32>, usize) {
    let mut value = 0u32;
    let mut index = start;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        let Some(next) = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(bytes[index] - b'0')))
        else {
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            return (None, index);
        };
        value = next;
        index += 1;
    }
    ((index > start).then_some(value), index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn little_endian_samples(values: &[u16]) -> Vec<Vec<u8>> {
        vec![
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        ]
    }
    #[test]
    fn rejects_files_beyond_bounded_dimension_search() {
        let pixels = u64::from(MAX_DIMENSION) * u64::from(MAX_DIMENSION) + 1;
        let report = probe_raw_candidates(&RawProbeInput {
            file_name: Some("huge.raw".to_owned()),
            file_len: pixels * 2,
            samples: Vec::new(),
        });

        assert!(report.candidates.is_empty());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("最大边长"))
        );
    }

    #[test]
    fn filename_resolution_ranks_first() {
        let input = RawProbeInput {
            file_name: Some("capture_1920x1080.raw".to_owned()),
            file_len: 1920 * 1080 * 2,
            samples: little_endian_samples(&[0, 64, 512, 1023]),
        };

        let report = probe_raw_candidates(&input);

        assert_eq!(
            (report.candidates[0].width, report.candidates[0].height),
            (1920, 1080)
        );
        assert!(
            report.candidates[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("文件名"))
        );
    }

    #[test]
    fn low_dynamic_range_keeps_multiple_bit_depth_candidates() {
        let input = RawProbeInput {
            file_name: Some("frame_640x480.raw".to_owned()),
            file_len: 640 * 480 * 2,
            samples: little_endian_samples(&[0, 16, 128, 511]),
        };

        let report = probe_raw_candidates(&input);
        let depths = report
            .candidates
            .iter()
            .filter(|candidate| candidate.width == 640 && candidate.height == 480)
            .map(|candidate| candidate.effective_bit_depth)
            .collect::<Vec<_>>();

        assert!(depths.contains(&10));
        assert!(depths.contains(&12));
        assert!(depths.contains(&16));
    }

    #[test]
    fn big_endian_candidates_are_reported_but_unsupported() {
        let input = RawProbeInput {
            file_name: Some("frame_640x480.raw".to_owned()),
            file_len: 640 * 480 * 2,
            samples: vec![
                [0u16, 255, 511, 1023]
                    .into_iter()
                    .flat_map(u16::to_be_bytes)
                    .collect(),
            ],
        };

        let report = probe_raw_candidates(&input);
        let big = report
            .candidates
            .iter()
            .find(|candidate| candidate.endian == RawEndian::Big)
            .unwrap();

        assert!(!big.is_supported());
    }

    #[test]
    fn odd_byte_count_has_no_uint16_candidates() {
        let report = probe_raw_candidates(&RawProbeInput {
            file_name: Some("broken.raw".to_owned()),
            file_len: 3,
            samples: vec![vec![0, 1, 2]],
        });

        assert!(report.candidates.is_empty());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("整数倍"))
        );
    }
    #[test]
    fn candidate_limit_preserves_resolution_diversity() {
        let input = RawProbeInput {
            file_name: Some("frame.raw".to_owned()),
            file_len: 1024 * 768 * 2,
            samples: little_endian_samples(&[0, 64, 512, 1023]),
        };

        let report = probe_raw_candidates(&input);
        let mut dimensions = report
            .candidates
            .iter()
            .map(|candidate| (candidate.width, candidate.height))
            .collect::<Vec<_>>();
        dimensions.sort_unstable();
        dimensions.dedup();

        assert!(dimensions.len() > 1);
    }
}
