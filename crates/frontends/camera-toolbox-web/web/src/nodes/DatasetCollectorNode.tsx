import { useEffect } from 'react';
import type { NodeProps } from '@xyflow/react';
import type {
  DatasetCollectorRuntimeOutput,
  DatasetSampleActionName,
  DatasetSampleRuntimeOutput,
  FlowNodeData,
  NodeActionControl,
} from '../workflow';
import { NodeActionButtons, NodeHeader, PortHandles, RuntimeOutputSummary, ScalarConfigFields } from './shared';

const DATASET_ACTIONS: readonly NodeActionControl[] = [
  { action: 'trigger', label: '输出数据集' },
  { action: 'clear', label: '清空样本' },
];

/** Dataset Collector：以运行时样本快照渲染可审核的文件浏览器式列表，不把列表状态写入工作流。 */
export function DatasetCollectorNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const dataset = parseDatasetOutput(nodeData.runtimeOutput);
  const configuredCapacity = node.config.maxSamples;
  const maxSamples = typeof configuredCapacity === 'number'
    && Number.isInteger(configuredCapacity)
    && configuredCapacity > 0
    ? configuredCapacity
    : undefined;

  // 节点挂载后请求一次最新快照；后续动作完成时 useEngine 会再次刷新。
  useEffect(() => {
    nodeData.onRefreshNodeOutput?.(node.id);
  }, [node.id, nodeData.onRefreshNodeOutput]);

  return (
    <div className="workflow-node-shell dataset-collector-node-shell">
      <section className={`workflow-node dataset-collector-node ${selected ? 'selected' : ''}`}>
        <NodeHeader node={node} runtimeState={nodeData.runtimeState} />
        <PortHandles node={node} />
        <div className="node-body compact">
          <span className="node-hint">样本仅在运行时缓存；“输出数据集”发送到 dataset 端口，不写入本地文件。</span>
          <ScalarConfigFields
            nodeId={node.id}
            config={node.config}
            onChange={nodeData.onNodeConfigChange}
          />
          <DatasetSampleBrowser
            dataset={dataset}
            maxSamples={maxSamples}
            pending={Boolean(nodeData.actionPending)}
            onAction={nodeData.onNodeAction ? (action, sampleId) => nodeData.onNodeAction?.(node.id, action, { sampleId }) : undefined}
          />
          {!dataset && nodeData.runtimeOutput !== undefined ? <RuntimeOutputSummary output={nodeData.runtimeOutput} /> : null}
          <NodeActionButtons
            nodeId={node.id}
            actions={DATASET_ACTIONS}
            pending={nodeData.actionPending}
            onAction={nodeData.onNodeAction}
            onRefreshOutput={nodeData.onRefreshNodeOutput}
          />
        </div>
      </section>
      {nodeData.runtimeDiagnostic ? <div className="node-diagnostic-below" title={nodeData.runtimeDiagnostic}>{nodeData.runtimeDiagnostic}</div> : null}
    </div>
  );
}

function DatasetSampleBrowser({
  dataset,
  maxSamples,
  pending,
  onAction,
}: {
  dataset?: DatasetCollectorRuntimeOutput;
  maxSamples?: number;
  pending: boolean;
  onAction?: (action: DatasetSampleActionName, sampleId: string) => void;
}) {
  const samples = dataset?.samples ?? [];
  const count = dataset?.count ?? 0;
  const capacity = maxSamples === undefined ? String(count) : `${count} / ${maxSamples}`;

  return (
    <section className="dataset-browser" aria-label="Dataset Collector samples">
      <header className="dataset-browser-header">
        <strong>样本</strong>
        <span>{capacity}</span>
      </header>
      {dataset ? (
        samples.length > 0 ? (
          <ul className="dataset-sample-list">
            {samples.map((sample) => (
              <DatasetSampleRow key={sample.id} sample={sample} pending={pending} onAction={onAction} />
            ))}
          </ul>
        ) : <p className="dataset-browser-empty">当前数据集没有可显示样本。</p>
      ) : <p className="dataset-browser-empty">等待运行时样本快照；可点击“刷新结果”。</p>}
    </section>
  );
}

