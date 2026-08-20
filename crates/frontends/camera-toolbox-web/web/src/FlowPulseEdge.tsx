import { useEffect, useSyncExternalStore } from 'react';
import { BaseEdge, useNodes, type EdgeProps } from '@xyflow/react';
import { portKindColor } from './nodes/shared';
import type { FlowEdgeData } from './workflow';

const ROUTE_EXIT_OFFSET = 28;
const OBSTACLE_CLEARANCE = 20;
// 障碍物本身已扩展 OBSTACLE_CLEARANCE；网格只额外留 1px，避免吞掉合法窄通道。
const GRID_OBSTACLE_MARGIN = 1;
const LOCAL_LANE_PITCH = 16;
const LOCAL_LANE_SLOTS = 7;
const MIN_DOGLEG_DIAGONAL = 20;

type CanvasPoint = { x: number; y: number };
type Obstacle = { left: number; right: number; top: number; bottom: number };
type FlowNodeBounds = {
  id: string;
  hidden?: boolean;
  position: CanvasPoint;
  measured?: { width?: number; height?: number };
};

let edgePathSnapshot = new Map<string, string>();
const edgePaths = new Map<string, string>();
const edgePathListeners = new Set<() => void>();

function publishEdgePaths() {
  edgePathSnapshot = new Map(edgePaths);
  edgePathListeners.forEach((listener) => listener());
}

export function registerFlowEdgePath(edgeId: string, path: string): () => void {
  if (edgePaths.get(edgeId) !== path) {
    edgePaths.set(edgeId, path);
    publishEdgePaths();
  }
  return () => {
    if (edgePaths.get(edgeId) === path) {
      edgePaths.delete(edgeId);
      publishEdgePaths();
    }
  };
}

export function useFlowEdgePaths(): ReadonlyMap<string, string> {
  return useSyncExternalStore(
    (listener) => {
      edgePathListeners.add(listener);
      return () => {
        edgePathListeners.delete(listener);
      };
    },
    () => edgePathSnapshot,
    () => edgePathSnapshot,
  );
}

/**
 * ReactFlow 自定义边：仅生成水平、垂直或 45° 的折线。
 * 优先选择不穿过其他节点边界的候选路径；复杂布局通过可见网格搜索横竖通道。
 */
export function FlowPulseEdge(props: EdgeProps & { data: FlowEdgeData }) {
  const nodes = useNodes();
  const source = { x: props.sourceX, y: props.sourceY };
  const target = { x: props.targetX, y: props.targetY };
  const obstacles = collectObstacles(nodes, props.source, props.target);
  const path = selectRoute(source, target, obstacles, localLaneOffset(props.id));
  const style = {
    ...props.style,
    stroke: portKindColor(props.data.kind),
    strokeLinecap: 'square' as const,
    strokeLinejoin: 'miter' as const,
  };

  useEffect(() => registerFlowEdgePath(props.id, path), [props.id, path]);

  return (
    <>
      {props.selected && (
        <BaseEdge
          id={`${props.id}-selection-outline`}
          path={path}
          style={{ stroke: '#f8fafc', strokeWidth: 7, opacity: 0.82, strokeLinecap: 'square', strokeLinejoin: 'miter' }}
          interactionWidth={0}
        />
      )}
      <BaseEdge
        id={props.id}
        path={path}
        style={style}
        markerStart={props.markerStart}
        markerEnd={props.markerEnd}
        interactionWidth={props.interactionWidth}
      />
    </>
  );
}

function collectObstacles(nodes: FlowNodeBounds[], sourceId: string, targetId: string): Obstacle[] {
  return nodes.flatMap((node) => {
    if (node.hidden || node.id === sourceId || node.id === targetId) {
      return [];
    }
    const width = node.measured?.width;
    const height = node.measured?.height;
    if (!width || !height) {
      return [];
    }
    return [{
      left: node.position.x - OBSTACLE_CLEARANCE,
      right: node.position.x + width + OBSTACLE_CLEARANCE,
      top: node.position.y - OBSTACLE_CLEARANCE,
      bottom: node.position.y + height + OBSTACLE_CLEARANCE,
    }];
  });
}

function selectRoute(source: CanvasPoint, target: CanvasPoint, obstacles: Obstacle[], laneOffset: number): string {
  const direct = octilinearDirectRoute(source, target, laneOffset);
  const midpoint = midpointRoute(source, target, laneOffset);
  const candidates = [direct, midpoint, ...detourRoutes(source, target, obstacles, laneOffset)].filter(
    (route): route is CanvasPoint[] => route !== null,
  );
  const selected = candidates.find((route) => isClearOfObstacles(route, obstacles));
  if (selected) {
    return toSvgPath(selected);
  }
  return toSvgPath(visibilityGridRoute(source, target, obstacles) ?? midpoint);
}

