import { useEffect, useState, type ReactNode } from 'react';
import type { NodeProps } from '@xyflow/react';
import type { FlowNodeData } from '../workflow';
import { configText } from '../nodeConfig';
import { registerSshPassword } from '../workflow';
import { NodeHeader, PortHandles, RuntimeOutputSummary } from './shared';

type ExtractorOutput = { id: string; pointer: string };
type EncoderInput = { id: string; name: string; required: boolean; offset: string; byteLength: string; encoding: string };

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
  const valid = host.trim().length > 0 && username.trim().length > 0 && (password.length > 0 || credentialRef.startsWith('session:'));
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
      await nodeData.onNodeConfigPatch(node.id, { host: host.trim(), port: '22', username: username.trim(), credentialRef: nextCredentialRef });
      nodeData.onNodeAction(node.id, 'connect');
    } catch (error) {
      setMessage(String(error));
    } finally {
      setConnecting(false);
    }
  };
  return <Shell data={data} selected={selected}>
    <label className="node-config-field"><code>IP</code><input className="nodrag nowheel" value={host} onChange={(event) => setHost(event.target.value)} /></label>
    <label className="node-config-field"><code>User</code><input className="nodrag nowheel" value={username} onChange={(event) => setUsername(event.target.value)} /></label>
    <label className="node-config-field"><code>Password</code><input className="nodrag nowheel" type="password" value={password} placeholder={credentialRef ? 'saved for this process' : 'not saved'} autoComplete="current-password" onChange={(event) => setPassword(event.target.value)} /></label>
    <div className="node-actions"><button className="nodrag nowheel" type="button" disabled={!valid || !nodeData.onNodeConfigPatch || !nodeData.onNodeAction || nodeData.actionPending || connecting} onClick={connect}>{connecting || nodeData.actionPending ? 'Connecting…' : 'Connect'}</button></div>
    {message ? <span className="node-hint">{message}</span> : null}
  </Shell>;
}

function configuredOutputs(config: Record<string, unknown>): ExtractorOutput[] {
  return Array.isArray(config.outputs) ? config.outputs.map((entry, index) => {
    const value = entry && typeof entry === 'object' ? entry as Record<string, unknown> : {};
    return { id: typeof value.id === 'string' ? value.id : `field${index + 1}`, pointer: typeof value.pointer === 'string' ? value.pointer : '/fields/0' };
  }) : [];
}

export function StructuredFieldExtractorNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const persisted = configuredOutputs(node.config);
  const [outputs, setOutputs] = useState<ExtractorOutput[]>(persisted);
  useEffect(() => setOutputs(persisted), [JSON.stringify(persisted)]);
  const update = (index: number, key: keyof ExtractorOutput, value: string) => setOutputs((current) => current.map((output, currentIndex) => currentIndex === index ? { ...output, [key]: value } : output));
  return <Shell data={data} selected={selected}>
    <span className="node-hint">Each output is an RFC6901 pointer to one complete Datum. Primitive type is validated from datum content.</span>
    {outputs.map((output, index) => <div className="node-config-fields" key={index}>
      <label className="node-config-field"><code>Output id</code><input className="nodrag nowheel" value={output.id} onChange={(event) => update(index, 'id', event.target.value)} /></label>
      <label className="node-config-field"><code>JSON pointer</code><input className="nodrag nowheel" value={output.pointer} onChange={(event) => update(index, 'pointer', event.target.value)} /></label>
      <button className="nodrag nowheel" type="button" onClick={() => setOutputs((current) => current.filter((_output, currentIndex) => currentIndex !== index))}>Remove</button>
    </div>)}
    <div className="node-actions"><button className="nodrag nowheel" type="button" onClick={() => setOutputs((current) => [...current, { id: `field${current.length + 1}`, pointer: '/fields/0' }])}>Add field</button><button className="nodrag nowheel" type="button" disabled={!nodeData.onPlanInterfaceChange} onClick={() => nodeData.onPlanInterfaceChange?.(node.id, { outputs })}>Apply interface changes</button></div>
  </Shell>;
}

