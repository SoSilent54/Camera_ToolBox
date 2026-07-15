//! RAW 解码参数、Preset 与证据排序。

use camera_toolbox_core::{
    BayerPattern, RawContainer, RawEncoding, RawEndian, RawProbeCandidate, RawProbeReport, RawSpec,
};
use thiserror::Error;

use crate::{FileMatcher, FileRef};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDecodeParams {
    pub spec: RawSpec,
    pub encoding: RawEncoding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawPresetEvidence {
    ExplicitMetadata,
    SavedPreset,
    ProbeCandidate,
    AssumedDefault,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawInterpretation {
    pub params: RawDecodeParams,
    pub parameter_evidence: RawPresetEvidence,
    pub bayer_evidence: RawPresetEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDecodePreset {
    pub name: String,
    pub matcher: FileMatcher,
    pub params: RawDecodeParams,
    pub expected_bytes: Option<u64>,
    pub priority: i32,
}

impl RawDecodePreset {
    #[must_use]
    pub fn matches(&self, reference: &FileRef, bytes: u64) -> bool {
        self.matcher.matches(reference)
            && self.expected_bytes.is_none_or(|expected| expected == bytes)
    }
}

pub(crate) fn select_automatic(
    reference: &FileRef,
    report: &RawProbeReport,
    presets: &[RawDecodePreset],
) -> Result<RawInterpretation, RawPresetError> {
    if let Some(preset) = presets
        .iter()
        .filter(|preset| preset.matches(reference, report.file_len))
        .max_by_key(|preset| preset.priority)
    {
        return Ok(RawInterpretation {
            params: preset.params.clone(),
            parameter_evidence: RawPresetEvidence::SavedPreset,
            bayer_evidence: RawPresetEvidence::SavedPreset,
        });
    }

    let candidate = report
        .candidates
        .iter()
        .find(|candidate| candidate.is_supported())
        .ok_or(RawPresetError::NoSupportedCandidate)?;
    let params = candidate_params(candidate)?;
    Ok(RawInterpretation {
        params,
        parameter_evidence: RawPresetEvidence::ProbeCandidate,
        bayer_evidence: RawPresetEvidence::AssumedDefault,
    })
}

pub(crate) fn explicit(params: RawDecodeParams) -> RawInterpretation {
    RawInterpretation {
        params,
        parameter_evidence: RawPresetEvidence::ExplicitMetadata,
        bayer_evidence: RawPresetEvidence::ExplicitMetadata,
    }
}

fn candidate_params(candidate: &RawProbeCandidate) -> Result<RawDecodeParams, RawPresetError> {
    let encoding = match (candidate.container, candidate.endian) {
        (RawContainer::UnpackedU16, RawEndian::Little) => RawEncoding::U16Le,
        _ => return Err(RawPresetError::UnsupportedCandidate),
    };
    Ok(RawDecodeParams {
        spec: RawSpec {
            width: candidate.width,
            height: candidate.height,
            bit_depth: candidate.effective_bit_depth,
            bayer: BayerPattern::Rggb,
        },
        encoding,
    })
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RawPresetError {
    #[error("RAW probe produced no supported decode candidate")]
    NoSupportedCandidate,
    #[error("selected RAW candidate uses an unsupported container or byte order")]
    UnsupportedCandidate,
}