function DatasetSampleRow({
  sample,
  pending,
  onAction,
}: {
  sample: DatasetSampleRuntimeOutput;
  pending: boolean;
  onAction?: (action: DatasetSampleActionName, sampleId: string) => void;
}) {
  const status = sampleStatus(sample);
  const score = sample.score && Number.isFinite(sample.score.score) ? sample.score.score.toFixed(3) : '—';
  const source = formatSampleSource(sample.provenance?.source) ?? sample.imageRef?.ref ?? sample.id;
  const sourceDetail = sampleSourceDetail(sample);
  const accepted = sample.acceptance?.accepted === true;
  const enabled = sample.acceptance?.enabled === true;
  const reviewAction: DatasetSampleActionName = accepted ? 'reject' : 'accept';
  const availabilityAction: DatasetSampleActionName = enabled ? 'disable' : 'enable';
  const disabled = !onAction || pending;

  return (
    <li className="dataset-sample-row" title={sample.id}>
      <div className="dataset-sample-main">
        <div className="dataset-sample-source">
          <span className="dataset-sample-icon">#</span>
          <span className="dataset-sample-name">{source}</span>
        </div>
        <div className="dataset-sample-meta">
          <span className={`dataset-sample-status ${status.tone}`}>{status.label}</span>
          <span className="dataset-sample-score">score {score}</span>
          {sourceDetail ? <span className="dataset-sample-detail">{sourceDetail}</span> : null}
        </div>
      </div>
      <div className="dataset-sample-actions">
        <button
          type="button"
          className="nodrag nowheel"
          disabled={disabled}
          onClick={() => onAction?.(reviewAction, sample.id)}
        >
          {reviewAction === 'accept' ? '采纳' : '拒绝'}
        </button>
        <button
          type="button"
          className="nodrag nowheel"
          disabled={disabled}
          onClick={() => onAction?.(availabilityAction, sample.id)}
        >
          {availabilityAction === 'enable' ? '启用' : '停用'}
        </button>
        <button
          type="button"
          className="nodrag nowheel dataset-sample-delete"
          disabled={disabled}
          onClick={() => onAction?.('delete', sample.id)}
        >
          删除
        </button>
      </div>
    </li>
  );
}

type SampleStatus = {
  label: string;
  tone: 'accepted' | 'rejected' | 'disabled' | 'pending';
};

/** 审核状态以 acceptance 为唯一来源，避免由 score 或检测结果推断可用性。 */
function sampleStatus(sample: DatasetSampleRuntimeOutput): SampleStatus {
  if (sample.acceptance?.accepted === false) {
    return { label: '已拒绝', tone: 'rejected' };
  }
  if (sample.acceptance?.enabled === false) {
    return { label: '已禁用', tone: 'disabled' };
  }
  if (sample.acceptance?.accepted === true && sample.acceptance.enabled === true) {
    return { label: '已采纳', tone: 'accepted' };
  }
  return { label: '待审核', tone: 'pending' };
}


/** 列表只显示资产元数据与帧身份，不读取或复制图像字节。 */
function sampleSourceDetail(sample: DatasetSampleRuntimeOutput): string | undefined {
  const dimensions = sample.imageRef ? `${sample.imageRef.width}×${sample.imageRef.height}` : undefined;
  const frameIdentity = sample.provenance?.frameIdentity;
  const frameSequence = frameIdentity?.frameSequence ?? sample.score?.frameSequence;
  return [
    dimensions,
    sample.imageRef?.format ?? undefined,
    frameSequence === undefined ? undefined : `frame ${frameSequence}`,
    formatSourcePts(frameIdentity?.sourcePts),
  ]
    .filter((value): value is string => Boolean(value))
    .join(' · ') || undefined;
}

/** 只接受完整的新版 dataset 输出，防止把无 sampleId 的旧检测数组误显示为可操作项。 */
function parseDatasetOutput(output: unknown): DatasetCollectorRuntimeOutput | undefined {
  const record = objectRecord(output);
  if (!record || record.kind !== 'calib.dataset.v1' || !Array.isArray(record.samples)) {
    return undefined;
  }
  const samples = record.samples
    .map(normalizeDatasetSample)
    .filter((sample): sample is DatasetSampleRuntimeOutput => sample !== undefined);
  return {
    kind: 'calib.dataset.v1',
    count: nonNegativeInteger(record.count) ?? samples.length,
    samples,
  };
}

/** 将 WS 运行时 JSON 窄化为可安全渲染且可按 sampleId 操作的样本。 */
function normalizeDatasetSample(value: unknown): DatasetSampleRuntimeOutput | undefined {
  const record = objectRecord(value);
  const id = record && stringValue(record.id);
  if (!record || !id) {
    return undefined;
  }
  const imageRef = normalizeImageReference(record.imageRef);
  const acceptance = normalizeAcceptance(record.acceptance);
  const score = normalizeScore(record.score);
  return {
    id,
    imageRef,
    detection: record.detection,
    score,
    acceptance,
    provenance: normalizeProvenance(record.provenance),
  };
}

/** imageRef 只接受后端明确输出的引用和元数据字段。 */
function normalizeImageReference(value: unknown): DatasetSampleRuntimeOutput['imageRef'] {
  const record = objectRecord(value);
  const ref = record && stringValue(record.ref);
  const width = record && nonNegativeInteger(record.width);
  const height = record && nonNegativeInteger(record.height);
  const format = record?.format === null ? null : stringValue(record?.format);
  if (!record || !ref || width === undefined || height === undefined || format === undefined) {
    return undefined;
  }
  return { ref, width, height, format };
}