function configuredInputs(config: Record<string, unknown>): EncoderInput[] {
  return Array.isArray(config.inputs) ? config.inputs.map((entry, index) => {
    const value = entry && typeof entry === 'object' ? entry as Record<string, unknown> : {};
    return { id: typeof value.id === 'string' ? value.id : `field${index + 1}`, name: typeof value.name === 'string' ? value.name : '', required: value.required !== false, offset: String(value.offset ?? 0), byteLength: String(value.byteLength ?? 1), encoding: typeof value.encoding === 'string' ? value.encoding : 'ascii' };
  }) : [];
}

/** 编码器只声明字节映射和 task；不会保存连接或执行 I²C。 */
export function I2cFieldEncoderNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const savedMode = configText(node, 'mapMode', 'builtin') === 'custom' ? 'custom' : 'builtin';
  const [mapMode, setMapMode] = useState<'builtin' | 'custom'>(savedMode);
  const [mapId, setMapId] = useState(configText(node, 'mapId', 'yg-stereo-p24c64g-v1'));
  const [mapYaml, setMapYaml] = useState(configText(node, 'mapYaml', ''));
  const [bus, setBus] = useState(configText(node, 'bus', '0'));
  const [address, setAddress] = useState(configText(node, 'address', ''));
  const [addressWidthBytes, setAddressWidthBytes] = useState(configText(node, 'addressWidthBytes', '2'));
  const [pageSizeBytes, setPageSizeBytes] = useState(configText(node, 'pageSizeBytes', '32'));
  const [writeCycleMs, setWriteCycleMs] = useState(configText(node, 'writeCycleMs', '5'));
  const [operation, setOperation] = useState(configText(node, 'operation', 'guarded_write'));
  const persisted = configuredInputs(node.config);
  const [inputs, setInputs] = useState<EncoderInput[]>(persisted);
  useEffect(() => { setMapMode(savedMode); setMapId(configText(node, 'mapId', 'yg-stereo-p24c64g-v1')); setMapYaml(configText(node, 'mapYaml', '')); setBus(configText(node, 'bus', '0')); setAddress(configText(node, 'address', '')); setAddressWidthBytes(configText(node, 'addressWidthBytes', '2')); setPageSizeBytes(configText(node, 'pageSizeBytes', '32')); setWriteCycleMs(configText(node, 'writeCycleMs', '5')); setOperation(configText(node, 'operation', 'guarded_write')); }, [node.config, savedMode]);
  useEffect(() => setInputs(persisted), [JSON.stringify(persisted)]);
  const asInteger = (value: string) => Number.isInteger(Number(value)) ? Number(value) : undefined;
  const targetValid = asInteger(bus) !== undefined && Number(bus) >= 0 && Number(bus) <= 0xffff_ffff && asInteger(address) !== undefined && Number(address) >= 0x03 && Number(address) <= 0x7f && (addressWidthBytes === '1' || addressWidthBytes === '2') && asInteger(pageSizeBytes) !== undefined && Number(pageSizeBytes) >= 1 && Number(pageSizeBytes) <= 0xffff && asInteger(writeCycleMs) !== undefined && Number(writeCycleMs) >= 0 && Number(writeCycleMs) <= 0xffff;
  const fieldsValid = inputs.every((input) => input.id.trim() && input.name.trim() && asInteger(input.offset) !== undefined && Number(input.offset) >= 0 && asInteger(input.byteLength) !== undefined && Number(input.byteLength) >= 1 && input.encoding.trim());
  const save = () => {
    if (!targetValid || !fieldsValid) return;
    const base = { bus: Number(bus), address: Number(address), addressWidthBytes: Number(addressWidthBytes), pageSizeBytes: Number(pageSizeBytes), writeCycleMs: Number(writeCycleMs), operation, inputs: inputs.map((input) => ({ ...input, id: input.id.trim(), name: input.name.trim(), offset: Number(input.offset), byteLength: Number(input.byteLength) })) };
    nodeData.onNodeConfigReplace?.(node.id, mapMode === 'builtin' ? { ...base, mapMode, mapId: mapId.trim() } : { ...base, mapMode, mapYaml });
  };
  const updateInput = (index: number, key: keyof EncoderInput, value: string | boolean) => setInputs((current) => current.map((input, currentIndex) => currentIndex === index ? { ...input, [key]: value } : input));
  return <Shell data={data} selected={selected}>
    <span className="node-hint">Inputs are generic FieldData. The encoder validates datum names, encodes bytes, applies checksum finalizers, then emits one task PacketData.</span>
    <label className="node-config-field"><code>Map mode</code><select className="nodrag nowheel" value={mapMode} onChange={(event) => setMapMode(event.target.value as 'builtin' | 'custom')}><option value="builtin">Built-in map</option><option value="custom">Custom YAML</option></select></label>
    {mapMode === 'builtin' ? <label className="node-config-field"><code>Map ID</code><input className="nodrag nowheel" value={mapId} onChange={(event) => setMapId(event.target.value)} /></label> : <label className="node-config-field"><code>Map YAML</code><textarea className="nodrag nowheel" value={mapYaml} spellCheck={false} onChange={(event) => setMapYaml(event.target.value)} /></label>}
    <label className="node-config-field"><code>Bus</code><input className="nodrag nowheel" type="number" min="0" value={bus} onChange={(event) => setBus(event.target.value)} /></label>
    <label className="node-config-field"><code>Address (7-bit)</code><input className="nodrag nowheel" type="number" min="3" max="127" value={address} onChange={(event) => setAddress(event.target.value)} /></label>
    <label className="node-config-field"><code>Address width</code><select className="nodrag nowheel" value={addressWidthBytes} onChange={(event) => setAddressWidthBytes(event.target.value)}><option value="1">1 byte</option><option value="2">2 bytes</option></select></label>
    <label className="node-config-field"><code>Page size</code><input className="nodrag nowheel" type="number" min="1" max="65535" value={pageSizeBytes} onChange={(event) => setPageSizeBytes(event.target.value)} /></label>
    <label className="node-config-field"><code>Write cycle (ms)</code><input className="nodrag nowheel" type="number" min="0" max="65535" value={writeCycleMs} onChange={(event) => setWriteCycleMs(event.target.value)} /></label>
    <label className="node-config-field"><code>Operation</code><select className="nodrag nowheel" value={operation} onChange={(event) => setOperation(event.target.value)}><option value="guarded_write">Guarded write</option><option value="read">Read</option></select></label>
    {inputs.map((input, index) => <div className="node-config-fields" key={index}>
      <label className="node-config-field"><code>Port id</code><input className="nodrag nowheel" value={input.id} onChange={(event) => updateInput(index, 'id', event.target.value)} /></label><label className="node-config-field"><code>Datum name</code><input className="nodrag nowheel" value={input.name} onChange={(event) => updateInput(index, 'name', event.target.value)} /></label><label className="node-config-field"><code>Offset</code><input className="nodrag nowheel" type="number" min="0" value={input.offset} onChange={(event) => updateInput(index, 'offset', event.target.value)} /></label><label className="node-config-field"><code>Byte length</code><input className="nodrag nowheel" type="number" min="1" value={input.byteLength} onChange={(event) => updateInput(index, 'byteLength', event.target.value)} /></label><label className="node-config-field"><code>Encoding</code><input className="nodrag nowheel" value={input.encoding} onChange={(event) => updateInput(index, 'encoding', event.target.value)} /></label><label className="node-config-field"><code>Required</code><input className="nodrag nowheel" type="checkbox" checked={input.required} onChange={(event) => updateInput(index, 'required', event.target.checked)} /></label><button className="nodrag nowheel" type="button" onClick={() => setInputs((current) => current.filter((_input, currentIndex) => currentIndex !== index))}>Remove</button>
    </div>)}
    {!targetValid || !fieldsValid ? <span className="node-hint">Set a valid 7-bit target and complete every field mapping before applying the encoder configuration.</span> : null}
    <div className="node-actions"><button className="nodrag nowheel" type="button" onClick={() => setInputs((current) => [...current, { id: `field${current.length + 1}`, name: '', required: true, offset: '0', byteLength: '1', encoding: 'ascii' }])}>Add field</button><button className="nodrag nowheel" type="button" disabled={!targetValid || !fieldsValid || !nodeData.onNodeConfigReplace} onClick={save}>Apply encoder configuration</button></div>
  </Shell>;
}

