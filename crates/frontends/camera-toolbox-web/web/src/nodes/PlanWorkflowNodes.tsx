import { useEffect, useState, type ReactNode } from 'react';
import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { configText } from '../nodeConfig';
import { registerSshPassword } from '../workflow';
import { NodeHeader, PortHandles, RuntimeOutputSummary } from './shared';

type ExtractorOutput = { id: string; pointer: string; type: PrimitiveType };
type PrimitiveType = 'bool' | 'u8' | 'i8' | 'u16' | 'i16' | 'u32' | 'i32' | 'u64' | 'i64' | 'f32' | 'f64' | 'str' | 'bytes';

const PRIMITIVE_TYPES: readonly PrimitiveType[] = ['bool', 'u8', 'i8', 'u16', 'i16', 'u32', 'i32', 'u64', 'i64', 'f32', 'f64', 'str', 'bytes'];

function Shell({ data, selected, children }: { data: NodeProps['data']; selected?: boolean; children: ReactNode }) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  return <div className="workflow-node-shell"><section className={`workflow-node remote-node ${selected ? 'selected' : ''}`}>
    <NodeHeader node={node} runtimeState={nodeData.runtimeState} />
    <PortHandles node={node} />
    <div className="node-body">{children}<RuntimeOutputSummary output={nodeData.runtimeOutput} /></div>
  </section>{nodeData.runtimeDiagnostic ? <div className="node-diagnostic-below">{nodeData.runtimeDiagnostic}</div> : null}</div>;
}


/** SSH 节点只持久化无密钥配置；Connect 原子登记密码、写入配置并建立会话。 */
export function SshConnectionNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const savedHost = configText(node, 'host', '');
  const savedUsername = configText(node, 'username', 'root');
  const savedCredentialRef = configText(node, 'credentialRef', '');
  const [host, setHost] = useState(savedHost);
  const [username, setUsername] = useState(savedUsername);
  const [credentialRef, setCredentialRef] = useState(savedCredentialRef);
  const [password, setPassword] = useState('');
  const [connecting, setConnecting] = useState(false);
  const [message, setMessage] = useState<string>();
  useEffect(() => setHost(savedHost), [savedHost]);
  useEffect(() => setUsername(savedUsername), [savedUsername]);
  useEffect(() => setCredentialRef(savedCredentialRef), [savedCredentialRef]);
  const valid = host.trim().length > 0
    && username.trim().length > 0
    && (password.length > 0 || credentialRef.startsWith('session:'));
  const connect = async () => {
    if (!valid || !nodeData.onNodeConfigPatch || !nodeData.onNodeAction) return;
    setConnecting(true);
    setMessage(undefined);
    try {
      let nextCredentialRef = credentialRef;
      if (password.length > 0) {
        const result = await registerSshPassword(node.id, password);
        nextCredentialRef = result.credentialRef;
        setCredentialRef(nextCredentialRef);
        setPassword('');
      }
      await nodeData.onNodeConfigPatch(node.id, {
        host: host.trim(), port: '22', username: username.trim(), credentialRef: nextCredentialRef,
      });
      nodeData.onNodeAction(node.id, 'connect');
    } catch (error) {
      setMessage(String(error));
    } finally {
      setConnecting(false);
    }
  };
  const connectDisabled = !valid || !nodeData.onNodeConfigPatch || !nodeData.onNodeAction || nodeData.actionPending || connecting;
  return <Shell data={data} selected={selected}>
    <label className="node-config-field"><code>IP</code><input className="nodrag nowheel" value={host} onChange={(event) => setHost(event.target.value)} /></label>
    <label className="node-config-field"><code>User</code><input className="nodrag nowheel" value={username} onChange={(event) => setUsername(event.target.value)} /></label>
    <label className="node-config-field"><code>Password</code><input className="nodrag nowheel" type="password" value={password} placeholder={credentialRef ? 'saved for this process' : 'not saved'} autoComplete="current-password" onChange={(event) => setPassword(event.target.value)} /></label>
    <div className="node-actions"><button className="nodrag nowheel" type="button" disabled={connectDisabled} onClick={connect}>{connecting || nodeData.actionPending ? 'Connecting…' : 'Connect'}</button></div>
    {message ? <span className="node-hint">{message}</span> : null}
  </Shell>;
}