/** 正常左右布局用 H→45°→H，长度不足时交给正交中线路径。 */
function octilinearDirectRoute(source: CanvasPoint, target: CanvasPoint, laneOffset: number): CanvasPoint[] | null {
  const dx = target.x - source.x;
  const dy = target.y - source.y;
  if (dx <= 0) {
    return null;
  }
  if (dy === 0) {
    return sameRowDoglegRoute(source, target, laneOffset);
  }
  const diagonalLength = Math.abs(dy);
  if (dx < diagonalLength) {
    return null;
  }
  const horizontalLength = Math.max(0, Math.min(dx - diagonalLength, (dx - diagonalLength) / 2 + laneOffset));
  return compactPoints([
    source,
    { x: source.x + horizontalLength, y: source.y },
    { x: source.x + horizontalLength + diagonalLength, y: target.y },
    target,
  ]);
}

/** 同高端点也保留两个 45° 折点，避免常见水平边退化成视觉上未改变的直线。 */
function sameRowDoglegRoute(source: CanvasPoint, target: CanvasPoint, laneOffset: number): CanvasPoint[] | null {
  const dx = target.x - source.x;
  const availableDiagonal = (dx - ROUTE_EXIT_OFFSET * 2) / 2;
  if (availableDiagonal < MIN_DOGLEG_DIAGONAL) {
    return null;
  }
  const signedOffset = laneOffset === 0 ? MIN_DOGLEG_DIAGONAL : laneOffset;
  const diagonalLength = Math.min(Math.abs(signedOffset), availableDiagonal);
  const horizontalLength = (dx - diagonalLength * 2) / 2;
  const doglegY = source.y + Math.sign(signedOffset) * diagonalLength;
  return compactPoints([
    source,
    { x: source.x + horizontalLength, y: source.y },
    { x: source.x + horizontalLength + diagonalLength, y: doglegY },
    { x: source.x + horizontalLength + diagonalLength * 2, y: source.y },
    target,
  ]);
}

function midpointRoute(source: CanvasPoint, target: CanvasPoint, laneOffset: number): CanvasPoint[] {
  const lowerBound = Math.min(source.x, target.x);
  const upperBound = Math.max(source.x, target.x);
  const middleX = Math.max(lowerBound, Math.min(upperBound, (source.x + target.x) / 2 + laneOffset));
  return compactPoints([source, { x: middleX, y: source.y }, { x: middleX, y: target.y }, target]);
}

/** 上下通道仅在源端能先水平离开、目标端能水平进入时尝试。 */
function detourRoutes(source: CanvasPoint, target: CanvasPoint, obstacles: Obstacle[], laneOffset: number): CanvasPoint[][] {
  const entryX = source.x + ROUTE_EXIT_OFFSET;
  const exitX = target.x - ROUTE_EXIT_OFFSET;
  if (entryX >= exitX || obstacles.length === 0) {
    return [];
  }
  const laneClearance = ROUTE_EXIT_OFFSET + Math.abs(laneOffset);
  const top = Math.min(...obstacles.map((obstacle) => obstacle.top)) - laneClearance;
  const bottom = Math.max(...obstacles.map((obstacle) => obstacle.bottom)) + laneClearance;
  return [top, bottom].map((corridorY) => compactPoints([
    source,
    { x: entryX, y: source.y },
    { x: entryX, y: corridorY },
    { x: exitX, y: corridorY },
    { x: exitX, y: target.y },
    target,
  ]));
}

/**
 * 快速候选都被阻断时，沿障碍物外侧形成有限可见网格，搜索不穿过节点的横竖通道。
 * 仅作为兜底，正常布局仍保留更短、更易读的八方向路径。
 */