/** 执行器只接收已编译 task 与进程内 SSH connection，并以一个 Execute 动作完成原子操作。 */
export function I2cTaskExecutorNode({ data, selected }: NodeProps) {
  const nodeData = data as FlowNodeData;
  const node = nodeData.workflowNode;
  const readReport = i2cReadReport(nodeData.runtimeOutput);
  const report = executionReport(nodeData.runtimeOutput);
  const disabled = nodeData.runtimeState === 'disabled' || Boolean(nodeData.actionPending) || !nodeData.onNodeAction;
  return <Shell data={data} selected={selected}>
    <span className="node-hint">Inputs: task PacketData and SSH connection. Execute never parses maps, business fields, or checksums.</span>
    <div className="node-actions"><button className="nodrag nowheel" type="button" disabled={disabled} onClick={() => nodeData.onNodeAction?.(node.id, 'execute')}>{nodeData.actionPending ? 'Executing…' : 'Execute'}</button></div>
    {readReport ? <div className="node-config-fields"><strong>Read report</strong><span className="node-hint">Image SHA-256: {readReport.imageSha256}</span><span className="node-hint">Byte length: {readReport.byteLength}</span><span className="node-hint">Valid: {readReport.valid}</span><span className="node-hint">Error: {readReport.error}</span></div> : null}
    {report ? <div className="node-config-fields"><strong>Execution report</strong><span className="node-hint">Final verification: {report.finalVerified}</span><span className="node-hint">Pages: {report.pageCount}</span><span className="node-hint">Error: {report.error}</span>{report.pages.map((page) => <span className="node-hint" key={page.offset}>Offset {page.offset}: expected {page.expectedHex}; readback {page.readbackHex ?? 'unavailable'}; error {page.error ?? 'none'}</span>)}</div> : null}
  </Shell>;
}

