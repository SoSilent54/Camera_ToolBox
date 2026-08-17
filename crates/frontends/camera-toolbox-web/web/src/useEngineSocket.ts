/**
 * WebSocket 单例连接管理：前后端统一信封协议（见
 * .ai_doc/plans/20260817_132759__websocket_full_migration__ws_protocol_unification.md）。
 *
 * 信封：
 *   请求   { id, kind:"request",  path, payload }
 *   响应   { id, kind:"response", ok, payload? | error? }
 *   推送   { kind:"push", topic:"status"|"event"|"frame"|"frame_meta", payload }
 *   快照   { kind:"snapshot", payload:{ graph, statuses } }
 *
 * 连接单例，支持断线自动重连 + 心跳；wsRequest 按 id 配对响应（8s 超时）。
 * 未连接期间发起的请求统一入队，open 后一次性 flush；二进制帧（JPEG）由订阅方另行处理。
 */

const WS_URL = '/api/ws';
const WS_RECONNECT_DELAY_MS = 1000;
const WS_REQUEST_TIMEOUT_MS = 8000;
const HEARTBEAT_INTERVAL_MS = 15000;

export type EngineTopic = 'status' | 'event' | 'frame' | 'frame_meta';

export interface WsRequestEnvelope {
  id: number;
  kind: 'request';
  path: string;
  payload?: unknown;
}

export interface WsResponseEnvelope {
  id: number;
  kind: 'response';
  ok: boolean;
  payload?: unknown;
  error?: string;
}

export interface WsPushEnvelope {
  kind: 'push';
  topic: EngineTopic;
  payload: unknown;
}

export interface WsSnapshotEnvelope {
  kind: 'snapshot';
  payload: {
    graph?: unknown;
    statuses?: Array<{ nodeId: string; state: string; diagnostic: string }>;
  };
}

type WsInboundMessage = WsResponseEnvelope | WsPushEnvelope | WsSnapshotEnvelope;

type PendingRequest = {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  timer: ReturnType<typeof setTimeout>;
};

type TopicHandler = (payload: unknown) => void;

/** 快照处理器：连接就绪/重连后后端重推 snapshot 时回调（全量图 + 全量状态）。 */
type SnapshotHandler = (snapshot: WsSnapshotEnvelope) => void;

/** 二进制帧处理器：收到 JPEG 帧（Blob）时回调；订阅者按 nodeId 区分。 */
type FrameHandler = (blob: Blob) => void;

/** frame_meta 推送的节点 ID 头：{ nodeId, seq, width, height }。 */
interface FrameMetaPush {
  nodeId: string;
  seq?: number;
  width?: number;
  height?: number;
}

/** 待发送队列条目：连接就绪后统一 flush；超时由 pending 的 timer 兜底。 */
type QueuedRequest = {
  id: number;
  envelope: WsRequestEnvelope;
};

let socket: WebSocket | null = null;
let nextId = 1;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let heartbeatTimer: ReturnType<typeof setInterval> | null = null;
let manuallyClosed = false;
const pending = new Map<number, PendingRequest>();
/** 尚未发送的请求（连接未就绪期间入队，open 后 flush）。 */
const outbox: QueuedRequest[] = [];
const topicHandlers = new Map<EngineTopic, Set<TopicHandler>>();
const frameHandlers = new Map<string, Set<FrameHandler>>();
const snapshotHandlers = new Set<SnapshotHandler>();

/**
 * frame_meta 头与本帧二进制正文的配对约定：后端对每个 viewer 先推一条 frame_meta
 * （JSON，含 nodeId/seq/width/height），紧接一条独立的 JPEG 二进制帧。此处记住最近
 * 一次 frame_meta 的 nodeId，二进制消息到达时路由到该节点（单线程 bridge 循环保证
 * meta 先于其二进制帧）。
 */
let pendingFrameNodeId: string | null = null;

/** 订阅某个 topic 的推送；返回取消订阅函数。 */
export function subscribeTopic(topic: EngineTopic, handler: TopicHandler): () => void {
  let handlers = topicHandlers.get(topic);
  if (!handlers) {
    handlers = new Set();
    topicHandlers.set(topic, handlers);
  }
  handlers.add(handler);
  return () => {
    handlers.delete(handler);
  };
}

/** 订阅指定节点的二进制 JPEG 帧；返回取消订阅函数。 */
export function subscribeFrame(nodeId: string, handler: FrameHandler): () => void {
  let handlers = frameHandlers.get(nodeId);
  if (!handlers) {
    handlers = new Set();
    frameHandlers.set(nodeId, handlers);
  }
  handlers.add(handler);
  return () => {
    handlers.delete(handler);
  };
}

/** 连接就绪后的最近一次快照（snapshot 信封），重连后后端会重推补齐。 */
let latestSnapshot: WsSnapshotEnvelope | null = null;

/** 读取最近一次快照（无则不返回）。 */
export function getLatestSnapshot(): WsSnapshotEnvelope | null {
  return latestSnapshot;
}

/** 清空快照（例如卸载图后）。 */
export function clearSnapshot(): void {
  latestSnapshot = null;
}

/** 订阅 snapshot 信封（连接就绪/重连后后端重推）；返回取消订阅函数。 */
export function subscribeSnapshot(handler: SnapshotHandler): () => void {
  snapshotHandlers.add(handler);
  return () => {
    snapshotHandlers.delete(handler);
  };
}

function scheduleReconnect(): void {
  if (manuallyClosed || reconnectTimer) {
    return;
  }
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, WS_RECONNECT_DELAY_MS);
}