function visibilityGridRoute(source: CanvasPoint, target: CanvasPoint, obstacles: Obstacle[]): CanvasPoint[] | null {
  if (obstacles.length === 0) {
    return null;
  }
  const xCoordinates = sortedUniqueCoordinates([
    source.x,
    target.x,
    ...obstacles.flatMap((obstacle) => [obstacle.left - GRID_OBSTACLE_MARGIN, obstacle.right + GRID_OBSTACLE_MARGIN]),
  ]);
  const yCoordinates = sortedUniqueCoordinates([
    source.y,
    target.y,
    ...obstacles.flatMap((obstacle) => [obstacle.top - GRID_OBSTACLE_MARGIN, obstacle.bottom + GRID_OBSTACLE_MARGIN]),
  ]);
  const points: Array<CanvasPoint & { xIndex: number; yIndex: number }> = [];
  const pointByGridIndex = new Map<string, number>();
  for (let yIndex = 0; yIndex < yCoordinates.length; yIndex += 1) {
    for (let xIndex = 0; xIndex < xCoordinates.length; xIndex += 1) {
      const point = { x: xCoordinates[xIndex], y: yCoordinates[yIndex] };
      if (obstacles.some((obstacle) => pointInsideObstacle(point, obstacle))) {
        continue;
      }
      pointByGridIndex.set(gridPointKey(xIndex, yIndex), points.length);
      points.push({ ...point, xIndex, yIndex });
    }
  }
  const sourceIndex = pointByGridIndex.get(gridPointKey(xCoordinates.indexOf(source.x), yCoordinates.indexOf(source.y)));
  const targetIndex = pointByGridIndex.get(gridPointKey(xCoordinates.indexOf(target.x), yCoordinates.indexOf(target.y)));
  if (sourceIndex === undefined || targetIndex === undefined) {
    return null;
  }

  const parent = new Int32Array(points.length);
  parent.fill(-2);
  parent[sourceIndex] = -1;
  const queue = [sourceIndex];
  for (let head = 0; head < queue.length && parent[targetIndex] === -2; head += 1) {
    const currentIndex = queue[head];
    const current = points[currentIndex];
    for (const [xIndex, yIndex] of [
      [current.xIndex - 1, current.yIndex],
      [current.xIndex + 1, current.yIndex],
      [current.xIndex, current.yIndex - 1],
      [current.xIndex, current.yIndex + 1],
    ]) {
      const neighborIndex = pointByGridIndex.get(gridPointKey(xIndex, yIndex));
      if (neighborIndex === undefined || parent[neighborIndex] !== -2) {
        continue;
      }
      if (!isClearOfObstacles([current, points[neighborIndex]], obstacles)) {
        continue;
      }
      parent[neighborIndex] = currentIndex;
      queue.push(neighborIndex);
    }
  }
  if (parent[targetIndex] === -2) {
    return null;
  }
  const route: CanvasPoint[] = [];
  for (let index = targetIndex; index !== -1; index = parent[index]) {
    route.push(points[index]);
  }
  return compactOrthogonalPoints(route.reverse());
}

function sortedUniqueCoordinates(coordinates: number[]): number[] {
  return [...new Set(coordinates)].sort((left, right) => left - right);
}

function gridPointKey(xIndex: number, yIndex: number): string {
  return `${xIndex}:${yIndex}`;
}

function compactOrthogonalPoints(points: CanvasPoint[]): CanvasPoint[] {
  const distinct = compactPoints(points);
  return distinct.filter((point, index) => {
    if (index === 0 || index === distinct.length - 1) {
      return true;
    }
    const previous = distinct[index - 1];
    const next = distinct[index + 1];
    return !((previous.x === point.x && point.x === next.x) || (previous.y === point.y && point.y === next.y));
  });
}

/** 同一图的边 ID 固定映射到对称的局部轨道，避免 render 顺序导致跳线。 */
function localLaneOffset(edgeId: string): number {
  let hash = 0;
  for (let index = 0; index < edgeId.length; index += 1) {
    hash = (hash * 31 + edgeId.charCodeAt(index)) | 0;
  }
  const lane = (Math.abs(hash) % LOCAL_LANE_SLOTS) - Math.floor(LOCAL_LANE_SLOTS / 2);
  return lane * LOCAL_LANE_PITCH;
}

/** 临时连线与已提交边使用同一八方向几何，但不占用局部轨道。 */
export function octilinearPreviewPath(source: CanvasPoint, target: CanvasPoint): string {
  return toSvgPath(octilinearDirectRoute(source, target, 0) ?? midpointRoute(source, target, 0));
}

function compactPoints(points: CanvasPoint[]): CanvasPoint[] {
  return points.filter((point, index) => index === 0 || point.x !== points[index - 1].x || point.y !== points[index - 1].y);
}

function toSvgPath(points: CanvasPoint[]): string {
  return points.map((point, index) => `${index === 0 ? 'M' : 'L'} ${point.x} ${point.y}`).join(' ');
}

function isClearOfObstacles(points: CanvasPoint[], obstacles: Obstacle[]): boolean {
  return points.slice(1).every((point, index) => !obstacles.some((obstacle) => segmentIntersectsObstacle(points[index], point, obstacle)));
}

/** 采用闭区间 slab 裁剪；擦过安全边界也视为碰撞，避免线段贴住节点边缘。 */
function segmentIntersectsObstacle(start: CanvasPoint, end: CanvasPoint, obstacle: Obstacle): boolean {
  const dx = end.x - start.x;
  const dy = end.y - start.y;
  let entering = 0;
  let leaving = 1;
  for (const [direction, distance] of [
    [-dx, start.x - obstacle.left],
    [dx, obstacle.right - start.x],
    [-dy, start.y - obstacle.top],
    [dy, obstacle.bottom - start.y],
  ]) {
    if (direction === 0) {
      if (distance < 0) {
        return false;
      }
      continue;
    }
    const intersection = distance / direction;
    if (direction < 0) {
      if (intersection > leaving) {
        return false;
      }
      entering = Math.max(entering, intersection);
    } else {
      if (intersection < entering) {
        return false;
      }
      leaving = Math.min(leaving, intersection);
    }
  }
  return entering <= leaving;
}

function pointInsideObstacle(point: CanvasPoint, obstacle: Obstacle): boolean {
  return point.x >= obstacle.left && point.x <= obstacle.right && point.y >= obstacle.top && point.y <= obstacle.bottom;
}
