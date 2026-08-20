import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { NodeHeader, PortHandles, RuntimeOutputSummary, ScalarConfigFields } from './shared';

/** 标定参数源：编辑轻量配置后显式发射强类型参数包，避免在保存图中混入运行态负载。 */
export function CalibrationParameterNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const runtimeState = nodeData.runtimeState;
  const runtimeDiagnostic = nodeData.runtimeDiagnostic;
  const actionPending = Boolean(nodeData.actionPending);
  const isBoard = node.kind === 'calibrationBoardParams';
  const subject = isBoard ? '棋盘参数' : '相机参数';
  const scalarConfig = { ...node.config };
  if (isBoard) {
    delete scalarConfig.boardKind;
  } else {
    delete scalarConfig.cameraModelKind;
    delete scalarConfig.distortionKind;
  }

  return (
    <div className="workflow-node-shell">
      <section className={`workflow-node generic-node ${selected ? 'selected' : ''}`}>
        <NodeHeader node={node} runtimeState={runtimeState} />
        <PortHandles node={node} />
        <div className="node-body compact">
          <span className="node-hint">
            {isBoard
              ? 'cols/rows 为棋盘内角点数；squareSizeMm 的单位为 mm。'
              : '焦距和主点单位为 px；当前仅支持 pinhole 与 none 畸变。'}
          </span>
          {isBoard ? (
            <label className="node-config-field">
              <code>boardKind</code>
              <select
                className="nodrag nowheel"
                value={node.config.boardKind === 'chessboard' ? 'chessboard' : 'chessboard'}
                disabled={!nodeData.onNodeConfigChange}
                onChange={(event) => nodeData.onNodeConfigChange?.(node.id, 'boardKind', event.target.value)}
              >
                <option value="chessboard">Chessboard</option>
              </select>
            </label>
          ) : (
            <>
              <label className="node-config-field">
                <code>cameraModelKind</code>
                <select
                  className="nodrag nowheel"
                  value={node.config.cameraModelKind === 'pinhole' ? 'pinhole' : 'pinhole'}
                  disabled={!nodeData.onNodeConfigChange}
                  onChange={(event) => nodeData.onNodeConfigChange?.(node.id, 'cameraModelKind', event.target.value)}
                >
                  <option value="pinhole">Pinhole</option>
                </select>
              </label>
              <label className="node-config-field">
                <code>distortionKind</code>
                <select
                  className="nodrag nowheel"
                  value={node.config.distortionKind === 'none' ? 'none' : 'none'}
                  disabled={!nodeData.onNodeConfigChange}
                  onChange={(event) => nodeData.onNodeConfigChange?.(node.id, 'distortionKind', event.target.value)}
                >
                  <option value="none">None</option>
                </select>
              </label>
            </>
          )}
          <ScalarConfigFields
            nodeId={node.id}
            config={scalarConfig}
            onChange={nodeData.onNodeConfigChange}
          />
          <RuntimeOutputSummary output={nodeData.runtimeOutput} />
          <div className="node-actions">
            <button
              type="button"
              className="nodrag nowheel"
              disabled={runtimeState === 'disabled' || actionPending || !nodeData.onNodeAction}
              onClick={() => nodeData.onNodeAction?.(node.id, 'trigger')}
            >
              {actionPending ? '输出中…' : `输出${subject}`}
            </button>
          </div>
        </div>
      </section>
      {runtimeDiagnostic ? <div className="node-diagnostic-below" title={runtimeDiagnostic}>{runtimeDiagnostic}</div> : null}
    </div>
  );
}
