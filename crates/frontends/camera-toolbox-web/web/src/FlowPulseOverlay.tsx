import { useEffect, useRef } from 'react';
import { ViewportPortal } from '@xyflow/react';
import { useFlowEdgePaths } from './FlowPulseEdge';
import { portKindColor } from './nodes/shared';
import { EDGE_FLOW_PULSE_DURATION_MS } from './useEdgeFlowPulses';
import type { EdgePulseView } from './workflow';

type FlowPulseOverlayProps = {
  pulses: ReadonlyMap<string, readonly EdgePulseView[]>;
};

type OverlayPulse = {
  pulse: EdgePulseView;
  path: string;
};

type OverlayPulseMarkerProps = {
  pulse: OverlayPulse;
};

function OverlayPulseMarker({ pulse }: OverlayPulseMarkerProps) {
  const animationRef = useRef<SVGAnimateMotionElement | null>(null);

  useEffect(() => {
    // overlay 中的 pulse 也是动态插入；显式 begin 可避免直接落到终点。
    animationRef.current?.beginElement();
  }, []);

  const color = portKindColor(pulse.pulse.packetKind);
  return (
    <circle
      r={4}
      className="edge-flow-pulse"
      style={{ fill: color, filter: `drop-shadow(0 0 6px ${color})` }}
    >
      <animateMotion
        ref={animationRef}
        path={pulse.path}
        dur={`${EDGE_FLOW_PULSE_DURATION_MS}ms`}
        begin="indefinite"
        fill="freeze"
      />
    </circle>
  );
}

/** 单层脉冲 overlay：edge 组件只注册 path，所有动画节点集中在一个 SVG 层。 */
export function FlowPulseOverlay({ pulses }: FlowPulseOverlayProps) {
  const edgePaths = useFlowEdgePaths();
  const overlayPulses: OverlayPulse[] = [];

  for (const [edgeId, edgePulses] of pulses) {
    const path = edgePaths.get(edgeId);
    if (!path) {
      continue;
    }
    for (const pulse of edgePulses) {
      overlayPulses.push({ pulse, path });
    }
  }

  if (overlayPulses.length === 0) {
    return null;
  }

  return (
    <ViewportPortal>
      <svg
        className="edge-flow-overlay"
        aria-hidden="true"
        style={{
          position: 'absolute',
          left: 0,
          top: 0,
          width: 1,
          height: 1,
          overflow: 'visible',
          pointerEvents: 'none',
        }}
      >
        {overlayPulses.map((pulse) => <OverlayPulseMarker key={pulse.pulse.id} pulse={pulse} />)}
      </svg>
    </ViewportPortal>
  );
}
