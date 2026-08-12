import type { WorkflowNode } from './workflow';

export function configText(node: WorkflowNode, key: string, fallback: string): string {
  const value = node.config[key];
  return typeof value === 'string' || typeof value === 'number' ? String(value) : fallback;
}

export function normalizeSourcePathDraft(value: string): string {
  return value.trim().replace(/^\/+/, '').split('/').filter((component) => component && component !== '.').join('/');
}