function normalizeAcceptance(value: unknown): DatasetSampleRuntimeOutput['acceptance'] {
  const record = objectRecord(value);
  if (!record) {
    return undefined;
  }
  const accepted = typeof record.accepted === 'boolean' ? record.accepted : undefined;
  const enabled = typeof record.enabled === 'boolean' ? record.enabled : undefined;
  return accepted === undefined && enabled === undefined ? undefined : { accepted, enabled };
}

/** null score 是未评分样本的正常状态，不能当作零分。 */
function normalizeScore(value: unknown): DatasetSampleRuntimeOutput['score'] {
  if (value === null) {
    return null;
  }
  const record = objectRecord(value);
  const score = record && typeof record.score === 'number' && Number.isFinite(record.score)
    ? record.score
    : undefined;
  const frameSequence = record && typeof record.frameSequence === 'number'
    && Number.isInteger(record.frameSequence)
    && record.frameSequence >= 0
    ? record.frameSequence
    : undefined;
  return score === undefined || frameSequence === undefined ? undefined : { score, frameSequence };
}

function normalizeProvenance(value: unknown): DatasetSampleRuntimeOutput['provenance'] {
  const record = objectRecord(value);
  if (!record) {
    return undefined;
  }
  const source = normalizeStructuredRecord(record.source);
  const frameIdentity = normalizeFrameIdentity(record.frameIdentity);
  return source === undefined && frameIdentity === undefined ? undefined : { source, frameIdentity };
}

function normalizeFrameIdentity(value: unknown): NonNullable<DatasetSampleRuntimeOutput['provenance']>['frameIdentity'] {
  const record = objectRecord(value);
  if (!record) {
    return undefined;
  }
  const frameSequence = typeof record.frameSequence === 'number'
    && Number.isInteger(record.frameSequence)
    && record.frameSequence >= 0
    ? record.frameSequence
    : undefined;
  const sourcePts = normalizeStructuredRecord(record.sourcePts);
  const hostMonotonicTimeNs = typeof record.hostMonotonicTimeNs === 'number'
    && Number.isInteger(record.hostMonotonicTimeNs)
    && record.hostMonotonicTimeNs >= 0
    ? record.hostMonotonicTimeNs
    : undefined;
  return frameSequence === undefined && sourcePts === undefined && hostMonotonicTimeNs === undefined
    ? undefined
    : { frameSequence, sourcePts, hostMonotonicTimeNs };
}

function normalizeStructuredRecord(value: unknown): Record<string, unknown> | undefined {
  return objectRecord(value);
}

function formatSampleSource(source: Record<string, unknown> | undefined): string | undefined {
  if (!source) {
    return undefined;
  }
  const kind = stringValue(source.kind);
  if (kind === 'stream') {
    const streamId = stringValue(source.streamId);
    const channel = nonNegativeInteger(source.channel);
    return [streamId ? `stream ${streamId}` : 'stream', channel === undefined ? undefined : `ch ${channel}`]
      .filter((value): value is string => Boolean(value))
      .join(' · ');
  }
  if (kind === 'device') {
    const driver = stringValue(source.driver) ?? 'device';
    const channel = nonNegativeInteger(source.channel);
    const camera = nonNegativeInteger(source.camera);
    return [driver, channel === undefined ? undefined : `ch ${channel}`, camera === undefined ? undefined : `cam ${camera}`]
      .filter((value): value is string => Boolean(value))
      .join(' · ');
  }
  if (kind === 'file') {
    return stringValue(source.source) ?? 'file';
  }
  if (kind === 'unknown') {
    return stringValue(source.reason) ?? 'unknown';
  }
  return kind;
}

function formatSourcePts(sourcePts: Record<string, unknown> | undefined): string | undefined {
  if (!sourcePts) {
    return undefined;
  }
  const kind = stringValue(sourcePts.kind);
  if (kind === 'known') {
    const ticks = typeof sourcePts.ticks === 'number' && Number.isFinite(sourcePts.ticks) ? sourcePts.ticks : undefined;
    const timeBase = objectRecord(sourcePts.timeBase);
    const numerator = timeBase && nonNegativeInteger(timeBase.numerator);
    const denominator = timeBase && nonNegativeInteger(timeBase.denominator);
    const rate = numerator === undefined || denominator === undefined ? undefined : `${numerator}/${denominator}`;
    return ticks === undefined ? 'PTS known' : `PTS ${ticks}${rate ? ` @ ${rate}` : ''}`;
  }
  if (kind === 'unavailable') {
    return stringValue(sourcePts.reason) ?? 'PTS unavailable';
  }
  return kind;
}

function objectRecord(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
}

function stringValue(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim() ? value : undefined;
}
function nonNegativeInteger(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 ? value : undefined;
}