function recordValue(value: unknown): Record<string, unknown> | undefined { return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : undefined; }
function stringValue(value: unknown): string | undefined { return typeof value === 'string' && value.length > 0 ? value : undefined; }
function numberValue(value: unknown): number | undefined { return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : undefined; }
function i2cReadReport(output: unknown): { imageSha256: string; byteLength: string; valid: string; error: string } | undefined { const report = recordValue(output); return !report || report.type !== 'i2c.read-report.v1' ? undefined : { imageSha256: stringValue(report.imageSha256) ?? 'unavailable', byteLength: typeof report.byteLength === 'number' ? String(report.byteLength) : 'unknown', valid: report.valid === true ? 'yes' : 'no', error: stringValue(report.error) ?? 'none' }; }
function executionReport(output: unknown): { finalVerified: string; pageCount: string; error: string; pages: Array<{ offset: number; expectedHex: string; readbackHex?: string; error?: string }> } | undefined { const report = recordValue(output); if (!report || report.type !== 'i2c.execution-report.v1') return undefined; const pages = Array.isArray(report.pages) ? report.pages.flatMap((page) => { const value = recordValue(page); const offset = value && numberValue(value.offset); const expectedHex = value && stringValue(value.expectedHex); const readbackHex = value && stringValue(value.readbackHex); const error = value && stringValue(value.error); return offset === undefined || !expectedHex ? [] : [{ offset, expectedHex, ...(readbackHex ? { readbackHex } : {}), ...(error ? { error } : {}) }]; }) : []; return { finalVerified: report.finalVerified === true ? 'passed' : 'not verified', pageCount: typeof report.pageCount === 'number' ? String(report.pageCount) : 'unknown', error: stringValue(report.error) ?? 'none', pages }; }