function configuredOutputs(config: Record<string, unknown>): ExtractorOutput[] {
  const raw = config.outputs;
  if (!Array.isArray(raw)) return [];
  return raw.map((entry, index) => {
    const value = entry && typeof entry === 'object' ? entry as Record<string, unknown> : {};
    const type = typeof value.type === 'string' && PRIMITIVE_TYPES.includes(value.type as PrimitiveType) ? value.type as PrimitiveType : 'f64';
    return {
      id: typeof value.id === 'string' ? value.id : `field${index + 1}`,
      pointer: typeof value.pointer === 'string' ? value.pointer : '/fields/0',
      type,
    };
  });
}

export function StructuredFieldExtractorNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const persisted = configuredOutputs(node.config);
  const [outputs, setOutputs] = useState<ExtractorOutput[]>(persisted);
  useEffect(() => setOutputs(persisted), [JSON.stringify(persisted)]);
  const update = (index: number, key: keyof ExtractorOutput, value: string) => setOutputs((current) => current.map((output, currentIndex) => currentIndex === index ? { ...output, [key]: value } as ExtractorOutput : output));
  const save = () => nodeData.onPlanInterfaceChange?.(node.id, { outputs });
  return <Shell data={data} selected={selected}>
    <span className="node-hint">Each output is an RFC6901 pointer to a complete primitive datum.</span>
    {outputs.map((output, index) => <div className="node-config-fields" key={index}>
      <label className="node-config-field"><code>Output id</code><input className="nodrag nowheel" value={output.id} onChange={(event) => update(index, 'id', event.target.value)} /></label>
      <label className="node-config-field"><code>JSON pointer</code><input className="nodrag nowheel" value={output.pointer} onChange={(event) => update(index, 'pointer', event.target.value)} /></label>
      <label className="node-config-field"><code>Primitive type</code><select className="nodrag nowheel" value={output.type} onChange={(event) => update(index, 'type', event.target.value)}>{PRIMITIVE_TYPES.map((type) => <option key={type} value={type}>{type}</option>)}</select></label>
      <button className="nodrag nowheel" type="button" onClick={() => setOutputs((current) => current.filter((_output, currentIndex) => currentIndex !== index))}>Remove</button>
    </div>)}
    <div className="node-actions"><button className="nodrag nowheel" type="button" onClick={() => setOutputs((current) => [...current, { id: `field${current.length + 1}`, pointer: '/fields/0', type: 'f64' }])}>Add field</button><button className="nodrag nowheel" type="button" disabled={!nodeData.onPlanInterfaceChange} onClick={save}>Apply interface changes</button></div>
  </Shell>;
}

