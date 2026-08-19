import { useEffect, useRef } from 'react';
import { BaseEdge, getBezierPath, type EdgeProps } from '@xyflow/react';
import { EDGE_FLOW_PULSE_DURATION_MS } from './useEdgeFlowPulses';
import type { FlowEdgeData } from './workflow';

type PulseMarkerProps = {
  className: string;
  path: string;
};

function PulseMarker({ className, path }: PulseMarkerProps) {
  const animationRef = useRef<SVGAnimateMotionElement | null>(null);

  useEffect(() => {
    // SMIL 的 `begin="0s"` 绑定到 SVG 文档时间轴；动态插入时会直接落到终点。
    // 这里在圆点挂载后显式启动，保证每个 pulse 都从源端沿边运动。
    animationRef.current?.beginElement();
  }, []);

  return (
    <circle r={4} className={className}>
      <animateMotion
        ref={animationRef}
        path={path}
        dur={`${EDGE_FLOW_PULSE_DURATION_MS}ms`}
        begin="indefinite"
        fill="freeze"
      />
    </circle>
  );
}

/** ReactFlow 自定义边：基础连线仍由 BaseEdge 负责，额外叠加短生命周期脉冲圆点。 */
export function FlowPulseEdge(props: EdgeProps & { data: FlowEdgeData }) {
  const [path] = getBezierPath(props);
  const pulses = props.data?.pulses ?? [];
  return (
    <>
      <BaseEdge
        id={props.id}
        path={path}
        style={props.style}
        markerStart={props.markerStart}
        markerEnd={props.markerEnd}
        interactionWidth={props.interactionWidth}
      />
      {pulses.map((pulse) => {
        const className = pulse.packetKind.includes('frame') ? 'edge-flow-pulse frame' : 'edge-flow-pulse control';
        return <PulseMarker key={pulse.id} className={className} path={path} />;
      })}
    </>
  );
}