function stopHeartbeat(): void {
  if (heartbeatTimer) {
    clearInterval(heartbeatTimer);
    heartbeatTimer = null;
  }
}

function startHeartbeat(): void {
  stopHeartbeat();
  heartbeatTimer = setInterval(() => {
    if (socket && socket.readyState === WebSocket.OPEN) {
      // 心跳走 request-reply（heartbeat 不关心响应，仅靠 response 唤醒连接活性）。
      wsRequest('heartbeat', undefined).catch(() => {});
    }
  }, HEARTBEAT_INTERVAL_MS);
}

function flushPending(error: Error): void {
  for (const [, entry] of pending) {
    clearTimeout(entry.timer);
    entry.reject(error);
  }
  pending.clear();
  // 连接已中断，清空尚未发送的请求（其 pending 同样已被 reject）。
  outbox.length = 0;
}

/** 连接就绪后批量补发 outbox 中的请求。 */
function flushOutbox(): void {
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    return;
  }
  while (outbox.length > 0) {
    const item = outbox.shift();
    if (!item) {
      break;
    }
    socket.send(JSON.stringify(item.envelope));
  }
}

function dispatchMessage(raw: string): void {
  let message: WsInboundMessage;
  try {
    message = JSON.parse(raw) as WsInboundMessage;
  } catch {
    return;
  }

  if (message.kind === 'response') {
    const entry = pending.get(message.id);
    if (!entry) {
      return;
    }
    pending.delete(message.id);
    clearTimeout(entry.timer);
    if (message.ok) {
      entry.resolve(message.payload);
    } else {
      entry.reject(new Error(message.error ?? 'request failed'));
    }
    return;
  }

  if (message.kind === 'snapshot') {
    latestSnapshot = message;
    for (const handler of snapshotHandlers) {
      handler(message);
    }
    return;
  }

  if (message.kind === 'push') {
    // frame_meta 头：记录节点 ID，供紧随其后的二进制帧路由；同时照常分发给 topic 订阅者。
    if (message.topic === 'frame_meta') {
      const meta = message.payload as FrameMetaPush;
      if (meta && typeof meta.nodeId === 'string') {
        pendingFrameNodeId = meta.nodeId;
      }
    }
    const handlers = topicHandlers.get(message.topic);
    if (!handlers) {
      return;
    }
    for (const handler of handlers) {
      handler(message.payload);
    }
    return;
  }
}

/** 分发二进制帧：路由到最近一次 frame_meta 声明的节点；无归属则丢弃。 */
function dispatchBinaryFrame(data: Blob): void {
  const nodeId = pendingFrameNodeId;
  if (!nodeId) {
    return;
  }
  const handlers = frameHandlers.get(nodeId);
  if (!handlers) {
    return;
  }
  for (const handler of handlers) {
    handler(data);
  }
}

/** 建立（或重建）WS 连接；幂等，已连接或连接中时直接复用。 */
export function connect(): void {
  manuallyClosed = false;
  if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) {
    return;
  }

  const ws = new WebSocket(WS_URL);
  socket = ws;

  ws.onopen = () => {
    flushOutbox();
    startHeartbeat();
  };

  ws.onmessage = (event: MessageEvent) => {
    if (typeof event.data === 'string') {
      dispatchMessage(event.data);
      return;
    }
    // 二进制帧（JPEG）：Blob 或 ArrayBuffer 统一归一为 Blob，路由到 frame_meta 声明的节点。
    if (event.data instanceof Blob) {
      dispatchBinaryFrame(event.data);
    } else if (event.data instanceof ArrayBuffer) {
      dispatchBinaryFrame(new Blob([event.data]));
    } else if (ArrayBuffer.isView(event.data)) {
      // 视图（如 Uint8Array）的 .buffer 类型为 ArrayBufferLike；取其底层字节切片。
      const view = event.data;
      const buffer = (view.buffer as ArrayBuffer).slice(
        view.byteOffset,
        view.byteOffset + view.byteLength,
      );
      dispatchBinaryFrame(new Blob([buffer]));
    }
  };

  ws.onerror = () => {
    // onclose 随后触发，此处仅记录，由 onclose 统一重连。
  };

  ws.onclose = () => {
    stopHeartbeat();
    flushPending(new Error('websocket disconnected'));
    socket = null;
    scheduleReconnect();
  };
}

/** 关闭连接并停止重连（应用卸载时调用一次）。 */
export function disconnect(): void {
  manuallyClosed = true;
  stopHeartbeat();
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  flushPending(new Error('websocket closed'));
  if (socket) {
    socket.close();
    socket = null;
  }
}

/** 发送 request 信封并按 id 配对 response；超时 reject。 */
export function wsRequest(
  path: string,
  payload?: unknown,
  timeoutMs: number = WS_REQUEST_TIMEOUT_MS,
): Promise<unknown> {
  connect();

  const id = nextId;
  nextId += 1;

  const envelope: WsRequestEnvelope = { id, kind: 'request', path, payload };

  return new Promise<unknown>((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      removeFromOutbox(id);
      reject(new Error(`request timed out: ${path}`));
    }, timeoutMs);

    pending.set(id, { resolve, reject, timer });

    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(envelope));
    } else {
      // 连接尚未就绪：入队，等待 open 后统一 flush（超时由 timer 兜底）。
      outbox.push({ id, envelope });
    }
  });
}

/** 从 outbox 移除指定 id（超时后清理避免补发已超时的请求）。 */
function removeFromOutbox(id: number): void {
  for (let i = 0; i < outbox.length; i += 1) {
    if (outbox[i].id === id) {
      outbox.splice(i, 1);
      return;
    }
  }
}

/** 应用启动时自动连接（幂等）。 */
connect();