/** Builder 配置严格对应后端的 mapMode/mapId|mapYaml/bus 单一路径。 */
export function I2cTaskBuilderNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const savedMode = configText(node, 'mapMode', 'builtin') === 'custom' ? 'custom' : 'builtin';
  const savedMapId = configText(node, 'mapId', 'yg-stereo-p24c64g-v1');
  const savedMapYaml = configText(node, 'mapYaml', '');
  const [mapMode, setMapMode] = useState<'builtin' | 'custom'>(savedMode);
  const [mapId, setMapId] = useState(savedMapId);
  const [mapYaml, setMapYaml] = useState(savedMapYaml);
  const [bus, setBus] = useState(configText(node, 'bus', '0'));
  useEffect(() => setMapMode(savedMode), [savedMode]);
  useEffect(() => setMapId(savedMapId), [savedMapId]);
  useEffect(() => setMapYaml(savedMapYaml), [savedMapYaml]);
  useEffect(() => setBus(configText(node, 'bus', '0')), [node.config.bus]);
  const parsedBus = Number(bus);
  const busValid = Number.isInteger(parsedBus) && parsedBus >= 0 && parsedBus <= 0xffff_ffff;
  const mapValid = mapMode === 'builtin' ? mapId.trim().length > 0 : mapYaml.trim().length > 0;
  const configDirty = mapMode !== savedMode || mapId !== savedMapId || mapYaml !== savedMapYaml || bus !== configText(node, 'bus', '0');
  const actionDisabled = nodeData.runtimeState === 'disabled' || Boolean(nodeData.actionPending) || !nodeData.onNodeAction || !busValid || !mapValid || configDirty;
  const readReport = i2cReadReport(nodeData.runtimeOutput);
  const report = executionReport(nodeData.runtimeOutput);
  const save = () => {
    if (!busValid || !mapValid) return;
    const config = mapMode === 'builtin'
      ? { mapMode, mapId: mapId.trim(), bus: parsedBus }
      : { mapMode, mapYaml, bus: parsedBus };
    nodeData.onNodeConfigReplace?.(node.id, config);
  };
  return <Shell data={data} selected={selected}>
    <span className="node-hint">Inputs: packet, serial.number, connection. Outputs: readReport, report.</span>
    <label className="node-config-field"><code>Map mode</code><select className="nodrag nowheel" value={mapMode} onChange={(event) => setMapMode(event.target.value as 'builtin' | 'custom')}><option value="builtin">Built-in map</option><option value="custom">Custom YAML</option></select></label>
    {mapMode === 'builtin'
      ? <label className="node-config-field"><code>Map ID</code><input className="nodrag nowheel" value={mapId} onChange={(event) => setMapId(event.target.value)} /></label>
      : <label className="node-config-field"><code>Map YAML</code><textarea className="nodrag nowheel" value={mapYaml} spellCheck={false} onChange={(event) => setMapYaml(event.target.value)} /></label>}
    <label className="node-config-field"><code>Bus</code><input className="nodrag nowheel" type="number" min="0" max="4294967295" value={bus} onChange={(event) => setBus(event.target.value)} /></label>
    {!busValid ? <span className="node-hint">Bus must be an unsigned 32-bit integer.</span> : null}
    {mapMode === 'custom' ? <span className="node-hint">YAML is submitted verbatim; leading lines and indentation are retained so compiler line/column diagnostics match this editor.</span> : null}
    <div className="node-actions"><button className="nodrag nowheel" type="button" disabled={!busValid || !mapValid || !configDirty || !nodeData.onNodeConfigReplace} onClick={save}>Apply map configuration</button><button className="nodrag nowheel" type="button" disabled={actionDisabled} onClick={() => nodeData.onNodeAction?.(node.id, 'read')}>{nodeData.actionPending ? 'Reading…' : 'Read'}</button><button className="nodrag nowheel" type="button" disabled={actionDisabled} onClick={() => nodeData.onNodeAction?.(node.id, 'write')}>{nodeData.actionPending ? 'Writing…' : 'Write'}</button></div>
    {readReport ? <div className="node-config-fields"><strong>Read report</strong><span className="node-hint">Image SHA-256: {readReport.imageSha256}</span><span className="node-hint">Byte length: {readReport.byteLength}</span><span className="node-hint">Valid: {readReport.valid}</span><span className="node-hint">Error: {readReport.error}</span></div> : null}
    {report ? <div className="node-config-fields"><strong>Execution report</strong><span className="node-hint">Final verification: {report.finalVerified}</span><span className="node-hint">Pages: {report.pageCount}</span><span className="node-hint">Error: {report.error}</span>{report.pages.map((page) => <span className="node-hint" key={page.offset}>Offset {page.offset}: expected {page.expectedHex}; readback {page.readbackHex ?? 'unavailable'}; error {page.error ?? 'none'}</span>)}</div> : null}
  </Shell>;
}

function recordValue(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
}

function stringValue(value: unknown): string | undefined {
  return typeof value === 'string' && value.length > 0 ? value : undefined;
}

function numberValue(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : undefined;
}

function i2cReadReport(output: unknown): { imageSha256: string; byteLength: string; valid: string; error: string } | undefined {
  const report = recordValue(output);
  if (!report || report.type !== 'i2c.read-report.v1') return undefined;
  return {
    imageSha256: stringValue(report.imageSha256) ?? 'unavailable',
    byteLength: typeof report.byteLength === 'number' ? String(report.byteLength) : 'unknown',
    valid: report.valid === true ? 'yes' : 'no',
    error: stringValue(report.error) ?? 'none',
  };
}

function executionReport(output: unknown): { finalVerified: string; pageCount: string; error: string; pages: Array<{ offset: number; expectedHex: string; readbackHex?: string; error?: string }> } | undefined {
  const report = recordValue(output);
  if (!report || report.type !== 'i2c.execution-report.v1') return undefined;
  const pages = Array.isArray(report.pages) ? report.pages.flatMap((page) => {
    const pageRecord = recordValue(page);
    const offset = pageRecord && numberValue(pageRecord.offset);
    const expectedHex = pageRecord && stringValue(pageRecord.expectedHex);
    const readbackHex = pageRecord && stringValue(pageRecord.readbackHex);
    const error = pageRecord && stringValue(pageRecord.error);
    return offset === undefined || !expectedHex ? [] : [{ offset, expectedHex, ...(readbackHex ? { readbackHex } : {}), ...(error ? { error } : {}) }];
  }) : [];
  return {
    finalVerified: report.finalVerified === true ? 'passed' : 'not verified',
    pageCount: typeof report.pageCount === 'number' ? String(report.pageCount) : 'unknown',
    error: stringValue(report.error) ?? 'none',
    pages,
  };
}
