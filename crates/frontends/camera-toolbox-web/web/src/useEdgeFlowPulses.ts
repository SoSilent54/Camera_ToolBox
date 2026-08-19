import { useEffect, useMemo, useRef, useState } from 'react';
import { subscribeTopic } from './useEngineSocket';
import type { EdgePulseView } from './workflow';

export const EDGE_FLOW_PULSE_DURATION_MS = 700;

const EDGE_FLOW_PULSE_RETENTION_MS = EDGE_FLOW_PULSE_DURATION_MS + 150;
const MAX_EDGE_PULSES = 96;

type FlowPulsePayload = {
  pulses?: unknown;
};

type FlowPulseMessage = {
  edgeId: string;
  packetKind: string;
  sequence?: number;
};

function parseFlowPulse(value: unknown): FlowPulseMessage | null {
  if (!value || typeof value !== 'object') {
    return null;
  }
  const record = value as Record<string, unknown>;
  if (typeof record.edgeId !== 'string' || !record.edgeId || typeof record.packetKind !== 'string') {
    return null;
  }
  return {
    edgeId: record.edgeId,
    packetKind: record.packetKind,
    sequence: typeof record.sequence === 'number' && Number.isFinite(record.sequence) ? record.sequence : undefined,
  };
}

/** 订阅后端边级 flow pulse，并维护每条边的短生命周期动画队列。 */
export function useEdgeFlowPulses(): ReadonlyMap<string, readonly EdgePulseView[]> {
  const nextIdRef = useRef(1);
  const pulsesRef = useRef<Map<string, EdgePulseView[]>>(new Map());
  const [version, setVersion] = useState(0);

  useEffect(() => {
    const prune = (now: number) => {
      let changed = false;
      const next = new Map<string, EdgePulseView[]>();
      for (const [edgeId, pulses] of pulsesRef.current) {
        const alive = pulses.filter((pulse) => now - pulse.startedAt <= EDGE_FLOW_PULSE_RETENTION_MS);
        if (alive.length > 0) {
          next.set(edgeId, alive);
        }
        if (alive.length !== pulses.length) {
          changed = true;
        }
      }
      if (changed) {
        pulsesRef.current = next;
        setVersion((current) => current + 1);
      }
    };

    const timer = window.setInterval(() => prune(performance.now()), 250);
    const unsubscribe = subscribeTopic('flow', (payload) => {
      const body = payload as FlowPulsePayload;
      if (!Array.isArray(body?.pulses)) {
        return;
      }
      const now = performance.now();
      const next = new Map(pulsesRef.current);
      let changed = false;
      for (const raw of body.pulses) {
        const pulse = parseFlowPulse(raw);
        if (!pulse) {
          continue;
        }
        const current = next.get(pulse.edgeId) ?? [];
        const alive = current.filter((item) => now - item.startedAt <= EDGE_FLOW_PULSE_RETENTION_MS);
        if (alive.length !== current.length) {
          changed = true;
        }
        if (alive.length >= MAX_EDGE_PULSES) {
          // 高频 RTSP 只丢新的动画事件，不能裁掉仍在运动的圆点，否则会提前消失。
          next.set(pulse.edgeId, alive);
          continue;
        }
        const view: EdgePulseView = {
          id: `${pulse.edgeId}:${pulse.sequence ?? 'na'}:${now}:${nextIdRef.current}`,
          edgeId: pulse.edgeId,
          packetKind: pulse.packetKind,
          sequence: pulse.sequence,
          startedAt: now,
        };
        nextIdRef.current += 1;
        next.set(pulse.edgeId, [...alive, view]);
        changed = true;
      }
      if (changed) {
        pulsesRef.current = next;
        setVersion((current) => current + 1);
      }
    });

    return () => {
      unsubscribe();
      window.clearInterval(timer);
    };
  }, []);

  return useMemo(() => new Map(pulsesRef.current), [version]);
}
